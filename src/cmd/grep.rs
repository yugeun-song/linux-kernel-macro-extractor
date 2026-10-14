use rustc_hash::FxHashMap;

use super::show::{BATCH, Batch, lite_options, reports, reports_each};
use crate::analysis::report::{Candidate, Report, Status, SubjectKind};
use crate::cli::{Global, Shown};
use crate::error::{Error, Result, exit};
use crate::index::{ENUM_TAG_PREFIX, Matcher};
use crate::render::style::{self, paint};
use crate::render::{Output, paths};
use crate::syntax::scan;
use crate::workspace::Workspace;

struct Hit {
    path: String,
    line: u32,
    text: String,
    tag: String,
    status: Status,
    name: String,
}

fn short(detail: &str) -> &str {
    detail.split(" (kbuild flags").next().unwrap_or(detail)
}

fn strip_comments(directive: &str) -> String {
    let mut out = String::new();
    let mut rest = directive;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        rest = rest[i + 2..]
            .find("*/")
            .map_or("", |e| &rest[i + 2 + e + 2..]);
    }
    out.push_str(rest);
    out.split("//").next().unwrap_or("").trim().to_string()
}

fn failed_condition(c: &crate::analysis::report::Condition) -> String {
    let taken_before = c.notes.iter().find_map(|n| {
        let (_, rest) = n.split_once(": ")?;
        let (directive, verdict) = rest.split_once(" -> ")?;
        verdict
            .starts_with("true")
            .then(|| format!("{} is true", strip_comments(directive)))
    });
    taken_before.unwrap_or_else(|| strip_comments(&c.directive))
}

fn candidate_tag(c: &Candidate) -> String {
    let label = c.status.label();
    match c.status {
        Status::ConditionFalse => match c.conditions.iter().find(|x| !x.taken) {
            Some(x) => format!("{label}: {}", failed_condition(x)),
            None => label.to_string(),
        },
        Status::Overridden
        | Status::Undefined
        | Status::Replaced
        | Status::Elsewhere
        | Status::NotReached
        | Status::Shadowed
            if !c.detail.is_empty() =>
        {
            format!("{label}: {}", short(&c.detail))
        }
        _ => label.to_string(),
    }
}

fn hits_for(report: &Report) -> Vec<Hit> {
    let mut out = Vec::new();
    let name = report.query.clone();
    if report.kind == SubjectKind::EnumConstant
        && let Some(info) = &report.enum_info
        && let Some(m) = info.members.iter().find(|m| m.name == name)
    {
        let text = match &m.init {
            Some(init) => format!("{} = {init}", m.name),
            None => m.name.clone(),
        };
        let (tag, status) = match (m.active, &m.value, &m.excluded_by) {
            (true, Some(v), _) => (format!("active = {}", v.display), Status::Active),
            (true, None, _) => ("active".to_string(), Status::Active),
            (false, _, Some(c)) => (
                format!("condition false: {}", failed_condition(c)),
                Status::ConditionFalse,
            ),
            (false, _, None) => ("condition false".to_string(), Status::ConditionFalse),
        };
        out.push(Hit {
            path: info.site.path.clone(),
            line: m.line,
            text,
            tag,
            status,
            name: name.clone(),
        });
        return out;
    }
    if let Some(site) = &report.active {
        let mut tag = match &report.value {
            Some(v) => format!("active = {}", v.display),
            None => "active".to_string(),
        };
        if report.tu_dependent.is_some() {
            tag.push_str(", varies by TU");
        }
        out.push(Hit {
            path: site.path.clone(),
            line: site.line,
            text: site.text.clone(),
            tag,
            status: Status::Active,
            name: name.clone(),
        });
    }
    for c in &report.candidates {
        out.push(Hit {
            path: c.path.clone(),
            line: c.line,
            text: c.text.clone(),
            tag: candidate_tag(c),
            status: c.status,
            name: name.clone(),
        });
    }
    out
}

pub(super) struct Lines<'w> {
    ws: &'w Workspace,
    files: FxHashMap<String, Vec<u8>>,
}

