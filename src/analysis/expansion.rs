use rustc_hash::FxHashMap;

use super::candidates::{self, Cand, Reach};
use super::explain;
use super::report::{Arg, CTarget, Candidate, Condition, ExpNode, Site, Value};
use super::value::{self, Evaluated, ValueCtx};
use crate::intern::{Interner, Sym, sym};
use crate::pp::expand::Trace;
use crate::pp::kbuild::CompileModel;
use crate::pp::macros::{MacroDef, Origin, Params, Variadic};
use crate::pp::sim::Sim;
use crate::syntax::lex::{self, F_NOEXP, F_SPACE, Token, TokenKind};
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alternatives {
    None,
    All,
}

#[derive(Clone, Debug)]
pub struct TreeOpts {
    pub max_depth: Option<usize>,
    pub alternatives: Alternatives,
    pub reach: Reach,
    pub full_bodies: bool,
}

const BODY_LINES: usize = 20;

fn expand_tabs(line: &str) -> String {
    let mut out = String::new();
    let mut col = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let n = 8 - col % 8;
            out.push_str(&" ".repeat(n));
            col += n;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    out
}

pub struct Builder<'a, 'w> {
    pub sim: &'a mut Sim<'w>,
    pub ws: &'w Workspace,
    pub model: &'a CompileModel,
    pub opts: &'a TreeOpts,
    pub char_unsigned: bool,
    seen: FxHashMap<String, String>,
    alt_cache: FxHashMap<Sym, Vec<Candidate>>,
    targets: rustc_hash::FxHashSet<Sym>,
    known: FxHashMap<Sym, Sym>,
    vars: rustc_hash::FxHashSet<Sym>,
}

const MAX_TEXT: usize = 4000;

fn clip(s: String) -> String {
    if s.len() <= MAX_TEXT {
        return s;
    }
    let mut cut = MAX_TEXT;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{} … ({} more bytes)", &s[..cut], s.len() - cut)
}

impl<'a, 'w> Builder<'a, 'w> {
    pub fn new(
        sim: &'a mut Sim<'w>,
        ws: &'w Workspace,
        model: &'a CompileModel,
        opts: &'a TreeOpts,
    ) -> Self {
        let char_unsigned =
            model.flags.iter().any(|f| f == "-funsigned-char") || ws.target.arch.srcarch() != "x86";
        Builder {
            sim,
            ws,
            model,
            opts,
            char_unsigned,
            seen: FxHashMap::default(),
            alt_cache: FxHashMap::default(),
            targets: rustc_hash::FxHashSet::default(),
            known: FxHashMap::default(),
            vars: rustc_hash::FxHashSet::default(),
        }
    }

    pub fn site_of(&mut self, def: &MacroDef) -> Option<Site> {
        let name = self.sim.it.get(def.name).to_string();
        let body = lex::render(&def.body, &self.sim.it);
        let params = explain::params_of(def, &self.sim.it);
        let head = if def.is_function() {
            format!("{name}({})", params.join(", "))
        } else {
            name
        };
        match def.origin {
            Origin::Source | Origin::Synthesized => {
                let site = def.site?;
                let content = self.sim.files.content(site.file);
                let toks = lex::lex_all(
                    &content[site.off as usize..site.end as usize],
                    false,
                    &mut self.sim.it,
                );
                Some(Site {
                    path: self.sim.files.display(site.file).to_string(),
                    line: site.line,
                    text: clip(lex::render(&toks, &self.sim.it)),
                    origin: "source".into(),
                })
            }
            Origin::Predefined => Some(Site {
                path: "<compiler predefined>".into(),
                line: 0,
                text: format!("#define {head} {body}"),
                origin: "predefined".into(),
            }),
            Origin::CommandLine => Some(Site {
                path: "<command line>".into(),
                line: 0,
                text: format!("-D{head}={body}"),
                origin: "command line".into(),
            }),
            Origin::Builtin => Some(Site {
                path: "<builtin>".into(),
                line: 0,
                text: format!("{head} (preprocessor builtin)"),
                origin: "builtin".into(),
            }),
        }
    }

