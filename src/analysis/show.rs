use std::path::PathBuf;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use rayon::prelude::*;
use rustc_hash::FxHashMap;

use super::build_graph::BuildGraph;
use super::candidates::{self, Cand, Class};
use super::census::{HeaderOutcome, ModuleVariant};
use super::context::{self, ContextChoice, Ctx, SimInfo, context_info, contexts};
use super::enums::{self, EnumLookup, EnumPlan};
use super::expansion::{Builder, TreeOpts};
use super::kconf;
use super::report::{
    Candidate, Condition, ContextInfo, ExpNode, Report, Site, Status, SubjectKind, Value,
};
use crate::index::DefKind;
use crate::pp::kbuild::{CompileModel, DefineOp};
use crate::pp::sim::{DiagKind, Main, Sim};
use crate::syntax::lex::{Token, TokenKind};
use crate::workspace::Workspace;

#[derive(Clone, Debug)]
pub struct ShowOptions {
    pub context: ContextChoice,
    pub tree: TreeOpts,
    pub autoconf: Option<String>,
    pub what_if: Vec<String>,
    pub census: bool,
}

pub fn empty_report(ws: &Workspace, name: &str, kind: SubjectKind) -> Report {
    Report {
        query: name.to_string(),
        kind,
        context: context::tree_info(ws),
        active: None,
        active_conditions: Vec::new(),
        expansion: None,
        value: None,
        candidates: Vec::new(),
        census: None,
        enum_info: None,
        kconfig: None,
        tu_dependent: None,
        notes: Vec::new(),
        warnings: Vec::new(),
        conclusion: None,
    }
}

pub fn tu_dependence(
    report: &Report,
    cands: &[Cand],
    name: &str,
    header_variants: usize,
) -> Option<String> {
    let local: Vec<&str> = cands
        .iter()
        .filter(|c| c.class == Class::TuLocal && c.posting.kind != DefKind::Undef)
        .map(|c| c.path.as_str())
        .collect();
    if !local.is_empty() {
        return Some(format!(
            "defined inside {} source file(s), so the value depends on the TU",
            local.len()
        ));
    }
    if header_variants > 1 {
        return Some(format!(
            "{header_variants} headers define it differently, so the value depends on which one a TU includes"
        ));
    }
    let fallback = report
        .active_conditions
        .iter()
        .any(|c| c.directive.starts_with("#ifndef") && c.atoms.iter().any(|a| a.text == name));
    if fallback {
        return Some(format!(
            "fallback definition under #ifndef {name}: a TU may define it first"
        ));
    }
    const PER_OBJECT: [&str; 5] = [
        "MODULE",
        "KBUILD_MODNAME",
        "KBUILD_BASENAME",
        "KBUILD_MODFILE",
        "__KBUILD_MODNAME",
    ];
    report
        .active_conditions
        .iter()
        .flat_map(|c| c.atoms.iter().chain(&c.prior))
        .find(|a| PER_OBJECT.iter().any(|p| a.text.contains(p)))
        .map(|a| format!("selected by `{}`, which kbuild sets per object", a.text))
}

pub fn show(ws: &Workspace, name: &str, opts: &ShowOptions) -> Report {
    complete(ws, show_inner(ws, name, opts), opts)
}

fn complete(ws: &Workspace, mut report: Report, opts: &ShowOptions) -> Report {
    kconf::attach(ws, &mut report, &opts.what_if);
    if report.kind == SubjectKind::NotFound && report.kconfig.is_some() {
        report.kind = SubjectKind::Config;
        report.notes.clear();
        if let Some(k) = &report.kconfig {
            report.notes.push(format!(
                "{} = {}: not defined as a macro because autoconf.h only defines enabled symbols",
                k.symbol, k.value
            ));
        }
    }
    report
}

struct DefOutcome {
    site: Option<Site>,
    value: Option<Value>,
    conditions: Vec<Condition>,
    expansion: Option<ExpNode>,
    truncated: bool,
    conclusion: Option<String>,
}

struct ActiveDefinition {
    site: Option<Site>,
    label: String,
    used_by: Option<u32>,
    decides: Option<u32>,
}