impl<'w> Lines<'w> {
    pub(super) fn new(ws: &'w Workspace) -> Lines<'w> {
        Lines {
            ws,
            files: FxHashMap::default(),
        }
    }

    pub(super) fn column(&mut self, path: &str, line: u32, word: &str) -> usize {
        let ws = self.ws;
        let content = self.files.entry(path.to_string()).or_insert_with(|| {
            std::fs::read(ws.layout.srctree.join(path))
                .or_else(|_| std::fs::read(ws.layout.objtree.join(path)))
                .unwrap_or_default()
        });
        let Some(text) = content
            .split(|&b| b == b'\n')
            .nth(line.saturating_sub(1) as usize)
        else {
            return 1;
        };
        let w = word.as_bytes();
        let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        (0..text.len().saturating_sub(w.len()) + 1)
            .find(|&i| {
                text[i..].starts_with(w)
                    && (i == 0 || !is_ident(text[i - 1]))
                    && text.get(i + w.len()).is_none_or(|&b| !is_ident(b))
            })
            .map_or(1, |i| i + 1)
    }
}

fn tag_style(status: Status) -> anstyle::Style {
    match status {
        Status::Active => style::OK,
        Status::TuLocal | Status::Elsewhere => style::MAYBE,
        Status::OtherArch | Status::Host | Status::NotIncluded => style::DIM,
        _ => style::BAD,
    }
}

fn render(hit: &Hit, word: &str, g: &Global, lines: &mut Lines<'_>) -> String {
    let tag = paths::in_text(&hit.tag);
    if g.vimgrep {
        let col = lines.column(&hit.path, hit.line, word);
        return format!(
            "{}:{}:{col}:{}  [{tag}]\n",
            paths::openable(&hit.path),
            hit.line,
            hit.text
        );
    }
    let shown = paths::shown(&hit.path);
    format!(
        "{}:{}: {}  {}\n",
        paint(&shown, style::PATH),
        paint(&hit.line.to_string(), style::LINE),
        style::code(&hit.text),
        paint(&format!("[{tag}]"), tag_style(hit.status))
    )
}

fn per_group(g: &Global) -> Option<usize> {
    if g.vimgrep {
        return None;
    }
    match g.shown(Shown::PerGroup(3)).unwrap_or(Shown::PerGroup(3)) {
        Shown::All => None,
        Shown::Active => Some(0),
        Shown::PerGroup(n) => Some(n),
    }
}

fn emit_grouped(
    hits: &[Hit],
    relation: &str,
    word_of: &dyn Fn(&Hit) -> String,
    limit: Option<usize>,
    g: &Global,
    lines: &mut Lines<'_>,
    out: &mut Output,
) {
    let mut i = 0;
    while i < hits.len() {
        let status = hits[i].status;
        let j = hits[i..]
            .iter()
            .position(|h| h.status != status || h.name != hits[i].name)
            .map_or(hits.len(), |k| i + k);
        let group = &hits[i..j];
        let keep = if status == Status::Active {
            group.len()
        } else {
            limit.unwrap_or(usize::MAX).min(group.len())
        };
        for h in &group[..keep] {
            out.emit(&render(h, &word_of(h), g, lines));
        }
        if keep < group.len() {
            out.emit(&format!(
                "{}\n",
                paint(
                    &format!(
                        "... {} more {} definitions {relation} {} (--all)",
                        group.len() - keep,
                        status.label(),
                        group[0].name
                    ),
                    style::DIM
                )
            ));
        }
        i = j;
    }
}

fn literal_prefix(glob: &str) -> &str {
    let end = glob.find(['*', '?', '[', '{', '\\']).unwrap_or(glob.len());
    &glob[..end]
}

fn regex_prefix(pattern: &str, word: bool) -> &str {
    let anchored = if word {
        Some(pattern)
    } else {
        pattern.strip_prefix('^')
    };
    let Some(body) = anchored.filter(|b| !b.contains('|')) else {
        return "";
    };
    let end = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(body.len());
    let quantified = body[end..].starts_with(['?', '*', '{']);
    let end = if quantified {
        end.saturating_sub(1)
    } else {
        end
    };
    &body[..end]
}

pub fn run_grep(ws: &Workspace, pattern: &str, word: bool, glob: bool, g: &Global) -> Result<i32> {
    let make: Box<dyn Fn() -> Matcher + Sync> = if glob {
        let m = globset::Glob::new(pattern)
            .map_err(|e| Error::Usage(format!("grep -g {pattern}: {e}")))?
            .compile_matcher();
        Box::new(move || {
            let m = m.clone();
            Box::new(move |s| !s.starts_with(ENUM_TAG_PREFIX) && m.is_match(s))
        })
    } else {
        let source = if word {
            format!("^(?:{pattern})$")
        } else {
            pattern.to_string()
        };
        let re =
            regex::Regex::new(&source).map_err(|e| Error::Usage(format!("grep {pattern}: {e}")))?;
        Box::new(move || {
            let re = re.clone();
            Box::new(move |s| !s.starts_with(ENUM_TAG_PREFIX) && re.is_match(s))
        })
    };
    let prefix = if glob {
        literal_prefix(pattern)
    } else {
        regex_prefix(pattern, word)
    };
    let names = ws.index.names_matching(prefix, &make);
    if names.is_empty() {
        eprintln!("kmx: error: no macro or enum name matches {pattern}");
        return Ok(exit::NOT_FOUND);
    }
    let opts = lite_options(ws, g)?;
    let limit = per_group(g);
    let mut out = Output::open(g);
    let mut lines = Lines::new(ws);
    let mut printed = false;
    let batch = Batch::new("lite", names.len());
    for chunk in names.chunks(BATCH) {
        reports_each(ws, chunk, &opts, g, &batch, &mut |report| {
            let hits = hits_for(&report);
            if !printed && !hits.is_empty() && !g.vimgrep {
                out.tree_heading();
            }
            printed |= !hits.is_empty();
            emit_grouped(
                &hits,
                "of",
                &|h| h.name.clone(),
                limit,
                g,
                &mut lines,
                &mut out,
            );
        });
    }
    if !printed {
        eprintln!(
            "kmx: error: names match {pattern}, but none is defined in the kernel proper; pass --scope all to include tools/ and scripts/"
        );
        return Ok(exit::NOT_FOUND);
    }
    Ok(exit::OK)
}

pub fn run_uses(ws: &Workspace, name: &str, g: &Global) -> Result<i32> {
    let users = ws.index.uses(name);
    if users.is_empty() {
        eprintln!("kmx: error: {name}: no macro definition in the tree uses this name");
        return Ok(exit::NOT_FOUND);
    }
    let mut definers: Vec<String> = users
        .iter()
        .filter_map(|p| {
            let view = ws.index.file(p.file)?;
            let content = ws.file_bytes(&ws.abs(&view))?;
            let range = scan::directive_at(content, p.offset as usize)?;
            let d = scan::DirLine {
                off: range.start as u32,
                end: range.end as u32,
                line: p.line,
            };
            let head = scan::parse_head(content, d);
            head.name.map(|n| String::from_utf8_lossy(&n).into_owned())
        })
        .collect();
    definers.sort();
    definers.dedup();
    let wanted: rustc_hash::FxHashSet<(String, u32)> = users
        .iter()
        .filter_map(|p| ws.index.file(p.file).map(|v| (v.path.to_string(), p.line)))
        .collect();
    let opts = lite_options(ws, g)?;
    let mut hits: Vec<Hit> = reports(ws, &definers, &opts, g, "lite")
        .iter()
        .flat_map(hits_for)
        .filter(|h| wanted.contains(&(h.path.clone(), h.line)))
        .collect();
    hits.sort_by(|a, b| {
        (a.status.order(), &a.path, a.line).cmp(&(b.status.order(), &b.path, b.line))
    });
    hits.dedup_by(|a, b| a.path == b.path && a.line == b.line);
    if hits.is_empty() {
        eprintln!(
            "kmx: error: {name}: only definitions outside the kernel proper use this name; pass --scope all to include them"
        );
        return Ok(exit::NOT_FOUND);
    }
    for h in &mut hits {
        h.name = name.to_string();
    }
    let mut out = Output::open(g);
    if !g.vimgrep {
        out.tree_heading();
    }
    let mut lines = Lines::new(ws);
    let word = name.to_string();
    emit_grouped(
        &hits,
        "using",
        &|_| word.clone(),
        per_group(g),
        g,
        &mut lines,
        &mut out,
    );
    Ok(exit::OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_prefixes_are_safe() {
        assert_eq!(regex_prefix("PAGE_SIZE", true), "PAGE_SIZE");
        assert_eq!(regex_prefix("PAGE_SIZE", false), "");
        assert_eq!(regex_prefix("^SZ_", false), "SZ_");
        assert_eq!(regex_prefix("^PAGE_S(IZE|HIFT)", false), "");
        assert_eq!(regex_prefix("^PAGE_S[IH]", false), "PAGE_S");
        assert_eq!(regex_prefix("^ab?c", false), "a");
        assert_eq!(regex_prefix("A|B", true), "");
    }
}
