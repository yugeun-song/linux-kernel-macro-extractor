use super::style::{self, paint};

pub fn narrow(
    header: &[&str],
    body: &[Vec<String>],
    headline: &[usize],
    g: &style::Glyphs,
) -> String {
    let details: Vec<usize> = (0..header.len())
        .filter(|i| !headline.contains(i))
        .collect();
    let labels: Vec<String> = details.iter().map(|&i| header[i].to_lowercase()).collect();
    let pad = labels.iter().map(String::len).max().unwrap_or(0) + 2;
    let mut out = String::new();
    for (n, row) in body.iter().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let head: Vec<&str> = headline.iter().map(|&i| row[i].as_str()).collect();
        out.push_str(&head.join("  "));
        out.push('\n');
        for (k, (&i, label)) in details.iter().zip(&labels).enumerate() {
            let glyph = if k + 1 == details.len() {
                g.last
            } else {
                g.branch
            };
            out.push_str(&format!(
                "{glyph}{}{}{}\n",
                paint(label, style::LABEL),
                " ".repeat(pad - label.len()),
                row[i]
            ));
        }
    }
    if !body.is_empty() {
        out.push('\n');
    }
    out
}

pub fn wide(header: &[&str], body: &[Vec<String>]) -> String {
    let columns = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in body {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(style::width(cell));
        }
    }
    let mut out = String::new();
    let line = |cells: Vec<String>, out: &mut String| {
        let mut text = String::new();
        for (i, cell) in cells.iter().enumerate() {
            text.push_str(cell);
            if i + 1 < columns {
                text.push_str(&" ".repeat(widths[i] - style::width(cell) + 2));
            }
        }
        out.push_str(text.trim_end());
        out.push('\n');
    };
    line(
        header.iter().map(|h| paint(h, style::LABEL)).collect(),
        &mut out,
    );
    for row in body {
        line(row.clone(), &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_align_on_display_width() {
        let t = wide(
            &["A", "LONGER"],
            &[
                vec!["xxxx".into(), "1".into()],
                vec!["y".into(), "22".into()],
            ],
        );
        let plain: Vec<String> = t
            .lines()
            .map(|l| anstream::adapter::strip_str(l).to_string())
            .collect();
        assert_eq!(plain, ["A     LONGER", "xxxx  1", "y     22"]);
    }
}
