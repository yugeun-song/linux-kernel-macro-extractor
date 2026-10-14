use std::rc::Rc;

use super::hide_set::{HideSets, HsId};
use super::macros::{Builtin, MacroDef, Params};
use crate::intern::{Interner, Sym, sym};
use crate::syntax::lex::{self, F_NOEXP, F_SPACE, Token, TokenKind};

pub trait Env {
    fn it(&mut self) -> &mut Interner;
    fn hs(&mut self) -> &mut HideSets;
    fn lookup(&mut self, s: Sym) -> Option<Rc<MacroDef>>;
    fn builtin(&mut self, b: Builtin) -> Token;
    fn has_include(&mut self, name: &str, angled: bool, next: bool) -> bool;
    fn has_feature(&mut self, op: Sym, name: &str) -> Option<bool>;
    fn asm(&self) -> bool {
        false
    }
    fn defined(&mut self, s: Sym) -> bool {
        self.lookup(s).is_some()
    }
    fn operator(&mut self, _s: Sym) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Root,
    Arg(u16),
    Rescan,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub parent: u32,
    pub phase: Phase,
    pub sym: Sym,
    pub def: Option<Rc<MacroDef>>,
    pub args: Vec<Vec<Token>>,
    pub args_expanded: Vec<Option<Vec<Token>>>,
    pub substituted: Vec<Token>,
    pub result: Vec<Token>,
    pub children: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Trace {
    pub nodes: Vec<Node>,
}

impl Trace {
    fn new() -> Trace {
        Trace {
            nodes: vec![Node {
                parent: 0,
                phase: Phase::Root,
                sym: sym::EMPTY,
                def: None,
                args: Vec::new(),
                args_expanded: Vec::new(),
                substituted: Vec::new(),
                result: Vec::new(),
                children: Vec::new(),
            }],
        }
    }

    pub fn is_descendant(&self, mut n: u32, ancestor: u32) -> bool {
        if ancestor == 0 {
            return true;
        }
        while n >= ancestor {
            if n == ancestor {
                return true;
            }
            if n == 0 {
                return false;
            }
            n = self.nodes[n as usize].parent;
        }
        false
    }

    fn finalize(&mut self, output: &[Token]) {
        for i in 1..self.nodes.len() {
            let p = self.nodes[i].parent as usize;
            self.nodes[p].children.push(i as u32);
        }
        for t in output {
            let mut n = t.origin;
            while n != 0 {
                let node = &mut self.nodes[n as usize];
                node.result.push(*t);
                n = node.parent;
            }
            self.nodes[0].result.push(*t);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_nodes: usize,
    pub max_tokens: usize,
    pub max_depth: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_nodes: 200_000,
            max_tokens: 4_000_000,
            max_depth: 1_000,
        }
    }
}

pub struct Expander<'e> {
    env: &'e mut dyn Env,
    trace: Option<Trace>,
    directive: bool,
    limits: Limits,
    depth: u32,
    pub truncated: bool,
    pub diags: Vec<String>,
}

impl<'e> Expander<'e> {
    pub fn new(env: &'e mut dyn Env, directive: bool) -> Self {
        Expander {
            env,
            trace: None,
            directive,
            limits: Limits::default(),
            depth: 0,
            truncated: false,
            diags: Vec::new(),
        }
    }

    pub fn traced(mut self) -> Self {
        self.trace = Some(Trace::new());
        self
    }

    pub fn expand(&mut self, tokens: Vec<Token>) -> Vec<Token> {
        let out = self.expand_in(tokens, 0, Phase::Rescan);
        if let Some(t) = &mut self.trace {
            t.finalize(&out);
        }
        out
    }

    pub fn into_trace(self) -> Option<Trace> {
        self.trace
    }

    fn number(&mut self, v: bool, like: &Token) -> Token {
        let s = if v { "1" } else { "0" };
        let mut t = Token::new(
            TokenKind::Number,
            self.env.it().intern(s),
            like.flags & F_SPACE,
        );
        t.origin = like.origin;
        t
    }

