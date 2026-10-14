use std::path::PathBuf;

use rustc_hash::FxHashMap;

use super::candidates::{Reach, include_spec};
use super::context::{self, Ctx, SimInfo, context_info, file_context};
use super::expansion::Builder;
use super::explain;
use super::report::{Condition, EnumInfo, EnumMember, Report, Site, SubjectKind, Value};
use super::show::{ShowOptions, empty_report};
use super::value::{self, Evaluated, ValueCtx};
use crate::index::{DefKind, ENUM_TAG_PREFIX, Posting, Scope};
use crate::intern::sym;
use crate::pp::kbuild::{CompileModel, normalize};
use crate::pp::sim::{Main, Sim};
use crate::syntax::enums::{EnumDecl, find_enums, merged_skips};
use crate::syntax::lex::{self, Token, TokenKind};
use crate::syntax::scan::scan_full;
use crate::workspace::Workspace;

struct Decl<'w> {
    abs: PathBuf,
    path: String,
    decl: EnumDecl,
    content: &'w [u8],
    line_starts: Vec<usize>,
}

impl Decl<'_> {
    fn line_of(&self, off: u32) -> u32 {
        self.line_starts.partition_point(|&s| s <= off as usize) as u32
    }
}

fn load_decl<'w>(ws: &'w Workspace, p: &Posting) -> Option<Decl<'w>> {
    let view = ws.index.file(p.file)?;
    let abs = normalize(&ws.abs(&view));
    let content = ws.file_bytes(&abs)?;
    let mut dirs = Vec::new();
    let mut skipped = Vec::new();
    scan_full(content, &mut dirs, Some(&mut skipped));
    let skips = merged_skips(&dirs, &skipped);
    let decl = find_enums(content, &skips)
        .into_iter()
        .find(|d| d.keyword == p.offset)?;
    let mut line_starts = vec![0usize];
    line_starts.extend(memchr::memchr_iter(b'\n', content).map(|i| i + 1));
    Some(Decl {
        abs,
        path: view.path.to_string(),
        decl,
        content,
        line_starts,
    })
}

fn active_body(sim: &mut Sim<'_>, d: &Decl) -> Vec<u8> {
    let start = d.decl.open as usize + 1;
    let end = d.decl.close as usize;
    let mut body = d.content[start..end].to_vec();
    let file = sim.files.lookup(&d.abs);
    if let Some(f) = file {
        for dl in sim.files.directives(f).iter() {
            let (a, b) = (dl.off as usize, dl.end as usize);
            if b <= start || a >= end {
                continue;
            }
            for i in a.max(start)..b.min(end) {
                if body[i - start] != b'\n' {
                    body[i - start] = b' ';
                }
            }
        }
        let first_line = d.line_of(start as u32);
        let last_line = d.line_of(end as u32);
        for line in first_line..=last_line {
            if sim.skip_at(f, line).is_none() {
                continue;
            }
            let ls = d.line_starts.get(line as usize - 1).copied().unwrap_or(0);
            let le = d
                .line_starts
                .get(line as usize)
                .copied()
                .unwrap_or(d.content.len());
            for i in ls.max(start)..le.min(end) {
                if body[i - start] != b'\n' {
                    body[i - start] = b' ';
                }
            }
        }
    }
    body
}

fn split_items(toks: &[Token]) -> Vec<Vec<Token>> {
    let mut out = vec![Vec::new()];
    let mut depth = 0i32;
    for t in toks {
        if t.is_punct(sym::LPAREN) || t.is_punct(sym::LBRACKET) || t.is_punct(sym::LBRACE) {
            depth += 1;
        } else if t.is_punct(sym::RPAREN) || t.is_punct(sym::RBRACKET) || t.is_punct(sym::RBRACE) {
            depth -= 1;
        }
        if depth == 0 && t.is_punct(sym::COMMA) {
            out.push(Vec::new());
            continue;
        }
        if let Some(last) = out.last_mut() {
            last.push(*t);
        }
    }
    out.retain(|i| !i.is_empty());
    out
}

struct Item {
    name: String,
    init: Option<Vec<Token>>,
    generated_by: Option<String>,
}

