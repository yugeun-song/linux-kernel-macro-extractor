use std::fmt::Write as _;

use super::style::{self, Glyphs, paint};
use super::tree;
use crate::analysis::report::{ExpNode, Report};

fn walk<'a>(n: &'a ExpNode, out: &mut Vec<&'a ExpNode>) {
    out.push(n);
    for c in &n.children {
        walk(c, out);
    }
}

pub fn render(r: &Report, g: &Glyphs, per_group: Option<usize>, onion: bool) -> String {
    let mut s = String::new();
    let mut title = paint(&r.query, style::QUERY);
    if let Some(v) = &r.value {
        let _ = write!(title, " = {}", paint(&v.display, style::VALUE));
        if let Some(t) = &v.ty {
            let _ = write!(title, "  {}", paint(t, style::TYPE));
        }
    }
    let _ = writeln!(s, "{title}");
    let c = &r.context;
    let _ = writeln!(s, "  {}     {}", paint("tree", style::LABEL), c.tree);
    if let Some(o) = &c.objtree {
        let _ = writeln!(s, "  {}    {o}", paint("build", style::LABEL));
    }
    let _ = writeln!(
        s,
        "  {}  {} {} {}  {}  {}",
        paint("context", style::LABEL),
        c.release,
        g.git,
        c.commit,
        c.arch,
        c.compiler
    );
    let _ = writeln!(s, "           {}\n", paint(&c.mode, style::DIM));
    let Some(root) = &r.expansion else {
        let _ = writeln!(
            s,
            "{}",
            paint("no active definition in this context", style::BAD)
        );
        return s;
    };
    let mut nodes = Vec::new();
    walk(root, &mut nodes);
    let _ = writeln!(s, "{}", paint(" #  rule", style::HEADING));
    for (i, n) in nodes.iter().enumerate() {
        let site = n
            .site
            .as_ref()
            .map(|st| format!("{}:{}", st.path, st.line))
            .unwrap_or_default();
        let rule = if n.repeat_of.is_some() {
            format!("{} {} {}", n.call, g.arrow, n.result)
        } else {
            format!("{} {} {}", n.call, g.arrow, n.substituted)
        };
        let _ = writeln!(
            s,
            "{:>2}  {}  {}",
            i + 1,
            style::code(&rule),
            paint(&site, style::DIM)
        );
    }
    let mut last = format!(" =  {}", style::code(&root.result));
    if let Some(v) = &r.value {
        let _ = write!(last, " = {}", paint(&v.display, style::VALUE));
    }
    let _ = writeln!(s, "{last}");
    if !r.candidates.is_empty() && !onion {
        let _ = writeln!(s, "\n{}", paint("alternatives", style::HEADING));
        for l in tree::candidate_lines(&r.candidates, per_group, g, false) {
            let _ = writeln!(s, "  {l}");
        }
    }
    s
}
