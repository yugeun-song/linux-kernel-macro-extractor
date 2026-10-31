use rustc_hash::FxHashSet;

use super::c_const::{self, Konst};
use super::c_type::{self, Base, Step, TypeEnv};
use super::value::{IntTy, common, promote};
use crate::intern::{Interner, Sym};
use crate::syntax::lex::{self, Token, TokenKind};

const MAX_TOKENS: usize = 20_000;
const MAX_DEPTH: usize = 160;

const COMMA: u8 = 1;
const ASSIGN: u8 = 2;
const COND: u8 = 3;
const LOR: u8 = 4;
const UNARY: u8 = 14;
const POSTFIX: u8 = 15;
const PRIMARY: u8 = 16;

const TYPE_WORDS: &[&str] = &[
    "void",
    "char",
    "short",
    "int",
    "long",
    "float",
    "double",
    "signed",
    "unsigned",
    "_Bool",
    "_Complex",
    "const",
    "volatile",
    "restrict",
    "__restrict",
    "__restrict__",
    "struct",
    "union",
    "enum",
    "typeof",
    "__typeof__",
    "__typeof",
    "typeof_unqual",
    "__typeof_unqual__",
    "_Atomic",
    "__signed__",
    "__signed",
    "__volatile__",
    "__volatile",
    "__const",
    "__const__",
    "__int128",
    "_Float16",
    "_Float32",
    "_Float64",
    "_Float128",
    "bool",
];

const QUALIFIERS: &[&str] = &[
    "const",
    "volatile",
    "restrict",
    "__restrict",
    "__restrict__",
    "_Atomic",
    "__volatile__",
    "__volatile",
    "__const",
    "__const__",
];

const GROUPED_WORDS: &[&str] = &[
    "typeof",
    "__typeof__",
    "__typeof",
    "typeof_unqual",
    "__typeof_unqual__",
    "__attribute__",
    "__attribute",
    "_Atomic",
];

const SIZE_WORDS: &[&str] = &["sizeof", "_Alignof", "__alignof__", "__alignof", "alignof"];

pub fn is_type_word(w: &str) -> bool {
    TYPE_WORDS.contains(&w) && !QUALIFIERS.contains(&w)
}

pub fn is_keyword(w: &str) -> bool {
    TYPE_WORDS.contains(&w)
        || SIZE_WORDS.contains(&w)
        || GROUPED_WORDS.contains(&w)
        || matches!(
            w,
            "register"
                | "static"
                | "extern"
                | "inline"
                | "__inline"
                | "__inline__"
                | "auto"
                | "__extension__"
                | "return"
                | "if"
                | "else"
                | "for"
                | "while"
                | "do"
                | "switch"
                | "case"
                | "default"
                | "goto"
                | "break"
                | "continue"
                | "typedef"
        )
}

const BINARY: &[(&str, u8)] = &[
    ("||", 4),
    ("&&", 5),
    ("|", 6),
    ("^", 7),
    ("&", 8),
    ("==", 9),
    ("!=", 9),
    ("<", 10),
    (">", 10),
    ("<=", 10),
    (">=", 10),
    ("<<", 11),
    (">>", 11),
    ("+", 12),
    ("-", 12),
    ("*", 13),
    ("/", 13),
    ("%", 13),
];

const ASSIGNS: &[&str] = &[
    "=", "*=", "/=", "%=", "+=", "-=", "<<=", ">>=", "&=", "^=", "|=",
];

const NOT_OPERAND: &[&str] = &[
    ")", "]", "}", ",", ";", ":", "?", "/", "%", "<<", ">>", "<", ">", "<=", ">=", "==", "!=", "^",
    "|", "||", "=", "*=", "/=", "%=", "+=", "-=", "<<=", ">>=", "&=", "^=", "|=", ".", "->", "[",
];

#[derive(Clone, Copy)]
struct T<'a> {
    kind: TokenKind,
    text: &'a str,
    sym: Sym,
    space: bool,
}

impl T<'_> {
    fn punct(&self, p: &str) -> bool {
        self.kind == TokenKind::Punct && self.text == p
    }

    fn word(&self) -> bool {
        self.kind == TokenKind::Ident
    }
}