struct CtxOutcome {
    origin: Ctx,
    label: String,
    header: Option<PathBuf>,
    covers: Vec<PathBuf>,
    used_by: Option<u32>,
    info: ContextInfo,
    warnings: Vec<String>,
    judged: Vec<Option<Candidate>>,
    def: Option<DefOutcome>,
    extra: Vec<String>,
}

const AUTO_INCLUDE_ROUNDS: usize = 3;

fn with_includes(main: &Main, extra: &[String]) -> Main {
    match main {
        Main::Virtual { name, content } if !extra.is_empty() => {
            let mut content = content.clone();
            for h in extra {
                content.push_str(&format!("#include {h}\n"));
            }
            Main::Virtual {
                name: name.clone(),
                content,
            }
        }
        m => m.clone(),
    }
}

fn fresh_sim<'w>(
    ws: &'w Workspace,
    ctx: &Ctx,
    extra: &[String],
    opts: &ShowOptions,
) -> (Sim<'w>, SimInfo) {
    let (mut sim, info) = context::sim_for(ws, ctx.model.clone(), opts.autoconf.clone());
    if let Some((p, l)) = ctx.stop.clone() {
        sim.set_stop(p, l);
    }
    sim.run(with_includes(&ctx.main, extra));
    sim.symbolic_builtins = true;
    (sim, info)
}

struct Job<'a, 'w> {
    ws: &'w Workspace,
    name: &'a str,
    cands: &'a [Cand],
    opts: &'a ShowOptions,
    ctx: &'a Ctx,
    used_by: Option<u32>,
    headers: &'a [PathBuf],
    auto: bool,
}

#[derive(Default)]
struct Rounds {
    extra: Vec<String>,
    covers: Option<Vec<PathBuf>>,
    rounds: usize,
    last_batch: usize,
    first_site: Option<Option<(String, u32)>>,
}

enum Step {
    Again,
    Done(Box<CtxOutcome>),
}

fn analyze_context<'w>(
    job: &Job<'_, 'w>,
    st: &mut Rounds,
    sim: &mut Sim<'w>,
    info: &SimInfo,
) -> Step {
    let (ws, ctx, opts) = (job.ws, job.ctx, job.opts);
    let covers = st
        .covers
        .get_or_insert_with(|| {
            job.headers
                .iter()
                .filter(|h| Some(h.as_path()) != ctx.header.as_deref())
                .filter(|h| sim.files.lookup(h).is_some_and(|f| sim.visited(f)))
                .cloned()
                .collect()
        })
        .clone();
    let sym = sim.it.intern(job.name);
    let found = sim.def_of(sym);
    let site = found.as_ref().and_then(|d| {
        let mut b = Builder::new(sim, ws, &ctx.model, &opts.tree);
        b.site_of(d)
    });
    let key = site.as_ref().map(|s| (s.path.clone(), s.line));
    match &st.first_site {
        None => st.first_site = Some(key),
        Some(first) if *first != key => {
            st.extra.truncate(st.last_batch);
            st.rounds = AUTO_INCLUDE_ROUNDS;
            return Step::Again;
        }
        Some(_) => {}
    }
    let mut def = None;
    if let Some(d) = found {
        let mut b = Builder::new(sim, ws, &ctx.model, &opts.tree);
        let conditions = b.why(&d);
        let call = b.invocation(&d);
        drop(b);
        let (out, trace, truncated) = sim.expand_text(call, true);
        if job.auto && st.rounds < AUTO_INCLUDE_ROUNDS {
            let more = candidates::headers_defining_undefined(ws, sim, &out, &ctx.model, &st.extra);
            if !more.is_empty() {
                st.last_batch = st.extra.len();
                st.extra.extend(more);
                st.rounds += 1;
                return Step::Again;
            }
        }
        let conclusion = (!truncated)
            .then(|| super::c_expr::simplify(&out, &sim.it, &rustc_hash::FxHashSet::default()))
            .flatten();
        let mut b = Builder::new(sim, ws, &ctx.model, &opts.tree);
        let expansion = trace.and_then(|t| b.build(&t).into_iter().next());
        let value = if d.is_function() {
            None
        } else {
            let (out, _, _) = b
                .sim
                .expand_text(vec![Token::new(TokenKind::Ident, sym, 0)], false);
            b.value_of(&out)
        };
        def = Some(DefOutcome {
            site,
            value,
            conditions,
            expansion,
            truncated,
            conclusion,
        });
    }
    let info = context_info(ws, info, &ctx.label, &opts.what_if);
    let warnings = diag_warnings(sim);
    let judged = job
        .cands
        .iter()
        .map(|c| candidates::judge(sim, sym, c).map(|j| candidates::to_candidate(sim, ws, c, &j)))
        .collect();
    Step::Done(Box::new(CtxOutcome {
        origin: ctx.clone(),
        label: ctx.label.clone(),
        header: ctx.header.clone(),
        covers,
        used_by: job.used_by,
        info,
        warnings,
        judged,
        def,
        extra: st.extra.clone(),
    }))
}