    pub fn why(&mut self, def: &MacroDef) -> Vec<Condition> {
        let Some(site) = def.site else {
            return Vec::new();
        };
        let cond = self
            .sim
            .events_for(def.name)
            .find(|e| {
                e.site
                    .is_some_and(|s| s.file == site.file && s.line == site.line)
            })
            .map(|e| e.cond);
        let path = self.sim.files.display(site.file).to_string();
        match cond {
            Some(c) => explain::chain(self.sim, c, &self.ws.config)
                .into_iter()
                .filter(|cond| cond.path == path)
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn refine(&self, toks: &[Token]) -> Option<String> {
        super::c_expr::simplify(toks, &self.sim.it, &self.vars).map(clip)
    }

    pub fn value_of(&self, toks: &[Token]) -> Option<Value> {
        if toks.len() > 4000 {
            return None;
        }
        let substituted: Vec<Token>;
        let toks = if !self.known.is_empty()
            && toks
                .iter()
                .any(|t| t.kind == TokenKind::Ident && self.known.contains_key(&t.sym))
        {
            substituted = toks
                .iter()
                .map(|t| match self.known.get(&t.sym) {
                    Some(&number) if t.kind == TokenKind::Ident => {
                        Token::new(TokenKind::Number, number, t.flags)
                    }
                    _ => *t,
                })
                .collect();
            &substituted
        } else {
            toks
        };
        let cx = ValueCtx {
            it: &self.sim.it,
            config: &self.ws.config,
            char_unsigned: self.char_unsigned,
        };
        match value::evaluate(toks, &cx) {
            Evaluated::Int(v) => {
                let (display, ty) = value::display(&v, self.char_unsigned);
                Some(Value {
                    display,
                    ty: Some(ty),
                })
            }
            Evaluated::Str(s) => Some(Value {
                display: format!("\"{s}\""),
                ty: Some("string".into()),
            }),
            Evaluated::NotConstant(_) => None,
        }
    }

    pub fn alternatives(&mut self, sym: Sym, active: Option<&Site>) -> Vec<Candidate> {
        if self.opts.alternatives == Alternatives::None {
            return Vec::new();
        }
        if let Some(v) = self.alt_cache.get(&sym) {
            return v.clone();
        }
        let name = self.sim.it.get(sym).to_string();
        let cands: Vec<Cand> = candidates::gather(self.ws, &name, self.model, self.opts.reach);
        let mut out: Vec<Candidate> = Vec::with_capacity(cands.len());
        for c in &cands {
            if active.is_some_and(|a| a.path == c.path && a.line == c.posting.line) {
                continue;
            }
            let j = candidates::judge(self.sim, sym, c)
                .unwrap_or_else(|| candidates::static_status(c, self.ws));
            out.push(candidates::to_candidate(self.sim, self.ws, c, &j));
        }
        out.sort_by_key(|c| (c.status.order(), c.path.clone(), c.line));
        self.alt_cache.insert(sym, out.clone());
        out
    }

    pub fn invocation(&mut self, def: &MacroDef) -> Vec<Token> {
        use crate::intern::sym;
        let mut toks = vec![Token::new(TokenKind::Ident, def.name, 0)];
        let Params::Function { names, variadic } = &def.params else {
            return toks;
        };
        let mut params: Vec<Sym> = names.clone();
        if *variadic == Variadic::Anonymous {
            params.push(sym::VA_ARGS);
        }
        toks.push(Token::new(TokenKind::Punct, sym::LPAREN, 0));
        for (i, p) in params.into_iter().enumerate() {
            if i > 0 {
                toks.push(Token::new(TokenKind::Punct, sym::COMMA, 0));
            }
            let space = if i > 0 { F_SPACE } else { 0 };
            toks.push(Token::new(TokenKind::Ident, p, F_NOEXP | space));
        }
        toks.push(Token::new(TokenKind::Punct, sym::RPAREN, 0));
        toks
    }

    pub fn build(&mut self, trace: &Trace) -> Vec<ExpNode> {
        let roots: Vec<u32> = trace.nodes[0].children.clone();
        roots
            .into_iter()
            .map(|id| self.node(trace, id, 0))
            .collect()
    }

    fn node(&mut self, trace: &Trace, id: u32, depth: usize) -> ExpNode {
        let n = trace.nodes[id as usize].clone();
        let name = self.sim.it.get(n.sym).to_string();
        let def = n.def.clone();
        let params = def
            .as_ref()
            .map(|d| explain::params_of(d, &self.sim.it))
            .unwrap_or_default();
        let call = if def.as_ref().is_some_and(|d| d.is_function()) {
            let args: Vec<String> = n
                .args
                .iter()
                .map(|a| lex::render(a, &self.sim.it))
                .collect();
            format!("{name}({})", args.join(", "))
        } else {
            name.clone()
        };
        let result_tokens = if n.result.is_empty() && !n.substituted.is_empty() {
            self.sim.expand_text(n.substituted.clone(), false).0
        } else {
            n.result.clone()
        };
        let result = clip(lex::render(&result_tokens, &self.sim.it));
        let refined = self.refine(&result_tokens).filter(|r| *r != result);
        let key = format!("{call}\u{0}{result}");
        let site = def.as_ref().and_then(|d| self.site_of(d));
        if let Some(first) = self.seen.get(&key) {
            return ExpNode {
                name,
                call,
                site,
                why: Vec::new(),
                alternatives: Vec::new(),
                args: Vec::new(),
                substituted: String::new(),
                result,
                refined,
                value: self.value_of(&result_tokens),
                children: Vec::new(),
                note: None,
                repeat_of: Some(first.clone()),
                config: None,
                kconfig: None,
                target: None,
            };
        }
        self.seen.insert(key, call.clone());
        let why = def.as_ref().map(|d| self.why(d)).unwrap_or_default();
        let alternatives = self.alternatives(n.sym, site.as_ref());
        let args = params
            .iter()
            .enumerate()
            .map(|(i, p)| Arg {
                param: p.clone(),
                raw: n
                    .args
                    .get(i)
                    .map(|a| lex::render(a, &self.sim.it))
                    .unwrap_or_default(),
                expanded: n
                    .args_expanded
                    .get(i)
                    .and_then(|e| e.as_ref())
                    .map(|e| clip(lex::render(e, &self.sim.it))),
                refined: n
                    .args_expanded
                    .get(i)
                    .and_then(|e| e.as_ref())
                    .and_then(|e| {
                        let shown = clip(lex::render(e, &self.sim.it));
                        self.refine(e).filter(|r| *r != shown)
                    }),
            })
            .collect();
        let mut note = None;
        let deep_enough = self.opts.max_depth.is_some_and(|m| depth + 1 >= m);
        let mut children = if deep_enough {
            if !n.children.is_empty() {
                note = Some(format!(
                    "{} nested expansions hidden (--depth)",
                    n.children.len()
                ));
            }
            Vec::new()
        } else {
            n.children
                .iter()
                .map(|&c| self.node(trace, c, depth + 1))
                .collect()
        };
        let names_itself = result_tokens
            .iter()
            .any(|t| t.kind == TokenKind::Ident && t.sym == n.sym && t.flags & F_NOEXP != 0);
        let mut value = self.value_of(&result_tokens);
        if names_itself && !deep_enough {
            let prefer = def.as_ref().and_then(|d| d.site).map(|s| s.file);
            let resolved_before = self.targets.contains(&n.sym);
            match self.c_target(n.sym, prefer, depth + 1) {
                Some(t) => {
                    if value.is_none() && result.trim() == name {
                        value = t.value.clone();
                    }
                    children.push(t);
                }
                None if resolved_before => {}
                None => {
                    note = Some(format!(
                        "{name} names itself, but no function or enum constant named {name} is visible in this context"
                    ));
                }
            }
        }
        let config = explain::config_site(&self.ws.config, &name);
        let positional = n.result.iter().any(|t| {
            t.flags & F_NOEXP != 0
                && matches!(
                    self.sim.it.get(t.sym),
                    "__COUNTER__" | "__LINE__" | "__FILE__"
                )
        });
        if positional && note.is_none() {
            note = Some("__COUNTER__/__LINE__/__FILE__ are kept by name: their value depends on where the macro is used".into());
        }
        ExpNode {
            name,
            call,
            site,
            why,
            alternatives,
            args,
            substituted: clip(lex::render(&n.substituted, &self.sim.it)),
            value,
            result,
            refined,
            config,
            kconfig: None,
            children,
            note,
            repeat_of: None,
            target: None,
        }
    }

    fn c_target(
        &mut self,
        sym: Sym,
        prefer: Option<crate::pp::macros::SrcFile>,
        depth: usize,
    ) -> Option<ExpNode> {
        if !self.targets.insert(sym) {
            return None;
        }
        let name = self.sim.it.get(sym).to_string();
        let is_enum = self
            .ws
            .index
            .lookup(&name)
            .iter()
            .any(|p| p.kind == crate::index::DefKind::Enumerator);
        if is_enum && let Some(node) = self.enum_target(&name) {
            return Some(node);
        }
        let f = super::c_func::find(self.sim, &name, prefer)?;
        let content = self.sim.files.content(f.file);
        let text = String::from_utf8_lossy(&content);
        let first = f.found.start_line as usize - 1;
        let last = f.found.end_line as usize;
        let all: Vec<&str> = text.lines().skip(first).take(last - first).collect();
        let shown = if self.opts.full_bodies {
            all.len()
        } else {
            all.len().min(BODY_LINES)
        };
        let lines: Vec<String> = all[..shown].iter().map(|l| expand_tabs(l)).collect();
        let mut children = Vec::new();
        let params = signature_params(&f.signature, &name, &mut self.sim.it);
        let vars: rustc_hash::FxHashSet<Sym> = params.iter().filter_map(|(_, s)| *s).collect();
        let mut returns = None;
        if let Some((open, close)) = f.found.body {
            let open_line = f.found.start_line
                + content[f.found.start..open]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count() as u32;
            let limit_line = f.found.start_line + shown as u32;
            let mut body = Vec::new();
            let mut in_directive = false;
            let chunks = content[open + 1..close].split_inclusive(|&b| b == b'\n');
            for (line, chunk) in (open_line..).zip(chunks) {
                let first_text = chunk.iter().position(|b| !b.is_ascii_whitespace());
                let directive = in_directive || first_text.is_some_and(|i| chunk[i] == b'#');
                in_directive = directive && chunk.ends_with(b"\\\n");
                if line < limit_line && !directive && self.sim.skip_at(f.file, line).is_none() {
                    body.extend_from_slice(chunk);
                } else {
                    body.push(b'\n');
                }
            }
            let toks = lex::lex_all(&body, false, &mut self.sim.it);
            let (out, trace, truncated) = self.sim.expand_text(toks, true);
            if shown == all.len()
                && !truncated
                && let Some(expr) = single_return(&out, &self.sim.it)
            {
                returns = super::c_expr::simplify(expr, &self.sim.it, &vars)
                    .or_else(|| Some(lex::render(expr, &self.sim.it)));
            }
            if let Some(trace) = trace
                && !self.opts.max_depth.is_some_and(|m| depth + 1 >= m)
            {
                let outer = std::mem::replace(&mut self.vars, vars);
                children = trace.nodes[0]
                    .children
                    .clone()
                    .into_iter()
                    .map(|id| self.node(&trace, id, depth + 1))
                    .collect();
                self.vars = outer;
            }
        }
        Some(ExpNode {
            name: name.clone(),
            call: format!("{name}()"),
            site: Some(Site {
                path: self.sim.files.display(f.file).to_string(),
                line: f.found.line,
                text: f.signature.clone(),
                origin: "function".into(),
            }),
            why: Vec::new(),
            alternatives: Vec::new(),
            args: Vec::new(),
            substituted: String::new(),
            result: String::new(),
            refined: None,
            value: None,
            children,
            note: None,
            repeat_of: None,
            config: None,
            kconfig: None,
            target: Some(CTarget {
                kind: f.kind.to_string(),
                lines,
                hidden: all.len() - shown,
                params: params.into_iter().map(|(text, _)| text).collect(),
                returns,
            }),
        })
    }

    fn enum_target(&mut self, name: &str) -> Option<ExpNode> {
        let opts = super::show::ShowOptions {
            context: super::context::ContextChoice::Auto,
            tree: TreeOpts {
                max_depth: Some(1),
                alternatives: Alternatives::None,
                reach: self.opts.reach,
                full_bodies: false,
            },
            autoconf: self.sim.autoconf_override().map(str::to_string),
            what_if: Vec::new(),
            census: false,
        };
        let r = super::enums::show_enum(self.ws, name, &opts)?;
        let info = r.enum_info.as_ref()?;
        let member = info.members.iter().find(|m| m.name == name)?;
        let text = match &member.init {
            Some(init) => format!("{name} = {init}"),
            None => name.to_string(),
        };
        let display = r.value.as_ref().map(|v| v.display.clone());
        if let Some(number) = display
            .as_deref()
            .and_then(|d| d.split_whitespace().next())
            .filter(|n| n.parse::<i128>().is_ok() || n.parse::<u128>().is_ok())
        {
            let sym = self.sim.it.intern(name);
            let number = self.sim.it.intern(number);
            self.known.insert(sym, number);
        }
        Some(ExpNode {
            name: name.to_string(),
            call: name.to_string(),
            site: Some(Site {
                path: info.site.path.clone(),
                line: member.line,
                text,
                origin: "enum".into(),
            }),
            why: Vec::new(),
            alternatives: Vec::new(),
            args: Vec::new(),
            substituted: String::new(),
            result: display.unwrap_or_else(|| name.to_string()),
            refined: None,
            value: r.value.clone(),
            children: Vec::new(),
            note: (!member.active).then(|| "this enumerator is excluded by #if here".to_string()),
            repeat_of: None,
            config: None,
            kconfig: None,
            target: Some(CTarget {
                kind: match &info.tag {
                    Some(t) => format!("enum constant of enum {t}"),
                    None => "enum constant".to_string(),
                },
                lines: Vec::new(),
                hidden: 0,
                params: Vec::new(),
                returns: None,
            }),
        })
    }
}

fn signature_params(signature: &str, name: &str, it: &mut Interner) -> Vec<(String, Option<Sym>)> {
    let toks = lex::lex_all(signature.as_bytes(), false, it);
    let Some(open) = toks
        .windows(2)
        .position(|w| {
            w[0].kind == TokenKind::Ident && it.get(w[0].sym) == name && w[1].is_punct(sym::LPAREN)
        })
        .map(|i| i + 1)
    else {
        return Vec::new();
    };
    let mut depth = 0usize;
    let mut params: Vec<Vec<Token>> = vec![Vec::new()];
    for t in &toks[open..] {
        if t.is_punct(sym::LPAREN) || t.is_punct(sym::LBRACKET) {
            depth += 1;
            if depth == 1 {
                continue;
            }
        } else if t.is_punct(sym::RPAREN) || t.is_punct(sym::RBRACKET) {
            depth -= 1;
            if depth == 0 {
                break;
            }
        } else if depth == 1 && t.is_punct(sym::COMMA) {
            params.push(Vec::new());
            continue;
        }
        if let Some(p) = params.last_mut() {
            p.push(*t);
        }
    }
    let words = |p: &[Token]| -> Vec<&str> {
        p.iter()
            .filter(|t| t.kind == TokenKind::Ident)
            .map(|t| it.get(t.sym))
            .collect()
    };
    if params.len() == 1 && matches!(words(&params[0]).as_slice(), [] | ["void"]) {
        return Vec::new();
    }
    params
        .iter()
        .map(|p| {
            let pointer = p
                .windows(2)
                .position(|w| w[0].is_punct(sym::LPAREN) && w[1].is_punct(sym::STAR))
                .and_then(|k| {
                    let mut j = k + 1;
                    while p.get(j).is_some_and(|t| {
                        t.is_punct(sym::STAR)
                            || (t.kind == TokenKind::Ident
                                && super::c_expr::is_keyword(it.get(t.sym)))
                    }) {
                        j += 1;
                    }
                    let name = p.get(j)?;
                    (name.kind == TokenKind::Ident
                        && !super::c_expr::is_keyword(it.get(name.sym))
                        && p.get(j + 1).is_some_and(|t| t.is_punct(sym::RPAREN)))
                    .then_some(name)
                });
            let mut level = 0i32;
            let mut top: Vec<Token> = Vec::new();
            for t in p {
                if t.is_punct(sym::LPAREN) || t.is_punct(sym::LBRACKET) {
                    level += 1;
                } else if t.is_punct(sym::RPAREN) || t.is_punct(sym::RBRACKET) {
                    level -= 1;
                } else if level == 0 {
                    top.push(*t);
                }
            }
            let word = |t: &Token| (t.kind == TokenKind::Ident).then(|| it.get(t.sym));
            let last = top.last().copied().filter(|t| {
                let tagged = top.len() >= 2
                    && word(&top[top.len() - 2])
                        .is_some_and(|w| matches!(w, "struct" | "union" | "enum"));
                word(t).is_some_and(|w| !super::c_expr::is_keyword(w)) && !tagged
            });
            let typed = last.is_some()
                && top[..top.len() - 1].iter().any(|t| match word(t) {
                    Some(w) => super::c_expr::is_type_word(w) || !super::c_expr::is_keyword(w),
                    None => false,
                });
            let named = pointer.copied().or(last.filter(|_| typed));
            match named {
                Some(t) => (it.get(t.sym).to_string(), Some(t.sym)),
                None => (lex::render(p, it), None),
            }
        })
        .collect()
}

fn single_return<'t>(out: &'t [Token], it: &Interner) -> Option<&'t [Token]> {
    let toks: Vec<usize> = (0..out.len())
        .filter(|&i| out[i].kind != TokenKind::Placemarker)
        .collect();
    let first = *toks.first()?;
    let last = *toks.last()?;
    if out[first].kind != TokenKind::Ident
        || it.get(out[first].sym) != "return"
        || !out[last].is_punct(sym::SEMI)
    {
        return None;
    }
    let expr = &out[first + 1..last];
    let mut depth = 0i32;
    for t in expr {
        if t.is_punct(sym::LPAREN) || t.is_punct(sym::LBRACKET) || t.is_punct(sym::LBRACE) {
            depth += 1;
        } else if t.is_punct(sym::RPAREN) || t.is_punct(sym::RBRACKET) || t.is_punct(sym::RBRACE) {
            depth -= 1;
        } else if depth == 0 && (t.is_punct(sym::SEMI) || t.is_punct(sym::LBRACE)) {
            return None;
        }
    }
    (depth == 0 && expr.iter().any(|t| t.kind != TokenKind::Placemarker)).then_some(expr)
}
