use std::borrow::Cow;

use crate::intern::{Interner, Sym, sym};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum TokenKind {
    Ident,
    Number,
    Char,
    Str,
    Punct,
    Other,
    Placemarker,
}

pub const F_SPACE: u8 = 1;
pub const F_NOEXP: u8 = 2;
pub const F_BOL: u8 = 4;

#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub kind: TokenKind,
    pub flags: u8,
    pub sym: Sym,
    pub hs: u32,
    pub origin: u32,
}

impl Token {
    #[inline]
    pub fn new(kind: TokenKind, sym: Sym, flags: u8) -> Self {
        Token {
            kind,
            flags,
            sym,
            hs: 0,
            origin: 0,
        }
    }

    #[inline]
    pub fn placemarker() -> Self {
        Token::new(TokenKind::Placemarker, sym::EMPTY, 0)
    }

    #[inline]
    pub fn is_punct(&self, s: Sym) -> bool {
        self.kind == TokenKind::Punct && self.sym == s
    }

    #[inline]
    pub fn space(&self) -> bool {
        self.flags & F_SPACE != 0
    }

    #[inline]
    pub fn with_space(mut self, on: bool) -> Self {
        if on {
            self.flags |= F_SPACE;
        } else {
            self.flags &= !F_SPACE;
        }
        self
    }
}

pub fn splice(src: &[u8]) -> Cow<'_, [u8]> {
    let mut i = match memchr::memchr(b'\\', src) {
        None => return Cow::Borrowed(src),
        Some(i) => i,
    };
    let has_splice = loop {
        let rest = &src[i + 1..];
        if rest.first() == Some(&b'\n')
            || (rest.first() == Some(&b'\r') && rest.get(1) == Some(&b'\n'))
        {
            break true;
        }
        match memchr::memchr(b'\\', &src[i + 1..]) {
            None => break false,
            Some(j) => i = i + 1 + j,
        }
    };
    if !has_splice {
        return Cow::Borrowed(src);
    }
    let mut out = Vec::with_capacity(src.len());
    let mut k = 0;
    while k < src.len() {
        let c = src[k];
        if c == b'\\' {
            if src.get(k + 1) == Some(&b'\n') {
                k += 2;
                continue;
            }
            if src.get(k + 1) == Some(&b'\r') && src.get(k + 2) == Some(&b'\n') {
                k += 3;
                continue;
            }
        }
        out.push(c);
        k += 1;
    }
    Cow::Owned(out)
}

#[inline]
pub fn is_ident_start(c: u8, asm: bool) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || (c == b'$' && !asm)
}

#[inline]
pub fn is_ident_cont(c: u8, asm: bool) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || (c == b'$' && !asm)
}

fn punct3(a: u8, b: u8, c: u8) -> Option<Option<Sym>> {
    match (a, b, c) {
        (b'<', b'<', b'=') | (b'>', b'>', b'=') => Some(None),
        (b'.', b'.', b'.') => Some(Some(sym::ELLIPSIS)),
        _ => None,
    }
}

fn punct2(a: u8, b: u8) -> Option<Option<Sym>> {
    let s = match (a, b) {
        (b'#', b'#') => Some(sym::HASHHASH),
        (b'<', b'<') => Some(sym::LSHIFT),
        (b'>', b'>') => Some(sym::RSHIFT),
        (b'<', b'=') => Some(sym::LE),
        (b'>', b'=') => Some(sym::GE),
        (b'=', b'=') => Some(sym::EQEQ),
        (b'!', b'=') => Some(sym::NE),
        (b'&', b'&') => Some(sym::ANDAND),
        (b'|', b'|') => Some(sym::OROR),
        (b'-', b'>') => Some(sym::ARROW),
        (b'+', b'+')
        | (b'-', b'-')
        | (b'*', b'=')
        | (b'/', b'=')
        | (b'%', b'=')
        | (b'+', b'=')
        | (b'-', b'=')
        | (b'&', b'=')
        | (b'^', b'=')
        | (b'|', b'=')
        | (b':', b':') => None,
        _ => return None,
    };
    Some(s)
}