fn run_context(
    ws: &Workspace,
    name: &str,
    cands: &[Cand],
    opts: &ShowOptions,
    ctx: Ctx,
    used_by: Option<u32>,
    headers: &[PathBuf],
) -> CtxOutcome {
    let job = Job {
        ws,
        name,
        cands,
        opts,
        ctx: &ctx,
        used_by,
        headers,
        auto: ctx.header.is_some() && matches!(ctx.main, Main::Virtual { .. }),
    };
    continue_rounds(&job, Rounds::default())
}

fn continue_rounds(job: &Job<'_, '_>, mut st: Rounds) -> CtxOutcome {
    loop {
        let (mut sim, info) = fresh_sim(job.ws, job.ctx, &st.extra, job.opts);
        if let Step::Done(outcome) = analyze_context(job, &mut st, &mut sim, &info) {
            return *outcome;
        }
    }
}

fn selected_by_module(report: &Report) -> bool {
    report
        .active_conditions
        .iter()
        .flat_map(|c| c.atoms.iter().chain(&c.prior))
        .any(|a| a.text == "MODULE")
}

fn module_variant(
    ws: &Workspace,
    name: &str,
    opts: &ShowOptions,
    mut ctx: Ctx,
    builtin: Option<Site>,
) -> Option<ModuleVariant> {
    ctx.model
        .ops
        .push(DefineOp::Define("MODULE".into(), "1".into()));
    let d = run_context(ws, name, &[], opts, ctx, None, &[]).def?;
    let differs = match (&d.site, &builtin) {
        (Some(a), Some(b)) => a.path != b.path || a.line != b.line,
        _ => true,
    };
    differs.then_some(ModuleVariant {
        builtin,
        site: d.site,
        value: d.value,
    })
}

struct Plan<'w> {
    report: Report,
    cands: Vec<Cand>,
    ctxs: Vec<Ctx>,
    used_by: Vec<Option<u32>>,
    all_headers: Vec<PathBuf>,
    usage: Option<&'w BuildGraph>,
    config_query: bool,
}

