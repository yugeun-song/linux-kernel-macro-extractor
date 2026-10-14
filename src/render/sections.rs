use std::fmt::Write as _;

use super::style::{self, Glyphs, paint};
use super::tree;
use crate::analysis::report::{ExpNode, Report};

fn deps<'a>(n: &'a ExpNode, out: &mut Vec<&'a ExpNode>) {
    for c in &n.children {
        if c.repeat_of.is_none() && !out.iter().any(|x| x.name == c.name) {
            out.push(c);
        }
        deps(c, out);
    }
}

pub fn render(r: &Report, g: &Glyphs, per_group: Option<usize>, onion: bool) -> String {
    let mut s = String::new();
    let row = |s: &mut String, k: &str, v: String| {
        let _ = writeln!(s, "  {:<10} {v}", paint(k, style::LABEL));
    };
    let _ = writeln!(s, "{}", paint(&r.query, style::QUERY));
    if let Some(v) = &r.value {
        row(
            &mut s,
            "value",
            format!(
                "{}  {}",
                paint(&v.display, style::VALUE),
                paint(v.ty.as_deref().unwrap_or(""), style::TYPE)
            ),
        );
    }
    if let Some(a) = &r.active {
        row(
            &mut s,
            "defined",
            format!(
                "{}:{}",
                paint(&a.path, style::PATH),
                paint(&a.line.to_string(), style::LINE)
            ),
        );
        row(&mut s, "body", style::code(&a.text));
    }
    if let Some(e) = &r.expansion {
        row(&mut s, "expanded", style::code(&e.result));
    }
    let c = &r.context;
    row(&mut s, "tree", c.tree.clone());
    if let Some(o) = &c.objtree {
        row(&mut s, "build", o.clone());
    }
    row(
        &mut s,
        "context",
        format!(
            "{} {} {}  {}  {}",
            c.release, g.git, c.commit, c.arch, c.compiler
        ),
    );
    row(&mut s, "mode", paint(&c.mode, style::DIM));
    for cond in &r.active_conditions {
        row(&mut s, "because", tree::condition_line(cond, g));
    }
    if let Some(t) = &r.tu_dependent {
        row(&mut s, "TU", paint(t, style::WARN));
    }
    if let Some(e) = &r.expansion {
        let mut list = Vec::new();
        deps(e, &mut list);
        if !list.is_empty() {
            let _ = writeln!(s, "\n{}", paint("dependencies", style::HEADING));
            let w = list.iter().map(|n| n.name.len()).max().unwrap_or(0).min(32);
            for n in list {
                let site = n
                    .site
                    .as_ref()
                    .map(|st| format!("{}:{}", st.path, st.line))
                    .unwrap_or_default();
                let val = n
                    .value
                    .as_ref()
                    .map(|v| format!(" = {}", paint(&v.display, style::VALUE)))
                    .unwrap_or_default();
                let _ = writeln!(
                    s,
                    "  {}{}  {}{val}",
                    paint(&n.name, style::NAME),
                    " ".repeat(w.saturating_sub(n.name.len())),
                    paint(&site, style::DIM)
                );
            }
        }
    }
    if !r.candidates.is_empty() && !onion {
        let _ = writeln!(
            s,
            "\n{}",
            paint(
                &format!(
                    "candidates ({})",
                    r.candidates.len() + usize::from(r.active.is_some())
                ),
                style::HEADING
            )
        );
        if let Some(a) = &r.active {
            let _ = writeln!(
                s,
                "  {} {:<15} {}:{}",
                paint(g.ok, style::OK),
                paint("active", style::LABEL),
                paint(&a.path, style::PATH),
                paint(&a.line.to_string(), style::LINE)
            );
        }
        for l in tree::candidate_lines(&r.candidates, per_group, g, false) {
            let _ = writeln!(s, "  {l}");
        }
    }
    for n in &r.notes {
        let _ = writeln!(s, "{}", paint(n, style::DIM));
    }
    s
}
