use std::fmt::Write as _;

use super::c_format;
use super::style::{self, Glyphs, paint};
use crate::analysis::report::{
    Candidate, Condition, EnumInfo, ExpNode, KconfigInfo, Report, Status, SubjectKind,
};

pub struct Opts {
    pub glyphs: Glyphs,
    pub width: usize,
    pub per_group: Option<usize>,
    pub column: usize,
    pub config_section: bool,
    pub onion: bool,
}

fn loc(path: &str, line: u32) -> String {
    if line == 0 {
        paint(path, style::DIM)
    } else {
        format!(
            "{}:{}",
            paint(path, style::PATH),
            paint(&line.to_string(), style::LINE)
        )
    }
}

fn atoms_text(c: &Condition) -> String {
    let parts: Vec<String> = c
        .atoms
        .iter()
        .take(6)
        .map(|a| {
            let v = if a.value.len() > 40 {
                format!("{}…", &a.value[..a.value.floor_char_boundary(40)])
            } else {
                a.value.clone()
            };
            format!("{}={}", a.text, paint(&v, style::VALUE))
        })
        .collect();
    let more = c.atoms.len().saturating_sub(6);
    let mut s = parts.join(", ");
    if more > 0 {
        let _ = write!(s, ", +{more}");
    }
    s
}

pub fn condition_line(c: &Condition, g: &Glyphs) -> String {
    let mark = if c.taken {
        paint(g.ok, style::OK)
    } else if c.evaluated {
        paint(g.bad, style::BAD)
    } else {
        paint(g.maybe, style::MAYBE)
    };
    let mut s = format!("{mark} {}", style::code(&c.directive));
    if c.line > 0 {
        let _ = write!(
            s,
            "  {}",
            paint(&format!("{}:{}", c.path, c.line), style::DIM)
        );
    }
    if !c.atoms.is_empty() {
        let _ = write!(s, "  [{}]", atoms_text(c));
    }
    s
}

fn condition_notes(c: &Condition) -> Vec<String> {
    c.notes.iter().map(|n| paint(n, style::DIM)).collect()
}

fn other_arch_line(items: &[&Candidate], g: &Glyphs) -> String {
    let mut arches: Vec<&str> = items
        .iter()
        .filter_map(|c| {
            c.path
                .strip_prefix("arch/")
                .and_then(|r| r.split('/').next())
        })
        .collect();
    arches.sort();
    arches.dedup();
    format!(
        "{} {} {}  {}",
        paint(g.bad, style::BAD),
        paint("other arch", style::LABEL),
        paint(&items.len().to_string(), style::VALUE),
        paint(&arches.join(" "), style::DIM)
    )
}

pub fn candidate_lines(
    cands: &[Candidate],
    per_group: Option<usize>,
    g: &Glyphs,
    detailed: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut statuses: Vec<Status> = cands.iter().map(|c| c.status).collect();
    statuses.sort_by_key(|s| s.order());
    statuses.dedup();
    for st in statuses {
        let items: Vec<&Candidate> = cands.iter().filter(|c| c.status == st).collect();
        if st == Status::OtherArch && per_group != Some(usize::MAX) {
            out.push(other_arch_line(&items, g));
            continue;
        }
        let limit = per_group.unwrap_or(usize::MAX);
        for c in items.iter().take(limit) {
            let mark = match st {
                Status::Active => paint(g.ok, style::OK),
                Status::TuLocal | Status::Elsewhere => paint(g.maybe, style::MAYBE),
                _ => paint(g.bad, style::BAD),
            };
            let mut line = format!(
                "{mark} {} {}",
                paint(&format!("{:<15}", st.label()), style::LABEL),
                loc(&c.path, c.line)
            );
            if !c.text.is_empty() {
                let _ = write!(line, "  {}", style::code(&clip(&c.text, 90)));
            }
            out.push(line);
            if !c.detail.is_empty() {
                out.push(format!("    {}", paint(&c.detail, style::DIM)));
            }
            if detailed || st == Status::ConditionFalse {
                for cond in c
                    .conditions
                    .iter()
                    .filter(|x| !x.taken || st != Status::ConditionFalse)
                {
                    out.push(format!("    {}", condition_line(cond, g)));
                    for n in condition_notes(cond) {
                        out.push(format!("      {n}"));
                    }
                }
            }
        }
        if items.len() > limit {
            out.push(paint(
                &format!("  … {} more {} (--all)", items.len() - limit, st.label()),
                style::DIM,
            ));
        }
    }
    out
}