enum E<'a> {
    Atom(&'a str),
    Lit(String),
    Block(Vec<Item<'a>>),
    Strs(Vec<&'a str>),
    Raw(&'a [T<'a>]),
    Cast(&'a [T<'a>], Box<E<'a>>),
    Prefix(&'a str, Box<E<'a>>),
    Size(&'a str, Box<E<'a>>),
    Postfix(Box<E<'a>>, &'a str),
    Call(Box<E<'a>>, Vec<E<'a>>),
    Index(Box<E<'a>>, Box<E<'a>>),
    Member(Box<E<'a>>, &'a str, &'a str),
    Binary(&'a str, u8, Box<E<'a>>, Box<E<'a>>),
    Cond(Box<E<'a>>, Box<E<'a>>, Box<E<'a>>),
    Assign(&'a str, Box<E<'a>>, Box<E<'a>>),
    Comma(Box<E<'a>>, Box<E<'a>>),
}

impl E<'_> {
    fn prec(&self) -> u8 {
        match self {
            E::Atom(_) | E::Lit(_) | E::Block(_) | E::Strs(_) | E::Raw(_) => PRIMARY,
            E::Postfix(..) | E::Call(..) | E::Index(..) | E::Member(..) => POSTFIX,
            E::Cast(..) | E::Prefix(..) | E::Size(..) => UNARY,
            E::Binary(_, p, ..) => *p,
            E::Cond(..) => COND,
            E::Assign(..) => ASSIGN,
            E::Comma(..) => COMMA,
        }
    }
}

enum Item<'a> {
    Expr(E<'a>, bool),
    Decl(&'a [T<'a>], Vec<(&'a [T<'a>], Option<E<'a>>)>, bool),
    Raw(&'a [T<'a>], bool),
}

#[derive(Clone, Default)]
struct Scope {
    vars: FxHashSet<Sym>,
    types: FxHashSet<Sym>,
}

impl Scope {
    fn forget(&mut self, toks: &[T<'_>], vars: bool) {
        for t in toks.iter().filter(|t| t.word()) {
            self.types.remove(&t.sym);
            if vars {
                self.vars.remove(&t.sym);
            }
        }
    }

    fn declare_var(&mut self, s: Sym) {
        self.types.remove(&s);
        self.vars.insert(s);
    }

    fn declare_type(&mut self, s: Sym) {
        self.vars.remove(&s);
        self.types.insert(s);
    }
}

struct Fail;

type R<'a> = Result<E<'a>, Fail>;

fn binary_prec(t: &T<'_>) -> Option<u8> {
    if t.kind != TokenKind::Punct {
        return None;
    }
    BINARY.iter().find(|(op, _)| *op == t.text).map(|(_, p)| *p)
}

fn closing(toks: &[T<'_>], open: usize) -> Option<usize> {
    let (o, c) = match toks[open].text {
        "(" => ("(", ")"),
        "[" => ("[", "]"),
        "{" => ("{", "}"),
        _ => return None,
    };
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate().skip(open) {
        if t.kind != TokenKind::Punct {
            continue;
        }
        if t.text == o {
            depth += 1;
        } else if t.text == c {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

fn is_group_start(t: &T<'_>) -> bool {
    t.punct("(") || t.punct("[") || t.punct("{")
}

fn top_level<'a>(toks: &'a [T<'a>]) -> Vec<&'a [T<'a>]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let end = if is_group_start(&toks[i]) {
            closing(toks, i).unwrap_or(toks.len() - 1)
        } else {
            i
        };
        out.push(&toks[i..=end]);
        i = end + 1;
    }
    out
}

fn pointer_declarator(group: &[T<'_>]) -> bool {
    if !group.first().is_some_and(|t| t.punct("(")) {
        return false;
    }
    let inner = &group[1..group.len() - 1];
    inner.first().is_some_and(|t| t.punct("*"))
        && top_level(inner).iter().all(|part| {
            let t = part[0];
            (part.len() == 1 && (t.punct("*") || QUALIFIERS.contains(&t.text)))
                || pointer_declarator(part)
                || (part.len() > 1 && t.punct("["))
        })
}

fn definite_type(content: &[T<'_>]) -> bool {
    let parts = top_level(content);
    let mut keyword = false;
    let mut prev_grouping = false;
    let mut prev_declarator = false;
    for part in &parts {
        let t = part[0];
        if part.len() > 1 {
            let declarator = pointer_declarator(part);
            let attached = t.punct("(") && (prev_grouping || prev_declarator);
            if !declarator && !attached && !(prev_declarator && t.punct("[")) {
                return false;
            }
            keyword |= declarator;
            prev_declarator = declarator;
            prev_grouping = false;
            continue;
        }
        prev_declarator = false;
        if t.word() {
            keyword |= TYPE_WORDS.contains(&t.text);
            prev_grouping = GROUPED_WORDS.contains(&t.text);
        } else if t.punct("*") {
            prev_grouping = false;
        } else {
            return false;
        }
    }
    keyword
        || parts
            .last()
            .is_some_and(|p| p.len() == 1 && p[0].punct("*"))
}

fn names_a_type(content: &[T<'_>]) -> bool {
    top_level(content).iter().any(|part| {
        (part.len() == 1 && part[0].word() && TYPE_WORDS.contains(&part[0].text))
            || pointer_declarator(part)
    }) || content.last().is_some_and(|t| t.punct("*"))
}

struct Parser<'a, 'k> {
    toks: &'a [T<'a>],
    at: usize,
    scope: &'k Scope,
    depth: usize,
}

impl<'a, 'k> Parser<'a, 'k> {
    fn new(toks: &'a [T<'a>], scope: &'k Scope, depth: usize) -> Parser<'a, 'k> {
        Parser {
            toks,
            at: 0,
            scope,
            depth,
        }
    }

    fn peek(&self) -> Option<T<'a>> {
        self.toks.get(self.at).copied()
    }

    fn peek_is(&self, p: &str) -> bool {
        self.peek().is_some_and(|t| t.punct(p))
    }

    fn whole(toks: &'a [T<'a>], scope: &'k Scope, depth: usize) -> R<'a> {
        if depth > MAX_DEPTH || toks.is_empty() {
            return Err(Fail);
        }
        let mut p = Parser::new(toks, scope, depth);
        let e = p.comma()?;
        if p.at == toks.len() { Ok(e) } else { Err(Fail) }
    }

    fn group(&self, open: usize) -> Result<(usize, &'a [T<'a>]), Fail> {
        let close = closing(self.toks, open).ok_or(Fail)?;
        Ok((close, &self.toks[open + 1..close]))
    }

    fn comma(&mut self) -> R<'a> {
        let mut lhs = self.assign()?;
        while self.peek_is(",") {
            self.at += 1;
            let rhs = self.assign()?;
            lhs = E::Comma(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn assign(&mut self) -> R<'a> {
        let lhs = self.cond()?;
        let Some(op) = self
            .peek()
            .filter(|t| t.kind == TokenKind::Punct && ASSIGNS.contains(&t.text))
        else {
            return Ok(lhs);
        };
        if lhs.prec() < UNARY {
            return Err(Fail);
        }
        self.at += 1;
        let rhs = self.assign()?;
        Ok(E::Assign(op.text, Box::new(lhs), Box::new(rhs)))
    }

    fn cond(&mut self) -> R<'a> {
        let c = self.binary(LOR)?;
        if !self.peek_is("?") {
            return Ok(c);
        }
        self.at += 1;
        if self.peek_is(":") {
            return Err(Fail);
        }
        let t = self.comma()?;
        if !self.peek_is(":") {
            return Err(Fail);
        }
        self.at += 1;
        let f = self.cond()?;
        Ok(E::Cond(Box::new(c), Box::new(t), Box::new(f)))
    }

    fn binary(&mut self, min: u8) -> R<'a> {
        let mut lhs = self.unary()?;
        while let Some(t) = self.peek()
            && let Some(p) = binary_prec(&t)
            && p >= min
        {
            self.at += 1;
            let rhs = self.binary(p + 1)?;
            lhs = E::Binary(t.text, p, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> R<'a> {
        let t = self.peek().ok_or(Fail)?;
        if t.kind == TokenKind::Punct {
            match t.text {
                "+" | "-" | "!" | "~" | "*" | "&" | "++" | "--" => {
                    self.at += 1;
                    let e = self.unary()?;
                    return Ok(E::Prefix(t.text, Box::new(e)));
                }
                "(" => return self.parenthesized(),
                _ => {}
            }
        }
        if t.word() && SIZE_WORDS.contains(&t.text) {
            self.at += 1;
            if self.peek_is("(") {
                let open = self.at;
                let (close, _) = self.group(open)?;
                self.at = close + 1;
                if self.peek().is_some_and(|n| {
                    n.punct("[")
                        || n.punct("(")
                        || n.punct(".")
                        || n.punct("->")
                        || n.punct("++")
                        || n.punct("--")
                }) {
                    return Err(Fail);
                }
                return Ok(E::Raw(&self.toks[open - 1..=close]));
            }
            let e = self.unary()?;
            return Ok(E::Size(t.text, Box::new(e)));
        }
        let e = self.primary()?;
        self.postfix(e)
    }

    fn parenthesized(&mut self) -> R<'a> {
        let open = self.at;
        let (close, content) = self.group(open)?;
        let next = self.toks.get(close + 1).copied();
        let whole = &self.toks[open..=close];
        if content.is_empty() {
            return Err(Fail);
        }
        if content[0].punct("{") {
            self.at = close + 1;
            let block = match closing(content, 0) {
                Some(end) if end + 1 == content.len() => self.block(&content[1..end]),
                _ => None,
            };
            return self.postfix(block.unwrap_or(E::Raw(whole)));
        }
        let cast = if definite_type(content) {
            true
        } else if content.len() == 1 && content[0].word() {
            let lone = content[0];
            if TYPE_WORDS.contains(&lone.text) || SIZE_WORDS.contains(&lone.text) {
                return Err(Fail);
            }
            if self.scope.types.contains(&lone.sym) {
                true
            } else if self.scope.vars.contains(&lone.sym) {
                false
            } else {
                match next {
                    None => false,
                    Some(n) if n.kind == TokenKind::Punct && NOT_OPERAND.contains(&n.text) => false,
                    Some(n) if n.kind != TokenKind::Punct => true,
                    Some(n) if n.punct("!") || n.punct("~") => true,
                    Some(_) => return Err(Fail),
                }
            }
        } else if names_a_type(content) {
            return Err(Fail);
        } else {
            false
        };
        self.at = close + 1;
        if cast {
            if self.peek_is("{") || self.peek().is_none() {
                return Err(Fail);
            }
            let operand = self.unary()?;
            return Ok(E::Cast(content, Box::new(operand)));
        }
        let inner = Parser::whole(content, self.scope, self.depth + 1).unwrap_or(E::Raw(whole));
        self.postfix(inner)
    }

    fn block(&self, inner: &'a [T<'a>]) -> Option<E<'a>> {
        if self.depth > MAX_DEPTH {
            return None;
        }
        let mut scope = self.scope.clone();
        let items: Vec<Item<'a>> = statements(inner)
            .into_iter()
            .map(|(toks, semi)| item(toks, semi, &mut scope, self.depth + 1))
            .collect();
        (!items.is_empty()).then_some(E::Block(items))
    }

    fn primary(&mut self) -> R<'a> {
        let t = self.peek().ok_or(Fail)?;
        match t.kind {
            TokenKind::Ident if TYPE_WORDS.contains(&t.text) || t.text == "__extension__" => {
                Err(Fail)
            }
            TokenKind::Ident | TokenKind::Number | TokenKind::Char => {
                self.at += 1;
                Ok(E::Atom(t.text))
            }
            TokenKind::Str => {
                let mut parts = Vec::new();
                while let Some(s) = self.peek().filter(|s| s.kind == TokenKind::Str) {
                    parts.push(s.text);
                    self.at += 1;
                }
                Ok(E::Strs(parts))
            }
            _ => Err(Fail),
        }
    }

    fn postfix(&mut self, mut e: E<'a>) -> R<'a> {
        while let Some(t) = self.peek() {
            if t.kind != TokenKind::Punct {
                break;
            }
            match t.text {
                "(" => {
                    let (close, content) = self.group(self.at)?;
                    let args = split_args(content)
                        .into_iter()
                        .map(|arg| {
                            Parser::whole(arg, self.scope, self.depth + 1).unwrap_or(E::Raw(arg))
                        })
                        .collect();
                    if content.is_empty() {
                        e = E::Call(Box::new(e), Vec::new());
                    } else {
                        e = E::Call(Box::new(e), args);
                    }
                    self.at = close + 1;
                }
                "[" => {
                    let (close, content) = self.group(self.at)?;
                    let idx = Parser::whole(content, self.scope, self.depth + 1)
                        .unwrap_or(E::Raw(content));
                    e = E::Index(Box::new(e), Box::new(idx));
                    self.at = close + 1;
                }
                "." | "->" => {
                    let name = self
                        .toks
                        .get(self.at + 1)
                        .filter(|n| n.word())
                        .ok_or(Fail)?;
                    e = E::Member(Box::new(e), t.text, name.text);
                    self.at += 2;
                }
                "++" | "--" => {
                    e = E::Postfix(Box::new(e), t.text);
                    self.at += 1;
                }
                _ => break,
            }
        }
        Ok(e)
    }
}

const SPEC_WORDS: &[&str] = &[
    "static",
    "register",
    "extern",
    "auto",
    "__auto_type",
    "_Thread_local",
    "__thread",
    "thread_local",
    "inline",
    "__inline",
    "__inline__",
];

const STATEMENT_WORDS: &[&str] = &[
    "__label__",
    "__extension__",
    "asm",
    "__asm",
    "__asm__",
    "_Static_assert",
    "static_assert",
    "__attribute__",
    "__attribute",
];

fn statements<'a>(inner: &'a [T<'a>]) -> Vec<(&'a [T<'a>], bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < inner.len() {
        if is_group_start(&inner[i]) {
            i = closing(inner, i).unwrap_or(inner.len() - 1) + 1;
            continue;
        }
        if inner[i].punct(";") {
            out.push((&inner[start..i], true));
            start = i + 1;
        }
        i += 1;
    }
    if start < inner.len() {
        out.push((&inner[start..], false));
    }
    out
}

fn split_top<'a>(toks: &'a [T<'a>], sep: &str) -> Vec<&'a [T<'a>]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < toks.len() {
        if is_group_start(&toks[i]) {
            i = closing(toks, i).unwrap_or(toks.len() - 1) + 1;
            continue;
        }
        if toks[i].punct(sep) {
            out.push(&toks[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    out.push(&toks[start..]);
    out
}

fn declared_name(d: &[T<'_>]) -> Option<(Sym, bool)> {
    let (name, rest) = d.split_last()?;
    if !name.word() || is_keyword(name.text) || STATEMENT_WORDS.contains(&name.text) {
        return None;
    }
    let mut pointer = false;
    for t in rest {
        if t.punct("*") {
            pointer = true;
        } else if !(t.word() && QUALIFIERS.contains(&t.text)) {
            return None;
        }
    }
    Some((name.sym, pointer))
}

fn declaration<'a>(toks: &'a [T<'a>], scope: &mut Scope, depth: usize) -> Option<Item<'a>> {
    let mut i = 0;
    let mut has_type = false;
    let mut typedef_name = None;
    while let Some(t) = toks.get(i).filter(|t| t.word()) {
        if GROUPED_WORDS.contains(&t.text) && TYPE_WORDS.contains(&t.text) && t.text != "_Atomic" {
            if !toks.get(i + 1).is_some_and(|n| n.punct("(")) {
                return None;
            }
            i = closing(toks, i + 1)? + 1;
            has_type = true;
        } else if matches!(t.text, "struct" | "union" | "enum") {
            let tag = toks.get(i + 1)?;
            if !tag.word() || is_keyword(tag.text) || toks.get(i + 2).is_some_and(|n| n.punct("{"))
            {
                return None;
            }
            i += 2;
            has_type = true;
        } else if TYPE_WORDS.contains(&t.text) || SPEC_WORDS.contains(&t.text) {
            has_type |= (!QUALIFIERS.contains(&t.text) && !SPEC_WORDS.contains(&t.text))
                || t.text == "__auto_type";
            i += 1;
        } else if is_keyword(t.text) || STATEMENT_WORDS.contains(&t.text) {
            return None;
        } else if !has_type {
            let next = toks.get(i + 1)?;
            if !(next.word() && !is_keyword(next.text)) && !next.punct("*") {
                return None;
            }
            typedef_name = Some(t.sym);
            has_type = true;
            i += 1;
        } else {
            break;
        }
    }
    if !has_type || i == 0 || i >= toks.len() {
        return None;
    }
    let mut parts = Vec::new();
    for (n, part) in split_top(&toks[i..], ",").into_iter().enumerate() {
        let pieces = split_top(part, "=");
        let (d, init) = match pieces.as_slice() {
            [d] => (*d, None),
            [d, _, ..] => (*d, Some(&part[d.len() + 1..])),
            [] => return None,
        };
        let (name, pointer) = declared_name(d)?;
        if n == 0 && pointer && typedef_name.is_some() && init.is_none() {
            return None;
        }
        if init.is_some_and(|x| x.is_empty() || x[0].punct("{")) {
            return None;
        }
        parts.push((d, name, init));
    }
    if let Some(t) = typedef_name {
        scope.declare_type(t);
    }
    let mut decls = Vec::with_capacity(parts.len());
    for (d, name, init) in parts {
        scope.declare_var(name);
        let init = init.map(|x| Parser::whole(x, scope, depth).unwrap_or(E::Raw(x)));
        decls.push((d, init));
    }
    Some(Item::Decl(&toks[..i], decls, false))
}

fn item<'a>(toks: &'a [T<'a>], semi: bool, scope: &mut Scope, depth: usize) -> Item<'a> {
    if toks.is_empty() {
        return Item::Raw(toks, semi);
    }
    if let Some(Item::Decl(spec, decls, _)) = declaration(toks, scope, depth) {
        return Item::Decl(spec, decls, semi);
    }
    let first = toks[0];
    let statement = first.word()
        && (is_keyword(first.text) || STATEMENT_WORDS.contains(&first.text))
        && !SIZE_WORDS.contains(&first.text);
    let declarator_like = first.word()
        && !is_keyword(first.text)
        && !scope.vars.contains(&first.sym)
        && toks.get(1).is_some_and(|t| t.punct("*"));
    if !statement
        && !declarator_like
        && let Ok(e) = Parser::whole(toks, scope, depth)
    {
        scope.forget(toks, false);
        return Item::Expr(e, semi);
    }
    scope.forget(toks, true);
    Item::Raw(toks, semi)
}

fn split_args<'a>(content: &'a [T<'a>]) -> Vec<&'a [T<'a>]> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < content.len() {
        if is_group_start(&content[i]) {
            i = closing(content, i).unwrap_or(content.len() - 1) + 1;
            continue;
        }
        if content[i].punct(",") {
            out.push(&content[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    out.push(&content[start..]);
    out
}

const BASIC_TYPE_WORDS: &[&str] = &[
    "void",
    "char",
    "short",
    "int",
    "long",
    "float",
    "double",
    "signed",
    "unsigned",
    "_Bool",
    "bool",
    "_Complex",
    "__int128",
    "__signed__",
    "__signed",
    "const",
    "volatile",
    "__volatile__",
    "__volatile",
    "__const",
    "__const__",
];

fn basic_type(toks: &[T<'_>]) -> bool {
    !toks.is_empty()
        && toks
            .iter()
            .all(|t| t.punct("*") || (t.word() && BASIC_TYPE_WORDS.contains(&t.text)))
        && toks
            .iter()
            .any(|t| t.word() && !QUALIFIERS.contains(&t.text))
}

fn same_type(a: &[T<'_>], b: &[T<'_>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.text == y.text)
}

fn unevaluated_size(raw: &[T<'_>]) -> bool {
    let [word, open, content @ .., close] = raw else {
        return false;
    };
    if !SIZE_WORDS.contains(&word.text) || !open.punct("(") || !close.punct(")") {
        return false;
    }
    if basic_type(content) {
        return true;
    }
    let scope = Scope::default();
    match Parser::whole(content, &scope, 0) {
        Ok(E::Cast(ty, _)) => basic_type(ty),
        Ok(E::Atom(text)) => c_const::literal(text).is_some(),
        _ => false,
    }
}

fn dead(e: &E<'_>) -> bool {
    let E::Cast(ty, operand) = e else {
        return false;
    };
    if !matches!(ty, [t] if t.text == "void") {
        return false;
    }
    match &**operand {
        E::Lit(_) => true,
        E::Atom(text) => c_const::literal(text).is_some(),
        E::Raw(raw) => unevaluated_size(raw),
        _ => false,
    }
}

fn prune<'a>(items: Vec<Item<'a>>) -> Vec<Item<'a>> {
    let last = items.len().saturating_sub(1);
    items
        .into_iter()
        .enumerate()
        .filter(|(i, item)| {
            *i == last
                || !match item {
                    Item::Expr(e, true) => dead(e),
                    Item::Raw(toks, true) => toks.is_empty(),
                    _ => false,
                }
        })
        .map(|(_, item)| item)
        .collect()
}

struct Folded<'a> {
    e: E<'a>,
    k: Option<Konst>,
    ops: bool,
    pure: bool,
}

fn leaf(e: E<'_>) -> Folded<'_> {
    Folded {
        e,
        k: None,
        ops: false,
        pure: false,
    }
}

fn settle<'a>(e: E<'a>, k: Option<Konst>, ops: bool, pure: bool) -> Folded<'a> {
    let pure = pure && k.is_some();
    if ops
        && let Some(konst) = k
        && let Some((neg, text)) = c_const::spell(konst)
    {
        let lit = E::Lit(text);
        let e = if neg {
            E::Prefix("-", Box::new(lit))
        } else {
            lit
        };
        return Folded { e, k, ops, pure };
    }
    Folded { e, k, ops, pure }
}

struct Cx<'c, 'a> {
    env: &'c dyn TypeEnv,
    bound: FxHashSet<&'a str>,
}

impl<'a> Cx<'_, 'a> {
    fn inner(&self, items: &[Item<'a>]) -> Cx<'_, 'a> {
        let mut bound = self.bound.clone();
        for item in items {
            if let Item::Decl(_, decls, _) = item {
                for (d, _) in decls {
                    if let Some(name) = d.last() {
                        bound.insert(name.text);
                    }
                }
            }
        }
        Cx {
            env: self.env,
            bound,
        }
    }
}

fn fold_item<'a>(item: Item<'a>, cx: &Cx<'_, 'a>) -> Item<'a> {
    match item {
        Item::Expr(e, semi) => Item::Expr(fold(e, cx).e, semi),
        Item::Decl(spec, decls, semi) => Item::Decl(
            spec,
            decls
                .into_iter()
                .map(|(d, init)| (d, init.map(|e| fold(e, cx).e)))
                .collect(),
            semi,
        ),
        raw @ Item::Raw(..) => raw,
    }
}

fn ident(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && !is_keyword(text)
}

fn texts<'a>(toks: &[T<'a>]) -> Vec<&'a str> {
    toks.iter().map(|t| t.text).collect()
}

fn null_pointer(e: &E<'_>) -> bool {
    match e {
        E::Atom(t) => c_const::literal(t).is_some_and(|k| k.v == 0),
        E::Lit(t) => c_const::literal(t).is_some_and(|k| k.v == 0),
        E::Cast(ty, x) => texts(ty) == ["void", "*"] && null_pointer(x),
        _ => false,
    }
}

fn pointee<'a>(ty: &[T<'a>]) -> Option<Vec<&'a str>> {
    let mut words = texts(ty);
    let star = words.iter().rposition(|w| *w == "*")?;
    if words[star + 1..]
        .iter()
        .any(|w| !matches!(*w, "const" | "volatile" | "__restrict" | "restrict"))
    {
        return None;
    }
    words.truncate(star);
    Some(words)
}

fn member_path<'a>(e: &E<'a>, path: &mut Vec<Step<'a>>) -> Option<Vec<&'a str>> {
    match e {
        E::Member(base, op, name) => {
            let root = match (*op, &**base) {
                ("->", E::Cast(ty, x)) if null_pointer(x) => Some(pointee(ty)?),
                (".", E::Prefix("*", inner)) => match &**inner {
                    E::Cast(ty, x) if null_pointer(x) => Some(pointee(ty)?),
                    _ => None,
                },
                (".", inner) => {
                    let r = member_path(inner, path)?;
                    path.push(Step::Field(name));
                    return Some(r);
                }
                _ => None,
            }?;
            path.push(Step::Field(name));
            Some(root)
        }
        E::Index(base, idx) => {
            let r = member_path(base, path)?;
            let k = fold_ref(idx)?;
            path.push(Step::Index(u64::try_from(k.v).ok()?));
            Some(r)
        }
        _ => None,
    }
}

fn fold_ref(e: &E<'_>) -> Option<Konst> {
    match e {
        E::Atom(t) => c_const::literal(t),
        E::Lit(t) => c_const::literal(t),
        E::Prefix("-", x) => c_const::unary("-", fold_ref(x)?),
        _ => None,
    }
}

fn base_of<'a>(words: &[&'a str], cx: &Cx<'_, 'a>) -> Option<Base<'a>> {
    let t = c_type::parse_type_name(words)?;
    if !t.ops.is_empty() {
        return None;
    }
    match t.base {
        Base::Typedef(n) if cx.bound.contains(n) => None,
        b @ (Base::Tag(..) | Base::Typedef(_)) => Some(b),
        _ => None,
    }
}

fn type_of_tokens<'a>(toks: &'a [T<'a>], cx: &Cx<'_, 'a>) -> Option<IntTy> {
    let scope = Scope::default();
    if let Ok(e) = Parser::whole(toks, &scope, 0) {
        let f = fold(e, cx);
        if let Some(k) = f.k {
            return Some(k.ty);
        }
        if let Some(t) = int_type(&f.e, cx) {
            return Some(t);
        }
    }
    let words = texts(toks);
    let t = c_type::parse_type_name(&words)?;
    if matches!(t.base, Base::Typedef(n) if cx.bound.contains(n)) {
        return None;
    }
    cx.env.int_of(&t)
}

fn cast_type<'a>(ty: &'a [T<'a>], cx: &Cx<'_, 'a>) -> Option<IntTy> {
    let words = texts(ty);
    let t = c_type::parse_type_name(&words)?;
    if !t.ops.is_empty() {
        return None;
    }
    match t.base {
        Base::Typedef(n) if cx.bound.contains(n) => None,
        Base::Typeof(lo, hi) => type_of_tokens(&ty[lo..hi], cx),
        _ => cx.env.int_of(&t),
    }
}

fn size_of_type<'a>(content: &'a [T<'a>], cx: &Cx<'_, 'a>) -> Option<u64> {
    let words = texts(content);
    let t = c_type::parse_type_name(&words)?;
    if matches!(t.base, Base::Typedef(n) if cx.bound.contains(n)) {
        return None;
    }
    let typeof_size = match t.base {
        Base::Typeof(lo, hi) => type_of_tokens(&content[lo..hi], cx).and_then(c_type::lp64_bytes),
        _ => None,
    };
    cx.env.size_of(&t, typeof_size)
}

fn size_of_expr<'a>(e: E<'a>, cx: &Cx<'_, 'a>) -> Option<u64> {
    if !cx.env.lp64() {
        return None;
    }
    let mut path = Vec::new();
    if let Some(root) = member_path(&e, &mut path) {
        let base = base_of(&root, cx)?;
        return cx.env.offset(&base, &path)?.1;
    }
    if let E::Cast(ty, _) = &e {
        return size_of_type(ty, cx);
    }
    let f = fold(e, cx);
    match f.k {
        Some(k) => c_type::lp64_bytes(k.ty),
        None => int_type(&f.e, cx).and_then(c_type::lp64_bytes),
    }
}

fn sizeof_raw<'a>(raw: &'a [T<'a>], cx: &Cx<'_, 'a>) -> Option<Konst> {
    let [word, open, content @ .., close] = raw else {
        return None;
    };
    if word.text != "sizeof" || !open.punct("(") || !close.punct(")") || content.is_empty() {
        return None;
    }
    if let Some(n) = size_of_type(content, cx) {
        return Some(c_const::size_t(n));
    }
    let scope = Scope::default();
    let e = Parser::whole(content, &scope, 0).ok()?;
    size_of_expr(e, cx).map(c_const::size_t)
}

fn offsetof<'a>(args: &[E<'a>], cx: &Cx<'_, 'a>) -> Option<Konst> {
    let [ty, designator] = args else {
        return None;
    };
    let words: Vec<&str> = match ty {
        E::Raw(t) => texts(t),
        E::Atom(t) => vec![*t],
        _ => return None,
    };
    let base = base_of(&words, cx)?;
    let mut path = Vec::new();
    designator_path(designator, &mut path)?;
    let (off, _) = cx.env.offset(&base, &path)?;
    Some(c_const::size_t(off))
}

