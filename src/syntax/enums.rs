use super::lex::{is_ident_cont, is_ident_start};
use super::scan::DirLine;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumDecl {
    pub keyword: u32,
    pub tag: Option<(u32, u32)>,
    pub open: u32,
    pub close: u32,
    pub members: Vec<Member>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub start: u32,
    pub end: u32,
    pub has_init: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tok {
    Ident(u32, u32),
    Punct(u8),
    Other,
}

struct Walker<'a> {
    src: &'a [u8],
    skip: &'a [(u32, u32)],
    pos: usize,
    peeked: Option<(usize, Tok, usize)>,
}

impl<'a> Walker<'a> {
    fn skip_range_at(&self, p: usize) -> Option<usize> {
        let idx = self.skip.partition_point(|&(a, _)| (a as usize) <= p);
        let &(a, b) = self.skip.get(idx.checked_sub(1)?)?;
        ((a as usize) <= p && p < b as usize).then_some(b as usize)
    }

    fn raw_next(&self, mut p: usize) -> Option<(usize, Tok, usize)> {
        let s = self.src;
        loop {
            if p >= s.len() {
                return None;
            }
            if let Some(end) = self.skip_range_at(p) {
                p = end.max(p + 1);
                continue;
            }
            match s[p] {
                b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | b'\\' => p += 1,
                c if is_ident_start(c, false) => {
                    let mut e = p + 1;
                    while e < s.len() && is_ident_cont(s[e], false) {
                        e += 1;
                    }
                    return Some((p, Tok::Ident(p as u32, e as u32), e));
                }
                c if c.is_ascii_digit() => {
                    let mut e = p + 1;
                    while e < s.len()
                        && (s[e].is_ascii_alphanumeric() || s[e] == b'_' || s[e] == b'.')
                    {
                        e += 1;
                    }
                    return Some((p, Tok::Other, e));
                }
                c @ (b'{' | b'}' | b'(' | b')' | b'[' | b']' | b',' | b'=' | b';' | b':') => {
                    if c == b'=' && s.get(p + 1) == Some(&b'=') {
                        return Some((p, Tok::Other, p + 2));
                    }
                    return Some((p, Tok::Punct(c), p + 1));
                }
                _ => return Some((p, Tok::Other, p + 1)),
            }
        }
    }

    fn next(&mut self) -> Option<(usize, Tok)> {
        if let Some((at, t, end)) = self.peeked.take() {
            self.pos = end;
            return Some((at, t));
        }
        let (at, t, end) = self.raw_next(self.pos)?;
        self.pos = end;
        Some((at, t))
    }

    fn peek(&mut self) -> Option<Tok> {
        if self.peeked.is_none() {
            self.peeked = self.raw_next(self.pos);
        }
        self.peeked.map(|(_, t, _)| t)
    }

    fn skip_group(&mut self, open: u8, close: u8) -> Option<()> {
        let mut depth = 1u32;
        while depth > 0 {
            match self.next()?.1 {
                Tok::Punct(c) if c == open => depth += 1,
                Tok::Punct(c) if c == close => depth -= 1,
                _ => {}
            }
        }
        Some(())
    }
}

pub fn merged_skips(dirs: &[DirLine], skipped: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> = Vec::with_capacity(dirs.len() + skipped.len());
    v.extend(dirs.iter().map(|d| (d.off, d.end)));
    v.extend_from_slice(skipped);
    v.sort_unstable();
    v
}

fn is_word_at(src: &[u8], at: usize, len: usize) -> bool {
    let before = at == 0 || !is_ident_cont(src[at - 1], false);
    let after = src.get(at + len).is_none_or(|&c| !is_ident_cont(c, false));
    before && after
}

pub fn find_enums(src: &[u8], skips: &[(u32, u32)]) -> Vec<EnumDecl> {
    let mut out = Vec::new();
    let probe = Walker {
        src,
        skip: skips,
        pos: 0,
        peeked: None,
    };
    for at in memchr::memmem::find_iter(src, b"enum") {
        if !is_word_at(src, at, 4) || probe.skip_range_at(at).is_some() {
            continue;
        }
        let mut w = Walker {
            src,
            skip: skips,
            pos: at + 4,
            peeked: None,
        };
        if let Some(d) = parse_decl(&mut w, at as u32) {
            out.push(d);
        }
    }
    out
}

