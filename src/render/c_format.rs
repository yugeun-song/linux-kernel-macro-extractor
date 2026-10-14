use crate::intern::Interner;
use crate::render::style;
use crate::syntax::lex::{self, TokenKind};

const INDENT: usize = 8;

const LEVELS: &[&[&str]] = &[
    &["||"],
    &["&&"],
    &["|"],
    &["^"],
    &["&"],
    &["==", "!="],
    &["<", ">", "<=", ">="],
    &["<<", ">>"],
    &["+", "-"],
    &["*", "/", "%"],
];

const ASSIGNS: &[&str] = &[
    "=", "*=", "/=", "%=", "+=", "-=", "<<=", ">>=", "&=", "^=", "|=",
];

const LEADING_WORDS: &[&str] = &["return", "case", "goto", "else", "do"];

struct Tok {
    text: String,
    kind: TokenKind,
    space: bool,
}

enum Node {
    Tok(usize),
    Group(usize, Vec<Node>, Option<usize>),
}

enum Doc {
    Text(String),
    Line(&'static str),
    Nest(usize, Box<Doc>),
    Align(Box<Doc>),
    Group(Box<Doc>),
    Cat(Vec<Doc>),
    Fill(Vec<Doc>),
}

fn closer(open: &str) -> Option<&'static str> {
    match open {
        "(" => Some(")"),
        "[" => Some("]"),
        "{" => Some("}"),
        _ => None,
    }
}

fn tree(toks: &[Tok]) -> Vec<Node> {
    let mut stack: Vec<(usize, Vec<Node>)> = Vec::new();
    let mut top: Vec<Node> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let punct = t.kind == TokenKind::Punct;
        if punct && closer(&t.text).is_some() {
            stack.push((i, std::mem::take(&mut top)));
            continue;
        }
        let closes = punct
            && stack
                .last()
                .is_some_and(|(open, _)| closer(&toks[*open].text) == Some(t.text.as_str()));
        if closes {
            let (open, outer) = stack.pop().expect("an open group");
            let items = std::mem::replace(&mut top, outer);
            top.push(Node::Group(open, items, Some(i)));
        } else {
            top.push(Node::Tok(i));
        }
    }
    while let Some((open, outer)) = stack.pop() {
        let items = std::mem::replace(&mut top, outer);
        top.push(Node::Group(open, items, None));
    }
    top
}

fn first(node: &Node) -> usize {
    match node {
        Node::Tok(i) | Node::Group(i, ..) => *i,
    }
}

struct Builder<'t> {
    toks: &'t [Tok],
}

