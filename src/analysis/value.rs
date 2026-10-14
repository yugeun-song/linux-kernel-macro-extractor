use crate::intern::Interner;
use crate::syntax::lex::{Token, TokenKind};
use crate::tree::dot_config::DotConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntTy {
    Bool,
    Char,
    SChar,
    UChar,
    Short,
    UShort,
    Int,
    UInt,
    Long,
    ULong,
    LongLong,
    ULongLong,
}

impl IntTy {
    pub fn bits(self) -> u32 {
        match self {
            IntTy::Bool => 1,
            IntTy::Char | IntTy::SChar | IntTy::UChar => 8,
            IntTy::Short | IntTy::UShort => 16,
            IntTy::Int | IntTy::UInt => 32,
            IntTy::Long | IntTy::ULong | IntTy::LongLong | IntTy::ULongLong => 64,
        }
    }

    fn width(self) -> u32 {
        match self {
            IntTy::Bool => 8,
            other => other.bits(),
        }
    }

    pub fn signed(self, char_unsigned: bool) -> bool {
        match self {
            IntTy::Char => !char_unsigned,
            IntTy::SChar | IntTy::Short | IntTy::Int | IntTy::Long | IntTy::LongLong => true,
            _ => false,
        }
    }

    fn rank(self) -> u8 {
        match self {
            IntTy::Bool => 0,
            IntTy::Char | IntTy::SChar | IntTy::UChar => 1,
            IntTy::Short | IntTy::UShort => 2,
            IntTy::Int | IntTy::UInt => 3,
            IntTy::Long | IntTy::ULong => 4,
            IntTy::LongLong | IntTy::ULongLong => 5,
        }
    }