fn designator_path<'a>(e: &E<'a>, path: &mut Vec<Step<'a>>) -> Option<()> {
    match e {
        E::Atom(name) if ident(name) => path.push(Step::Field(name)),
        E::Member(base, ".", name) => {
            designator_path(base, path)?;
            path.push(Step::Field(name));
        }
        E::Index(base, idx) => {
            designator_path(base, path)?;
            path.push(Step::Index(u64::try_from(fold_ref(idx)?.v).ok()?));
        }
        _ => return None,
    }
    Some(())
}

fn address_offset<'a>(e: &E<'a>, ty: &'a [T<'a>], cx: &Cx<'_, 'a>) -> Option<Konst> {
    let E::Prefix("&", inner) = e else {
        return None;
    };
    let target = cast_type(ty, cx)?;
    let mut path = Vec::new();
    let root = member_path(inner, &mut path)?;
    let base = base_of(&root, cx)?;
    let (off, _) = cx.env.offset(&base, &path)?;
    c_const::cast_to(target, c_const::size_t(off))
}

const BUILTIN_INT: &[&str] = &[
    "__builtin_clz",
    "__builtin_clzl",
    "__builtin_clzll",
    "__builtin_ctz",
    "__builtin_ctzl",
    "__builtin_ctzll",
    "__builtin_popcount",
    "__builtin_popcountl",
    "__builtin_popcountll",
    "__builtin_parity",
    "__builtin_parityl",
    "__builtin_parityll",
    "__builtin_ffs",
    "__builtin_ffsl",
    "__builtin_ffsll",
    "__builtin_clrsb",
    "__builtin_clrsbl",
    "__builtin_clrsbll",
    "__builtin_constant_p",
];