fn punct1(a: u8) -> Option<Sym> {
    Some(match a {
        b'#' => sym::HASH,
        b'(' => sym::LPAREN,
        b')' => sym::RPAREN,
        b',' => sym::COMMA,
        b'<' => sym::LT,
        b'>' => sym::GT,
        b'+' => sym::PLUS,
        b'-' => sym::MINUS,
        b'*' => sym::STAR,
        b'/' => sym::SLASH,
        b'%' => sym::PERCENT,
        b'&' => sym::AMP,
        b'|' => sym::PIPE,
        b'^' => sym::CARET,
        b'~' => sym::TILDE,
        b'!' => sym::BANG,
        b'?' => sym::QUESTION,
        b':' => sym::COLON,
        b';' => sym::SEMI,
        b'=' => sym::EQ,
        b'[' => sym::LBRACKET,
        b']' => sym::RBRACKET,
        b'{' => sym::LBRACE,
        b'}' => sym::RBRACE,
        b'.' => sym::DOT,
        _ => return None,
    })
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    asm: bool,
    pending: u8,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a [u8], asm: bool) -> Self {
        Lexer {
            src,
            pos: 0,
            asm,
            pending: 0,
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn rest(&self) -> &'a [u8] {
        &self.src[self.pos..]
    }

    pub fn skip_ws(&mut self) -> u8 {
        let s = self.src;
        let mut flags = 0u8;
        while self.pos < s.len() {
            match s[self.pos] {
                b' ' | b'\t' | b'\r' | 0x0b | 0x0c => {
                    flags |= F_SPACE;
                    self.pos += 1;
                }
                b'\n' => {
                    flags |= F_SPACE | F_BOL;
                    self.pos += 1;
                }
                b'/' if s.get(self.pos + 1) == Some(&b'*') => {
                    flags |= F_SPACE;
                    match memchr::memmem::find(&s[self.pos + 2..], b"*/") {
                        Some(j) => self.pos += 2 + j + 2,
                        None => self.pos = s.len(),
                    }
                }
                b'/' if s.get(self.pos + 1) == Some(&b'/') => {
                    flags |= F_SPACE;
                    match memchr::memchr(b'\n', &s[self.pos..]) {
                        Some(j) => self.pos += j,
                        None => self.pos = s.len(),
                    }
                }
                _ => break,
            }
        }
        flags
    }

    fn literal_end(&self, start: usize, q: u8) -> Option<usize> {
        let s = self.src;
        let mut i = start;
        while i < s.len() {
            let c = s[i];
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                return Some(i + 1);
            }
            if c == b'\n' {
                return None;
            }
            i += 1;
        }
        None
    }

    pub fn next(&mut self, it: &mut Interner) -> Option<Token> {
        let flags = self.skip_ws() | std::mem::take(&mut self.pending);
        let s = self.src;
        if self.pos >= s.len() {
            self.pending = flags;
            return None;
        }
        let start = self.pos;
        let c = s[start];
        if is_ident_start(c, self.asm) {
            let mut i = start + 1;
            while i < s.len() && is_ident_cont(s[i], self.asm) {
                i += 1;
            }
            let word = &s[start..i];
            if let Some(&q) = s.get(i) {
                let is_prefix = match word {
                    b"L" | b"u" | b"U" => q == b'"' || q == b'\'',
                    b"u8" => q == b'"' || q == b'\'',
                    _ => false,
                };
                if is_prefix && let Some(end) = self.literal_end(i + 1, q) {
                    self.pos = end;
                    let kind = if q == b'"' {
                        TokenKind::Str
                    } else {
                        TokenKind::Char
                    };
                    return Some(Token::new(kind, it.intern_bytes(&s[start..end]), flags));
                }
            }
            self.pos = i;
            return Some(Token::new(TokenKind::Ident, it.intern_bytes(word), flags));
        }
        if c.is_ascii_digit() || (c == b'.' && s.get(start + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let mut i = start + 1;
            while i < s.len() {
                let d = s[i];
                let sign =
                    matches!(d, b'+' | b'-') && matches!(s[i - 1], b'e' | b'E' | b'p' | b'P');
                if !(sign || d.is_ascii_alphanumeric() || matches!(d, b'_' | b'.')) {
                    break;
                }
                i += 1;
            }
            self.pos = i;
            return Some(Token::new(
                TokenKind::Number,
                it.intern_bytes(&s[start..i]),
                flags,
            ));
        }
        if c == b'"' || c == b'\'' {
            if let Some(end) = self.literal_end(start + 1, c) {
                self.pos = end;
                let kind = if c == b'"' {
                    TokenKind::Str
                } else {
                    TokenKind::Char
                };
                return Some(Token::new(kind, it.intern_bytes(&s[start..end]), flags));
            }
            self.pos = start + 1;
            return Some(Token::new(
                TokenKind::Other,
                it.intern_bytes(&s[start..start + 1]),
                flags,
            ));
        }
        if start + 2 < s.len()
            && let Some(p) = punct3(c, s[start + 1], s[start + 2])
        {
            self.pos = start + 3;
            let sy = match p {
                Some(p) => p,
                None => it.intern_bytes(&s[start..start + 3]),
            };
            return Some(Token::new(TokenKind::Punct, sy, flags));
        }
        if start + 1 < s.len()
            && let Some(p) = punct2(c, s[start + 1])
        {
            self.pos = start + 2;
            let sy = match p {
                Some(p) => p,
                None => it.intern_bytes(&s[start..start + 2]),
            };
            return Some(Token::new(TokenKind::Punct, sy, flags));
        }
        if let Some(p) = punct1(c) {
            self.pos = start + 1;
            return Some(Token::new(TokenKind::Punct, p, flags));
        }
        let len = utf8_len(c).min(s.len() - start);
        self.pos = start + len;
        Some(Token::new(
            TokenKind::Other,
            it.intern_bytes(&s[start..start + len]),
            flags,
        ))
    }
}