    fn over_budget(&self, pending: usize) -> bool {
        let nodes = self.trace.as_ref().map_or(0, |t| t.nodes.len());
        nodes > self.limits.max_nodes
            || pending > self.limits.max_tokens
            || self.depth > self.limits.max_depth
    }

    fn begin(
        &mut self,
        name: &Token,
        def: &Rc<MacroDef>,
        ctx: u32,
        phase: Phase,
        args: Vec<Vec<Token>>,
    ) -> u32 {
        let Some(trace) = &mut self.trace else {
            return 0;
        };
        let parent = if trace.is_descendant(name.origin, ctx) {
            name.origin
        } else {
            ctx
        };
        let phase = if parent == ctx && ctx != 0 {
            phase
        } else {
            Phase::Rescan
        };
        let n = args.len();
        trace.nodes.push(Node {
            parent,
            phase,
            sym: name.sym,
            def: Some(def.clone()),
            args,
            args_expanded: vec![None; n],
            substituted: Vec::new(),
            result: Vec::new(),
            children: Vec::new(),
        });
        (trace.nodes.len() - 1) as u32
    }

    fn expand_in(&mut self, tokens: Vec<Token>, ctx: u32, phase: Phase) -> Vec<Token> {
        let mut input: Vec<Token> = tokens;
        input.reverse();
        let mut out: Vec<Token> = Vec::with_capacity(input.len());
        while let Some(t) = input.pop() {
            if t.kind != TokenKind::Ident || t.flags & F_NOEXP != 0 {
                out.push(t);
                continue;
            }
            if self.directive && self.directive_operator(&t, &mut input, &mut out) {
                continue;
            }
            if self.env.hs().contains(t.hs, t.sym) {
                let mut p = t;
                p.flags |= F_NOEXP;
                out.push(p);
                continue;
            }
            let Some(def) = self.env.lookup(t.sym) else {
                out.push(t);
                continue;
            };
            if self.truncated || self.over_budget(out.len() + input.len()) {
                self.truncated = true;
                out.push(t);
                continue;
            }
            if let Some(b) = def.builtin {
                let mut v = self.env.builtin(b);
                v.flags = (v.flags & !F_SPACE) | (t.flags & F_SPACE);
                v.origin = t.origin;
                out.push(v);
                continue;
            }
            match &def.params {
                Params::Object => {
                    let node = self.begin(&t, &def, ctx, phase, Vec::new());
                    let hs = self.env.hs().add(t.hs, t.sym);
                    let body = self.subst_object(&def, hs, node, t.flags & F_SPACE);
                    input.extend(body.into_iter().rev());
                }
                Params::Function { .. } => {
                    if !input.last().is_some_and(|p| p.is_punct(sym::LPAREN)) {
                        out.push(t);
                        continue;
                    }
                    let lparen = input.pop().unwrap_or(t);
                    let Some((args, rparen, consumed)) = collect_args(&mut input, &def) else {
                        out.push(t);
                        out.push(lparen);
                        continue;
                    };
                    let Some(args) = fit_args(&def, args) else {
                        let name = self.env.it().get(t.sym).to_string();
                        self.diags.push(format!(
                            "macro `{name}` takes {} argument(s) but {} were given; GCC reports an error and drops the arguments",
                            def.slots(),
                            consumed.iter().filter(|c| c.is_punct(sym::COMMA)).count() + 1
                        ));
                        out.push(t);
                        continue;
                    };
                    let hs = {
                        let h = self.env.hs();
                        let i = h.intersect(t.hs, rparen.hs);
                        h.add(i, t.sym)
                    };
                    let node = self.begin(&t, &def, ctx, phase, args.clone());
                    let body = self.subst_function(&def, &args, hs, node, t.flags & F_SPACE);
                    input.extend(body.into_iter().rev());
                }
            }
        }
        out
    }

