use crate::analysis::expand;
use crate::cli::Global;
use crate::error::{Error, Result, exit};
use crate::render::Output;
use crate::workspace::Workspace;

use super::show::{memo_key, options};

fn emit(out: &mut Output, report: &crate::analysis::report::Report, g: &Global) -> Result<()> {
    if g.json {
        let line = serde_json::to_string(report).map_err(|e| Error::Internal(e.to_string()))?;
        out.emit(&format!("{line}\n"));
    } else {
        out.emit(&crate::render::report_text(report, g));
    }
    Ok(())
}

pub fn run_expand(ws: &Workspace, text: &[String], g: &Global) -> Result<i32> {
    let text = text.join(" ");
    if text.trim().is_empty() {
        return Err(Error::Usage("expand needs some text to expand".into()));
    }
    let (opts, _) = options(ws, g)?;
    let memo = ws.memo();
    let key = memo_key("expand", &text, g);
    let report = match (!g.no_memo)
        .then(|| memo.get(&key))
        .flatten()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(r) => r,
        None => {
            let r = expand::expand_text(ws, &text, &opts);
            if !g.no_memo
                && let Ok(b) = serde_json::to_vec(&r)
            {
                memo.put(&key, &b);
            }
            r
        }
    };
    let mut out = Output::open(g);
    emit(&mut out, &report, g)?;
    Ok(exit::OK)
}

pub fn run_line(ws: &Workspace, location: &str, g: &Global) -> Result<i32> {
    let (rel, line) = match super::validate::location(ws, location, "line")? {
        (rel, Some(line)) => (rel, line),
        (_, None) => return Err(Error::Usage(format!("line {location}: expected FILE:LINE"))),
    };
    let (opts, _) = options(ws, g)?;
    let memo = ws.memo();
    let key = memo_key("line", location, g);
    let report = match (!g.no_memo)
        .then(|| memo.get(&key))
        .flatten()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(r) => r,
        None => {
            let r = expand::expand_line(ws, &rel, line, &opts).map_err(Error::Usage)?;
            if !g.no_memo
                && let Ok(b) = serde_json::to_vec(&r)
            {
                memo.put(&key, &b);
            }
            r
        }
    };
    let mut out = Output::open(g);
    emit(&mut out, &report, g)?;
    Ok(exit::OK)
}

pub fn run_dump(ws: &Workspace, g: &Global, json_format: bool) -> Result<i32> {
    let (opts, _) = options(ws, g)?;
    let (label, rows) = expand::dump(ws, &opts).map_err(Error::Usage)?;
    let mut out = Output::open(g);
    if g.json || json_format {
        for r in &rows {
            let line = serde_json::to_string(r).map_err(|e| Error::Internal(e.to_string()))?;
            out.emit(&format!("{line}\n"));
        }
    } else {
        out.emit(&format!(
            "# {label}\n# name\tparams\tvalue\texpansion\tdefinition\tlocation\n"
        ));
        for r in &rows {
            out.emit(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}:{}\n",
                r.name,
                r.params.as_deref().unwrap_or("-"),
                r.value.as_deref().unwrap_or("-"),
                r.expansion.as_deref().unwrap_or("-").replace('\t', " "),
                r.body.replace('\t', " "),
                crate::render::paths::openable(&r.path),
                r.line
            ));
        }
    }
    Ok(exit::OK)
}