fn int_type<'a>(e: &E<'a>, cx: &Cx<'_, 'a>) -> Option<IntTy> {
    match e {
        E::Lit(t) => c_const::literal(t).map(|k| k.ty),
        E::Atom(t) => match c_const::literal(t) {
            Some(k) => Some(k.ty),
            None if ident(t) && !cx.bound.contains(t) => cx.env.enumerator(t).map(|_| IntTy::Int),
            None => None,
        },
        E::Prefix(op, x) => match *op {
            "-" | "+" | "~" => int_type(x, cx).map(promote),
            "!" => Some(IntTy::Int),
            _ => None,
        },
        E::Binary(op, _, l, r) => match *op {
            "<<" | ">>" => {
                int_type(r, cx)?;
                int_type(l, cx).map(promote)
            }
            "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||" => Some(IntTy::Int),
            _ => Some(common(int_type(l, cx)?, int_type(r, cx)?, false)),
        },
        E::Cast(ty, _) => cast_type(ty, cx),
        E::Cond(_, a, b) => Some(common(int_type(a, cx)?, int_type(b, cx)?, false)),
        E::Call(f, _) => match &**f {
            E::Atom(name) if BUILTIN_INT.contains(name) => Some(IntTy::Int),
            E::Atom("__builtin_bswap32") => Some(IntTy::UInt),
            E::Atom("__builtin_bswap64") => Some(IntTy::ULong),
            E::Atom(name) if ident(name) && !cx.bound.contains(name) => cx.env.fn_return(name),
            _ => None,
        },
        E::Raw([w, ..]) if w.text == "sizeof" => cx.env.lp64().then_some(IntTy::ULong),
        E::Size("sizeof", _) => cx.env.lp64().then_some(IntTy::ULong),
        _ => None,
    }
}