    fn finish_subst(&mut self, mut out: Vec<Token>, hs: HsId, node: u32, lead: u8) -> Vec<Token> {
        out.retain(|t| t.kind != TokenKind::Placemarker);
        for t in &mut out {
            t.hs = self.env.hs().union(t.hs, hs);
            if let Some(trace) = &self.trace
                && (!trace.is_descendant(t.origin, node) || t.origin == 0)
            {
                t.origin = node;
            }
        }
        if let Some(first) = out.first_mut() {
            first.flags = (first.flags & !F_SPACE) | lead;
        }
        if let Some(trace) = &mut self.trace
            && node != 0
        {
            trace.nodes[node as usize].substituted = out.clone();
        }
        out
    }

    fn paste_into(&mut self, out: &mut Vec<Token>, rhs: &[Token]) {
        let Some(lhs) = out.pop() else {
            out.extend_from_slice(rhs);
            return;
        };
        let Some(first) = rhs.first() else {
            out.push(lhs);
            return;
        };
        let asm = self.env.asm();
        match lex::paste(&lhs, first, asm, self.env.it()) {
            Some(p) => {
                out.push(p);
                out.extend_from_slice(&rhs[1..]);
            }
            None => {
                let (a, b) = (
                    self.env.it().get(lhs.sym).to_string(),
                    self.env.it().get(first.sym).to_string(),
                );
                self.diags.push(format!(
                    "pasting \"{a}\" and \"{b}\" does not give a valid preprocessing token"
                ));
                out.push(lhs);
                out.extend_from_slice(rhs);
            }
        }
    }

    fn subst_object(&mut self, def: &MacroDef, hs: HsId, node: u32, lead: u8) -> Vec<Token> {
        let body = &def.body;
        let mut out = Vec::with_capacity(body.len());
        let mut i = 0;
        while i < body.len() {
            let t = body[i];
            if t.is_punct(sym::HASHHASH) && !out.is_empty() && i + 1 < body.len() {
                let rhs = [body[i + 1]];
                self.paste_into(&mut out, &rhs);
                i += 2;
                continue;
            }
            out.push(t);
            i += 1;
        }
        self.finish_subst(out, hs, node, lead)
    }

    fn expanded_arg(
        &mut self,
        cache: &mut [Option<Vec<Token>>],
        args: &[Vec<Token>],
        p: usize,
        node: u32,
    ) -> Vec<Token> {
        if let Some(v) = &cache[p] {
            return v.clone();
        }
        self.depth += 1;
        let v = self.expand_in(args[p].clone(), node, Phase::Arg(p as u16));
        self.depth -= 1;
        if let Some(trace) = &mut self.trace
            && node != 0
        {
            trace.nodes[node as usize].args_expanded[p] = Some(v.clone());
        }
        cache[p] = Some(v.clone());
        v
    }

    fn subst_function(
        &mut self,
        def: &MacroDef,
        args: &[Vec<Token>],
        hs: HsId,
        node: u32,
        lead: u8,
    ) -> Vec<Token> {
        let mut cache: Vec<Option<Vec<Token>>> = vec![None; args.len()];
        let body = def.body.clone();
        let out = self.subst_seq(def, &body, args, &mut cache, node);
        self.finish_subst(out, hs, node, lead)
    }

