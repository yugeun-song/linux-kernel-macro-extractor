use super::candidates;
use super::context::{self, ContextChoice, Ctx, context_info, contexts, file_context};
use super::expansion::Builder;
use super::report::{ExpNode, Report, SubjectKind};
use super::show::{ShowOptions, collect_diags, empty_report};
use crate::pp::kbuild::CompileModel;
use crate::pp::sim::Main;
use crate::syntax::lex::{self, Token, TokenKind};
use crate::workspace::Workspace;

const MAX_ROUNDS: usize = 5;

fn virtual_ctx(base: &CompileModel, headers: &[String]) -> Ctx {
    let content: String = headers.iter().map(|h| format!("#include {h}\n")).collect();
    let label = if headers.is_empty() {
        "virtual TU with only the force-included headers".to_string()
    } else {
        format!("virtual TU: {}", headers.join(" "))
    };
    Ctx {
        label,
        model: base.clone(),
        main: Main::Virtual {
            name: "<virtual TU>".into(),
            content,
        },
        stop: None,
        header: None,
    }
}

pub fn expand_text(ws: &Workspace, text: &str, opts: &ShowOptions) -> Report {
    let base = CompileModel::default_for(&ws.layout);
    let mut report = empty_report(ws, text, SubjectKind::Macro);
    report.context.what_if = opts.what_if.clone();
    let mut headers: Vec<String> = Vec::new();
    let explicit = !matches!(opts.context, ContextChoice::Auto);
    for round in 0..=MAX_ROUNDS {
        let ctx = if explicit {
            contexts(ws, &opts.context, &[], &base)
                .into_iter()
                .next()
                .unwrap_or_else(|| virtual_ctx(&base, &[]))
        } else {
            virtual_ctx(&base, &headers)
        };
        let (mut sim, info) = context::sim_for(ws, ctx.model.clone(), opts.autoconf.clone());
        if let Some((p, l)) = ctx.stop.clone() {
            sim.set_stop(p, l);
        }
        sim.run(ctx.main);
        sim.symbolic_builtins = true;
        let toks = lex::lex_all(text.as_bytes(), false, &mut sim.it);
        let (out, trace, truncated) = sim.expand_text(toks.clone(), true);
        if !explicit && round < MAX_ROUNDS {
            let more = candidates::headers_defining_undefined(ws, &mut sim, &out, &base, &headers);
            if !more.is_empty() {
                headers.extend(more);
                continue;
            }
        }
        report.context = context_info(ws, &info, &ctx.label, &opts.what_if);
        collect_diags(&sim, &mut report);
        if truncated {
            report
                .warnings
                .push("expansion truncated: it exceeded the size limits".into());
        }
        if !headers.is_empty() {
            report.notes.push(format!(
                "auto-included to define the macros used: {}",
                headers.join(" ")
            ));
        }
        let topts = opts.tree.clone();
        let mut b = Builder::new(&mut sim, ws, &ctx.model, &topts);
        let children = trace.map(|t| b.build(&t)).unwrap_or_default();
        let value = b.value_of(&out);
        let result = lex::render(&out, &b.sim.it);
        let refined = b.refine(&out).filter(|r| *r != result);
        report.value = value.clone();
        report.expansion = Some(ExpNode {
            target: None,
            name: "input".into(),
            call: text.to_string(),
            site: None,
            why: Vec::new(),
            alternatives: Vec::new(),
            args: Vec::new(),
            substituted: text.to_string(),
            result,
            refined,
            value,
            children,
            note: None,
            repeat_of: None,
            config: None,
            kconfig: None,
        });
        return report;
    }
    report
}

pub fn line_text(content: &[u8], line: u32) -> Option<(String, u32)> {
    let lines: Vec<&[u8]> = content.split(|&b| b == b'\n').collect();
    let start = (line as usize).checked_sub(1)?;
    let mut text = String::new();
    let mut depth = 0i32;
    let mut last = start;
    for (i, l) in lines.iter().enumerate().skip(start).take(64) {
        let s = String::from_utf8_lossy(l);
        for c in s.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
        }
        text.push_str(&s);
        text.push('\n');
        last = i;
        if depth <= 0 && !s.trim_end().ends_with('\\') && !s.trim_end().ends_with(',') {
            break;
        }
    }
    Some((text, last as u32 + 1))
}