fn fold<'a>(e: E<'a>, cx: &Cx<'_, 'a>) -> Folded<'a> {
    match e {
        E::Atom(text) => {
            if let Some(k) = c_const::literal(text) {
                return Folded {
                    e: E::Atom(text),
                    k: Some(k),
                    ops: false,
                    pure: true,
                };
            }
            if ident(text)
                && !cx.bound.contains(text)
                && let Some(k) = cx.env.enumerator(text).and_then(c_const::int)
            {
                return settle(E::Atom(text), Some(k), true, true);
            }
            leaf(E::Atom(text))
        }
        E::Prefix(op, x) => {
            let x = fold(*x, cx);
            let k = x.k.and_then(|k| c_const::unary(op, k));
            settle(E::Prefix(op, Box::new(x.e)), k, x.ops, x.pure)
        }
        E::Binary(op, p, l, r) => {
            let (l, r) = (fold(*l, cx), fold(*r, cx));
            let k = match (l.k, r.k) {
                (Some(a), Some(b)) => c_const::binary(op, a, b),
                _ => None,
            };
            let pure = l.pure && r.pure;
            settle(
                E::Binary(op, p, Box::new(l.e), Box::new(r.e)),
                k,
                true,
                pure,
            )
        }
        E::Cast(ty, x) => {
            let mut x = fold(*x, cx);
            if let E::Cast(inner, operand) = x.e {
                x.e = if basic_type(ty) && same_type(ty, inner) {
                    *operand
                } else {
                    E::Cast(inner, operand)
                };
            }
            let words: Option<Vec<&str>> = ty.iter().map(|t| t.word().then_some(t.text)).collect();
            let typed = |k: Konst| cast_type(ty, cx).and_then(|t| c_const::cast_to(t, k));
            let (k, ops, pure) = match (x.k, words) {
                (Some(k), Some(w)) => (c_const::cast(&w, k).or_else(|| typed(k)), x.ops, x.pure),
                (Some(k), None) => (typed(k), x.ops, x.pure),
                (None, _) => match address_offset(&x.e, ty, cx) {
                    Some(k) => (Some(k), true, true),
                    None => (None, x.ops, false),
                },
            };
            settle(E::Cast(ty, Box::new(x.e)), k, ops, pure)
        }
        E::Cond(c, t, f) => {
            let (c, t, f) = (fold(*c, cx), fold(*t, cx), fold(*f, cx));
            let pure = c.pure && t.pure && f.pure;
            if let (Some(a), Some(b), Some(d)) = (c.k, t.k, f.k) {
                let k = c_const::cond(a, b, d);
                return settle(
                    E::Cond(Box::new(c.e), Box::new(t.e), Box::new(f.e)),
                    k,
                    true,
                    pure,
                );
            }
            if let Some(ck) = c.k {
                let (chosen, other) = if ck.v != 0 { (&t, &f) } else { (&f, &t) };
                let ct = chosen.k.map(|k| k.ty).or_else(|| int_type(&chosen.e, cx));
                let ot = other.k.map(|k| k.ty).or_else(|| int_type(&other.e, cx));
                if let (Some(ct), Some(ot)) = (ct, ot) {
                    let ty = common(ct, ot, false);
                    if let Some(k) = chosen.k.and_then(|k| c_const::cast_to(ty, k)) {
                        return settle(
                            E::Cond(Box::new(c.e), Box::new(t.e), Box::new(f.e)),
                            Some(k),
                            true,
                            false,
                        );
                    }
                    if chosen.k.is_none() && ty == ct {
                        let chosen = if ck.v != 0 { t } else { f };
                        return Folded {
                            e: chosen.e,
                            k: None,
                            ops: true,
                            pure: false,
                        };
                    }
                }
            }
            settle(
                E::Cond(Box::new(c.e), Box::new(t.e), Box::new(f.e)),
                None,
                true,
                false,
            )
        }
        E::Comma(l, r) => leaf(E::Comma(Box::new(fold(*l, cx).e), Box::new(fold(*r, cx).e))),
        E::Call(f, args) => {
            let name = match &*f {
                E::Atom(n) => Some(*n),
                _ => None,
            };
            match (name, args.len()) {
                (Some("__builtin_offsetof"), 2) => {
                    if let Some(k) = offsetof(&args, cx) {
                        return settle(E::Call(f, args), Some(k), true, true);
                    }
                }
                (Some("__builtin_choose_expr"), 3) => {
                    let mut args = args;
                    let c = fold(args.remove(0), cx);
                    let (a, b) = (args.remove(0), args.remove(0));
                    if let Some(k) = c.k.filter(|_| c.pure) {
                        let chosen = fold(if k.v != 0 { a } else { b }, cx);
                        let pure = chosen.pure;
                        return settle(chosen.e, chosen.k, true, pure);
                    }
                    let (a, b) = (fold(a, cx), fold(b, cx));
                    return leaf(E::Call(f, vec![c.e, a.e, b.e]));
                }
                (Some(n), 1) if n.starts_with("__builtin_") => {
                    let mut args = args;
                    let a = fold(args.remove(0), cx);
                    let (k, pure) = match (n, a.k) {
                        ("__builtin_constant_p", Some(_)) if a.pure => (c_const::int(1), true),
                        ("__builtin_constant_p", _) => (None, false),
                        (_, Some(k)) => (c_const::builtin(n, k), a.pure),
                        _ => (None, false),
                    };
                    return settle(E::Call(f, vec![a.e]), k, true, pure);
                }
                _ => {}
            }
            leaf(E::Call(
                Box::new(fold(*f, cx).e),
                args.into_iter().map(|a| fold(a, cx).e).collect(),
            ))
        }
        E::Index(b, i) => leaf(E::Index(Box::new(fold(*b, cx).e), Box::new(fold(*i, cx).e))),
        E::Member(b, op, name) => leaf(E::Member(Box::new(fold(*b, cx).e), op, name)),
        E::Postfix(b, op) => leaf(E::Postfix(Box::new(fold(*b, cx).e), op)),
        E::Assign(op, l, r) => leaf(E::Assign(
            op,
            Box::new(fold(*l, cx).e),
            Box::new(fold(*r, cx).e),
        )),
        E::Size(word, x) => {
            if word == "sizeof"
                && let Some(n) = size_of_expr(clone_e(&x), cx)
            {
                return settle(E::Size(word, x), Some(c_const::size_t(n)), true, true);
            }
            leaf(E::Size(word, Box::new(fold(*x, cx).e)))
        }
        E::Block(items) => {
            let inner = cx.inner(&items);
            leaf(E::Block(prune(
                items.into_iter().map(|i| fold_item(i, &inner)).collect(),
            )))
        }
        E::Raw(raw) => match sizeof_raw(raw, cx) {
            Some(k) => settle(E::Raw(raw), Some(k), true, true),
            None => leaf(E::Raw(raw)),
        },
        other @ (E::Lit(_) | E::Strs(_)) => leaf(other),
    }
}