impl Builder<'_> {
    fn sep(&self, i: usize) -> &'static str {
        if self.toks[i].space { " " } else { "" }
    }

    fn text(&self, i: usize) -> &str {
        &self.toks[i].text
    }

    fn tok(&self, i: usize, lead: bool) -> Doc {
        if lead {
            Doc::Text(format!("{}{}", self.sep(i), self.text(i)))
        } else {
            Doc::Text(self.text(i).to_string())
        }
    }

    fn punct(&self, node: &Node, p: &[&str]) -> bool {
        matches!(node, Node::Tok(i) if self.toks[*i].kind == TokenKind::Punct && p.contains(&self.text(*i)))
    }

    fn operand_end(&self, node: &Node) -> bool {
        match node {
            Node::Group(..) => true,
            Node::Tok(i) => {
                let t = &self.toks[*i];
                match t.kind {
                    TokenKind::Ident => !matches!(t.text.as_str(), "return" | "case" | "sizeof"),
                    TokenKind::Number | TokenKind::Char | TokenKind::Str => true,
                    TokenKind::Punct => matches!(t.text.as_str(), ")" | "]" | "++" | "--"),
                    _ => false,
                }
            }
        }
    }

    fn lead(&self, nodes: &[Node], lead: bool, body: impl FnOnce(&Self) -> Doc) -> Doc {
        let space = lead && nodes.first().is_some_and(|n| self.toks[first(n)].space);
        if space {
            Doc::Cat(vec![Doc::Text(" ".into()), body(self)])
        } else {
            body(self)
        }
    }

    fn split<'n>(&self, nodes: &'n [Node], at: &[usize]) -> Vec<&'n [Node]> {
        let mut parts = Vec::with_capacity(at.len() + 1);
        let mut start = 0;
        for &k in at {
            parts.push(&nodes[start..k]);
            start = k + 1;
        }
        parts.push(&nodes[start..]);
        parts
    }

    fn broken(&self, nodes: &[Node]) -> Doc {
        match nodes.first() {
            Some(n) => Doc::Cat(vec![Doc::Line(self.sep(first(n))), self.seq(nodes, false)]),
            None => Doc::Cat(Vec::new()),
        }
    }

    fn seq(&self, nodes: &[Node], lead: bool) -> Doc {
        if nodes.is_empty() {
            return Doc::Cat(Vec::new());
        }
        self.lead(nodes, lead, |b| b.inner(nodes))
    }

    fn inner(&self, nodes: &[Node]) -> Doc {
        let at = |p: &[&str]| -> Vec<usize> {
            (0..nodes.len())
                .filter(|&k| self.punct(&nodes[k], p))
                .collect()
        };
        let semis = at(&[";"]);
        if !semis.is_empty() {
            return self.statements(nodes, &semis);
        }
        let commas = at(&[","]);
        if !commas.is_empty() {
            let mut items = Vec::new();
            let mut start = 0;
            for &k in &commas {
                let mut part = vec![
                    self.seq(&nodes[start..k], false),
                    self.tok(first(&nodes[k]), true),
                ];
                if start > 0 {
                    part.insert(0, Doc::Line(self.sep(first(&nodes[start]))));
                }
                items.push(Doc::Cat(part));
                start = k + 1;
            }
            if start < nodes.len() {
                items.push(Doc::Cat(vec![
                    Doc::Line(self.sep(first(&nodes[start]))),
                    self.seq(&nodes[start..], false),
                ]));
            }
            return Doc::Fill(Self::fill(items));
        }
        if let Node::Tok(i) = nodes[0]
            && nodes.len() > 1
            && self.toks[i].kind == TokenKind::Ident
            && LEADING_WORDS.contains(&self.text(i))
        {
            return Doc::Cat(vec![self.tok(i, false), self.seq(&nodes[1..], true)]);
        }
        if let Some(k) = (1..nodes.len()).find(|&k| self.punct(&nodes[k], ASSIGNS)) {
            let rhs = &nodes[k + 1..];
            return Doc::Group(Box::new(Doc::Cat(vec![
                self.seq(&nodes[..k], false),
                self.tok(first(&nodes[k]), true),
                Doc::Nest(INDENT, Box::new(self.broken(rhs))),
            ])));
        }
        if let Some(q) = (1..nodes.len()).find(|&k| self.punct(&nodes[k], &["?"])) {
            let mut depth = 0;
            let colon = (q + 1..nodes.len()).find(|&k| {
                if self.punct(&nodes[k], &["?"]) {
                    depth += 1;
                } else if self.punct(&nodes[k], &[":"]) {
                    if depth == 0 {
                        return true;
                    }
                    depth -= 1;
                }
                false
            });
            if let Some(c) = colon {
                return Doc::Group(Box::new(Doc::Cat(vec![
                    self.seq(&nodes[..q], false),
                    Doc::Nest(
                        INDENT,
                        Box::new(Doc::Cat(vec![
                            Doc::Line(self.sep(first(&nodes[q]))),
                            self.tok(first(&nodes[q]), false),
                            self.seq(&nodes[q + 1..c], true),
                            Doc::Line(self.sep(first(&nodes[c]))),
                            self.tok(first(&nodes[c]), false),
                            self.seq(&nodes[c + 1..], true),
                        ])),
                    ),
                ])));
            }
        }
        for level in LEVELS {
            let ops: Vec<usize> = (1..nodes.len())
                .filter(|&k| {
                    self.punct(&nodes[k], level)
                        && self.operand_end(&nodes[k - 1])
                        && k + 1 < nodes.len()
                })
                .collect();
            if ops.is_empty() {
                continue;
            }
            let parts = self.split(nodes, &ops);
            let mut items = Vec::with_capacity(parts.len() * 2);
            for (k, part) in parts.iter().enumerate() {
                if k > 0 {
                    items.push(Doc::Line(self.sep(first(&part[0]))));
                }
                let mut body = vec![self.seq(part, false)];
                if let Some(op) = ops.get(k) {
                    body.push(self.tok(first(&nodes[*op]), true));
                }
                items.push(Doc::Cat(body));
            }
            return Doc::Align(Box::new(Doc::Fill(items)));
        }
        let mut docs = Vec::with_capacity(nodes.len());
        for (k, node) in nodes.iter().enumerate() {
            docs.push(self.node(node, k > 0));
        }
        Doc::Cat(docs)
    }

    fn fill(items: Vec<Doc>) -> Vec<Doc> {
        let mut out = Vec::with_capacity(items.len() * 2);
        for (k, item) in items.into_iter().enumerate() {
            match item {
                Doc::Cat(mut parts) if k > 0 && matches!(parts.first(), Some(Doc::Line(_))) => {
                    out.push(parts.remove(0));
                    out.push(Doc::Cat(parts));
                }
                other => out.push(other),
            }
        }
        out
    }

    fn statements(&self, nodes: &[Node], semis: &[usize]) -> Doc {
        let mut docs = Vec::new();
        let mut start = 0;
        for &k in semis {
            let stmt = &nodes[start..k];
            let semi = self.tok(first(&nodes[k]), !stmt.is_empty());
            let body = Doc::Cat(vec![self.seq(stmt, false), semi]);
            if start == 0 {
                docs.push(body);
            } else {
                docs.push(Doc::Line(self.sep(first(&nodes[start]))));
                docs.push(body);
            }
            start = k + 1;
        }
        if start < nodes.len() {
            docs.push(self.broken(&nodes[start..]));
        }
        Doc::Cat(docs)
    }

    fn node(&self, node: &Node, lead: bool) -> Doc {
        match node {
            Node::Tok(i) => self.tok(*i, lead),
            Node::Group(open, items, close) => {
                let mut docs = vec![self.tok(*open, lead)];
                let brace = self.text(*open) == "{";
                let statements = items.iter().any(|n| self.punct(n, &[";"]));
                match (brace, items.as_slice()) {
                    (false, [inner @ Node::Group(o, ..)]) if self.text(*o) == "{" => {
                        docs.push(self.node(inner, false));
                        if let Some(c) = close {
                            docs.push(self.tok(*c, true));
                        }
                    }
                    (true, _) if statements => {
                        let mut body = vec![self.broken(items)];
                        if let Some(c) = close {
                            body.push(Doc::Line(self.sep(*c)));
                        }
                        let mut inner = vec![Doc::Nest(INDENT, Box::new(body.remove(0)))];
                        inner.extend(body);
                        if let Some(c) = close {
                            inner.push(self.tok(*c, false));
                        }
                        docs.push(Doc::Group(Box::new(Doc::Cat(inner))));
                    }
                    _ => {
                        let inner = Doc::Group(Box::new(self.seq(items, false)));
                        docs.push(Doc::Align(Box::new(inner)));
                        if let Some(c) = close {
                            docs.push(self.tok(*c, !items.is_empty()));
                        }
                    }
                }
                Doc::Cat(docs)
            }
        }
    }

    fn top(&self, nodes: &[Node]) -> Doc {
        let directive = nodes.len() > 3
            && self.punct(&nodes[0], &["#"])
            && matches!(nodes[1], Node::Tok(i) if self.text(i) == "define")
            && matches!(nodes[2], Node::Tok(i) if self.toks[i].kind == TokenKind::Ident);
        if !directive {
            return self.seq(nodes, false);
        }
        let mut head = 3;
        if let Node::Group(open, ..) = &nodes[3]
            && self.text(*open) == "("
            && !self.toks[*open].space
        {
            head = 4;
        }
        let mut docs: Vec<Doc> = nodes[..head]
            .iter()
            .enumerate()
            .map(|(k, n)| self.node(n, k > 0))
            .collect();
        if head < nodes.len() {
            docs.push(Doc::Nest(INDENT, Box::new(self.broken(&nodes[head..]))));
        }
        Doc::Group(Box::new(Doc::Cat(docs)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Flat,
    Break,
}

enum Cmd<'d> {
    Doc(usize, Mode, &'d Doc),
    Fill(usize, &'d [Doc]),
}

fn fits(mut room: isize, first: &[Cmd<'_>], rest: &[Cmd<'_>]) -> bool {
    let mut stack: Vec<(Mode, &Doc)> = Vec::new();
    let mut pending = first.iter().chain(rest.iter().rev());
    loop {
        if room < 0 {
            return false;
        }
        let (mode, doc) = match stack.pop() {
            Some(x) => x,
            None => match pending.next() {
                None => return true,
                Some(Cmd::Doc(_, m, d)) => (*m, *d),
                Some(Cmd::Fill(..)) => return true,
            },
        };
        match doc {
            Doc::Text(s) => room -= style::width(s) as isize,
            Doc::Line(sep) => {
                if mode == Mode::Break {
                    return true;
                }
                room -= sep.len() as isize;
            }
            Doc::Nest(_, x) | Doc::Align(x) | Doc::Group(x) => stack.push((mode, x)),
            Doc::Cat(xs) | Doc::Fill(xs) => {
                for x in xs.iter().rev() {
                    stack.push((mode, x));
                }
            }
        }
    }
}

fn layout(doc: &Doc, start: usize, width: usize) -> Vec<(usize, String)> {
    let mut lines = vec![(start, String::new())];
    let mut col = start;
    let mut stack = vec![Cmd::Doc(0, Mode::Break, doc)];
    let room = |col: usize| width as isize - col as isize;
    while let Some(cmd) = stack.pop() {
        match cmd {
            Cmd::Doc(i, m, d) => match d {
                Doc::Text(s) => {
                    col += style::width(s);
                    lines.last_mut().expect("a line").1.push_str(s);
                }
                Doc::Line(sep) => {
                    if m == Mode::Flat {
                        col += sep.len();
                        lines.last_mut().expect("a line").1.push_str(sep);
                    } else {
                        lines.push((i, String::new()));
                        col = i;
                    }
                }
                Doc::Nest(n, x) => stack.push(Cmd::Doc(i + n, m, x)),
                Doc::Align(x) => stack.push(Cmd::Doc(col, m, x)),
                Doc::Group(x) => {
                    let mode = if m == Mode::Flat
                        || fits(room(col), &[Cmd::Doc(i, Mode::Flat, x)], &stack)
                    {
                        Mode::Flat
                    } else {
                        Mode::Break
                    };
                    stack.push(Cmd::Doc(i, mode, x));
                }
                Doc::Cat(xs) => {
                    for x in xs.iter().rev() {
                        stack.push(Cmd::Doc(i, m, x));
                    }
                }
                Doc::Fill(xs) => {
                    if m == Mode::Flat {
                        for x in xs.iter().rev() {
                            stack.push(Cmd::Doc(i, Mode::Flat, x));
                        }
                    } else if let Some((head, tail)) = xs.split_first() {
                        stack.push(Cmd::Fill(i, tail));
                        stack.push(Cmd::Doc(i, Mode::Break, head));
                    }
                }
            },
            Cmd::Fill(i, rest) => {
                if let [sep, item, tail @ ..] = rest {
                    let flat = fits(
                        room(col),
                        &[Cmd::Doc(i, Mode::Flat, sep), Cmd::Doc(i, Mode::Flat, item)],
                        &stack,
                    );
                    let mode = if flat { Mode::Flat } else { Mode::Break };
                    stack.push(Cmd::Fill(i, tail));
                    stack.push(Cmd::Doc(i, mode, item));
                    stack.push(Cmd::Doc(i, mode, sep));
                } else if let [item] = rest {
                    stack.push(Cmd::Doc(i, Mode::Break, item));
                }
            }
        }
    }
    for line in &mut lines {
        let trimmed = line.1.trim_end().len();
        line.1.truncate(trimmed);
    }
    lines
}

fn tokens(text: &str) -> Option<Vec<Tok>> {
    if text.contains("/*") || text.contains("//") {
        return None;
    }
    let mut it = Interner::new();
    let toks = lex::lex_all(text.as_bytes(), false, &mut it);
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    for t in &toks {
        if t.kind == TokenKind::Placemarker {
            continue;
        }
        let s = lex::spell(t, &it).to_string();
        let space = out
            .last()
            .is_some_and(|p| t.space() || lex::avoid_paste(&p.text, &s));
        out.push(Tok {
            text: s,
            kind: t.kind,
            space,
        });
    }
    Some(out)
}

pub fn one_line(text: &str) -> String {
    if !text.contains(['\n', '\t', '\r']) {
        return text.to_string();
    }
    let Some(toks) = tokens(text) else {
        return text.to_string();
    };
    let mut out = String::with_capacity(text.len());
    for t in &toks {
        if t.space {
            out.push(' ');
        }
        out.push_str(&t.text);
    }
    out
}

fn spelled(text: &str) -> Vec<String> {
    let mut it = Interner::new();
    lex::lex_all(text.as_bytes(), false, &mut it)
        .iter()
        .filter(|t| t.kind != TokenKind::Placemarker)
        .map(|t| lex::spell(t, &it).to_string())
        .collect()
}

fn reflow(text: &str, start: usize, width: usize) -> Option<Vec<(usize, String)>> {
    let toks = tokens(text)?;
    if toks.is_empty() {
        return None;
    }
    let nodes = tree(&toks);
    let doc = Builder { toks: &toks }.top(&nodes);
    Some(layout(&doc, start, width))
}

pub fn format(text: &str, start: usize, width: usize) -> Vec<(usize, String)> {
    let whole = vec![(start, text.to_string())];
    if start + style::width(text) <= width {
        return whole;
    }
    match reflow(text, start, width) {
        Some(lines) => {
            let joined: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
            if spelled(text) == spelled(&joined.join("\n")) {
                lines
            } else {
                whole
            }
        }
        None => whole,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(text: &str, start: usize, width: usize) -> String {
        format(text, start, width)
            .into_iter()
            .enumerate()
            .map(|(k, (indent, line))| {
                if k == 0 {
                    line
                } else {
                    format!("{}{line}", " ".repeat(indent))
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn short_text_is_left_alone() {
        assert_eq!(shown("a  +   b", 0, 80), "a  +   b");
    }

    #[test]
    fn statement_expressions_break_like_kernel_code() {
        let text = "({ phys_addr_t __x = (phys_addr_t)(unsigned long)x; __x & 0x4000000000UL ? (__x & 0x3fffffffffUL) + ({ memstart_addr; }) : __x - kimage_voffset; })";
        assert_eq!(
            shown(text, 18, 80),
            "({\n        phys_addr_t __x = (phys_addr_t)(unsigned long)x;\n        __x & 0x4000000000UL\n                ? (__x & 0x3fffffffffUL) + ({ memstart_addr; })\n                : __x - kimage_voffset;\n})"
        );
    }

    #[test]
    fn defines_move_their_body_to_the_next_line() {
        let text = "#define __virt_to_phys(x) ({ phys_addr_t __x = (phys_addr_t)(x); __x & BIT(VA_BITS - 1) ? (__x & ~PAGE_OFFSET) + PHYS_OFFSET : (__x - kimage_voffset); })";
        assert_eq!(
            shown(text, 0, 72),
            "#define __virt_to_phys(x)\n        ({\n                phys_addr_t __x = (phys_addr_t)(x);\n                __x & BIT(VA_BITS - 1)\n                        ? (__x & ~PAGE_OFFSET) + PHYS_OFFSET\n                        : (__x - kimage_voffset);\n        })"
        );
    }

    #[test]
    fn arguments_pack_and_align_after_the_bracket() {
        let text = "foo(alpha_beta_gamma, delta_epsilon_zeta, eta_theta_iota, kappa_lambda_mu, nu_xi_omicron)";
        assert_eq!(
            shown(text, 0, 50),
            "foo(alpha_beta_gamma, delta_epsilon_zeta,\n    eta_theta_iota, kappa_lambda_mu,\n    nu_xi_omicron)"
        );
    }

    #[test]
    fn operands_align_after_a_break() {
        let text = "first_operand_name + second_operand_name + third_operand_name";
        assert_eq!(
            shown(text, 0, 44),
            "first_operand_name + second_operand_name +\nthird_operand_name"
        );
    }

    #[test]
    fn formatting_never_changes_the_tokens() {
        let samples = [
            "((void *)((unsigned long)(address)+((unsigned long)page_offset_base)))",
            "#define X(a, b) do { if ((a) > (b)) { f(a, b); } else { g(b); } } while (0)",
            "a ? b ? c : d : e ? f : g",
            "x = y += z, w = (1, 2, 3)",
            "({ int t = sizeof(int[3]); t; }) + (struct S){ .a = 1, .b = { 2, 3 } }.a",
            "f(\"a string, with commas; and semicolons\", 'c', 0x1UL << 3)",
            "unbalanced ( parens [ here",
            "here ) are ] stray } closers",
            "- - -x + ++y-- * &z",
        ];
        for text in samples {
            for width in [10, 20, 33, 47, 60] {
                let lines = reflow(text, 3, width).expect("a layout");
                let joined: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
                assert_eq!(
                    spelled(text),
                    spelled(&joined.join("\n")),
                    "{text} at width {width}"
                );
                assert!(
                    lines.iter().all(|(_, l)| !l.ends_with(' ')),
                    "{text} at width {width}"
                );
            }
        }
    }

    #[test]
    fn comments_are_never_reformatted() {
        let text = "x = f(a, /* note */ b) + some_long_name_here + another_long_name";
        assert_eq!(format(text, 0, 20), vec![(0, text.to_string())]);
    }
}