    fn subst_seq(
        &mut self,
        def: &MacroDef,
        body: &[Token],
        args: &[Vec<Token>],
        cache: &mut [Option<Vec<Token>>],
        node: u32,
    ) -> Vec<Token> {
        let param = |t: &Token| -> Option<usize> {
            if t.kind == TokenKind::Ident {
                def.param_index(t.sym).filter(|&p| p < args.len())
            } else {
                None
            }
        };
        let variadic = def.variadic_slot();
        let mut out: Vec<Token> = Vec::with_capacity(body.len() + 8);
        let mut i = 0;
        while i < body.len() {
            let t = body[i];
            if t.is_punct(sym::HASH)
                && let Some(p) = body.get(i + 1).and_then(param)
            {
                let s = lex::stringify(&args[p], self.env.it());
                let sym_ = self.env.it().intern(&s);
                out.push(Token::new(TokenKind::Str, sym_, t.flags & F_SPACE));
                i += 2;
                continue;
            }
            if t.kind == TokenKind::Ident
                && t.sym == sym::VA_OPT
                && variadic.is_some()
                && let Some((content, next)) = va_opt_group(body, i)
            {
                let present = variadic.is_some_and(|v| !args[v].is_empty());
                if present {
                    let inner = self.subst_seq(def, &content, args, cache, node);
                    let mut inner = inner;
                    if let Some(f) = inner.first_mut() {
                        f.flags = (f.flags & !F_SPACE) | (t.flags & F_SPACE);
                    }
                    out.extend(inner);
                } else {
                    out.push(Token::placemarker());
                }
                i = next;
                continue;
            }
            if t.is_punct(sym::HASHHASH) && !out.is_empty() && i + 1 < body.len() {
                let rhs = body[i + 1];
                let rp = param(&rhs);
                if out.last().is_some_and(|c| c.is_punct(sym::COMMA))
                    && rp.is_some()
                    && rp == variadic
                {
                    let v = rp.unwrap_or(0);
                    if args[v].is_empty() {
                        out.pop();
                    } else {
                        out.extend_from_slice(&args[v]);
                    }
                    i += 2;
                    continue;
                }
                let rhs_tokens: Vec<Token> = match rp {
                    Some(p) if args[p].is_empty() => vec![Token::placemarker()],
                    Some(p) => args[p].clone(),
                    None => vec![rhs],
                };
                self.paste_into(&mut out, &rhs_tokens);
                i += 2;
                continue;
            }
            if let Some(p) = param(&t) {
                let next_is_paste = body.get(i + 1).is_some_and(|n| n.is_punct(sym::HASHHASH));
                let mut toks = if next_is_paste {
                    if args[p].is_empty() {
                        vec![Token::placemarker()]
                    } else {
                        args[p].clone()
                    }
                } else {
                    self.expanded_arg(cache, args, p, node)
                };
                if toks.is_empty() {
                    toks.push(Token::placemarker());
                }
                if let Some(f) = toks.first_mut() {
                    f.flags = (f.flags & !F_SPACE) | (t.flags & F_SPACE);
                }
                out.extend(toks);
                i += 1;
                continue;
            }
            out.push(t);
            i += 1;
        }
        out
    }