fn parse_decl(w: &mut Walker<'_>, keyword: u32) -> Option<EnumDecl> {
    let mut tag = None;
    let open = loop {
        match w.next()? {
            (_, Tok::Ident(a, b)) => {
                let word = &w.src[a as usize..b as usize];
                if word == b"__attribute__" || word == b"__aligned" {
                    if w.peek() == Some(Tok::Punct(b'(')) {
                        w.next()?;
                        w.skip_group(b'(', b')')?;
                    }
                } else if tag.is_none() {
                    tag = Some((a, b));
                }
            }
            (at, Tok::Punct(b'{')) => break at as u32,
            (_, Tok::Punct(b':')) => {}
            _ => return None,
        }
    };
    let mut members = Vec::new();
    let mut expect_name = true;
    loop {
        let (at, t) = w.next()?;
        match t {
            Tok::Punct(b'}') => {
                return Some(EnumDecl {
                    keyword,
                    tag,
                    open,
                    close: at as u32,
                    members,
                });
            }
            Tok::Punct(b'{') => {
                w.skip_group(b'{', b'}')?;
                expect_name = false;
            }
            Tok::Punct(b'(') => {
                w.skip_group(b'(', b')')?;
                expect_name = false;
            }
            Tok::Punct(b'[') => {
                w.skip_group(b'[', b']')?;
                expect_name = false;
            }
            Tok::Punct(b',') => expect_name = true,
            Tok::Punct(b';') => return None,
            Tok::Ident(a, b) if expect_name => match w.peek() {
                Some(Tok::Punct(b'(')) => {
                    w.next()?;
                    w.skip_group(b'(', b')')?;
                }
                Some(Tok::Punct(b'=')) => {
                    members.push(Member {
                        start: a,
                        end: b,
                        has_init: true,
                    });
                    expect_name = false;
                }
                Some(Tok::Punct(b',' | b'}')) => {
                    members.push(Member {
                        start: a,
                        end: b,
                        has_init: false,
                    });
                    expect_name = false;
                }
                _ => expect_name = false,
            },
            _ => expect_name = false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::scan::scan_full;

    fn enums(src: &str) -> Vec<(Option<String>, Vec<String>)> {
        let b = src.as_bytes();
        let mut dirs = Vec::new();
        let mut skipped = Vec::new();
        scan_full(b, &mut dirs, Some(&mut skipped));
        let skips = merged_skips(&dirs, &skipped);
        find_enums(b, &skips)
            .into_iter()
            .map(|d| {
                let tag = d.tag.map(|(a, e)| src[a as usize..e as usize].to_string());
                let names = d
                    .members
                    .iter()
                    .map(|m| src[m.start as usize..m.end as usize].to_string())
                    .collect();
                (tag, names)
            })
            .collect()
    }

    #[test]
    fn conditional_members() {
        let src = "enum fixed_addresses {\n#ifdef CONFIG_X86_32\n\tFIX_HOLE,\n#endif\n\tFIX_APIC_BASE = 3, /* c, d */\n\t__end_of_fixed_addresses\n};\n";
        assert_eq!(
            enums(src),
            vec![(
                Some("fixed_addresses".into()),
                vec![
                    "FIX_HOLE".into(),
                    "FIX_APIC_BASE".into(),
                    "__end_of_fixed_addresses".into()
                ]
            )]
        );
    }

    #[test]
    fn ignores_uses_comments_and_macros() {
        let src = "/* enum fake { A }; */\nenum foo x;\n#define E enum bar { B }\nstatic enum foo f(void);\nenum { FOO_LIST(X) Y = (1 << 2), Z };\n";
        assert_eq!(enums(src), vec![(None, vec!["Y".into(), "Z".into()])]);
    }

    #[test]
    fn initializer_with_nested_parens_and_commas() {
        let src = "enum e { A = f(1, 2), B = (3, 4), C };";
        assert_eq!(
            enums(src),
            vec![(Some("e".into()), vec!["A".into(), "B".into(), "C".into()])]
        );
    }
}