fn chosen_default_line(k: &KconfigInfo, g: &Glyphs) -> Option<String> {
    let d = k.defaults.iter().find(|d| d.chosen)?;
    let mut line = format!(
        "{} {} {}",
        paint(g.ok, style::OK),
        paint("Kconfig default", style::LABEL),
        paint(&d.value, style::VALUE)
    );
    if let Some(c) = &d.condition {
        let _ = write!(line, " if {}", style::code(&clip(c, 100)));
    }
    if let Some(cv) = &d.condition_value {
        let _ = write!(line, "  [{cv}]");
    }
    let _ = write!(line, "  {}", paint(&d.site, style::DIM));
    Some(line)
}

pub fn kconfig_lines(
    k: &KconfigInfo,
    g: &Glyphs,
    per_group: Option<usize>,
    full: bool,
    onion: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    if onion && !full {
        out.push(chosen_default_line(k, g).unwrap_or_else(|| {
            format!(
                "{} {} {}  {}",
                paint(g.ok, style::OK),
                paint("Kconfig", style::LABEL),
                paint(&k.reason, style::WARN),
                paint(&k.sites.join(", "), style::DIM)
            )
        }));
        return out;
    }
    out.push(format!(
        "{} {}  {}",
        paint("Kconfig", style::LABEL),
        paint(&k.reason, style::WARN),
        paint(&k.sites.join(", "), style::DIM)
    ));
    if full {
        out.push(format!(
            "  {} {}  {} {}",
            paint("type", style::LABEL),
            k.kind,
            paint("value", style::LABEL),
            paint(&k.value, style::VALUE)
        ));
        if let Some(p) = &k.prompt {
            out.push(format!(
                "  {} \"{p}\"  {}",
                paint("prompt", style::LABEL),
                paint(
                    &format!("visible={}", k.visible.as_deref().unwrap_or("?")),
                    style::DIM
                )
            ));
        }
        if let Some(d) = &k.depends {
            out.push(format!(
                "  {} {}  {}",
                paint("depends on", style::LABEL),
                style::code(&clip(d, 140)),
                paint(
                    &format!("= {}", k.depends_value.as_deref().unwrap_or("?")),
                    style::DIM
                )
            ));
        }
        if let Some(r) = &k.range {
            out.push(format!("  {} {r}", paint("range", style::LABEL)));
        }
    }
    if onion {
        out.extend(chosen_default_line(k, g));
    }
    let limit = if onion {
        0
    } else {
        per_group.map(|p| p.max(8)).unwrap_or(usize::MAX)
    };
    for d in k.defaults.iter().take(limit) {
        let mark = if d.chosen {
            paint(g.ok, style::OK)
        } else {
            paint(g.bad, style::BAD)
        };
        let mut line = format!("{mark} default {}", paint(&d.value, style::VALUE));
        if let Some(c) = &d.condition {
            let _ = write!(line, " if {}", style::code(&clip(c, 100)));
        }
        if let Some(cv) = &d.condition_value {
            let _ = write!(line, "  [{cv}]");
        }
        let _ = write!(line, "  {}", paint(&d.site, style::DIM));
        out.push(line);
    }
    if !onion && k.defaults.len() > limit {
        out.push(paint(
            &format!("  … {} more defaults (--all)", k.defaults.len() - limit),
            style::DIM,
        ));
    }
    let active: Vec<String> = k
        .selected_by
        .iter()
        .filter(|(_, v)| !v.starts_with('n'))
        .map(|(n, v)| format!("{n}={v}"))
        .collect();
    if !active.is_empty() {
        out.push(format!(
            "{} selected by {}",
            paint(g.ok, style::OK),
            active.join(", ")
        ));
    }
    if full && !onion && k.selected_by.len() > active.len() {
        out.push(paint(
            &format!(
                "  {} other selector(s) are n",
                k.selected_by.len() - active.len()
            ),
            style::DIM,
        ));
    }
    if let Some(ch) = k.choice.as_ref().filter(|_| !onion) {
        let members: Vec<String> = ch
            .iter()
            .map(|(n, v)| {
                if v == "y" {
                    paint(&format!("{n}={v}"), style::OK)
                } else {
                    format!("{n}={v}")
                }
            })
            .collect();
        out.push(format!(
            "{} {}",
            paint("choice", style::LABEL),
            members.join(" ")
        ));
    }
    out
}