fn utf8_len(c: u8) -> usize {
    match c {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

pub fn lex_all(src: &[u8], asm: bool, it: &mut Interner) -> Vec<Token> {
    let spliced = splice(src);
    let mut lx = Lexer::new(&spliced, asm);
    let mut out = Vec::new();
    while let Some(t) = lx.next(it) {
        out.push(t);
    }
    out
}

pub fn spell<'i>(t: &Token, it: &'i Interner) -> &'i str {
    if t.kind == TokenKind::Placemarker {
        ""
    } else {
        it.get(t.sym)
    }
}

fn last_byte(s: &str) -> u8 {
    s.as_bytes().last().copied().unwrap_or(0)
}

fn first_byte(s: &str) -> u8 {
    s.as_bytes().first().copied().unwrap_or(0)
}

pub fn avoid_paste(prev: &str, next: &str) -> bool {
    let a = last_byte(prev);
    let b = first_byte(next);
    if a == 0 || b == 0 {
        return false;
    }
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    if word(a) && (word(b) || b == b'"' || b == b'\'') {
        return true;
    }
    if prev.as_bytes()[0].is_ascii_digit() && (b == b'.' || b == b'+' || b == b'-') {
        return true;
    }
    matches!(
        (a, b),
        (b'+', b'+')
            | (b'-', b'-')
            | (b'+', b'=')
            | (b'-', b'=')
            | (b'-', b'>')
            | (b'<', b'<')
            | (b'<', b'=')
            | (b'<', b':')
            | (b'<', b'%')
            | (b'>', b'>')
            | (b'>', b'=')
            | (b'&', b'&')
            | (b'&', b'=')
            | (b'|', b'|')
            | (b'|', b'=')
            | (b'#', b'#')
            | (b'#', b'%')
            | (b'.', b'.')
            | (b'.', b'0'..=b'9')
            | (b'/', b'*')
            | (b'/', b'/')
            | (b'/', b'=')
            | (b'*', b'=')
            | (b'*', b'/')
            | (b'%', b'=')
            | (b'%', b'>')
            | (b'%', b':')
            | (b'^', b'=')
            | (b'!', b'=')
            | (b'=', b'=')
            | (b':', b'>')
            | (b':', b':')
    )
}

pub fn render(tokens: &[Token], it: &Interner) -> String {
    let mut out = String::new();
    let mut prev: Option<&str> = None;
    for t in tokens {
        if t.kind == TokenKind::Placemarker {
            continue;
        }
        let s = spell(t, it);
        if let Some(p) = prev
            && (t.space() || avoid_paste(p, s))
        {
            out.push(' ');
        }
        out.push_str(s);
        prev = Some(s);
    }
    out
}

pub fn stringify(tokens: &[Token], it: &Interner) -> String {
    let mut out = String::from("\"");
    let mut first = true;
    for t in tokens {
        if t.kind == TokenKind::Placemarker {
            continue;
        }
        if !first && t.space() {
            out.push(' ');
        }
        first = false;
        let s = spell(t, it);
        if matches!(t.kind, TokenKind::Str | TokenKind::Char) {
            for ch in s.chars() {
                if ch == '"' || ch == '\\' {
                    out.push('\\');
                }
                out.push(ch);
            }
        } else {
            out.push_str(s);
        }
    }
    out.push('"');
    out
}