    fn to_unsigned(self) -> IntTy {
        match self {
            IntTy::Int => IntTy::UInt,
            IntTy::Long => IntTy::ULong,
            IntTy::LongLong => IntTy::ULongLong,
            IntTy::Short => IntTy::UShort,
            IntTy::Char | IntTy::SChar => IntTy::UChar,
            other => other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            IntTy::Bool => "_Bool",
            IntTy::Char => "char",
            IntTy::SChar => "signed char",
            IntTy::UChar => "unsigned char",
            IntTy::Short => "short",
            IntTy::UShort => "unsigned short",
            IntTy::Int => "int",
            IntTy::UInt => "unsigned int",
            IntTy::Long => "long",
            IntTy::ULong => "unsigned long",
            IntTy::LongLong => "long long",
            IntTy::ULongLong => "unsigned long long",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CVal {
    pub v: i128,
    pub ty: IntTy,
    pub pointer: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Evaluated {
    Int(CVal),
    Str(String),
    NotConstant(String),
}

pub struct ValueCtx<'a> {
    pub it: &'a Interner,
    pub config: &'a DotConfig,
    pub char_unsigned: bool,
}

fn normalize(v: i128, ty: IntTy, cu: bool) -> i128 {
    let w = ty.width();
    if ty == IntTy::Bool {
        return i128::from(v != 0);
    }
    let mask: i128 = (1i128 << w) - 1;
    let u = v & mask;
    if ty.signed(cu) && u >> (w - 1) & 1 == 1 {
        u - (1i128 << w)
    } else {
        u
    }
}

pub(crate) fn promote(t: IntTy) -> IntTy {
    if t.rank() < IntTy::Int.rank() {
        IntTy::Int
    } else {
        t
    }
}

pub(crate) fn common(a: IntTy, b: IntTy, cu: bool) -> IntTy {
    let (a, b) = (promote(a), promote(b));
    if a == b {
        return a;
    }
    let (sa, sb) = (a.signed(cu), b.signed(cu));
    if sa == sb {
        return if a.rank() >= b.rank() { a } else { b };
    }
    let (u, s) = if sa { (b, a) } else { (a, b) };
    if u.rank() >= s.rank() {
        u
    } else if s.width() > u.width() {
        s
    } else {
        s.to_unsigned()
    }
}

fn literal_type(text: &str) -> Option<(i128, IntTy)> {
    let lower = text.to_ascii_lowercase();
    let body = lower.trim_end_matches(['u', 'l']);
    let suffix = &lower[body.len()..];
    let (radix, digits) = if let Some(h) = body.strip_prefix("0x") {
        (16, h)
    } else if let Some(b) = body.strip_prefix("0b") {
        (2, b)
    } else if body.len() > 1 && body.starts_with('0') {
        (8, &body[1..])
    } else {
        (10, body)
    };
    let v = u128::from_str_radix(if digits.is_empty() { "0" } else { digits }, radix).ok()?;
    let unsigned = suffix.contains('u');
    let longs = suffix.matches('l').count();
    let decimal = radix == 10;
    let candidates: &[IntTy] = match (unsigned, longs, decimal) {
        (false, 0, true) => &[IntTy::Int, IntTy::Long, IntTy::LongLong],
        (false, 0, false) => &[
            IntTy::Int,
            IntTy::UInt,
            IntTy::Long,
            IntTy::ULong,
            IntTy::LongLong,
            IntTy::ULongLong,
        ],
        (true, 0, _) => &[IntTy::UInt, IntTy::ULong, IntTy::ULongLong],
        (false, 1, true) => &[IntTy::Long, IntTy::LongLong],
        (false, 1, false) => &[IntTy::Long, IntTy::ULong, IntTy::LongLong, IntTy::ULongLong],
        (true, 1, _) => &[IntTy::ULong, IntTy::ULongLong],
        (false, _, true) => &[IntTy::LongLong],
        (false, _, false) => &[IntTy::LongLong, IntTy::ULongLong],
        (true, _, _) => &[IntTy::ULongLong],
    };
    for &t in candidates {
        let w = t.width();
        let max: u128 = if t.signed(false) {
            (1u128 << (w - 1)) - 1
        } else {
            (1u128 << w) - 1
        };
        if v <= max {
            return Some((v as i128, t));
        }
    }
    None
}

fn typedef(name: &str, cfg: &DotConfig) -> Option<IntTy> {
    Some(match name {
        "u8" | "__u8" | "uint8_t" | "u_int8_t" => IntTy::UChar,
        "s8" | "__s8" | "int8_t" => IntTy::SChar,
        "u16" | "__u16" | "uint16_t" | "__le16" | "__be16" => IntTy::UShort,
        "s16" | "__s16" | "int16_t" => IntTy::Short,
        "u32" | "__u32" | "uint32_t" | "__le32" | "__be32" | "gfp_t" | "slab_flags_t"
        | "fmode_t" => IntTy::UInt,
        "s32" | "__s32" | "int32_t" | "pid_t" => IntTy::Int,
        "u64" | "__u64" | "uint64_t" | "__le64" | "__be64" => IntTy::ULongLong,
        "s64" | "__s64" | "int64_t" | "loff_t" | "time64_t" => IntTy::LongLong,
        "size_t" | "__kernel_size_t" | "uintptr_t" | "pgoff_t" | "ulong" | "pteval_t"
        | "pmdval_t" | "pudval_t" | "pgdval_t" | "p4dval_t" => IntTy::ULong,
        "ssize_t" | "__kernel_ssize_t" | "intptr_t" | "ptrdiff_t" | "long_t" => IntTy::Long,
        "phys_addr_t" | "resource_size_t" => {
            if cfg.is_y("PHYS_ADDR_T_64BIT") {
                IntTy::ULongLong
            } else {
                IntTy::UInt
            }
        }
        "dma_addr_t" => {
            if cfg.is_y("ARCH_DMA_ADDR_T_64BIT") {
                IntTy::ULongLong
            } else {
                IntTy::UInt
            }
        }
        "bool" => IntTy::Bool,
        _ => return None,
    })
}

const QUALIFIERS: &[&str] = &[
    "const",
    "volatile",
    "__force",
    "__user",
    "__iomem",
    "__kernel",
    "__bitwise",
    "__percpu",
    "__rcu",
    "restrict",
    "__restrict",
    "__attribute_const__",
];

struct P<'a, 'c> {
    t: &'a [Token],
    pos: usize,
    cx: &'a ValueCtx<'c>,
}

type R = Result<CVal, String>;

impl P<'_, '_> {
    fn s(&self, i: usize) -> Option<&str> {
        self.t.get(i).map(|t| self.cx.it.get(t.sym))
    }

    fn peek(&self) -> Option<&str> {
        self.s(self.pos)
    }

    fn eat(&mut self, op: &str) -> bool {
        let is_op = self
            .t
            .get(self.pos)
            .is_some_and(|t| t.kind == TokenKind::Punct)
            && self.peek() == Some(op);
        if is_op {
            self.pos += 1;
        }
        is_op
    }

    fn type_name_at(&self, mut i: usize) -> Option<(IntTy, bool, usize)> {
        let mut signed = None;
        let mut longs = 0;
        let mut base: Option<&str> = None;
        let mut td = None;
        let mut pointer = false;
        let mut seen_any = false;
        while let Some(t) = self.t.get(i) {
            let w = self.cx.it.get(t.sym);
            match (t.kind, w) {
                (TokenKind::Ident, "signed") => signed = Some(true),
                (TokenKind::Ident, "unsigned") => signed = Some(false),
                (TokenKind::Ident, "long") => longs += 1,
                (TokenKind::Ident, "int" | "char" | "short" | "_Bool" | "void") => base = Some(w),
                (TokenKind::Ident, q) if QUALIFIERS.contains(&q) => {}
                (TokenKind::Ident, name) if !seen_any && td.is_none() => {
                    let ty = typedef(name, self.cx.config)?;
                    td = Some(ty)
                }
                (TokenKind::Punct, "*") if seen_any => pointer = true,
                _ => break,
            }
            seen_any = true;
            i += 1;
        }
        if !seen_any {
            return None;
        }
        if pointer {
            return Some((IntTy::ULong, true, i));
        }
        if let Some(t) = td {
            return Some((t, false, i));
        }
        let ty = match (base, longs, signed) {
            (Some("void"), _, _) => return None,
            (Some("_Bool"), _, _) => IntTy::Bool,
            (Some("char"), _, None) => IntTy::Char,
            (Some("char"), _, Some(true)) => IntTy::SChar,
            (Some("char"), _, Some(false)) => IntTy::UChar,
            (Some("short"), _, Some(false)) => IntTy::UShort,
            (Some("short"), _, _) => IntTy::Short,
            (_, 0, Some(false)) => IntTy::UInt,
            (_, 0, _) => IntTy::Int,
            (_, 1, Some(false)) => IntTy::ULong,
            (_, 1, _) => IntTy::Long,
            (_, _, Some(false)) => IntTy::ULongLong,
            (_, _, _) => IntTy::LongLong,
        };
        Some((ty, false, i))
    }

    fn mk(&self, v: i128, ty: IntTy) -> CVal {
        CVal {
            v: normalize(v, ty, self.cx.char_unsigned),
            ty,
            pointer: false,
        }
    }

    fn expr(&mut self) -> R {
        let mut v = self.cond()?;
        while self.eat(",") {
            v = self.cond()?;
        }
        Ok(v)
    }

    fn cond(&mut self) -> R {
        let c = self.bin(0)?;
        if !self.eat("?") {
            return Ok(c);
        }
        let a = self.expr()?;
        if !self.eat(":") {
            return Err("expected ':'".into());
        }
        let b = self.cond()?;
        let ty = common(a.ty, b.ty, self.cx.char_unsigned);
        let pick = if c.v != 0 { a } else { b };
        Ok(self.mk(pick.v, ty))
    }

    fn bin(&mut self, min: u8) -> R {
        let mut lhs = self.cast()?;
        loop {
            let is_punct = self
                .t
                .get(self.pos)
                .is_some_and(|t| t.kind == TokenKind::Punct);
            let Some(op) = self.peek().filter(|_| is_punct).map(str::to_string) else {
                break;
            };
            let prec = match op.as_str() {
                "||" => 1,
                "&&" => 2,
                "|" => 3,
                "^" => 4,
                "&" => 5,
                "==" | "!=" => 6,
                "<" | ">" | "<=" | ">=" => 7,
                "<<" | ">>" => 8,
                "+" | "-" => 9,
                "*" | "/" | "%" => 10,
                _ => break,
            };
            if prec < min {
                break;
            }
            self.pos += 1;
            let rhs = self.bin(prec + 1)?;
            lhs = self.apply(&op, lhs, rhs)?;
        }
        Ok(lhs)
    }

    fn apply(&self, op: &str, a: CVal, b: CVal) -> R {
        let cu = self.cx.char_unsigned;
        let ct = common(a.ty, b.ty, cu);
        let (x, y) = (normalize(a.v, ct, cu), normalize(b.v, ct, cu));
        Ok(match op {
            "||" => self.mk(i128::from(a.v != 0 || b.v != 0), IntTy::Int),
            "&&" => self.mk(i128::from(a.v != 0 && b.v != 0), IntTy::Int),
            "|" => self.mk(x | y, ct),
            "^" => self.mk(x ^ y, ct),
            "&" => self.mk(x & y, ct),
            "==" => self.mk(i128::from(x == y), IntTy::Int),
            "!=" => self.mk(i128::from(x != y), IntTy::Int),
            "<" => self.mk(i128::from(x < y), IntTy::Int),
            ">" => self.mk(i128::from(x > y), IntTy::Int),
            "<=" => self.mk(i128::from(x <= y), IntTy::Int),
            ">=" => self.mk(i128::from(x >= y), IntTy::Int),
            "<<" | ">>" => {
                let lt = promote(a.ty);
                if b.v < 0 || b.v >= i128::from(lt.width()) {
                    return Err(format!("shift by {} is undefined for {}", b.v, lt.name()));
                }
                let v = normalize(a.v, lt, cu);
                if op == "<<" {
                    self.mk(v << b.v, lt)
                } else {
                    self.mk(v >> b.v, lt)
                }
            }
            "+" => self.mk(x + y, ct),
            "-" => self.mk(x - y, ct),
            "*" => self.mk(x.wrapping_mul(y), ct),
            "/" | "%" => {
                if y == 0 {
                    return Err("division by zero".into());
                }
                if op == "/" {
                    self.mk(x / y, ct)
                } else {
                    self.mk(x % y, ct)
                }
            }
            _ => return Err(format!("operator '{op}'")),
        })
    }

    fn typeof_cast(&mut self) -> Option<R> {
        let open = self.pos;
        if self.s(open) != Some("(")
            || !matches!(self.s(open + 1), Some("__typeof__" | "typeof" | "__typeof"))
        {
            return None;
        }
        if self.s(open + 2) != Some("(") {
            return None;
        }
        let mut depth = 0;
        let mut i = open + 2;
        while i < self.t.len() {
            match self.s(i) {
                Some("(") => depth += 1,
                Some(")") => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if self.s(i + 1) != Some(")") {
            return None;
        }
        let inner = &self.t[open + 3..i];
        let mut sub = P {
            t: inner,
            pos: 0,
            cx: self.cx,
        };
        let ty = match sub.expr() {
            Ok(v) if sub.pos == inner.len() => v.ty,
            Ok(_) => return Some(Err("typeof of a non-constant expression".into())),
            Err(e) => return Some(Err(e)),
        };
        self.pos = i + 2;
        Some(self.cast().map(|v| self.mk(v.v, ty)))
    }

    fn cast(&mut self) -> R {
        if let Some(r) = self.typeof_cast() {
            return r;
        }
        if self.peek() == Some("(")
            && let Some((ty, pointer, end)) = self.type_name_at(self.pos + 1)
            && self.s(end) == Some(")")
        {
            self.pos = end + 1;
            let v = self.cast()?;
            let mut r = self.mk(v.v, ty);
            r.pointer = pointer;
            return Ok(r);
        }
        self.unary()
    }

    fn sizeof_type(&mut self) -> Option<CVal> {
        if self.peek() != Some("(") {
            return None;
        }
        let (ty, pointer, end) = self.type_name_at(self.pos + 1)?;
        if self.s(end) != Some(")") {
            return None;
        }
        self.pos = end + 1;
        let bytes = if pointer {
            8
        } else {
            i128::from(ty.width() / 8)
        };
        Some(self.mk(bytes, IntTy::ULong))
    }

    fn unary(&mut self) -> R {
        let cu = self.cx.char_unsigned;
        if self.eat("+") {
            let v = self.cast()?;
            return Ok(self.mk(v.v, promote(v.ty)));
        }
        if self.eat("-") {
            let v = self.cast()?;
            let t = promote(v.ty);
            return Ok(self.mk(-normalize(v.v, t, cu), t));
        }
        if self.eat("~") {
            let v = self.cast()?;
            let t = promote(v.ty);
            return Ok(self.mk(!normalize(v.v, t, cu), t));
        }
        if self.eat("!") {
            let v = self.cast()?;
            return Ok(self.mk(i128::from(v.v == 0), IntTy::Int));
        }
        let word = self.peek().map(str::to_string);
        if matches!(
            word.as_deref(),
            Some("sizeof" | "__alignof__" | "_Alignof" | "__alignof")
        ) {
            self.pos += 1;
            if let Some(v) = self.sizeof_type() {
                return Ok(v);
            }
            return Err("sizeof of a non-scalar type needs the type layout".into());
        }
        self.primary()
    }

    fn primary(&mut self) -> R {
        let Some(t) = self.t.get(self.pos).copied() else {
            return Err("unexpected end of expression".into());
        };
        self.pos += 1;
        let text = self.cx.it.get(t.sym);
        match t.kind {
            TokenKind::Number => {
                let (v, ty) = literal_type(text)
                    .ok_or_else(|| format!("`{text}` is not an integer constant"))?;
                Ok(self.mk(v, ty))
            }
            TokenKind::Char => {
                let n =
                    crate::pp::expr::parse_char(text, self.cx.char_unsigned).map_err(|e| e.0)?;
                Ok(self.mk(n.as_i64() as i128, IntTy::Int))
            }
            TokenKind::Punct if text == "(" => {
                let v = self.expr()?;
                if !self.eat(")") {
                    return Err("expected ')'".into());
                }
                Ok(v)
            }
            TokenKind::Ident => Err(format!(
                "depends on `{text}`, which is not a compile-time constant here"
            )),
            _ => Err(format!("`{text}` is not part of a constant expression")),
        }
    }
}

fn concat_strings(tokens: &[Token], it: &Interner) -> Option<String> {
    if tokens.is_empty()
        || !tokens
            .iter()
            .all(|t| t.kind == TokenKind::Str || t.kind == TokenKind::Placemarker)
    {
        return None;
    }
    let mut s = String::new();
    for t in tokens.iter().filter(|t| t.kind == TokenKind::Str) {
        let text = it.get(t.sym);
        let start = text.find('"')? + 1;
        s.push_str(&text[start..text.len() - 1]);
    }
    Some(s)
}

pub fn evaluate(tokens: &[Token], cx: &ValueCtx<'_>) -> Evaluated {
    let toks: Vec<Token> = tokens
        .iter()
        .copied()
        .filter(|t| t.kind != TokenKind::Placemarker)
        .collect();
    if toks.is_empty() {
        return Evaluated::NotConstant("empty".into());
    }
    if let Some(s) = concat_strings(&toks, cx.it) {
        return Evaluated::Str(s);
    }
    let mut p = P {
        t: &toks,
        pos: 0,
        cx,
    };
    match p.expr() {
        Ok(v) if p.pos == toks.len() => Evaluated::Int(v),
        Ok(_) => Evaluated::NotConstant("not a single expression".into()),
        Err(e) => Evaluated::NotConstant(e),
    }
}

fn size_suffix(v: i128) -> Option<String> {
    const UNITS: [(&str, i128); 4] = [
        ("TiB", 1 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ];
    if v < 1024 {
        return None;
    }
    UNITS
        .iter()
        .find(|(_, u)| v % u == 0 && v / u < 1024)
        .map(|(n, u)| format!("{} {n}", v / u))
}

pub fn display(v: &CVal, cu: bool) -> (String, String) {
    let ty = if v.pointer {
        "pointer".to_string()
    } else {
        v.ty.name().to_string()
    };
    let w = v.ty.width();
    let raw_bits = (v.v as u128) & ((1u128 << w) - 1);
    let mut s = if v.ty.signed(cu) {
        v.v.to_string()
    } else {
        raw_bits.to_string()
    };
    if v.v.unsigned_abs() >= 10 || v.v < 0 {
        s.push_str(&format!(" (0x{raw_bits:x}"));
        if let Some(sz) = size_suffix(v.v) {
            s.push_str(&format!(", {sz}"));
        }
        s.push(')');
    }
    (s, ty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::lex::lex_all;

    fn ev(src: &str) -> String {
        let mut it = Interner::new();
        let toks = lex_all(src.as_bytes(), false, &mut it);
        let cfg = DotConfig::parse("CONFIG_PHYS_ADDR_T_64BIT=y\n");
        let cx = ValueCtx {
            it: &it,
            config: &cfg,
            char_unsigned: true,
        };
        match evaluate(&toks, &cx) {
            Evaluated::Int(v) => {
                let (d, t) = display(&v, true);
                format!("{d} : {t}")
            }
            Evaluated::Str(s) => format!("\"{s}\""),
            Evaluated::NotConstant(why) => format!("NC {why}"),
        }
    }

    #[test]
    fn page_size() {
        assert_eq!(ev("((1UL) << 12)"), "4096 (0x1000, 4 KiB) : unsigned long");
        assert_eq!(
            ev("(~((1UL << 12)-1))"),
            "18446744073709547520 (0xfffffffffffff000) : unsigned long"
        );
    }

    #[test]
    fn types_and_conversions() {
        assert_eq!(ev("1 + 2"), "3 : int");
        assert_eq!(ev("-1 < 0u"), "0 : int");
        assert_eq!(ev("0xffffffff"), "4294967295 (0xffffffff) : unsigned int");
        assert_eq!(ev("2147483648"), "2147483648 (0x80000000, 2 GiB) : long");
        assert_eq!(ev("(u32)-1"), "4294967295 (0xffffffff) : unsigned int");
        assert_eq!(ev("(int)(sizeof(long) * 8)"), "64 (0x40) : int");
        assert_eq!(ev("sizeof(phys_addr_t)"), "8 : unsigned long");
        assert_eq!(ev("((void *)0)"), "0 : pointer");
        assert_eq!(ev("1 ? 2u : 3"), "2 : unsigned int");
        assert_eq!(ev("((__typeof__(1UL))(5))"), "5 : unsigned long");
        assert_eq!(
            ev("((((7UL)-1) | ((__typeof__(7UL))((1<<4)-1)))+1)"),
            "16 (0x10) : unsigned long"
        );
    }

    #[test]
    fn strings_and_failures() {
        assert_eq!(ev("\"6.12\" \".112\""), "\"6.12.112\"");
        assert!(ev("foo(1)").starts_with("NC depends on `foo`"));
        assert!(ev("1 << 64").starts_with("NC shift by 64"));
        assert!(ev("sizeof(struct page)").starts_with("NC sizeof"));
    }
}