enum Planned<'w> {
    Done(Report),
    Ready(Plan<'w>),
    Enum(Box<EnumPlan<'w>>),
}

fn show_inner(ws: &Workspace, name: &str, opts: &ShowOptions) -> Report {
    match plan(ws, name, opts) {
        Planned::Done(r) => r,
        Planned::Enum(p) => {
            let (mut sim, info) = enums::enum_sim(ws, &p.ctx, opts);
            let body = enums::enum_body(ws, &p, &mut sim);
            enums::enum_report(ws, name, &p, &body, &mut sim, &info, opts)
        }
        Planned::Ready(p) => {
            let outcomes: Vec<CtxOutcome> = p
                .ctxs
                .par_iter()
                .zip(&p.used_by)
                .map(|(ctx, used_by)| {
                    run_context(
                        ws,
                        name,
                        &p.cands,
                        opts,
                        ctx.clone(),
                        *used_by,
                        &p.all_headers,
                    )
                })
                .collect();
            finish(ws, name, opts, &p, outcomes)
        }
    }
}

fn context_key(ctx: &Ctx, opts: &ShowOptions) -> String {
    let autoconf = opts
        .autoconf
        .as_deref()
        .map(|a| crate::store::hash_hex(a.as_bytes()));
    format!(
        "{:?}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}",
        ctx.model, ctx.main, ctx.stop, autoconf
    )
}

enum Work {
    Macro(usize, usize),
    Enum(usize),
}

enum Outcome {
    Ctx(usize, Box<CtxOutcome>),
    Report(Box<Report>),
}

struct Progress {
    outs: Vec<Option<CtxOutcome>>,
    left: usize,
    report: Option<Report>,
}

fn run_group<'w>(
    ws: &'w Workspace,
    names: &[String],
    opts: &ShowOptions,
    planned: &[Planned<'w>],
    jobs: &[Work],
    done: &mut dyn FnMut(usize, Outcome),
) {
    let ctx = match jobs.first() {
        Some(&Work::Macro(i, j)) => match &planned[i] {
            Planned::Ready(p) => &p.ctxs[j],
            _ => return,
        },
        Some(&Work::Enum(i)) => match &planned[i] {
            Planned::Enum(e) => &e.ctx,
            _ => return,
        },
        None => return,
    };
    let (mut sim, info) = fresh_sim(ws, ctx, &[], opts);
    let mut body: Option<((PathBuf, u32), enums::EnumBody)> = None;
    for w in jobs {
        match *w {
            Work::Macro(i, j) => {
                let Planned::Ready(p) = &planned[i] else {
                    continue;
                };
                let ctx = &p.ctxs[j];
                let job = Job {
                    ws,
                    name: &names[i],
                    cands: &p.cands,
                    opts,
                    ctx,
                    used_by: p.used_by[j],
                    headers: &p.all_headers,
                    auto: ctx.header.is_some() && matches!(ctx.main, Main::Virtual { .. }),
                };
                let mut st = Rounds::default();
                let outcome = match analyze_context(&job, &mut st, &mut sim, &info) {
                    Step::Done(o) => o,
                    Step::Again => Box::new(continue_rounds(&job, st)),
                };
                done(i, Outcome::Ctx(j, outcome));
            }
            Work::Enum(i) => {
                let Planned::Enum(e) = &planned[i] else {
                    continue;
                };
                let key = e.decl_key();
                if body.as_ref().is_none_or(|(k, _)| *k != key) {
                    body = Some((key, enums::enum_body(ws, e, &mut sim)));
                }
                let Some((_, b)) = &body else { continue };
                let r = enums::enum_report(ws, &names[i], e, b, &mut sim, &info, opts);
                done(i, Outcome::Report(Box::new(r)));
            }
        }
    }
}

pub fn show_each(
    ws: &Workspace,
    names: &[String],
    opts: &ShowOptions,
    finalize: &(dyn Fn(usize, &mut Report) + Sync),
    emit: &mut dyn FnMut(usize, Report),
) {
    let mut planned: Vec<Planned<'_>> = names
        .par_iter()
        .enumerate()
        .map(|(i, n)| {
            let report = match plan(ws, n, opts) {
                Planned::Ready(p) if p.ctxs.is_empty() => finish(ws, n, opts, &p, Vec::new()),
                Planned::Done(r) => r,
                p => return p,
            };
            let mut r = complete(ws, report, opts);
            finalize(i, &mut r);
            Planned::Done(r)
        })
        .collect();
    let mut groups: Vec<Vec<Work>> = Vec::new();
    let mut index: FxHashMap<String, usize> = FxHashMap::default();
    let mut progress: Vec<Progress> = Vec::with_capacity(planned.len());
    for (i, p) in planned.iter_mut().enumerate() {
        let mut add = |key: String, w: Work| {
            let g = *index.entry(key).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[g].push(w);
        };
        progress.push(match p {
            Planned::Ready(plan) => {
                for (j, ctx) in plan.ctxs.iter().enumerate() {
                    add(context_key(ctx, opts), Work::Macro(i, j));
                }
                Progress {
                    outs: plan.ctxs.iter().map(|_| None).collect(),
                    left: plan.ctxs.len(),
                    report: None,
                }
            }
            Planned::Enum(e) => {
                add(context_key(&e.ctx, opts), Work::Enum(i));
                Progress {
                    outs: Vec::new(),
                    left: 1,
                    report: None,
                }
            }
            Planned::Done(r) => Progress {
                outs: Vec::new(),
                left: 0,
                report: Some(std::mem::replace(
                    r,
                    empty_report(ws, "", SubjectKind::NotFound),
                )),
            },
        });
    }
    let planned = &planned;
    let groups = &groups;
    let state = Mutex::new(progress);
    let lock = || state.lock().unwrap_or_else(PoisonError::into_inner);
    let lock = &lock;
    let next = AtomicUsize::new(0);
    let next = &next;
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let drain = |at: &mut usize| -> Vec<Report> {
        let mut st = lock();
        let mut ready = Vec::new();
        while let Some(p) = st.get_mut(*at)
            && p.left == 0
            && let Some(r) = p.report.take()
        {
            ready.push(r);
            *at += 1;
        }
        ready
    };
    std::thread::scope(|s| {
        s.spawn(move || {
            rayon::scope(|rs| {
                for _ in 0..rayon::current_num_threads().min(groups.len()) {
                    let tx = tx.clone();
                    rs.spawn(move |_| {
                        while let Some(jobs) = groups.get(next.fetch_add(1, Ordering::Relaxed)) {
                            run_group(ws, names, opts, planned, jobs, &mut |i, outcome| {
                                let report = match outcome {
                                    Outcome::Report(r) => *r,
                                    Outcome::Ctx(j, o) => {
                                        let outs = {
                                            let mut st = lock();
                                            let p = &mut st[i];
                                            p.outs[j] = Some(*o);
                                            p.left -= 1;
                                            (p.left == 0).then(|| std::mem::take(&mut p.outs))
                                        };
                                        let (Some(outs), Planned::Ready(plan)) =
                                            (outs, &planned[i])
                                        else {
                                            return;
                                        };
                                        let outs = outs.into_iter().flatten().collect();
                                        finish(ws, &names[i], opts, plan, outs)
                                    }
                                };
                                let mut r = complete(ws, report, opts);
                                finalize(i, &mut r);
                                {
                                    let mut st = lock();
                                    st[i].report = Some(r);
                                    st[i].left = 0;
                                }
                                let _ = tx.send(());
                            });
                        }
                    });
                }
            });
            drop(tx);
        });
        let mut at = 0;
        loop {
            let first = at;
            for (k, r) in drain(&mut at).into_iter().enumerate() {
                emit(first + k, r);
            }
            if at == names.len() {
                break;
            }
            if rx.recv().is_err() {
                let first = at;
                for (k, r) in drain(&mut at).into_iter().enumerate() {
                    emit(first + k, r);
                }
                break;
            }
        }
    });
}

pub fn show_many(ws: &Workspace, names: &[String], opts: &ShowOptions) -> Vec<Report> {
    let mut out = Vec::with_capacity(names.len());
    show_each(ws, names, opts, &|_, _| {}, &mut |_, r| out.push(r));
    out
}

fn plan<'w>(ws: &'w Workspace, name: &str, opts: &ShowOptions) -> Planned<'w> {
    let base = CompileModel::default_for(&ws.layout);
    let cands = candidates::gather(ws, name, &base, opts.tree.reach);
    ws.prefetch_definitions(&cands);
    if cands.is_empty() {
        let lookup = enums::plan_enum(ws, name, opts.tree.reach);
        let host_enum = matches!(lookup, EnumLookup::HostOnly);
        if let EnumLookup::Found(p) = lookup {
            return Planned::Enum(p);
        }
        let mut r = empty_report(ws, name, SubjectKind::NotFound);
        let only_host = host_enum
            || (opts.tree.reach != candidates::Reach::All
                && !candidates::gather(ws, name, &base, candidates::Reach::All).is_empty());
        r.notes.push(if only_host {
            "defined only in tools/, scripts/, Documentation/ or usr/; pass --scope all to include them"
                .to_string()
        } else {
            "no macro, enum constant or Kconfig symbol of this name exists in the tree".to_string()
        });
        return Planned::Done(r);
    }
    let mut report = empty_report(ws, name, SubjectKind::Macro);
    report.context.what_if = opts.what_if.clone();
    let config_query = name.starts_with("CONFIG_") && matches!(opts.context, ContextChoice::Auto);
    let mut ctxs: Vec<Ctx> = if config_query {
        let autoconf: Vec<Cand> = cands
            .iter()
            .filter(|c| c.path.ends_with("generated/autoconf.h"))
            .cloned()
            .collect();
        contexts(ws, &opts.context, &autoconf, &base)
    } else {
        contexts(ws, &opts.context, &cands, &base)
    };
    let usage = (ctxs.len() > 1 && ctxs.iter().all(|c| c.header.is_some()))
        .then(|| ws.build_graph())
        .flatten();
    let users = |c: &Ctx| usage.zip(c.header.as_deref()).map(|(g, h)| g.usage(h));
    if usage.is_some() {
        ctxs.sort_by_key(|c| std::cmp::Reverse(users(c).unwrap_or(0)));
    }
    if config_query && ctxs.is_empty() {
        report.kind = SubjectKind::Config;
    }
    if ctxs.is_empty() && !config_query {
        report.notes.push(format!(
            "{name} has no definition that can apply to {} (only other architectures or host code define it)",
            ws.target.arch
        ));
    }
    let all_headers: Vec<PathBuf> = ctxs.iter().filter_map(|c| c.header.clone()).collect();
    let used_by = ctxs.iter().map(users).collect();
    Planned::Ready(Plan {
        report,
        cands,
        ctxs,
        used_by,
        all_headers,
        usage,
        config_query,
    })
}

