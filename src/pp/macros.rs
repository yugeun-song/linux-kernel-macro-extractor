use crate::intern::{Interner, Sym, sym};
use crate::syntax::lex::{self, F_SPACE, Token, TokenKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SrcFile(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    pub file: SrcFile,
    pub line: u32,
    pub off: u32,
    pub end: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variadic {
    No,
    Anonymous,
    Named,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Params {
    Object,
    Function { names: Vec<Sym>, variadic: Variadic },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    File,
    Line,
    Counter,
    IncludeLevel,
    BaseFile,
    Date,
    Time,
    Timestamp,
}

impl Builtin {
    pub const ALL: [(Sym, Builtin); 8] = [
        (sym::FILE_, Builtin::File),
        (sym::LINE_, Builtin::Line),
        (sym::COUNTER_, Builtin::Counter),
        (sym::INCLUDE_LEVEL_, Builtin::IncludeLevel),
        (sym::BASE_FILE_, Builtin::BaseFile),
        (sym::DATE_, Builtin::Date),
        (sym::TIME_, Builtin::Time),
        (sym::TIMESTAMP_, Builtin::Timestamp),
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Source,
    Predefined,
    CommandLine,
    Synthesized,
    Builtin,
}

#[derive(Clone, Debug)]
pub struct MacroDef {
    pub name: Sym,
    pub site: Option<Site>,
    pub origin: Origin,
    pub params: Params,
    pub body: Vec<Token>,
    pub builtin: Option<Builtin>,
}

impl MacroDef {
    pub fn builtin(name: Sym, b: Builtin) -> MacroDef {
        MacroDef {
            name,
            site: None,
            origin: Origin::Builtin,
            params: Params::Object,
            body: Vec::new(),
            builtin: Some(b),
        }
    }

    pub fn is_function(&self) -> bool {
        matches!(self.params, Params::Function { .. })
    }

    pub fn slots(&self) -> usize {
        match &self.params {
            Params::Object => 0,
            Params::Function { names, variadic } => {
                names.len() + usize::from(*variadic == Variadic::Anonymous)
            }
        }
    }

    pub fn variadic_slot(&self) -> Option<usize> {
        match &self.params {
            Params::Function {
                names,
                variadic: Variadic::Anonymous,
            } => Some(names.len()),
            Params::Function {
                names,
                variadic: Variadic::Named,
            } => names.len().checked_sub(1),
            _ => None,
        }
    }

    pub fn param_index(&self, s: Sym) -> Option<usize> {
        let Params::Function { names, variadic } = &self.params else {
            return None;
        };
        if s == sym::VA_ARGS && *variadic == Variadic::Anonymous {
            return Some(names.len());
        }
        names.iter().position(|&n| n == s)
    }
}

pub struct Parsed {
    pub name: Sym,
    pub params: Params,
    pub body: Vec<Token>,
}

pub fn parse_define(text: &[u8], asm: bool, it: &mut Interner) -> Option<Parsed> {
    let toks = lex::lex_all(text, asm, it);
    let mut i = 0;
    if toks.first()?.is_punct(sym::HASH) {
        i += 1;
    }
    let kw = toks.get(i)?;
    if kw.kind != TokenKind::Ident || it.get(kw.sym) != "define" {
        return None;
    }
    i += 1;
    parse_after_keyword(&toks[i..])
}

pub fn parse_after_keyword(toks: &[Token]) -> Option<Parsed> {
    let name_tok = toks.first()?;
    if name_tok.kind != TokenKind::Ident {
        return None;
    }
    let mut i = 1;
    let params = match toks.get(1) {
        Some(t) if t.is_punct(sym::LPAREN) && !t.space() => {
            i = 2;
            let mut names = Vec::new();
            let mut variadic = Variadic::No;
            loop {
                let t = toks.get(i)?;
                if t.is_punct(sym::RPAREN) {
                    i += 1;
                    break;
                }
                if t.is_punct(sym::ELLIPSIS) {
                    variadic = Variadic::Anonymous;
                    i += 1;
                    continue;
                }
                if t.kind == TokenKind::Ident {
                    names.push(t.sym);
                    if toks.get(i + 1).is_some_and(|n| n.is_punct(sym::ELLIPSIS)) {
                        variadic = Variadic::Named;
                        i += 1;
                    }
                    i += 1;
                    match toks.get(i) {
                        Some(c) if c.is_punct(sym::COMMA) => i += 1,
                        Some(c) if c.is_punct(sym::RPAREN) => {}
                        _ => return None,
                    }
                    continue;
                }
                if t.is_punct(sym::COMMA) {
                    i += 1;
                    continue;
                }
                return None;
            }
            Params::Function { names, variadic }
        }
        _ => Params::Object,
    };
    let mut body: Vec<Token> = toks.get(i..).unwrap_or(&[]).to_vec();
    if let Some(first) = body.first_mut() {
        first.flags &= !F_SPACE;
    }
    Some(Parsed {
        name: name_tok.sym,
        params,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_and_function() {
        let mut it = Interner::new();
        let p = parse_define(
            b"#define PAGE_SIZE (_AC(1,UL) << PAGE_SHIFT)",
            false,
            &mut it,
        )
        .unwrap();
        assert_eq!(it.get(p.name), "PAGE_SIZE");
        assert_eq!(p.params, Params::Object);
        assert_eq!(lex::render(&p.body, &it), "(_AC(1,UL) << PAGE_SHIFT)");
        let f = parse_define(b"#  define __AC(X,Y)\t(X##Y)", false, &mut it).unwrap();
        let x = it.lookup("X").unwrap();
        let y = it.lookup("Y").unwrap();
        assert_eq!(
            f.params,
            Params::Function {
                names: vec![x, y],
                variadic: Variadic::No
            }
        );
        let o = parse_define(b"#define NOT_FN (x)", false, &mut it).unwrap();
        assert_eq!(o.params, Params::Object);
    }

    #[test]
    fn variadics() {
        let mut it = Interner::new();
        let a = parse_define(
            b"#define pr(fmt, ...) printk(fmt, ##__VA_ARGS__)",
            false,
            &mut it,
        )
        .unwrap();
        assert!(
            matches!(a.params, Params::Function { variadic: Variadic::Anonymous, ref names } if names.len() == 1)
        );
        let n = parse_define(b"#define dbg(args...) foo(args)", false, &mut it).unwrap();
        assert!(
            matches!(n.params, Params::Function { variadic: Variadic::Named, ref names } if names.len() == 1)
        );
        let z = parse_define(b"#define nothing() 1", false, &mut it).unwrap();
        assert!(matches!(z.params, Params::Function { ref names, .. } if names.is_empty()));
    }

    #[test]
    fn multiline_with_comment() {
        let mut it = Interner::new();
        let p = parse_define(
            b"#define M(a) \\\n  do { /* c */ (a)++; } while (0)",
            false,
            &mut it,
        )
        .unwrap();
        assert_eq!(lex::render(&p.body, &it), "do { (a)++; } while (0)");
    }
}
