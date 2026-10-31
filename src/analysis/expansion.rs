use rustc_hash::FxHashMap;

use super::c_type::{self, Base, Step, TypeCx, TypeEnv};
use super::candidates::{self, Cand, Reach};
use super::explain;
use super::report::{
    Arg, CTarget, Candidate, Condition, ExpNode, LayoutMember, Site, TypeLayout, Value,
};
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

pub fn char_unsigned(ws: &Workspace, model: &CompileModel) -> bool {
    model.flags.iter().any(|f| f == "-funsigned-char") || ws.target.arch.srcarch() != "x86"
}

pub struct Builder<'a, 'w> {
    pub sim: &'a mut Sim<'w>,
    pub ws: &'w Workspace,
    pub model: &'a CompileModel,
    pub opts: &'a TreeOpts,
    pub char_unsigned: bool,
    seen: FxHashMap<String, String>,
    shown_types: rustc_hash::FxHashSet<String>,
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
        let char_unsigned = char_unsigned(ws, model);
        Builder {
            sim,
            ws,
            model,
            opts,
            char_unsigned,
            seen: FxHashMap::default(),
            shown_types: rustc_hash::FxHashSet::default(),
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

    pub fn types(&self) -> TypeCx<'_, 'w> {
        TypeCx::new(self.ws, &*self.sim, self.char_unsigned)
    }

    pub fn bind(&mut self, names: impl IntoIterator<Item = Sym>) {
        self.vars.extend(names);
    }

    pub fn refine(&self, toks: &[Token]) -> Option<String> {
        let env = self.types();
        super::c_expr::simplify(toks, &self.sim.it, &self.vars, &env).map(clip)
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
        let env = self.types();
        if let Some(k) = super::c_expr::evaluate(toks, &self.sim.it, &self.vars, &env) {
            let v = value::CVal {
                v: k.v,
                ty: k.ty,
                pointer: false,
            };
            let (display, ty) = value::display(&v, self.char_unsigned);
            return Some(Value {
                display,
                ty: Some(ty),
            });
        }
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
                layout: None,
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
            let mut kids: Vec<ExpNode> = n
                .children
                .iter()
                .map(|&c| self.node(trace, c, depth + 1))
                .collect();
            kids.extend(self.evidence(&n));
            kids
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
            layout: None,
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
                let env = self.types();
                returns = super::c_expr::simplify(expr, &self.sim.it, &vars, &env)
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
            layout: None,
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
            layout: None,
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

const LAYOUT_LINES: usize = 20;

enum Operand {
    Size(Vec<Token>, String),
    Offset(Vec<Token>, Vec<Token>, String),
    Enum(Sym),
}

fn group_end(toks: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate().skip(open) {
        if t.is_punct(sym::LPAREN) {
            depth += 1;
        } else if t.is_punct(sym::RPAREN) {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

fn top_comma(toks: &[Token]) -> Option<usize> {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        if t.is_punct(sym::LPAREN) || t.is_punct(sym::LBRACKET) {
            depth += 1;
        } else if t.is_punct(sym::RPAREN) || t.is_punct(sym::RBRACKET) {
            depth -= 1;
        } else if depth == 0 && t.is_punct(sym::COMMA) {
            return Some(i);
        }
    }
    None
}

fn designator<'t>(words: &[&'t str]) -> Option<Vec<Step<'t>>> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut first = true;
    while i < words.len() {
        match words[i] {
            "." if !first => {
                out.push(Step::Field(words.get(i + 1)?));
                i += 2;
            }
            "[" => {
                let k = super::c_const::literal(words.get(i + 1)?)?;
                if words.get(i + 2) != Some(&"]") {
                    return None;
                }
                out.push(Step::Index(u64::try_from(k.v).ok()?));
                i += 3;
            }
            w if first => {
                out.push(Step::Field(w));
                i += 1;
            }
            _ => return None,
        }
        first = false;
    }
    (!out.is_empty()).then_some(out)
}

impl Builder<'_, '_> {
    fn operands(&self, toks: &[Token]) -> Vec<(String, Operand)> {
        let it = &self.sim.it;
        let toks: Vec<Token> = toks
            .iter()
            .filter(|t| t.kind != TokenKind::Placemarker)
            .copied()
            .collect();
        let mut out: Vec<(String, Operand)> = Vec::new();
        let mut enums = rustc_hash::FxHashSet::default();
        let mut i = 0;
        while i < toks.len() {
            let t = toks[i];
            if t.kind != TokenKind::Ident {
                i += 1;
                continue;
            }
            let w = it.get(t.sym);
            if matches!(w, "sizeof" | "__builtin_offsetof")
                && toks.get(i + 1).is_some_and(|n| n.is_punct(sym::LPAREN))
                && let Some(end) = group_end(&toks, i + 1)
            {
                let inner = toks[i + 2..end].to_vec();
                let text = lex::render(&inner, it);
                let spelled = lex::render(&toks[i..=end], it);
                if w == "sizeof" {
                    out.push((format!("s:{text}"), Operand::Size(inner, spelled)));
                } else if let Some(c) = top_comma(&inner) {
                    out.push((
                        format!("o:{text}"),
                        Operand::Offset(inner[..c].to_vec(), inner[c + 1..].to_vec(), spelled),
                    ));
                }
                i = end + 1;
                continue;
            }
            if !self.vars.contains(&t.sym)
                && enums.insert(t.sym)
                && self
                    .ws
                    .index
                    .lookup(w)
                    .iter()
                    .any(|p| p.kind == crate::index::DefKind::Enumerator)
            {
                out.push((format!("e:{w}"), Operand::Enum(t.sym)));
            }
            i += 1;
        }
        out
    }

    fn evidence(&mut self, n: &crate::pp::expand::Node) -> Vec<ExpNode> {
        let from_args: rustc_hash::FxHashSet<String> = n
            .args
            .iter()
            .flat_map(|a| self.operands(a))
            .map(|(k, _)| k)
            .collect();
        let mut out = Vec::new();
        for (key, op) in self.operands(&n.substituted) {
            if from_args.contains(&key) || self.shown_types.contains(&key) {
                continue;
            }
            let node = match op {
                Operand::Size(t, spelled) => self.size_node(&t, spelled),
                Operand::Offset(t, d, spelled) => self.offset_node(&t, &d, spelled),
                Operand::Enum(s) if s != n.sym => self.enum_node(s),
                Operand::Enum(_) => None,
            };
            if let Some(node) = node {
                self.shown_types.insert(key);
                out.push(node);
            }
        }
        out
    }

    fn type_site(&self, d: &crate::types::DefView<'_>, table: &crate::types::Table) -> Site {
        let env = self.types();
        let abs = env.abs(d.file);
        let path = match self.sim.files.lookup(&abs) {
            Some(f) => self.sim.files.display(f).to_string(),
            None => self.ws.locator().display_for(&abs),
        };
        let text = match d.kind {
            crate::types::DefKind::Typedef => {
                format!("typedef {}", c_type::declaration(table, d.target, d.name))
            }
            k if d.name.is_empty() => format!("{} {{...}}", k.keyword()),
            k => format!("{} {}", k.keyword(), d.name),
        };
        Site {
            path,
            line: d.line,
            text,
            origin: "type".into(),
        }
    }

    fn layout_of(&self, d: &crate::types::DefView<'_>, table: &crate::types::Table) -> TypeLayout {
        let env = self.types();
        let agg = match d.kind {
            crate::types::DefKind::Typedef if d.types_agree() => env.aggregate(table, d.target, 0),
            crate::types::DefKind::Typedef => None,
            _ => Some(*d),
        };
        let mut members = Vec::new();
        let mut total = 0usize;
        let limit = if self.opts.full_bodies {
            usize::MAX
        } else {
            LAYOUT_LINES
        };
        if let Some(a) = agg {
            match a.kind {
                crate::types::DefKind::Enum => {
                    for e in table.enumerators(&a) {
                        total += 1;
                        if members.len() < limit {
                            members.push(LayoutMember {
                                offset: String::new(),
                                size: String::new(),
                                decl: format!("{} = {}", e.name, e.value),
                            });
                        }
                    }
                }
                _ => {
                    let typed = a.types_agree();
                    for m in table.members(&a) {
                        total += 1;
                        if members.len() >= limit {
                            continue;
                        }
                        let (offset, size) = if m.bit_size > 0 {
                            (
                                m.bit_offset
                                    .map_or("?".into(), |b| format!("{}:{}", b / 8, b % 8)),
                                format!(
                                    "{} bit{}",
                                    m.bit_size,
                                    if m.bit_size == 1 { "" } else { "s" }
                                ),
                            )
                        } else {
                            (
                                m.offset.map_or("?".into(), |o| o.to_string()),
                                m.size.map_or("-".into(), |s| s.to_string()),
                            )
                        };
                        let decl = if typed {
                            c_type::declaration(table, m.ty, m.name)
                        } else {
                            m.name.to_string()
                        };
                        members.push(LayoutMember { offset, size, decl });
                    }
                }
            }
        }
        let mut source = format!(
            "DWARF of this build: the same in {} compile unit{}",
            d.cus,
            if d.cus == 1 { "" } else { "s" }
        );
        if agg.is_some_and(|a| !a.types_agree()) {
            source.push_str("; member types differ between compile units, so only names are shown");
        }
        TypeLayout {
            size: d.size,
            hidden: total - members.len(),
            members,
            source,
        }
    }

    fn size_value(&self, v: u64) -> Value {
        let (display, ty) = value::display(
            &value::CVal {
                v: i128::from(v),
                ty: value::IntTy::ULong,
                pointer: false,
            },
            self.char_unsigned,
        );
        Value {
            display,
            ty: Some(ty),
        }
    }

    fn type_node(
        &self,
        call: String,
        v: u64,
        d: &crate::types::DefView<'_>,
        table: &crate::types::Table,
    ) -> ExpNode {
        ExpNode {
            layout: Some(self.layout_of(d, table)),
            name: call.clone(),
            call,
            site: Some(self.type_site(d, table)),
            why: Vec::new(),
            alternatives: Vec::new(),
            args: Vec::new(),
            substituted: String::new(),
            result: v.to_string(),
            refined: None,
            value: Some(self.size_value(v)),
            children: Vec::new(),
            note: None,
            repeat_of: None,
            config: None,
            kconfig: None,
            target: None,
        }
    }

    fn size_node(&self, toks: &[Token], spelled: String) -> Option<ExpNode> {
        let it = &self.sim.it;
        let words: Vec<&str> = toks.iter().map(|t| it.get(t.sym)).collect();
        let t = c_type::parse_type_name(&words)?;
        if t.ops
            .iter()
            .any(|o| !matches!(o, c_type::Op::Array(Some(_))))
        {
            return None;
        }
        let (kind, name) = match t.base {
            Base::Tag(k, n) => (k, n),
            Base::Typedef(n) if !self.vars.iter().any(|v| it.get(*v) == n) => {
                (crate::types::DefKind::Typedef, n)
            }
            _ => return None,
        };
        let env = self.types();
        let size = env.size_of(&t, None)?;
        let d = env.def_named(kind, name)?;
        let table = env.table()?;
        Some(self.type_node(spelled, size, &d, table))
    }

    fn offset_node(&self, ty: &[Token], desig: &[Token], spelled: String) -> Option<ExpNode> {
        let it = &self.sim.it;
        let words: Vec<&str> = ty.iter().map(|t| it.get(t.sym)).collect();
        let t = c_type::parse_type_name(&words)?;
        if !t.ops.is_empty() {
            return None;
        }
        let (kind, name) = match t.base {
            Base::Tag(k, n) => (k, n),
            Base::Typedef(n) => (crate::types::DefKind::Typedef, n),
            _ => return None,
        };
        let dwords: Vec<&str> = desig.iter().map(|t| it.get(t.sym)).collect();
        let path = designator(&dwords)?;
        let env = self.types();
        let (off, _) = env.offset(&t.base, &path)?;
        let d = env.def_named(kind, name)?;
        let table = env.table()?;
        Some(self.type_node(spelled, off, &d, table))
    }

    fn enum_node(&self, s: Sym) -> Option<ExpNode> {
        let it = &self.sim.it;
        let name = it.get(s);
        let env = self.types();
        let (def, value) = env.enumerator_at(name)?;
        let table = env.table()?;
        let d = table.def(def)?;
        let abs = env.abs(d.file);
        let f = self.sim.files.lookup(&abs)?;
        let path = self.sim.files.display(f).to_string();
        let line = self
            .ws
            .index
            .lookup(name)
            .iter()
            .filter(|p| p.kind == crate::index::DefKind::Enumerator && p.line >= d.line)
            .filter(|p| {
                self.ws
                    .index
                    .file(p.file)
                    .is_some_and(|v| crate::pp::kbuild::normalize(&self.ws.abs(&v)) == abs)
            })
            .map(|p| p.line)
            .min()?;
        let content = self.sim.files.content(f);
        let text = (line as usize)
            .checked_sub(1)
            .and_then(|i| {
                String::from_utf8_lossy(&content)
                    .lines()
                    .nth(i)
                    .map(str::to_string)
            })
            .map(|l| l.trim().trim_end_matches(',').to_string())
            .unwrap_or_else(|| name.to_string());
        let shown = value::display(
            &value::CVal {
                v: value,
                ty: value::IntTy::Int,
                pointer: false,
            },
            self.char_unsigned,
        );
        Some(ExpNode {
            layout: None,
            name: name.to_string(),
            call: name.to_string(),
            site: Some(Site {
                path,
                line,
                text,
                origin: "enum".into(),
            }),
            why: Vec::new(),
            alternatives: Vec::new(),
            args: Vec::new(),
            substituted: String::new(),
            result: value.to_string(),
            refined: None,
            value: Some(Value {
                display: shown.0,
                ty: Some(shown.1),
            }),
            children: Vec::new(),
            note: None,
            repeat_of: None,
            config: None,
            kconfig: None,
            target: Some(CTarget {
                kind: if d.name.is_empty() {
                    "enum constant".to_string()
                } else {
                    format!("enum constant of enum {}", d.name)
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