fn finish(
    ws: &Workspace,
    name: &str,
    opts: &ShowOptions,
    plan: &Plan<'_>,
    outcomes: Vec<CtxOutcome>,
) -> Report {
    let mut report = plan.report.clone();
    let cands = &plan.cands;
    let usage = plan.usage;
    let config_query = plan.config_query;
    let decided: Option<Vec<u32>> = usage.map(|g| {
        let heads: Vec<HeaderOutcome> = outcomes
            .iter()
            .map(|o| HeaderOutcome {
                header: o.header.clone().unwrap_or_default(),
                site: o.def.as_ref().and_then(|d| d.site.clone()),
                value: None,
                covers: o.covers.clone(),
            })
            .collect();
        super::census::decided_by(g, &heads)
    });
    let mut ordered: Vec<(Option<u32>, CtxOutcome)> = outcomes
        .into_iter()
        .enumerate()
        .map(|(i, o)| (decided.as_ref().map(|d| d[i]), o))
        .collect();
    ordered.sort_by_key(|(d, _)| std::cmp::Reverse(*d));
    let headline = ordered.iter().position(|(_, o)| o.def.is_some());
    let labels: Vec<String> = ordered.iter().map(|(_, o)| o.label.clone()).collect();
    let mut rows: Vec<Vec<Option<Candidate>>> = ordered
        .iter_mut()
        .map(|(_, o)| std::mem::take(&mut o.judged))
        .collect();
    let mut best: Vec<Option<Candidate>> = Vec::with_capacity(cands.len());
    for i in 0..cands.len() {
        let mut pick = headline.and_then(|h| rows[h][i].take());
        if pick.is_none() {
            let mut other: Option<(usize, Candidate)> = None;
            for (k, row) in rows.iter_mut().enumerate() {
                if let Some(c) = row[i].take()
                    && other
                        .as_ref()
                        .is_none_or(|(_, b)| c.status.order() < b.status.order())
                {
                    other = Some((k, c));
                }
            }
            pick = other.map(|(k, mut c)| {
                if headline.is_some() {
                    if c.status == Status::Active {
                        c.status = Status::Elsewhere;
                    }
                    let note = format!("in {}", labels[k]);
                    c.detail = if c.detail.is_empty() {
                        note
                    } else {
                        format!("{}; {note}", c.detail)
                    };
                }
                c
            });
        }
        best.push(pick);
    }
    let headline_ctx = headline.map(|h| ordered[h].1.origin.clone());
    let mut per_context: Vec<ActiveDefinition> = Vec::new();
    let mut header_results: Vec<HeaderOutcome> = Vec::new();
    let mut expanded = false;
    for (k, (decides, o)) in ordered.into_iter().enumerate() {
        if k == 0 {
            report.context = o.info.clone();
        }
        for w in o.warnings {
            if !report.warnings.contains(&w) {
                report.warnings.push(w);
            }
        }
        let Some(d) = o.def else {
            if let Some(h) = o.header {
                header_results.push(HeaderOutcome {
                    header: h,
                    site: None,
                    value: None,
                    covers: o.covers,
                });
            }
            continue;
        };
        let value = if expanded {
            d.value
        } else {
            expanded = true;
            report.context = o.info;
            report.active_conditions = d.conditions;
            if d.truncated {
                report
                    .warnings
                    .push("expansion truncated: it exceeded the size limits".into());
            }
            report.expansion = d.expansion;
            report.conclusion = d.conclusion;
            report.value = report.expansion.as_ref().and_then(|n| n.value.clone());
            report.active = d.site.clone();
            if !o.extra.is_empty() {
                report.notes.push(format!(
                    "auto-included to define the macros used: {}",
                    o.extra.join(" ")
                ));
            }
            report.value.clone()
        };
        if let Some(h) = o.header {
            header_results.push(HeaderOutcome {
                header: h,
                site: d.site.clone(),
                value,
                covers: o.covers,
            });
        }
        per_context.push(ActiveDefinition {
            site: d.site,
            label: o.label,
            used_by: o.used_by,
            decides,
        });
    }
    let active_key = report.active.as_ref().map(|s| (s.path.clone(), s.line));
    for (i, c) in cands.iter().enumerate() {
        if best[i].is_none() {
            best[i] = Some(candidates::static_candidate(c, ws));
        }
    }
    report.candidates = best
        .into_iter()
        .flatten()
        .filter(|c| active_key.as_ref() != Some(&(c.path.clone(), c.line)))
        .collect();
    report
        .candidates
        .sort_by_key(|c| (c.status.order(), c.path.clone(), c.line));
    let mut distinct: Vec<(String, u32)> = per_context
        .iter()
        .filter_map(|w| w.site.as_ref().map(|s| (s.path.clone(), s.line)))
        .collect();
    distinct.sort();
    distinct.dedup();
    if distinct.len() > 1 {
        report.notes.push(format!(
            "context-dependent: {} different definitions win depending on which header a TU includes",
            distinct.len()
        ));
        if let Some(g) = usage {
            report.notes.push(format!(
                "  ordered by how many of the {} TUs in this build end up with each header's definition",
                g.members.len()
            ));
        }
        for w in &per_context {
            if let Some(s) = &w.site {
                let used = match (w.decides, w.used_by) {
                    (Some(d), Some(u)) => {
                        format!("; decides {d} of the {u} TU(s) that include the header")
                    }
                    _ => String::new(),
                };
                report
                    .notes
                    .push(format!("  {}:{} wins in {}{used}", s.path, s.line, w.label));
            }
        }
    }
    if report.active.is_none()
        && !report.candidates.is_empty()
        && report.notes.is_empty()
        && !config_query
    {
        report.notes.push(format!(
            "{name} is not defined in this context; every candidate below explains why"
        ));
    }
    report.tu_dependent = tu_dependence(&report, cands, name, distinct.len());
    if opts.census && report.tu_dependent.is_some() && matches!(opts.context, ContextChoice::Auto) {
        let module = headline_ctx
            .filter(|_| selected_by_module(&report))
            .and_then(|c| module_variant(ws, name, opts, c, report.active.clone()));
        report.census =
            super::census::run(ws, &report, cands, name, &header_results, module.as_ref());
        if report.census.is_none() {
            report.notes.push(
                "census skipped: the build directory has no compiled objects (.o.cmd files)".into(),
            );
        }
    }
    report
}

pub fn diag_warnings(sim: &Sim<'_>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut missing = 0;
    for d in &sim.diags {
        let w = match d.kind {
            DiagKind::Error if d.message.starts_with("#error") => format!(
                "{}:{}: {} (reached in this context)",
                sim.files.display(d.file),
                d.line,
                d.message
            ),
            DiagKind::MissingInclude => {
                missing += 1;
                continue;
            }
            DiagKind::Internal => d.message.clone(),
            _ => continue,
        };
        if !out.contains(&w) {
            out.push(w);
        }
    }
    if missing > 0 {
        out.push(format!(
            "{missing} #include(s) could not be resolved in this context"
        ));
    }
    out
}

pub fn collect_diags(sim: &Sim<'_>, report: &mut Report) {
    for w in diag_warnings(sim) {
        if !report.warnings.contains(&w) {
            report.warnings.push(w);
        }
    }
}