fn clone_e<'a>(e: &E<'a>) -> E<'a> {
    match e {
        E::Atom(t) => E::Atom(t),
        E::Lit(t) => E::Lit(t.clone()),
        E::Block(_) => E::Raw(&[]),
        E::Strs(p) => E::Strs(p.clone()),
        E::Raw(r) => E::Raw(r),
        E::Cast(t, x) => E::Cast(t, Box::new(clone_e(x))),
        E::Prefix(o, x) => E::Prefix(o, Box::new(clone_e(x))),
        E::Size(w, x) => E::Size(w, Box::new(clone_e(x))),
        E::Postfix(x, o) => E::Postfix(Box::new(clone_e(x)), o),
        E::Call(f, a) => E::Call(Box::new(clone_e(f)), a.iter().map(clone_e).collect()),
        E::Index(b, i) => E::Index(Box::new(clone_e(b)), Box::new(clone_e(i))),
        E::Member(b, o, n) => E::Member(Box::new(clone_e(b)), o, n),
        E::Binary(o, p, l, r) => E::Binary(o, *p, Box::new(clone_e(l)), Box::new(clone_e(r))),
        E::Cond(c, t, f) => E::Cond(
            Box::new(clone_e(c)),
            Box::new(clone_e(t)),
            Box::new(clone_e(f)),
        ),
        E::Assign(o, l, r) => E::Assign(o, Box::new(clone_e(l)), Box::new(clone_e(r))),
        E::Comma(l, r) => E::Comma(Box::new(clone_e(l)), Box::new(clone_e(r))),
    }
}

fn print_item(item: &Item<'_>, out: &mut String) {
    let semi = match item {
        Item::Expr(e, semi) => {
            print(e, 0, out);
            semi
        }
        Item::Decl(spec, decls, semi) => {
            out.push_str(&render(spec));
            for (i, (d, init)) in decls.iter().enumerate() {
                out.push_str(if i == 0 { " " } else { ", " });
                out.push_str(&render(d));
                if let Some(e) = init {
                    out.push_str(" = ");
                    print(e, ASSIGN, out);
                }
            }
            semi
        }
        Item::Raw(toks, semi) => {
            out.push_str(&render(toks));
            semi
        }
    };
    if *semi {
        out.push(';');
    }
}