fn items_of(sim: &mut Sim<'_>, toks: Vec<Token>, depth: usize) -> Vec<Item> {
    let mut out = Vec::new();
    for item in split_items(&toks) {
        let first = item[0];
        let simple = first.kind == TokenKind::Ident
            && (item.len() == 1 || item.get(1).is_some_and(|t| t.is_punct(sym::EQ)))
            && sim
                .def_of(first.sym)
                .is_none_or(|d| !d.is_function() && item.len() > 1);
        if simple || depth > 4 {
            out.push(Item {
                name: sim.it.get(first.sym).to_string(),
                init: (item.len() > 2).then(|| item[2..].to_vec()),
                generated_by: None,
            });
            continue;
        }
        let label = lex::render(&item, &sim.it);
        let (expanded, _, _) = sim.expand_text(item, false);
        for mut g in items_of(sim, expanded, depth + 1) {
            g.generated_by.get_or_insert_with(|| label.clone());
            out.push(g);
        }
    }
    out
}

fn substitute(toks: &[Token], known: &FxHashMap<String, i128>, sim: &mut Sim<'_>) -> Vec<Token> {
    toks.iter()
        .map(|t| {
            if t.kind == TokenKind::Ident
                && let Some(v) = known.get(sim.it.get(t.sym))
            {
                let mut n = Token::new(TokenKind::Number, sim.it.intern(&v.to_string()), t.flags);
                n.origin = t.origin;
                return n;
            }
            *t
        })
        .collect()
}

pub struct EnumPlan<'w> {
    focus: Option<String>,
    ranked: Vec<(u8, Posting, String)>,
    decl: Decl<'w>,
    pub ctx: Ctx,
}

impl EnumPlan<'_> {
    pub fn decl_key(&self) -> (PathBuf, u32) {
        (self.decl.abs.clone(), self.decl.decl.keyword)
    }
}

pub enum EnumLookup<'w> {
    Found(Box<EnumPlan<'w>>),
    HostOnly,
    Absent,
}

pub fn plan_enum<'w>(ws: &'w Workspace, name: &str, reach: Reach) -> EnumLookup<'w> {
    let member_posts: Vec<Posting> = ws
        .index
        .lookup(name)
        .into_iter()
        .filter(|p| p.kind == DefKind::Enumerator)
        .collect();
    let tag_posts: Vec<Posting> = ws
        .index
        .lookup(&format!("{ENUM_TAG_PREFIX}{name}"))
        .into_iter()
        .filter(|p| p.kind == DefKind::EnumTag)
        .collect();
    let (posts, focus) = if !member_posts.is_empty() {
        (member_posts, Some(name.to_string()))
    } else if !tag_posts.is_empty() {
        (tag_posts, None)
    } else {
        return EnumLookup::Absent;
    };
    let srcarch = ws.target.arch.srcarch();
    let mut host = false;
    let mut ranked: Vec<(u8, Posting, String)> = posts
        .iter()
        .filter_map(|p| {
            let v = ws.index.file(p.file)?;
            let outside = !matches!(v.scope(), Scope::Kernel | Scope::Generated);
            if outside && reach == Reach::Kernel {
                host = true;
                return None;
            }
            let rank = match v.arch {
                Some(a) if a == srcarch => 0,
                None if !outside => 1,
                None => 3,
                Some(_) => 2,
            };
            Some((rank, *p, v.path.to_string()))
        })
        .collect();
    if ranked.is_empty() {
        return if host {
            EnumLookup::HostOnly
        } else {
            EnumLookup::Absent
        };
    }
    ranked.sort_by_key(|(r, p, path)| (*r, path.clone(), p.line));
    let primary = ranked[0].1;
    let Some(decl) = load_decl(ws, &primary) else {
        return EnumLookup::Absent;
    };
    let base = CompileModel::default_for(&ws.layout);
    let close_line = decl.line_of(decl.decl.close);
    let ctx = match include_spec(&base, &decl.abs) {
        Some(spec) => {
            let content = format!("#include {spec}\n");
            Ctx {
                label: format!("virtual TU: {spec}, up to the end of the enum"),
                model: base.clone(),
                main: Main::Virtual {
                    name: "<virtual TU>".into(),
                    content,
                },
                stop: Some((decl.abs.clone(), close_line + 1)),
                header: Some(decl.abs.clone()),
            }
        }
        None => {
            let mut c = file_context(ws, &base, &decl.path, Some(close_line + 1));
            c.stop = Some((decl.abs.clone(), close_line + 1));
            c
        }
    };
    EnumLookup::Found(Box::new(EnumPlan {
        focus,
        ranked,
        decl,
        ctx,
    }))
}