pub fn paste(a: &Token, b: &Token, asm: bool, it: &mut Interner) -> Option<Token> {
    if a.kind == TokenKind::Placemarker {
        return Some(b.with_space(a.space()));
    }
    if b.kind == TokenKind::Placemarker {
        return Some(*a);
    }
    let mut text = String::with_capacity(16);
    text.push_str(it.get(a.sym));
    text.push_str(it.get(b.sym));
    let mut lx = Lexer::new(text.as_bytes(), asm);
    let t = lx.next(it)?;
    if lx.next(it).is_some() {
        return None;
    }
    let mut t = t;
    t.flags = a.flags & F_SPACE;
    t.hs = a.hs;
    t.origin = a.origin;
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> (Vec<Token>, Interner) {
        let mut it = Interner::new();
        let v = lex_all(s.as_bytes(), false, &mut it);
        (v, it)
    }

    fn spellings(s: &str) -> Vec<String> {
        let (v, it) = toks(s);
        v.iter().map(|t| spell(t, &it).to_string()).collect()
    }

    #[test]
    fn basic_tokens() {
        assert_eq!(
            spellings("(_AC(1,UL) << PAGE_SHIFT)"),
            [
                "(",
                "_AC",
                "(",
                "1",
                ",",
                "UL",
                ")",
                "<<",
                "PAGE_SHIFT",
                ")"
            ]
        );
    }

    #[test]
    fn numbers_and_literals() {
        assert_eq!(spellings("0x1fUL 1e+5 .5f"), ["0x1fUL", "1e+5", ".5f"]);
        assert_eq!(
            spellings(r#"L"a\"b" 'c' u8"x""#),
            [r#"L"a\"b""#, "'c'", r#"u8"x""#]
        );
    }

    #[test]
    fn comments_and_splices() {
        assert_eq!(spellings("a /* x\n y */ b // z"), ["a", "b"]);
        assert_eq!(spellings("A \\\n B"), ["A", "B"]);
        assert_eq!(spellings("FO\\\nO"), ["FOO"]);
    }

    #[test]
    fn punctuators() {
        assert_eq!(
            spellings("a##b # ... <<= ->"),
            ["a", "##", "b", "#", "...", "<<=", "->"]
        );
    }

    #[test]
    fn dollar_in_asm() {
        let mut it = Interner::new();
        let v = lex_all(b"movl $__KERNEL_DS, %eax", true, &mut it);
        let s: Vec<_> = v.iter().map(|t| spell(t, &it)).collect();
        assert_eq!(s, ["movl", "$", "__KERNEL_DS", ",", "%", "eax"]);
    }

    #[test]
    fn unterminated_quote_is_other() {
        assert_eq!(spellings("don't stop"), ["don", "'", "t", "stop"]);
    }

    #[test]
    fn render_avoids_paste() {
        let (v, it) = toks("( 1UL ) << 12");
        assert_eq!(render(&v, &it), "( 1UL ) << 12");
        let mut it2 = Interner::new();
        let a = Token::new(TokenKind::Punct, sym::PLUS, 0);
        let b = Token::new(TokenKind::Punct, sym::PLUS, 0);
        assert_eq!(render(&[a, b], &it2), "+ +");
        let x = Token::new(TokenKind::Ident, it2.intern("x"), 0);
        let y = Token::new(TokenKind::Ident, it2.intern("y"), 0);
        assert_eq!(render(&[x, y], &it2), "x y");
    }

    #[test]
    fn stringify_escapes() {
        let (v, it) = toks(r#"a  "q\n"   b"#);
        assert_eq!(stringify(&v, &it), r#""a \"q\\n\" b""#);
    }

    #[test]
    fn paste_tokens() {
        let mut it = Interner::new();
        let a = Token::new(TokenKind::Number, it.intern("1"), 0);
        let b = Token::new(TokenKind::Ident, it.intern("UL"), 0);
        let p = paste(&a, &b, false, &mut it).unwrap();
        assert_eq!(it.get(p.sym), "1UL");
        assert_eq!(p.kind, TokenKind::Number);
        let c = Token::new(TokenKind::Punct, sym::PLUS, 0);
        assert!(paste(&b, &c, false, &mut it).is_none());
    }
}