fn render(toks: &[T<'_>]) -> String {
    let mut out = String::new();
    let mut prev: Option<&str> = None;
    for t in toks {
        if let Some(p) = prev {
            let tight = matches!(p, "(" | "[") || matches!(t.text, ")" | "]" | "," | ";");
            if lex::avoid_paste(p, t.text) || (t.space && !tight) {
                out.push(' ');
            }
        }
        out.push_str(t.text);
        prev = Some(t.text);
    }
    out
}

fn family(op: &str) -> u8 {
    match op {
        "&&" | "||" => 1,
        "|" | "^" | "&" => 2,
        "<<" | ">>" => 3,
        "==" | "!=" | "<" | ">" | "<=" | ">=" => 4,
        _ => 5,
    }
}

fn confusing(parent: &str, child: &E<'_>) -> bool {
    let E::Binary(op, ..) = child else {
        return false;
    };
    if *op == parent {
        return false;
    }
    match family(parent) {
        1 => parent == "||" && *op == "&&",
        2 => true,
        3 => family(op) == 5,
        4 => family(op) == 4,
        _ => false,
    }
}

fn print(e: &E<'_>, min: u8, out: &mut String) {
    if e.prec() < min {
        out.push('(');
        print(e, 0, out);
        out.push(')');
        return;
    }
    match e {
        E::Atom(s) => out.push_str(s),
        E::Lit(s) => out.push_str(s),
        E::Block(items) => {
            out.push_str("({ ");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                print_item(item, out);
            }
            out.push_str(" })");
        }
        E::Strs(parts) => out.push_str(&parts.join(" ")),
        E::Raw(toks) => out.push_str(&render(toks)),
        E::Cast(ty, operand) => {
            out.push('(');
            out.push_str(&render(ty));
            out.push(')');
            print(operand, UNARY, out);
        }
        E::Prefix(op, operand) => {
            out.push_str(op);
            let mut inner = String::new();
            print(operand, UNARY, &mut inner);
            let clash = matches!(*op, "+" | "-" | "++" | "--" | "&") && inner.starts_with(&op[..1]);
            if clash {
                out.push(' ');
            }
            out.push_str(&inner);
        }
        E::Size(word, operand) => {
            out.push_str(word);
            out.push(' ');
            print(operand, UNARY, out);
        }
        E::Postfix(base, op) => {
            print(base, POSTFIX, out);
            out.push_str(op);
        }
        E::Call(callee, args) => {
            print(callee, POSTFIX, out);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print(a, ASSIGN, out);
            }
            out.push(')');
        }
        E::Index(base, idx) => {
            print(base, POSTFIX, out);
            out.push('[');
            print(idx, 0, out);
            out.push(']');
        }
        E::Member(base, op, name) => {
            print(base, POSTFIX, out);
            out.push_str(op);
            out.push_str(name);
        }
        E::Binary(op, p, lhs, rhs) => {
            side(op, lhs, *p, out);
            out.push(' ');
            out.push_str(op);
            out.push(' ');
            side(op, rhs, *p + 1, out);
        }
        E::Cond(c, t, f) => {
            print(c, LOR, out);
            out.push_str(" ? ");
            print(
                t,
                if matches!(**t, E::Cond(..)) {
                    PRIMARY
                } else {
                    COMMA
                },
                out,
            );
            out.push_str(" : ");
            print(f, COND, out);
        }
        E::Assign(op, lhs, rhs) => {
            print(lhs, UNARY, out);
            out.push(' ');
            out.push_str(op);
            out.push(' ');
            print(rhs, ASSIGN, out);
        }
        E::Comma(lhs, rhs) => {
            print(lhs, COMMA, out);
            out.push_str(", ");
            print(rhs, ASSIGN, out);
        }
    }
}

fn side(parent: &str, child: &E<'_>, min: u8, out: &mut String) {
    if child.prec() >= min && confusing(parent, child) {
        out.push('(');
        print(child, 0, out);
        out.push(')');
    } else {
        print(child, min, out);
    }
}

fn tokens<'a>(toks: &[Token], it: &'a Interner) -> Option<Vec<T<'a>>> {
    let out: Vec<T<'a>> = toks
        .iter()
        .filter(|t| t.kind != TokenKind::Placemarker)
        .map(|t| T {
            kind: t.kind,
            text: it.get(t.sym),
            sym: t.sym,
            space: t.space(),
        })
        .collect();
    out.iter()
        .all(|t| {
            matches!(
                t.kind,
                TokenKind::Ident
                    | TokenKind::Number
                    | TokenKind::Char
                    | TokenKind::Str
                    | TokenKind::Punct
            )
        })
        .then_some(out)
}

pub fn simplify(
    toks: &[Token],
    it: &Interner,
    known: &FxHashSet<Sym>,
    env: &dyn TypeEnv,
) -> Option<String> {
    analyze(toks, it, known, env).map(|(text, _)| text)
}

pub fn evaluate(
    toks: &[Token],
    it: &Interner,
    known: &FxHashSet<Sym>,
    env: &dyn TypeEnv,
) -> Option<Konst> {
    analyze(toks, it, known, env).and_then(|(_, k)| k)
}