pub fn enum_sim<'w>(ws: &'w Workspace, ctx: &Ctx, opts: &ShowOptions) -> (Sim<'w>, SimInfo) {
    let (mut sim, info) = context::sim_for(ws, ctx.model.clone(), opts.autoconf.clone());
    if let Some((p, l)) = ctx.stop.clone() {
        sim.set_stop(p, l);
    }
    sim.run(ctx.main.clone());
    sim.symbolic_builtins = true;
    (sim, info)
}

pub struct EnumBody {
    tag: Option<String>,
    site: Site,
    excluded: Option<Condition>,
    members: Vec<EnumMember>,
    known: FxHashMap<String, i128>,
    inits: FxHashMap<String, Option<Vec<Token>>>,
}

pub fn enum_body(ws: &Workspace, plan: &EnumPlan<'_>, sim: &mut Sim<'_>) -> EnumBody {
    let decl = &plan.decl;
    let tag = decl
        .decl
        .tag
        .map(|(a, b)| String::from_utf8_lossy(&decl.content[a as usize..b as usize]).into_owned());
    let kw_line = decl.line_of(decl.decl.keyword);
    let site = Site {
        path: decl.path.clone(),
        line: kw_line,
        text: format!(
            "enum {}{{ … }}",
            tag.as_ref().map(|t| format!("{t} ")).unwrap_or_default()
        ),
        origin: "source".into(),
    };
    let mut body = EnumBody {
        tag,
        site,
        excluded: None,
        members: Vec::new(),
        known: FxHashMap::default(),
        inits: FxHashMap::default(),
    };
    let file = sim.files.lookup(&decl.abs);
    if let Some(r) = file.and_then(|f| sim.skip_at(f, kw_line)) {
        body.excluded = Some(explain::condition(sim, r.cond, &ws.config));
        return body;
    }
    let text = active_body(sim, decl);
    let toks = lex::lex_all(&text, false, &mut sim.it);
    let items = items_of(sim, toks, 0);
    let char_unsigned = ws.target.arch.srcarch() != "x86";
    let mut next: Option<i128> = Some(0);
    for it in items {
        let mut expansion = None;
        let val = match &it.init {
            None => next,
            Some(init) => {
                let pre = substitute(init, &body.known, sim);
                let (exp, _, _) = sim.expand_text(pre, false);
                let exp = substitute(&exp, &body.known, sim);
                expansion = Some(lex::render(&exp, &sim.it));
                let cx = ValueCtx {
                    it: &sim.it,
                    config: &ws.config,
                    char_unsigned,
                };
                match value::evaluate(&exp, &cx) {
                    Evaluated::Int(v) => Some(v.v),
                    _ => None,
                }
            }
        };
        body.inits.insert(it.name.clone(), it.init.clone());
        if let Some(v) = val {
            body.known.insert(it.name.clone(), v);
        }
        next = val.map(|v| v + 1);
        let line = decl
            .decl
            .members
            .iter()
            .find(|m| decl.content[m.start as usize..m.end as usize] == *it.name.as_bytes())
            .map(|m| decl.line_of(m.start))
            .unwrap_or(0);
        body.members.push(EnumMember {
            name: it.name.clone(),
            line,
            init: it.init.as_ref().map(|i| lex::render(i, &sim.it)),
            active: true,
            value: val.map(|v| Value {
                display: if v.unsigned_abs() >= 10 {
                    format!("{v} (0x{v:x})")
                } else {
                    v.to_string()
                },
                ty: Some("int".into()),
            }),
            expansion: it
                .generated_by
                .map(|g| format!("generated by {g}"))
                .or(expansion),
            excluded_by: None,
        });
    }
    if let Some(f) = file {
        for m in &decl.decl.members {
            let line = decl.line_of(m.start);
            let mname = String::from_utf8_lossy(&decl.content[m.start as usize..m.end as usize])
                .into_owned();
            if body.members.iter().any(|x| x.name == mname) {
                continue;
            }
            if let Some(r) = sim.skip_at(f, line) {
                let cond = explain::condition(sim, r.cond, &ws.config);
                body.members.push(EnumMember {
                    name: mname,
                    line,
                    init: None,
                    active: false,
                    value: None,
                    expansion: None,
                    excluded_by: Some(cond),
                });
            }
        }
    }
    body.members.sort_by_key(|m| (m.line == 0, m.line));
    body
}

