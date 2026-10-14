use rayon::prelude::*;
use serde::Serialize;

use super::show::{lite_options, reach};
use crate::analysis::candidates::{self, Class};
use crate::analysis::report::{Report, SubjectKind};
use crate::cli::Global;
use crate::error::{Error, Result, exit};
use crate::index::{DefKind, ENUM_TAG_PREFIX, FileView, Matcher, Posting};
use crate::render::Output;
use crate::workspace::Workspace;

#[derive(Serialize)]
struct Row {
    name: String,
    macros: usize,
    enumerators: usize,
    enum_tags: usize,
    first: Option<String>,
    value: Option<String>,
}

fn literal_prefix(glob: &str) -> &str {
    let end = glob.find(['*', '?', '[', '{', '\\']).unwrap_or(glob.len());
    &glob[..end]
}

const RESOLVE_LIMIT: usize = 500;

pub fn run(ws: &Workspace, pattern: &str, regex: bool, resolve: bool, g: &Global) -> Result<i32> {
    let make: Box<dyn Fn() -> Matcher + Sync> = if regex {
        let re = regex::Regex::new(pattern).map_err(|e| Error::Usage(format!("bad regex: {e}")))?;
        Box::new(move || {
            let re = re.clone();
            Box::new(move |s| re.is_match(s.strip_prefix(ENUM_TAG_PREFIX).unwrap_or(s)))
        })
    } else {
        let glob = globset::Glob::new(pattern)
            .map_err(|e| Error::Usage(format!("bad glob: {e}")))?
            .compile_matcher();
        Box::new(move || {
            let glob = glob.clone();
            Box::new(move |s| glob.is_match(s.strip_prefix(ENUM_TAG_PREFIX).unwrap_or(s)))
        })
    };
    let prefix = if regex { "" } else { literal_prefix(pattern) };
    let mut names = ws.index.names_matching(prefix, &make);
    if !prefix.is_empty() && !regex {
        let tagged = format!("{ENUM_TAG_PREFIX}{prefix}");
        names.extend(ws.index.names_matching(&tagged, &make));
        names.sort_unstable();
    }
    names.dedup();
    let srcarch = ws.target.arch.srcarch();
    let scope = reach(g.scope);
    let mut rows: Vec<Row> = names
        .par_iter()
        .filter_map(|n| {
            let admitted: Vec<(Posting, FileView<'_>, Class)> = ws
                .index
                .lookup(n)
                .into_iter()
                .filter_map(|p| {
                    let v = ws.index.file(p.file)?;
                    let class =
                        candidates::fixed_class(&v, srcarch).unwrap_or(if v.in_include_dir() {
                            Class::Header
                        } else {
                            Class::LocalHeader
                        });
                    scope.admits(&class).then_some((p, v, class))
                })
                .collect();
            if admitted.is_empty() {
                return None;
            }
            let count = |k: &[DefKind]| {
                admitted
                    .iter()
                    .filter(|(p, ..)| k.contains(&p.kind))
                    .count()
            };
            let first = admitted
                .iter()
                .min_by_key(|(p, v, class)| {
                    (
                        p.kind == DefKind::Undef,
                        candidates::class_rank(class, v.path, srcarch),
                        v.path.matches('/').count(),
                        v.path,
                        p.line,
                    )
                })
                .map(|(p, v, _)| format!("{}:{}", v.path, p.line));
            Some(Row {
                name: n.clone(),
                macros: count(&[DefKind::Object, DefKind::Function]),
                enumerators: count(&[DefKind::Enumerator]),
                enum_tags: count(&[DefKind::EnumTag]),
                first,
                value: None,
            })
        })
        .collect();
    if resolve {
        let opts = lite_options(ws, g)?;
        let limit = if g.all { usize::MAX } else { RESOLVE_LIMIT };
        let targets: Vec<String> = rows
            .iter()
            .take(limit)
            .filter(|r| !r.name.starts_with(ENUM_TAG_PREFIX))
            .map(|r| r.name.clone())
            .collect();
        let batch = super::show::Batch::new("lite", targets.len());
        let mut computed: rustc_hash::FxHashMap<String, Report> = rustc_hash::FxHashMap::default();
        for chunk in targets.chunks(super::show::BATCH) {
            super::show::reports_each(ws, chunk, &opts, g, &batch, &mut |r| {
                computed.insert(r.query.clone(), r);
            });
        }
        let resolved: Vec<Option<(String, Option<String>)>> = rows
            .iter()
            .take(limit)
            .map(|r| {
                let rep = computed.remove(&r.name)?;
                let value = match (&rep.value, &rep.expansion) {
                    (Some(v), _) => v.display.clone(),
                    (None, Some(e)) => e.result.clone(),
                    _ => "undefined".to_string(),
                };
                let site = (rep.kind != SubjectKind::EnumConstant)
                    .then_some(rep.active)
                    .flatten()
                    .map(|s| format!("{}:{}", s.path, s.line));
                Some((value, site))
            })
            .collect();
        for (r, v) in rows.iter_mut().zip(resolved) {
            if let Some((value, site)) = v {
                r.value = Some(value);
                if site.is_some() {
                    r.first = site;
                }
            }
        }
        if rows.len() > limit {
            eprintln!("kmx: resolved the first {limit} names; pass --all to resolve every match");
        }
    }
    let mut out = Output::open(g);
    if !g.vimgrep && !g.json && !rows.is_empty() {
        out.tree_heading();
    }
    let mut lines = super::grep::Lines::new(ws);
    for r in &rows {
        if g.json {
            let line = serde_json::to_string(r).map_err(|e| Error::Internal(e.to_string()))?;
            out.emit(&format!("{line}\n"));
            continue;
        }
        let mut kinds = Vec::new();
        if r.macros > 0 {
            kinds.push(format!("{} macro def(s)", r.macros));
        }
        if r.enumerators > 0 {
            kinds.push(format!("{} enumerator(s)", r.enumerators));
        }
        if r.enum_tags > 0 {
            kinds.push(format!("{} enum decl(s)", r.enum_tags));
        }
        if g.vimgrep {
            let site = r
                .first
                .as_deref()
                .and_then(|f| f.rsplit_once(':'))
                .and_then(|(p, l)| l.parse::<u32>().ok().map(|l| (p, l)));
            if let Some((path, line)) = site {
                let word = r.name.strip_prefix(ENUM_TAG_PREFIX).unwrap_or(&r.name);
                let col = lines.column(path, line, word);
                let value = r
                    .value
                    .as_deref()
                    .map(|v| format!("  = {v}"))
                    .unwrap_or_default();
                out.emit(&format!(
                    "{}:{line}:{col}:{}  {}{value}\n",
                    crate::render::paths::openable(path),
                    r.name,
                    kinds.join(", ")
                ));
            }
            continue;
        }
        let first = r.first.as_deref().map_or(
            std::borrow::Cow::Borrowed("-"),
            crate::render::paths::in_text,
        );
        let mut line = format!("{}\t{}\t{first}", r.name, kinds.join(", "));
        if let Some(v) = &r.value {
            line.push('\t');
            line.push_str(v);
        }
        line.push('\n');
        out.emit(&line);
    }
    if rows.is_empty() {
        eprintln!("kmx: error: no macro, enum or Kconfig name matches {pattern}");
        return Ok(exit::NOT_FOUND);
    }
    Ok(exit::OK)
}