fn analyze(
    toks: &[Token],
    it: &Interner,
    known: &FxHashSet<Sym>,
    env: &dyn TypeEnv,
) -> Option<(String, Option<Konst>)> {
    if toks.is_empty() || toks.len() > MAX_TOKENS {
        return None;
    }
    let ts = tokens(toks, it)?;
    let mut body: &[T<'_>] = &ts;
    let mut stripped = None;
    let mut scope = Scope {
        vars: known.clone(),
        types: FxHashSet::default(),
    };
    for w in ts.windows(3) {
        if !(w[0].punct("(") && w[1].word() && w[2].punct(")"))
            || known.contains(&w[1].sym)
            || is_keyword(w[1].text)
            || scope.vars.contains(&w[1].sym)
            || scope.types.contains(&w[1].sym)
        {
            continue;
        }
        if env.enumerator(w[1].text).is_some() {
            scope.vars.insert(w[1].sym);
        } else if env.is_typedef(w[1].text) {
            scope.types.insert(w[1].sym);
        }
    }
    let cx = Cx {
        env,
        bound: known.iter().map(|s| it.get(*s)).collect(),
    };
    loop {
        match Parser::whole(body, &scope, 0) {
            Ok(E::Raw(raw)) if raw.len() == body.len() && raw[0].punct("(") => {
                let inner = &body[1..body.len() - 1];
                if inner.is_empty() || inner[0].punct("{") {
                    return Some((render(body), None));
                }
                stripped = Some(render(inner));
                body = inner;
            }
            Ok(e) => {
                let folded = fold(e, &cx);
                let k = folded.k;
                let e = folded.e;
                let mut out = String::new();
                print(
                    &e,
                    if matches!(e, E::Comma(..)) {
                        PRIMARY
                    } else {
                        0
                    },
                    &mut out,
                );
                return Some((out, k));
            }
            Err(Fail) => return stripped.map(|s| (s, None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str, known: &[&str]) -> Option<String> {
        let mut it = Interner::new();
        let toks = lex::lex_all(src.as_bytes(), false, &mut it);
        let known: FxHashSet<Sym> = known.iter().map(|k| it.intern(k)).collect();
        simplify(&toks, &it, &known, &c_type::Basic { lp64: true })
    }

    #[test]
    fn drops_only_provably_redundant_parentheses() {
        let cases: &[(&str, &[&str], Option<&str>)] = &[
            (
                "((void *)((unsigned long)(address)+((unsigned long)page_offset_base)))",
                &["address"],
                Some("(void *)((unsigned long)address + (unsigned long)page_offset_base)"),
            ),
            (
                "((void *)((unsigned long)(address)+((unsigned long)page_offset_base)))",
                &[],
                Some("(void *)((unsigned long)(address)+((unsigned long)page_offset_base))"),
            ),
            ("((1UL) << 12)", &[], Some("0x1000UL")),
            (
                "__builtin_expect(!!(x), 1)",
                &[],
                Some("__builtin_expect(!!x, 1)"),
            ),
            ("((x) - (y))", &[], Some("(x) - (y)")),
            ("((x) - (y))", &["x", "y"], Some("x - y")),
            ("(a + (b * c))", &[], Some("a + b * c")),
            ("((a + b) * c)", &[], Some("(a + b) * c")),
            ("(a - (b - c))", &[], Some("a - (b - c)")),
            ("((a & b) == c)", &[], Some("(a & b) == c")),
            ("((a == b) & c)", &[], Some("(a == b) & c")),
            ("((a << 2) + 1)", &[], Some("(a << 2) + 1")),
            ("(a || (b && c))", &[], Some("a || (b && c)")),
            ("(- (-1))", &[], Some("- -1")),
            ("((u32)(v))", &[], Some("(u32)(v)")),
            ("((u32)v)", &[], Some("(u32)v")),
            ("((void (*)(int))fn)", &[], Some("(void (*)(int))fn")),
            ("(a == b && c)", &[], Some("a == b && c")),
            ("((a + 1) << 2)", &[], Some("(a + 1) << 2")),
            ("((struct page *)(p))", &[], Some("(struct page *)p")),
            (
                "((struct page *)(p) + 1)",
                &[],
                Some("(struct page *)(p) + 1"),
            ),
            ("((struct page *)(p))", &["p"], Some("(struct page *)p")),
            ("(sizeof(x) * 8)", &[], Some("sizeof(x) * 8")),
            ("(f(x) + 1)", &[], Some("f(x) + 1")),
            ("((f)(x))", &[], Some("(f)(x)")),
            ("(f)(x)", &[], None),
            (
                "({ int __x = (y); __x; })",
                &[],
                Some("({ int __x = y; __x; })"),
            ),
            ("((a, b))", &[], Some("(a, b)")),
            ("(p->q[(i)])", &[], Some("p->q[i]")),
            ("((t) & 3)", &[], Some("(t) & 3")),
            ("((long)(t) & 3)", &[], Some("(long)(t) & 3")),
            ("(c ? (a) : (b))", &[], Some("c ? a : b")),
            ("(p ? (q ? 1 : 2) : 3)", &[], Some("p ? (q ? 1 : 2) : 3")),
            ("(p ? 1 : (q ? 2 : 3))", &[], Some("p ? 1 : q ? 2 : 3")),
            ("((x)) + 1", &[], Some("x + 1")),
            (
                "__virt_to_phys((unsigned long)((unsigned long)address))",
                &["address"],
                Some("__virt_to_phys((unsigned long)address)"),
            ),
            (
                "(unsigned long)(unsigned long)(unsigned long)(x)",
                &[],
                Some("(unsigned long)x"),
            ),
            ("(unsigned long)(unsigned long)(x) - 1", &[], None),
            (
                "(unsigned long)(unsigned long)(x) - 1",
                &["x"],
                Some("(unsigned long)x - 1"),
            ),
            ("(int *)(int *)p", &[], Some("(int *)p")),
            (
                "(unsigned long)(unsigned long)1",
                &[],
                Some("(unsigned long)1"),
            ),
            (
                "(long)(unsigned long)x",
                &[],
                Some("(long)(unsigned long)x"),
            ),
            ("(T)(T)x", &[], None),
            (
                "(typeof(x))(typeof(x))y",
                &[],
                Some("(typeof(x))(typeof(x))y"),
            ),
        ];
        for (src, known, want) in cases {
            assert_eq!(run(src, known).as_deref(), *want, "{src} with {known:?}");
        }
    }

    #[test]
    fn statement_expressions_fold_constants_and_drop_dead_statements() {
        let cases: &[(&str, &[&str], Option<&str>)] = &[
            (
                "({ phys_addr_t __x = (phys_addr_t)((unsigned long)(x)); __x & (1UL << ((39) - 1)) ? (__x & ~((0xffffffffffffffffUL) << ((39) - 1))) + ({ ((void)(sizeof(( long)(memstart_addr & 1)))); memstart_addr; }) : (__x - kimage_voffset); })",
                &["x"],
                Some(
                    "({ phys_addr_t __x = (phys_addr_t)(unsigned long)x; __x & 0x4000000000UL ? (__x & 0x3fffffffffUL) + ({ memstart_addr; }) : __x - kimage_voffset; })",
                ),
            ),
            (
                "({ int T = 1; (T) - 1; })",
                &[],
                Some("({ int T = 1; T - 1; })"),
            ),
            (
                "({ T t = (T)(a); (t) & 3; })",
                &[],
                Some("({ T t = (T)a; t & 3; })"),
            ),
            (
                "({ typedef int U; (U)(a); })",
                &[],
                Some("({ typedef int U; (U)(a); })"),
            ),
            ("({ (void)0; ; x; })", &[], Some("({ x; })")),
            ("({ (void)x; y; })", &[], Some("({ (void)x; y; })")),
            (
                "({ (void)sizeof(a); b; })",
                &[],
                Some("({ (void)sizeof(a); b; })"),
            ),
            (
                "({ (void)sizeof(int[n++]); b; })",
                &[],
                Some("({ (void)sizeof(int[n++]); b; })"),
            ),
            ("({ (void)sizeof(long *); b; })", &[], Some("({ b; })")),
            ("({ T *p; (p)(1); })", &[], Some("({ T *p; (p)(1); })")),
            (
                "({ T *p = q; (p) - 1; })",
                &[],
                Some("({ T *p = q; p - 1; })"),
            ),
            (
                "({ unsigned long f = (x), g = (f); g; })",
                &[],
                Some("({ unsigned long f = x, g = f; g; })"),
            ),
            (
                "({ if (a) b; else c; (d); })",
                &[],
                Some("({ if (a) b; else c; d; })"),
            ),
            ("((1 + 2) * a)", &[], Some("3 * a")),
            ("(-1UL)", &[], Some("-1UL")),
            ("(~0UL)", &[], Some("~0UL")),
            ("((unsigned long)5)", &[], Some("(unsigned long)5")),
            ("((unsigned long)(1 + 2))", &[], Some("3UL")),
            ("(1 << 31)", &[], Some("1 << 31")),
            ("(0x10 | 0x1)", &[], Some("0x11")),
            ("((int)4294967296 + 1)", &[], Some("(int)4294967296 + 1")),
            ("(2147483647 + 1)", &[], Some("2147483647 + 1")),
            ("((1, 2) + 3)", &[], Some("(1, 2) + 3")),
            ("(a - (2 - 5))", &[], Some("a - -3")),
            ("sizeof(( long)(x))", &[], Some("8UL")),
            ("(sizeof(unsigned long) * 8)", &[], Some("64UL")),
            ("sizeof(int[4]) + sizeof(char *)", &[], Some("24UL")),
            ("sizeof(int[n])", &[], Some("sizeof(int[n])")),
            ("__builtin_constant_p(4096UL)", &[], Some("1")),
            (
                "__builtin_constant_p(x)",
                &[],
                Some("__builtin_constant_p(x)"),
            ),
            ("__builtin_choose_expr(1, a, b)", &[], Some("a")),
            (
                "__builtin_choose_expr(sizeof(int) == 4, 1UL, 2)",
                &[],
                Some("1UL"),
            ),
            ("((typeof(1UL))0x40000000 - 1)", &[], Some("0x3fffffffUL")),
            (
                "__builtin_constant_p(63UL) ? 63 - __builtin_clzll(63UL) : f(63UL)",
                &[],
                Some("1 ? 5 : f(63UL)"),
            ),
            (
                "__builtin_constant_p(63UL) ? 63 - __builtin_clzll(63UL) : (int)f(63UL)",
                &[],
                Some("5"),
            ),
            ("sizeof((sizeof(int)) - 1) <= 4 ? 1 : 2", &[], Some("2")),
            ("1 ? (int)x : (int)y", &[], Some("(int)x")),
            ("0 ? (int)x : (long)y", &[], Some("(long)y")),
            ("1 ? (int)x : (long)y", &[], Some("1 ? (int)x : (long)y")),
            ("1 ? 5 : (long)y", &[], Some("5L")),
            ("__builtin_ffsll(0x100ULL) + 0", &[], Some("9")),
        ];
        for (src, known, want) in cases {
            assert_eq!(run(src, known).as_deref(), *want, "{src} with {known:?}");
        }
    }
}
