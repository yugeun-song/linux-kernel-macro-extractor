use rayon::prelude::*;

use crate::analysis::candidates::Reach;
use crate::analysis::context::ContextChoice;
use crate::analysis::expansion::{Alternatives, TreeOpts};
use crate::analysis::kconf;
use crate::analysis::report::Report;
use crate::analysis::show::{self, ShowOptions};
use crate::cli::{Global, ScopeArg, Shown};
use crate::error::{Error, Result, exit};
use crate::store::Memo;
use crate::workspace::Workspace;

pub fn context_choice(ws: &Workspace, g: &Global) -> Result<ContextChoice> {
    if let Some(spec) = &g.context_file {
        let (rel, line) = super::validate::location(ws, spec, "--in")?;
        return Ok(ContextChoice::File { rel, line });
    }
    if !g.include.is_empty() {
        let specs = g
            .include
            .iter()
            .map(|h| super::validate::header(ws, h))
            .collect::<Result<Vec<_>>>()?;
        return Ok(ContextChoice::Includes(specs));
    }
    Ok(ContextChoice::Auto)
}

pub fn reach(scope: ScopeArg) -> Reach {
    match scope {
        ScopeArg::Kernel => Reach::Kernel,
        ScopeArg::All => Reach::All,
    }
}

pub fn options(ws: &Workspace, g: &Global) -> Result<(ShowOptions, Vec<String>)> {
    let alternatives = match g.shown(Shown::Active).map_err(Error::Usage)? {
        Shown::Active => Alternatives::None,
        _ => Alternatives::All,
    };
    let mut opts = ShowOptions {
        context: context_choice(ws, g)?,
        tree: TreeOpts {
            max_depth: g.depth,
            alternatives,
            reach: reach(g.scope),
            full_bodies: g.shown(Shown::Active) == Ok(Shown::All),
        },
        autoconf: None,
        what_if: g.set.clone(),
        census: !g.no_census,
    };
    let mut notes = Vec::new();
    if !g.set.is_empty() {
        let w = kconf::what_if(ws, &g.set).map_err(Error::Usage)?;
        opts.autoconf = Some(w.autoconf);
        notes = w.notes;
    }
    Ok((opts, notes))
}

pub fn lite_options(ws: &Workspace, g: &Global) -> Result<ShowOptions> {
    let (mut opts, _) = options(ws, g)?;
    opts.census = false;
    opts.tree.alternatives = Alternatives::None;
    Ok(opts)
}

pub fn memo_key(kind: &str, subject: &str, g: &Global) -> String {
    Memo::key(&[
        kind,
        subject,
        &format!("{:?}", g.context_file),
        &format!("{:?}", g.include),
        &format!("{:?}", g.set),
        &format!("{:?}", g.depth),
        &format!("{:?}", g.scope),
        &format!("{}", g.shown(Shown::Active) == Ok(Shown::Active)),
        &format!("{}", g.no_census),
        crate::store::build_id(),
    ])
}

pub const BATCH: usize = 4096;

pub struct Batch<'a> {
    pub kind: &'a str,
    pub notes: &'a [String],
    pub store: bool,
}

impl<'a> Batch<'a> {
    pub fn new(kind: &'a str, total: usize) -> Batch<'a> {
        Batch {
            kind,
            notes: &[],
            store: total <= BATCH,
        }
    }
}

pub fn reports_each(
    ws: &Workspace,
    names: &[String],
    opts: &ShowOptions,
    g: &Global,
    batch: &Batch<'_>,
    emit: &mut dyn FnMut(Report),
) {
    let Batch { kind, notes, store } = *batch;
    let memo = ws.memo();
    let mut cached: Vec<Option<Report>> = names
        .par_iter()
        .map(|n| {
            (!g.no_memo)
                .then(|| memo.get(&memo_key(kind, n, g)))
                .flatten()
                .and_then(|b| serde_json::from_slice(&b).ok())
        })
        .collect();
    let missing: Vec<usize> = (0..names.len()).filter(|&i| cached[i].is_none()).collect();
    let todo: Vec<String> = missing.iter().map(|&i| names[i].clone()).collect();
    let finalize = |k: usize, r: &mut Report| {
        r.notes.extend(notes.iter().cloned());
        if store
            && !g.no_memo
            && let Ok(b) = serde_json::to_vec(r)
        {
            memo.put(&memo_key(kind, &todo[k], g), &b);
        }
    };
    let mut at = 0;
    let mut flush = |cached: &mut Vec<Option<Report>>, emit: &mut dyn FnMut(Report)| {
        while let Some(slot) = cached.get_mut(at)
            && let Some(r) = slot.take()
        {
            emit(r);
            at += 1;
        }
    };
    flush(&mut cached, emit);
    if !todo.is_empty() {
        show::show_each(ws, &todo, opts, &finalize, &mut |k, r| {
            cached[missing[k]] = Some(r);
            flush(&mut cached, emit);
        });
    }
}

pub fn reports(
    ws: &Workspace,
    names: &[String],
    opts: &ShowOptions,
    g: &Global,
    kind: &str,
) -> Vec<Report> {
    let mut out = Vec::with_capacity(names.len());
    let batch = Batch::new(kind, names.len());
    reports_each(ws, names, opts, g, &batch, &mut |r| out.push(r));
    out
}

pub fn run(ws: &Workspace, names: &[String], g: &Global) -> Result<i32> {
    let (opts, what_if_notes) = options(ws, g)?;
    let mut code = exit::OK;
    let mut out = crate::render::Output::open(g);
    let mut failed = None;
    let batch = Batch {
        notes: &what_if_notes,
        ..Batch::new("show", names.len())
    };
    for group in names.chunks(BATCH) {
        reports_each(ws, group, &opts, g, &batch, &mut |report| {
            if failed.is_some() {
                return;
            }
            if report.kind == crate::analysis::report::SubjectKind::NotFound {
                code = exit::NOT_FOUND;
                eprintln!(
                    "kmx: error: {}: {}",
                    report.query,
                    report.notes.first().map_or("not found", String::as_str)
                );
                return;
            }
            if g.json {
                match serde_json::to_string(&report) {
                    Ok(line) => out.emit(&format!("{line}\n")),
                    Err(e) => failed = Some(Error::Internal(e.to_string())),
                }
            } else {
                out.emit(&crate::render::report_text(&report, g));
            }
        });
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(code),
    }
}