pub fn enum_lines(e: &EnumInfo, g: &Glyphs) -> Vec<String> {
    let mut out = Vec::new();
    let active = e.members.iter().filter(|m| m.active).count();
    let excluded = e.members.len() - active;
    out.push(format!(
        "{} {}  {}  {}",
        paint("enum", style::KEYWORD),
        paint(e.tag.as_deref().unwrap_or("(anonymous)"), style::NAME),
        loc(&e.site.path, e.site.line),
        paint(
            &format!("{active} active, {excluded} excluded by #if"),
            style::DIM
        )
    ));
    let w = e
        .members
        .iter()
        .map(|m| m.name.len())
        .max()
        .unwrap_or(0)
        .min(36);
    let mut ordinal = 0usize;
    for m in &e.members {
        let is_focus = e.focus.as_deref() == Some(m.name.as_str());
        let mark = if is_focus {
            paint(g.arrow, style::OK)
        } else {
            " ".repeat(style::width(g.arrow))
        };
        let name = format!("{:<w$}", m.name, w = w);
        if !m.active {
            let why = m
                .excluded_by
                .as_ref()
                .map(|c| {
                    let atoms = c
                        .atoms
                        .iter()
                        .map(|a| format!("{}={}", a.text, a.value))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{} [{atoms}]", c.directive)
                })
                .unwrap_or_default();
            out.push(format!(
                "{mark} {}  {}  {}",
                paint(&format!("{:>4}", g.bad), style::BAD),
                paint(&name, style::DIM),
                paint(&format!("excluded: {why}"), style::DIM)
            ));
            continue;
        }
        let value = m
            .value
            .as_ref()
            .map(|v| v.display.clone())
            .unwrap_or_else(|| "?".into());
        let mut line = format!(
            "{mark} {}  {}  {}",
            paint(&format!("{ordinal:>4}"), style::DIM),
            if is_focus {
                paint(&name, style::QUERY)
            } else {
                paint(&name, style::NAME)
            },
            paint(&format!("{value:<18}"), style::VALUE)
        );
        match (&m.init, &m.expansion) {
            (Some(i), Some(x)) if x != i => {
                let _ = write!(
                    line,
                    " = {} {} {}",
                    style::code(&clip(i, 60)),
                    paint(g.arrow, style::DIM),
                    style::code(&clip(x, 70))
                );
            }
            (Some(i), _) => {
                let _ = write!(line, " = {}", style::code(&clip(i, 100)));
            }
            (None, Some(x)) => {
                let _ = write!(line, " {}", paint(x, style::DIM));
            }
            (None, None) => {
                let _ = write!(line, " {}", paint("(previous + 1)", style::DIM));
            }
        }
        out.push(line);
        ordinal += 1;
    }
    out
}

fn resolved_target(e: &ExpNode) -> Option<&ExpNode> {
    let result = e.result.trim();
    let mut stack = vec![e];
    while let Some(n) = stack.pop() {
        if n.target.is_some() && n.name == result {
            return Some(n);
        }
        stack.extend(n.children.iter().rev());
    }
    None
}

const CONCLUSION_LIMIT: usize = 400;
const INDENT: usize = 8;

fn census_summary(census: &crate::analysis::report::Census, g: &Glyphs) -> String {
    let mut line = paint(
        &format!("values across {} TUs of the current build", census.tu_total),
        style::HEADING,
    );
    if let Some(v) = census.variants.first() {
        let site = v
            .site
            .as_ref()
            .map(|s| loc(&s.path, s.line))
            .unwrap_or_else(|| paint("undefined", style::BAD));
        let _ = write!(
            line,
            ": {} TUs get {site}",
            paint(&v.tu_count.to_string(), style::VALUE)
        );
        if let Some(val) = &v.value {
            let _ = write!(line, " = {}", paint(&val.display, style::VALUE));
        } else if let Some(e) = &v.expansion {
            let _ = write!(
                line,
                " {} {}",
                paint(g.arrow, style::DIM),
                style::code(&clip(e, 60))
            );
        }
        let others = census.variants.len() - 1;
        if others > 0 {
            let _ = write!(
                line,
                "{}",
                paint(
                    &format!(
                        "; {others} other variant{} (--all)",
                        if others == 1 { "" } else { "s" }
                    ),
                    style::DIM
                )
            );
        }
    }
    line
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n).collect();
    format!("{cut}…")
}