pub fn enum_report<'w>(
    ws: &'w Workspace,
    name: &str,
    plan: &EnumPlan<'_>,
    body: &EnumBody,
    sim: &mut Sim<'w>,
    info: &SimInfo,
    opts: &ShowOptions,
) -> Report {
    let focus = plan.focus.clone();
    let mut report = empty_report(
        ws,
        name,
        if focus.is_some() {
            SubjectKind::EnumConstant
        } else {
            SubjectKind::EnumType
        },
    );
    report.context = context_info(ws, info, &plan.ctx.label, &opts.what_if);
    let site = body.site.clone();
    if let Some(cond) = &body.excluded {
        report.active_conditions = vec![cond.clone()];
        report.notes.push(format!(
            "the whole enum is excluded in this configuration ({}:{})",
            site.path, site.line
        ));
        report.enum_info = Some(EnumInfo {
            tag: body.tag.clone(),
            site,
            members: Vec::new(),
            focus,
        });
        return report;
    }
    if let Some(fname) = &focus {
        let m = body.members.iter().find(|m| &m.name == fname);
        report.value = m.and_then(|m| m.value.clone());
        if let Some(m) = m.filter(|m| !m.active) {
            report.active_conditions = m.excluded_by.clone().into_iter().collect();
            report.notes.push(format!(
                "{fname} is excluded from enum {} in this configuration",
                body.tag.clone().unwrap_or_default()
            ));
        }
        if let Some(Some(init)) = body.inits.get(fname) {
            let pre = substitute(init, &body.known, sim);
            let (_, trace, _) = sim.expand_text(pre, true);
            let topts = opts.tree.clone();
            let mut b = Builder::new(sim, ws, &plan.ctx.model, &topts);
            let nodes = trace.map(|t| b.build(&t)).unwrap_or_default();
            if !nodes.is_empty() {
                report
                    .notes
                    .push(format!("expansion of the initializer of {fname}:"));
            }
            report.expansion = nodes.into_iter().next();
        }
    }
    report.active = Some(site.clone());
    let srcarch = ws.target.arch.srcarch();
    for (_, p, path) in plan.ranked.iter().skip(1).take(64) {
        let view = ws.index.file(p.file);
        let other = view.and_then(|v| v.arch).filter(|a| *a != srcarch);
        report.candidates.push(super::report::Candidate {
            path: path.clone(),
            line: p.line,
            text: String::new(),
            status: if other.is_some() {
                super::report::Status::OtherArch
            } else {
                super::report::Status::NotIncluded
            },
            detail: match other {
                Some(a) => format!("arch/{a}; the current arch is {srcarch}"),
                None => "another declaration; not the one analysed here".into(),
            },
            conditions: Vec::new(),
        });
    }
    report.enum_info = Some(EnumInfo {
        tag: body.tag.clone(),
        site,
        members: body.members.clone(),
        focus,
    });
    report
}

pub fn show_enum(ws: &Workspace, name: &str, opts: &ShowOptions) -> Option<Report> {
    let EnumLookup::Found(plan) = plan_enum(ws, name, opts.tree.reach) else {
        return None;
    };
    let (mut sim, info) = enum_sim(ws, &plan.ctx, opts);
    let body = enum_body(ws, &plan, &mut sim);
    Some(enum_report(ws, name, &plan, &body, &mut sim, &info, opts))
}