pub fn expand_line(
    ws: &Workspace,
    rel: &str,
    line: u32,
    opts: &ShowOptions,
) -> Result<Report, String> {
    let base = CompileModel::default_for(&ws.layout);
    let abs = ws.layout.srctree.join(rel);
    let content = std::fs::read(&abs).map_err(|e| format!("{rel}: {e}"))?;
    let (text, end) =
        line_text(&content, line).ok_or_else(|| format!("{rel}:{line}: no such line"))?;
    let ctx = file_context(ws, &base, rel, Some(line));
    let (mut sim, info) = context::sim_for(ws, ctx.model.clone(), opts.autoconf.clone());
    if let Some((p, l)) = ctx.stop.clone() {
        sim.set_stop(p, l);
    }
    sim.run(ctx.main);
    sim.symbolic_builtins = true;
    let mut report = empty_report(ws, &format!("{rel}:{line}"), SubjectKind::Macro);
    report.context = context_info(ws, &info, &ctx.label, &opts.what_if);
    collect_diags(&sim, &mut report);
    let toks = lex::lex_all(text.as_bytes(), false, &mut sim.it);
    let (out, trace, _) = sim.expand_text(toks, true);
    let topts = opts.tree.clone();
    let mut b = Builder::new(&mut sim, ws, &ctx.model, &topts);
    let children = trace.map(|t| b.build(&t)).unwrap_or_default();
    let result = lex::render(&out, &b.sim.it);
    if end > line {
        report.notes.push(format!(
            "the statement continues to line {end}; all of it was expanded"
        ));
    }
    if children.is_empty() {
        report.notes.push("no macro is used on this line".into());
    }
    report.expansion = Some(ExpNode {
        target: None,
        name: format!("{rel}:{line}"),
        call: text.trim().to_string(),
        site: None,
        why: Vec::new(),
        alternatives: Vec::new(),
        args: Vec::new(),
        substituted: text.trim().to_string(),
        result,
        refined: None,
        value: None,
        children,
        note: None,
        repeat_of: None,
        config: None,
        kconfig: None,
    });
    Ok(report)
}

pub fn dump(ws: &Workspace, opts: &ShowOptions) -> Result<(String, Vec<DumpRow>), String> {
    let base = CompileModel::default_for(&ws.layout);
    let ctx = match &opts.context {
        ContextChoice::Auto => {
            return Err("dump needs a context: pass --in FILE[:LINE] or --include HEADER".into());
        }
        other => contexts(ws, other, &[], &base)
            .into_iter()
            .next()
            .ok_or("no context")?,
    };
    let (mut sim, _) = context::sim_for(ws, ctx.model.clone(), opts.autoconf.clone());
    if let Some((p, l)) = ctx.stop.clone() {
        sim.set_stop(p, l);
    }
    sim.run(ctx.main);
    sim.symbolic_builtins = true;
    let mut names: Vec<_> = sim.macro_names().collect();
    names.sort_by(|a, b| sim.it.get(*a).cmp(sim.it.get(*b)));
    let topts = opts.tree.clone();
    let mut rows = Vec::with_capacity(names.len());
    for s in names {
        let Some(def) = sim.def_of(s) else { continue };
        if def.builtin.is_some() {
            continue;
        }
        let mut b = Builder::new(&mut sim, ws, &ctx.model, &topts);
        let site = b.site_of(&def);
        let params = super::explain::params_of(&def, &b.sim.it);
        let body = lex::render(&def.body, &b.sim.it);
        let name = b.sim.it.get(s).to_string();
        let (value, expansion) = if def.is_function() {
            (None, None)
        } else {
            let tok = Token::new(TokenKind::Ident, s, 0);
            let (out, _, _) = b.sim.expand_text(vec![tok], false);
            let v = b.value_of(&out).map(|v| v.display);
            (v, Some(lex::render(&out, &b.sim.it)))
        };
        rows.push(DumpRow {
            name,
            params: (!params.is_empty() || def.is_function()).then(|| params.join(",")),
            body,
            expansion,
            value,
            path: site.as_ref().map(|s| s.path.clone()).unwrap_or_default(),
            line: site.map(|s| s.line).unwrap_or(0),
        });
    }
    Ok((ctx.label, rows))
}

#[derive(serde::Serialize)]
pub struct DumpRow {
    pub name: String,
    pub params: Option<String>,
    pub body: String,
    pub expansion: Option<String>,
    pub value: Option<String>,
    pub path: String,
    pub line: u32,
}