struct Code<'a> {
    cont: &'a str,
    base: usize,
    width: usize,
}

impl Code<'_> {
    fn assign(&self, out: &mut Vec<String>, first: String, col: usize, code: &str) -> usize {
        let w = style::width(code);
        let wrapped =
            col + w > self.width && self.base + INDENT + w <= self.width && !code.starts_with("({");
        if !wrapped {
            return self.emit(out, first, col, code).1;
        }
        out.push(first.trim_end().to_string());
        out.push(format!(
            "{}{}{}",
            self.cont,
            " ".repeat(INDENT),
            style::code(code)
        ));
        self.base + INDENT + w
    }

    fn emit(&self, out: &mut Vec<String>, first: String, col: usize, code: &str) -> (usize, usize) {
        let lines = c_format::format(
            code,
            col.saturating_sub(self.base),
            self.width.saturating_sub(self.base),
        );
        let mut first_end = col;
        let mut last_end = col;
        for (k, (indent, text)) in lines.iter().enumerate() {
            if k == 0 {
                out.push(format!("{first}{}", style::code(text)));
                first_end = col + style::width(text);
                last_end = first_end;
            } else {
                out.push(format!(
                    "{}{}{}",
                    self.cont,
                    " ".repeat(*indent),
                    style::code(text)
                ));
                last_end = self.base + indent + style::width(text);
            }
        }
        (first_end, last_end)
    }

    fn suffix(&self, out: &mut Vec<String>, end: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        if end + style::width(text) <= self.width {
            if let Some(last) = out.last_mut() {
                last.push_str(text);
            }
        } else {
            out.push(format!("{}{}", self.cont, text.trim_start()));
        }
    }

    fn attach_loc(
        &self,
        out: &mut Vec<String>,
        at: usize,
        first_end: usize,
        loc: &str,
        column: usize,
    ) {
        let w = style::width(loc);
        if first_end + 2 + w <= self.width {
            let pad = if column >= first_end + 2 && column + w <= self.width {
                column - first_end
            } else {
                2
            };
            out[at].push_str(&" ".repeat(pad));
            out[at].push_str(loc);
        } else {
            let pad = self.width.saturating_sub(self.base + w).max(2);
            out.insert(at + 1, format!("{}{}{loc}", self.cont, " ".repeat(pad)));
        }
    }
}

enum Rhs {
    Code(String, String),
    Plain(String),
}

impl Code<'_> {
    fn header(
        &self,
        out: &mut Vec<String>,
        lead: &str,
        call_plain: &str,
        call: &str,
        sep: &str,
        rhs: &Rhs,
    ) -> usize {
        let lead_w = style::width(lead);
        let sep_w = style::width(sep);
        let col = lead_w + style::width(call_plain);
        let tail = |out: &mut Vec<String>, end: usize, tail: &str| {
            self.suffix(out, end, tail);
            end + style::width(tail)
        };
        if col + sep_w < self.width {
            let first = format!("{lead}{call}{sep}");
            return match rhs {
                Rhs::Plain(p) => {
                    out.push(format!("{first}{p}"));
                    col + sep_w + style::width(p)
                }
                Rhs::Code(text, suffix) => {
                    let end = self.assign(out, first, col + sep_w, text);
                    tail(out, end, suffix)
                }
            };
        }
        let end = self.emit(out, lead.to_string(), lead_w, call_plain).1;
        match rhs {
            Rhs::Plain(p) => tail(out, end, &format!("{sep}{p}")),
            Rhs::Code(text, suffix) => {
                if let Some(last) = out.last_mut() {
                    last.push_str(sep.trim_end());
                }
                let cont = format!("{}{}", self.cont, " ".repeat(INDENT));
                let inner = Code {
                    cont: &cont,
                    base: self.base + INDENT,
                    width: self.width,
                };
                let end = inner.emit(out, cont.clone(), inner.base, text).1;
                tail(out, end, suffix)
            }
        }
    }
}