    fn directive_operator(
        &mut self,
        t: &Token,
        input: &mut Vec<Token>,
        out: &mut Vec<Token>,
    ) -> bool {
        match t.sym {
            s if s == sym::DEFINED => {
                let mut next = input.pop();
                let paren = next.is_some_and(|n| n.is_punct(sym::LPAREN));
                if paren {
                    next = input.pop();
                }
                let v = match next {
                    Some(n) if n.kind == TokenKind::Ident => self.env.defined(n.sym),
                    _ => {
                        self.diags
                            .push("operator \"defined\" requires an identifier".to_string());
                        false
                    }
                };
                if paren && input.last().is_some_and(|c| c.is_punct(sym::RPAREN)) {
                    input.pop();
                }
                let n = self.number(v, t);
                out.push(n);
                true
            }
            s if (s == sym::HAS_INCLUDE || s == sym::HAS_INCLUDE_NEXT) && self.env.operator(s) => {
                let next = s == sym::HAS_INCLUDE_NEXT;
                if !input.last().is_some_and(|c| c.is_punct(sym::LPAREN)) {
                    return false;
                }
                input.pop();
                let mut inner = Vec::new();
                let mut depth = 1;
                while let Some(x) = input.pop() {
                    if x.is_punct(sym::LPAREN) {
                        depth += 1;
                    } else if x.is_punct(sym::RPAREN) {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    inner.push(x);
                }
                if inner.first().is_some_and(|f| f.kind == TokenKind::Ident) {
                    inner = self.expand_in(inner, 0, Phase::Rescan);
                }
                let it = self.env.it();
                let (name, angled) = match inner.first() {
                    Some(f) if f.kind == TokenKind::Str => {
                        let s = it.get(f.sym);
                        (s.trim_matches('"').to_string(), false)
                    }
                    Some(f) if f.is_punct(sym::LT) => {
                        let mut s = String::new();
                        for x in &inner[1..] {
                            if x.is_punct(sym::GT) {
                                break;
                            }
                            s.push_str(lex::spell(x, it));
                        }
                        (s, true)
                    }
                    _ => (String::new(), false),
                };
                let v = !name.is_empty() && self.env.has_include(&name, angled, next);
                let n = self.number(v, t);
                out.push(n);
                true
            }
            s if (s == sym::HAS_ATTRIBUTE
                || s == sym::HAS_C_ATTRIBUTE
                || s == sym::HAS_CPP_ATTRIBUTE
                || s == sym::HAS_BUILTIN
                || s == sym::HAS_FEATURE
                || s == sym::HAS_EXTENSION)
                && self.env.operator(s) =>
            {
                if !input.last().is_some_and(|c| c.is_punct(sym::LPAREN)) {
                    return false;
                }
                input.pop();
                let mut name = String::new();
                let mut depth = 1;
                while let Some(x) = input.pop() {
                    if x.is_punct(sym::LPAREN) {
                        depth += 1;
                    } else if x.is_punct(sym::RPAREN) {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    name.push_str(lex::spell(&x, self.env.it()));
                }
                let v = self.env.has_feature(s, &name).unwrap_or(false);
                let n = self.number(v, t);
                out.push(n);
                true
            }
            _ => false,
        }
    }
}

fn va_opt_group(body: &[Token], at: usize) -> Option<(Vec<Token>, usize)> {
    let mut i = at + 1;
    if !body.get(i)?.is_punct(sym::LPAREN) {
        return None;
    }
    i += 1;
    let start = i;
    let mut depth = 1;
    while i < body.len() {
        if body[i].is_punct(sym::LPAREN) {
            depth += 1;
        } else if body[i].is_punct(sym::RPAREN) {
            depth -= 1;
            if depth == 0 {
                return Some((body[start..i].to_vec(), i + 1));
            }
        }
        i += 1;
    }
    None
}

fn fit_args(def: &MacroDef, mut args: Vec<Vec<Token>>) -> Option<Vec<Vec<Token>>> {
    let slots = def.slots();
    match def.variadic_slot() {
        Some(fixed) => {
            if args.len() < fixed {
                return None;
            }
            args.resize_with(slots, Vec::new);
            Some(args)
        }
        None => (args.len() == slots).then_some(args),
    }
}

type Collected = (Vec<Vec<Token>>, Token, Vec<Token>);

fn collect_args(input: &mut Vec<Token>, def: &MacroDef) -> Option<Collected> {
    let variadic = def.variadic_slot();
    let mut args: Vec<Vec<Token>> = vec![Vec::new()];
    let mut consumed = Vec::new();
    let mut depth = 0u32;
    loop {
        let Some(t) = input.pop() else {
            input.extend(consumed.into_iter().rev());
            return None;
        };
        consumed.push(t);
        if t.is_punct(sym::LPAREN) {
            depth += 1;
        } else if t.is_punct(sym::RPAREN) {
            if depth == 0 {
                if args.len() == 1 && args[0].is_empty() && def.slots() == 0 {
                    return Some((Vec::new(), t, consumed));
                }
                return Some((args, t, consumed));
            }
            depth -= 1;
        } else if t.is_punct(sym::COMMA) && depth == 0 {
            let in_variadic = variadic.is_some_and(|v| args.len() > v);
            if !in_variadic {
                args.push(Vec::new());
                continue;
            }
        }
        if let Some(last) = args.last_mut() {
            last.push(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pp::macros::{Origin, parse_define};
    use rustc_hash::FxHashMap;

    struct TestEnv {
        it: Interner,
        hs: HideSets,
        table: FxHashMap<Sym, Rc<MacroDef>>,
    }

    impl TestEnv {
        fn new(defs: &[&str]) -> TestEnv {
            let mut it = Interner::new();
            let mut table = FxHashMap::default();
            for d in defs {
                let p = parse_define(format!("#define {d}").as_bytes(), false, &mut it).unwrap();
                table.insert(
                    p.name,
                    Rc::new(MacroDef {
                        name: p.name,
                        site: None,
                        origin: Origin::Source,
                        params: p.params,
                        body: p.body,
                        builtin: None,
                    }),
                );
            }
            TestEnv {
                it,
                hs: HideSets::new(),
                table,
            }
        }

        fn run(&mut self, text: &str) -> String {
            let toks = lex::lex_all(text.as_bytes(), false, &mut self.it);
            let mut ex = Expander::new(self, false);
            let out = ex.expand(toks);
            lex::render(&out, &self.it)
        }

        fn run_if(&mut self, text: &str) -> String {
            let toks = lex::lex_all(text.as_bytes(), false, &mut self.it);
            let mut ex = Expander::new(self, true);
            let out = ex.expand(toks);
            lex::render(&out, &self.it)
        }
    }

    impl Env for TestEnv {
        fn it(&mut self) -> &mut Interner {
            &mut self.it
        }
        fn hs(&mut self) -> &mut HideSets {
            &mut self.hs
        }
        fn lookup(&mut self, s: Sym) -> Option<Rc<MacroDef>> {
            self.table.get(&s).cloned()
        }
        fn builtin(&mut self, _b: Builtin) -> Token {
            Token::new(TokenKind::Number, self.it.intern("42"), 0)
        }
        fn has_include(&mut self, name: &str, _angled: bool, _next: bool) -> bool {
            name == "linux/types.h"
        }
        fn has_feature(&mut self, _op: Sym, name: &str) -> Option<bool> {
            Some(name == "__fallthrough__")
        }
    }

    fn squash(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn page_size_chain() {
        let mut e = TestEnv::new(&[
            "__AC(X,Y) (X##Y)",
            "_AC(X,Y) __AC(X,Y)",
            "PAGE_SHIFT 12",
            "PAGE_SIZE (_AC(1,UL) << PAGE_SHIFT)",
        ]);
        assert_eq!(e.run("PAGE_SIZE"), "((1UL) << 12)");
    }

    #[test]
    fn standard_example_three() {
        let mut e = TestEnv::new(&[
            "x 2",
            "f(a) f(x * (a))",
            "g f",
            "z z[0]",
            "h g(~",
            "m(a) a(w)",
            "w 0,1",
            "t(a) a",
            "p() int",
            "q(x) x",
            "r(x,y) x ## y",
            "str(x) # x",
        ]);
        assert_eq!(
            squash(&e.run("f(y+1) + f(f(z)) % t(t(g)(0) + t)(1);")),
            squash("f(2 * (y+1)) + f(2 * (f(2 * (z[0])))) % f(2 * (0)) + t(1);")
        );
        assert_eq!(
            squash(&e.run("g(x+(3,4)-w) | h 5) & m\n(f)^m(m);")),
            squash("f(2 * (2+(3,4)-0,1)) | f(2 * (~ 5)) & f(2 * (0,1))^m(0,1);")
        );
        assert_eq!(
            squash(&e.run("p() i[q()] = { q(1), r(2,3), r(4,), r(,5), r(,) };")),
            squash("int i[] = { 1, 23, 4, 5, };")
        );
        assert_eq!(
            e.run("str(hello) str() str( a  \"b\\n\" )"),
            r#""hello" "" "a \"b\\n\"""#
        );
    }

    #[test]
    fn kconfig_is_enabled_trick() {
        let mut e = TestEnv::new(&[
            "__ARG_PLACEHOLDER_1 0,",
            "__take_second_arg(__ignored, val, ...) val",
            "__is_defined(x) ___is_defined(x)",
            "___is_defined(val) ____is_defined(__ARG_PLACEHOLDER_##val)",
            "____is_defined(arg1_or_junk) __take_second_arg(arg1_or_junk 1, 0)",
            "__or(x, y) ___or(x, y)",
            "___or(x, y) ____or(__ARG_PLACEHOLDER_##x, y)",
            "____or(arg1_or_junk, y) __take_second_arg(arg1_or_junk 1, y)",
            "IS_BUILTIN(option) __is_defined(option)",
            "IS_MODULE(option) __is_defined(option##_MODULE)",
            "IS_ENABLED(option) __or(IS_BUILTIN(option), IS_MODULE(option))",
            "CONFIG_FOO 1",
            "CONFIG_BAR_MODULE 1",
        ]);
        assert_eq!(e.run("IS_ENABLED(CONFIG_FOO)"), "1");
        assert_eq!(e.run("IS_ENABLED(CONFIG_BAR)"), "1");
        assert_eq!(e.run("IS_BUILTIN(CONFIG_BAR)"), "0");
        assert_eq!(e.run("IS_ENABLED(CONFIG_NOPE)"), "0");
    }

    #[test]
    fn gnu_comma_and_named_variadic() {
        let mut e = TestEnv::new(&[
            "pr(fmt, ...) printk(fmt, ##__VA_ARGS__)",
            "dbg(fmt, args...) printk(fmt, ## args)",
            "opt(a, ...) f(a __VA_OPT__(,) __VA_ARGS__)",
        ]);
        assert_eq!(e.run("pr(\"x\")"), "printk(\"x\")");
        assert_eq!(e.run("pr(\"x\", 1, 2)"), "printk(\"x\", 1, 2)");
        assert_eq!(e.run("dbg(\"y\")"), "printk(\"y\")");
        assert_eq!(e.run("dbg(\"y\", a, b)"), "printk(\"y\", a, b)");
        assert_eq!(squash(&e.run("opt(1)")), "f(1)");
        assert_eq!(squash(&e.run("opt(1, 2, 3)")), "f(1,2,3)");
    }

    #[test]
    fn recursion_is_blocked() {
        let mut e = TestEnv::new(&["foo foo + 1", "a b", "b a"]);
        assert_eq!(e.run("foo"), "foo + 1");
        assert_eq!(e.run("a"), "a");
    }

    #[test]
    fn directive_operators() {
        let mut e = TestEnv::new(&["CONFIG_X 1", "D defined(CONFIG_X)"]);
        assert_eq!(e.run_if("defined CONFIG_X && defined(CONFIG_Y)"), "1 && 0");
        assert_eq!(e.run_if("D"), "1");
        assert_eq!(
            e.run_if("__has_include(<linux/types.h>) + __has_include(\"x.h\")"),
            "1 + 0"
        );
        assert_eq!(e.run_if("__has_attribute(__fallthrough__)"), "1");
    }

    #[test]
    fn trace_records_layers() {
        let mut e = TestEnv::new(&[
            "__AC(X,Y) (X##Y)",
            "_AC(X,Y) __AC(X,Y)",
            "PAGE_SHIFT 12",
            "PAGE_SIZE (_AC(1,UL) << PAGE_SHIFT)",
        ]);
        let toks = lex::lex_all(b"PAGE_SIZE", false, &mut e.it);
        let mut ex = Expander::new(&mut e, false).traced();
        let out = ex.expand(toks);
        let trace = ex.into_trace().unwrap();
        let names: Vec<(String, u32)> = trace.nodes[1..]
            .iter()
            .map(|n| (e.it.get(n.sym).to_string(), n.parent))
            .collect();
        assert_eq!(
            names,
            vec![
                ("PAGE_SIZE".into(), 0),
                ("_AC".into(), 1),
                ("__AC".into(), 2),
                ("PAGE_SHIFT".into(), 1)
            ]
        );
        assert_eq!(lex::render(&trace.nodes[2].result, &e.it), "(1UL)");
        assert_eq!(
            lex::render(&trace.nodes[1].result, &e.it),
            lex::render(&out, &e.it)
        );
        assert_eq!(lex::render(&trace.nodes[4].result, &e.it), "12");
    }
}