fn single_number(text: &str) -> bool {
    let t = text.trim();
    let t = t.strip_prefix('-').unwrap_or(t);
    !t.is_empty()
        && t.as_bytes()[0].is_ascii_digit()
        && t.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn node(out: &mut Vec<String>, n: &ExpNode, prefix: &str, root: bool, last: bool, o: &Opts) {
    let g = &o.glyphs;
    let head_prefix = if root {
        String::new()
    } else {
        format!("{prefix}{}", if last { g.last } else { g.branch })
    };
    let child_prefix = if root {
        String::new()
    } else {
        format!("{prefix}{}", if last { g.blank } else { g.pipe })
    };
    let guide = g.pipe.chars().next().unwrap_or('|').to_string();
    let body_prefix = if n.children.is_empty() {
        format!("{child_prefix}  ")
    } else {
        format!("{child_prefix}{} ", paint(&guide, style::GUIDE))
    };
    let code = Code {
        cont: &body_prefix,
        base: style::width(&child_prefix) + 2,
        width: o.width,
    };
    let function = n.target.as_ref().filter(|t| !t.lines.is_empty());
    let call_plain = match function {
        Some(t) => format!("{}({})", n.name, t.params.join(", ")),
        None => c_format::one_line(&n.call),
    };
    let call = match call_plain.split_once('(') {
        Some((name, rest)) => format!("{}({rest}", paint(name, style::NAME)),
        None => paint(&call_plain, style::NAME),
    };
    let names_itself = n
        .children
        .iter()
        .find(|c| c.name == n.name)
        .and_then(|c| c.target.as_ref())
        .map(|t| format!("(names itself: the C {} below applies)", t.kind));
    let shown = n.refined.as_deref().unwrap_or(&n.result).trim().to_string();
    let mut notes: Vec<String> = Vec::new();
    if let Some(first) = &n.repeat_of {
        notes.push(if *first == n.call {
            "(same expansion as above)".to_string()
        } else {
            format!("(same as {first} above)")
        });
    }
    if let Some(note) = names_itself {
        notes.push(note);
    }
    let eq = format!(" {} ", paint("=", style::DIM));
    let value = |v: &crate::analysis::report::Value| Rhs::Plain(paint(&v.display, style::VALUE));
    let (sep, rhs) = match (function, &n.target, &n.value) {
        (Some(t), ..) => match &t.returns {
            Some(text) => (eq.clone(), Rhs::Code(text.clone(), String::new())),
            None => ("  ".to_string(), Rhs::Plain(paint(&t.kind, style::TYPE))),
        },
        (None, Some(_), Some(v)) => (eq.clone(), value(v)),
        (None, None, Some(v)) if single_number(&shown) => (eq.clone(), value(v)),
        _ if shown.is_empty() => (
            eq.clone(),
            Rhs::Plain(paint("(expands to nothing)", style::DIM)),
        ),
        _ => {
            let tail = match &n.value {
                Some(v) if v.display.split(' ').next() != Some(shown.as_str()) => {
                    format!("{eq}{}", paint(&v.display, style::VALUE))
                }
                _ => String::new(),
            };
            (eq.clone(), Rhs::Code(shown.clone(), tail))
        }
    };
    let lead = paint(&head_prefix, style::GUIDE);
    let mut end = code.header(out, &lead, &call_plain, &call, &sep, &rhs);
    for note in &notes {
        let text = format!("  {}", paint(note, style::DIM));
        code.suffix(out, end, &text);
        end += style::width(&text);
    }
    if n.repeat_of.is_some() {
        return;
    }
    let loc_of = |site: &crate::analysis::report::Site| loc(&site.path, site.line);
    match (&n.target, &n.site) {
        (Some(t), site) => {
            let at = out.len();
            out.push(format!("{body_prefix}{}", paint(&t.kind, style::TYPE)));
            if let Some(site) = site {
                code.attach_loc(
                    out,
                    at,
                    code.base + style::width(&t.kind),
                    &loc_of(site),
                    o.column,
                );
                if t.lines.is_empty() && !site.text.is_empty() {
                    code.emit(out, body_prefix.clone(), code.base, &site.text);
                }
            }
            for l in &t.lines {
                let text = l.trim_start();
                let indent = &l[..l.len() - text.len()];
                let cont = format!("{body_prefix}{indent}");
                let nested = Code {
                    cont: &cont,
                    base: code.base + style::width(indent),
                    width: o.width,
                };
                nested.emit(out, cont.clone(), nested.base, text);
            }
            if t.hidden > 0 {
                out.push(format!(
                    "{body_prefix}{}",
                    paint(&format!("… {} more lines (--all)", t.hidden), style::DIM)
                ));
            }
        }
        (None, Some(site)) => {
            let at = out.len();
            let (first_end, _) = code.emit(out, body_prefix.clone(), code.base, &site.text);
            code.attach_loc(out, at, first_end, &loc_of(site), o.column);
        }
        _ => {}
    }
    if let Some(cfg) = &n.config {
        out.push(format!(
            "{body_prefix}{}",
            paint(&format!("({cfg})"), style::DIM)
        ));
    }
    for c in &n.why {
        out.push(format!("{body_prefix}{}", condition_line(c, g)));
        for note in condition_notes(c) {
            out.push(format!("{body_prefix}  {note}"));
        }
    }
    if let Some(k) = n.kconfig.as_ref().filter(|_| !root || !o.config_section) {
        for l in kconfig_lines(k, g, o.per_group, false, o.onion) {
            out.push(format!("{body_prefix}{l}"));
        }
    }
    if !n.alternatives.is_empty() && !root && !o.onion {
        let lines = candidate_lines(&n.alternatives, o.per_group.map(|p| p.min(3)), g, false);
        for l in lines {
            out.push(format!("{body_prefix}{l}"));
        }
    }
    for a in &n.args {
        if let Some(expanded) = &a.expanded
            && *expanded != a.raw
        {
            let e = a.refined.as_ref().unwrap_or(expanded);
            let head = format!(
                "{body_prefix}{} {} {} {} ",
                paint(&a.param, style::LABEL),
                paint("=", style::DIM),
                style::code(&clip(&a.raw, 60)),
                paint(g.arrow, style::DIM)
            );
            let col = code.base
                + style::width(&a.param)
                + 3
                + style::width(&clip(&a.raw, 60))
                + 1
                + style::width(g.arrow)
                + 1;
            let cont = format!("{body_prefix}{}", " ".repeat(INDENT));
            let arg = Code {
                cont: &cont,
                base: code.base + INDENT,
                width: o.width,
            };
            arg.emit(out, head, col, e);
        }
    }
    if let Some(note) = &n.note {
        out.push(format!("{body_prefix}{}", paint(note, style::DIM)));
    }
    let count = n.children.len();
    for (i, c) in n.children.iter().enumerate() {
        node(out, c, &child_prefix, false, i + 1 == count, o);
    }
}

pub fn headline(r: &Report, o: &Opts) -> Vec<String> {
    let g = &o.glyphs;
    let mut title = paint(&r.query, style::QUERY);
    let mut title_lines: Vec<String> = Vec::new();
    let headline = Code {
        cont: "",
        base: 0,
        width: o.width,
    };
    match (&r.kind, &r.value, &r.expansion) {
        (SubjectKind::Config, ..) => {
            if let Some(k) = &r.kconfig {
                let shown = if k.value == "n" {
                    "n (is not set)".to_string()
                } else {
                    k.value.clone()
                };
                let _ = write!(
                    title,
                    " = {}  {}",
                    paint(&shown, style::VALUE),
                    paint(&format!("Kconfig {}", k.kind), style::TYPE)
                );
            }
        }
        (SubjectKind::NotFound, ..) => {
            let _ = write!(
                title,
                " {}",
                paint(
                    "is not a macro, enum constant or CONFIG symbol in this tree",
                    style::BAD
                )
            );
        }
        (SubjectKind::EnumConstant | SubjectKind::EnumType, v, _) => {
            if let Some(v) = v {
                let _ = write!(title, " = {}", paint(&v.display, style::VALUE));
            }
            if let Some(e) = &r.enum_info {
                let what = match r.kind {
                    SubjectKind::EnumType => "enum type".to_string(),
                    _ => format!(
                        "member of enum {}",
                        e.tag.as_deref().unwrap_or("(anonymous)")
                    ),
                };
                let _ = write!(title, "  {}", paint(&what, style::TYPE));
            }
        }
        (_, Some(v), _) => {
            let _ = write!(title, " = {}", paint(&v.display, style::VALUE));
            if let Some(k) = &r.kconfig {
                let _ = write!(
                    title,
                    "  {}",
                    paint(&format!("(Kconfig {}={})", k.symbol, k.value), style::TYPE)
                );
            } else if let Some(t) = &v.ty {
                let _ = write!(title, "  {}", paint(t, style::TYPE));
            }
        }
        (_, None, Some(e)) => {
            let limit = if o.per_group.is_none() {
                usize::MAX
            } else {
                CONCLUSION_LIMIT
            };
            let target = resolved_target(e);
            match target.and_then(|n| n.target.as_ref().map(|t| (n, t))) {
                Some((_, t)) if t.returns.is_some() => {
                    let returns = clip(t.returns.as_deref().unwrap_or_default(), limit);
                    let call = format!("{}({})", r.query, t.params.join(", "));
                    headline.header(
                        &mut title_lines,
                        "",
                        &call,
                        &paint(&call, style::QUERY),
                        &format!(" {} ", paint("=", style::DIM)),
                        &Rhs::Code(returns, String::new()),
                    );
                }
                Some((n, t)) => {
                    let _ = write!(
                        title,
                        " {} {}  {}",
                        paint(g.arrow, style::DIM),
                        paint(&n.call, style::NAME),
                        paint(&t.kind, style::TYPE)
                    );
                }
                None => {
                    let shown = r
                        .conclusion
                        .as_deref()
                        .or(e.refined.as_deref())
                        .unwrap_or(&e.result);
                    let shown = clip(shown, limit);
                    let call = c_format::one_line(&e.call);
                    headline.header(
                        &mut title_lines,
                        "",
                        &call,
                        &paint(&call, style::QUERY),
                        &format!(" {} ", paint(g.arrow, style::DIM)),
                        &Rhs::Code(shown, String::new()),
                    );
                }
            }
        }
        (_, None, None) => {
            let _ = write!(
                title,
                " {}",
                paint("is not defined in this context", style::BAD)
            );
        }
    }
    if title_lines.is_empty() {
        vec![title]
    } else {
        title_lines
    }
}

pub fn render(r: &Report, o: &Opts) -> String {
    let g = &o.glyphs;
    let mut out = headline(r, o);
    out.push(String::new());
    let c = &r.context;
    out.push(format!("  {}     {}", paint("tree", style::LABEL), c.tree));
    if let Some(o) = &c.objtree {
        out.push(format!("  {}    {o}", paint("build", style::LABEL)));
    }
    out.push(format!(
        "  {}  {} {} {}  {}  {}",
        paint("context", style::LABEL),
        c.release,
        g.git,
        c.commit,
        c.arch,
        c.compiler
    ));
    if !c.mode.is_empty() {
        out.push(format!("           {}", paint(&c.mode, style::DIM)));
    }
    if !c.predefines.is_empty() {
        out.push(format!(
            "           {}",
            paint(&format!("predefined macros: {}", c.predefines), style::DIM)
        ));
    }
    for h in &c.compiler_headers {
        out.push(format!(
            "           {}",
            paint(&format!("compiler headers: {}", h.used), style::DIM)
        ));
        out.push(format!(
            "             {}",
            paint(
                &format!("(the build's {} is absent here)", h.recorded),
                style::DIM
            )
        ));
    }
    if !c.what_if.is_empty() {
        out.push(format!(
            "  {}  {}",
            paint("what-if", style::LABEL),
            c.what_if.join(" ")
        ));
    }
    if let Some(t) = &r.tu_dependent {
        out.push(format!(
            "  {}  {}",
            paint("TU", style::LABEL),
            paint(t, style::WARN)
        ));
    }
    for w in &r.warnings {
        out.push(format!("  {}  {}", paint("warning", style::WARN), w));
    }
    if let Some(k) = &r.kconfig {
        out.push(String::new());
        for l in kconfig_lines(k, g, o.per_group, true, o.onion) {
            out.push(l);
        }
    }
    if let Some(e) = &r.enum_info {
        out.push(String::new());
        out.extend(enum_lines(e, g));
        for c in &r.active_conditions {
            out.push(format!("  {}", condition_line(c, g)));
        }
    }
    if let Some(e) = &r.expansion {
        out.push(String::new());
        node(&mut out, e, "", true, true, o);
    } else if let Some(a) = r.active.as_ref().filter(|_| r.enum_info.is_none()) {
        out.push(String::new());
        out.push(format!(
            "{}  {}",
            loc(&a.path, a.line),
            style::code(&a.text)
        ));
    }
    if !r.active_conditions.is_empty() && r.expansion.is_none() && r.enum_info.is_none() {
        for cnd in &r.active_conditions {
            out.push(format!("  {}", condition_line(cnd, g)));
        }
    }
    if let Some(census) = r.census.as_ref().filter(|_| o.onion) {
        out.push(String::new());
        out.push(census_summary(census, g));
    }
    if let Some(census) = r.census.as_ref().filter(|_| !o.onion) {
        out.push(String::new());
        out.push(paint(
            &format!(
                "values across {} TUs of the current build ({} examined, {:.1}s, {})",
                census.tu_total, census.tu_examined, census.seconds, census.source
            ),
            style::HEADING,
        ));
        let shown = o.per_group.map(|p| p.max(8)).unwrap_or(usize::MAX);
        for v in census.variants.iter().take(shown) {
            let site = v
                .site
                .as_ref()
                .map(|s| loc(&s.path, s.line))
                .unwrap_or_else(|| paint("undefined", style::BAD));
            let mut line = format!(
                "  {} TUs  {site}",
                paint(&format!("{:>5}", v.tu_count), style::VALUE)
            );
            let local = v
                .site
                .as_ref()
                .is_some_and(|s| s.path.ends_with(".c") || s.path.ends_with(".S"));
            if local && v.tu_count > 1 {
                let _ = write!(
                    line,
                    " {}",
                    paint("(and other files with the same definition)", style::DIM)
                );
            }
            if let Some(val) = &v.value {
                let _ = write!(line, "  = {}", paint(&val.display, style::VALUE));
            } else if let Some(e) = &v.expansion {
                let _ = write!(
                    line,
                    "  {} {}",
                    paint(g.arrow, style::DIM),
                    style::code(&clip(e, 80))
                );
            }
            out.push(line);
            if !v.sample.is_empty() {
                out.push(format!(
                    "            {}",
                    paint(&format!("e.g. {}", v.sample.join(", ")), style::DIM)
                ));
            }
        }
        if census.variants.len() > shown {
            let rest: usize = census.variants[shown..].iter().map(|v| v.tu_count).sum();
            out.push(paint(
                &format!(
                    "  … {} more distinct definitions covering {rest} TUs (--all)",
                    census.variants.len() - shown
                ),
                style::DIM,
            ));
        }
        if !census.not_built.is_empty() {
            let n = census.not_built.len();
            let limit = o.per_group.map(|p| p.max(4)).unwrap_or(usize::MAX);
            let mut list = census
                .not_built
                .iter()
                .take(limit)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            if n > limit {
                let _ = write!(list, ", +{} more (--all)", n - limit);
            }
            out.push(format!(
                "  {}",
                paint(
                    &format!(
                        "{n} defining file(s) are not compiled into vmlinux or a module in this build \
                         (not selected by .config, or built separately such as the vDSO): {list}"
                    ),
                    style::DIM
                )
            ));
        }
    }
    let nothing_active = r.expansion.is_none() && r.active.is_none();
    if !r.candidates.is_empty() && (!o.onion || nothing_active) {
        out.push(String::new());
        out.push(paint(
            &format!("other definitions of {} ({})", r.query, r.candidates.len()),
            style::HEADING,
        ));
        for l in candidate_lines(&r.candidates, o.per_group, g, true) {
            out.push(format!("  {l}"));
        }
    }
    if !r.notes.is_empty() {
        out.push(String::new());
        for n in &r.notes {
            out.push(paint(n, style::DIM));
        }
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}
