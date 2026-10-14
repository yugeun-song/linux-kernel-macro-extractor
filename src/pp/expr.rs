use crate::intern::{Interner, sym};
use crate::syntax::lex::{Token, TokenKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Num {
    pub bits: u64,
    pub unsigned: bool,
}

impl Num {
    pub fn signed(v: i64) -> Num {
        Num {
            bits: v as u64,
            unsigned: false,
        }
    }

    pub fn unsigned(v: u64) -> Num {
        Num {
            bits: v,
            unsigned: true,
        }
    }

    pub fn is_true(self) -> bool {
        self.bits != 0
    }

    pub fn as_i64(self) -> i64 {
        self.bits as i64
    }

    pub fn display(self) -> String {
        if self.unsigned {
            format!("{}", self.bits)
        } else {
            format!("{}", self.bits as i64)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct EvalError(pub String);

pub fn parse_int(text: &str) -> Result<Num, EvalError> {
    let lower = text.to_ascii_lowercase();
    let digits_end = lower.trim_end_matches(['u', 'l']).len();
    let (body, suffix) = lower.split_at(digits_end);
    let unsigned_suffix = suffix.contains('u');
    if !suffix.chars().all(|c| c == 'u' || c == 'l') || suffix.matches('u').count() > 1 {
        return Err(EvalError(format!(
            "invalid suffix \"{suffix}\" on integer constant {text}"
        )));
    }
    let (radix, digits) = if let Some(h) = body.strip_prefix("0x") {
        (16, h)
    } else if let Some(b) = body.strip_prefix("0b") {
        (2, b)
    } else if body.len() > 1 && body.starts_with('0') {
        (8, &body[1..])
    } else {
        (10, body)
    };
    if digits.is_empty() && radix != 8 {
        return Err(EvalError(format!("invalid integer constant {text}")));
    }
    if digits.contains(['.', 'e', 'p']) && radix != 16 || digits.contains('.') {
        return Err(EvalError(format!(
            "floating constant {text} in preprocessor expression"
        )));
    }
    let digits = if digits.is_empty() { "0" } else { digits };
    let v = u64::from_str_radix(digits, radix)
        .map_err(|_| EvalError(format!("invalid integer constant {text}")))?;
    let unsigned = unsigned_suffix || v > i64::MAX as u64;
    Ok(Num { bits: v, unsigned })
}

pub fn parse_char(text: &str, unsigned_char: bool) -> Result<Num, EvalError> {
    let (prefix, rest) = match text.find('\'') {
        Some(i) => text.split_at(i),
        None => return Err(EvalError(format!("bad character constant {text}"))),
    };
    let inner = rest
        .strip_prefix('\'')
        .and_then(|r| r.strip_suffix('\''))
        .ok_or_else(|| EvalError(format!("bad character constant {text}")))?;
    let bytes = inner.as_bytes();
    let mut vals: Vec<u32> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            let ch = inner[i..].chars().next().unwrap_or('\0');
            vals.push(ch as u32);
            i += ch.len_utf8();
            continue;
        }
        i += 1;
        let Some(&e) = bytes.get(i) else { break };
        i += 1;
        let v = match e {
            b'n' => 10,
            b't' => 9,
            b'r' => 13,
            b'a' => 7,
            b'b' => 8,
            b'f' => 12,
            b'v' => 11,
            b'e' | b'E' => 27,
            b'x' => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
                u32::from_str_radix(&inner[start..i], 16).unwrap_or(0)
            }
            b'0'..=b'7' => {
                let start = i - 1;
                while i < bytes.len() && i - start < 3 && (b'0'..=b'7').contains(&bytes[i]) {
                    i += 1;
                }
                u32::from_str_radix(&inner[start..i], 8).unwrap_or(0)
            }
            other => u32::from(other),
        };
        vals.push(v);
    }
    if prefix.is_empty() {
        let mut v: i64 = 0;
        for &c in &vals {
            v = (v << 8) | i64::from(c & 0xff);
        }
        if vals.len() == 1 && !unsigned_char && v >= 0x80 {
            v -= 0x100;
        }
        Ok(Num::signed(v))
    } else {
        Ok(Num::signed(i64::from(vals.last().copied().unwrap_or(0))))
    }
}

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    it: &'a Interner,
    unsigned_char: bool,
}

fn arith(a: Num, b: Num) -> bool {
    a.unsigned || b.unsigned
}

fn shift_left(a: Num, n: i64) -> Num {
    if n < 0 {
        return shift_right(a, -n);
    }
    let bits = if n >= 64 { 0 } else { a.bits << n };
    Num {
        bits,
        unsigned: a.unsigned,
    }
}

fn shift_right(a: Num, n: i64) -> Num {
    if n < 0 {
        return shift_left(a, -n);
    }
    let bits = if a.unsigned {
        if n >= 64 { 0 } else { a.bits >> n }
    } else {
        let s = a.bits as i64;
        (if n >= 64 { s >> 63 } else { s >> n }) as u64
    };
    Num {
        bits,
        unsigned: a.unsigned,
    }
}

fn compare(a: Num, b: Num, op: &str) -> bool {
    let u = arith(a, b);
    match op {
        "<" if u => a.bits < b.bits,
        "<" => (a.bits as i64) < (b.bits as i64),
        ">" if u => a.bits > b.bits,
        ">" => (a.bits as i64) > (b.bits as i64),
        "<=" if u => a.bits <= b.bits,
        "<=" => (a.bits as i64) <= (b.bits as i64),
        ">=" if u => a.bits >= b.bits,
        ">=" => (a.bits as i64) >= (b.bits as i64),
        "==" => a.bits == b.bits,
        _ => a.bits != b.bits,
    }
}

fn bool_num(v: bool) -> Num {
    Num::signed(i64::from(v))
}

impl<'a> Parser<'a> {
    fn peek_op(&self) -> Option<&'a str> {
        let t = self.toks.get(self.pos)?;
        (t.kind == TokenKind::Punct).then(|| self.it.get(t.sym))
    }

    fn expect(&mut self, op: &str) -> Result<(), EvalError> {
        if self.peek_op() == Some(op) {
            self.pos += 1;
            Ok(())
        } else {
            Err(EvalError(format!(
                "expected '{op}' in preprocessor expression"
            )))
        }
    }

    fn expr(&mut self, live: bool) -> Result<Num, EvalError> {
        let mut v = self.ternary(live)?;
        while self.peek_op() == Some(",") {
            self.pos += 1;
            v = self.ternary(live)?;
        }
        Ok(v)
    }

    fn ternary(&mut self, live: bool) -> Result<Num, EvalError> {
        let c = self.binary(0, live)?;
        if self.peek_op() != Some("?") {
            return Ok(c);
        }
        self.pos += 1;
        let a = self.expr(live && c.is_true())?;
        self.expect(":")?;
        let b = self.ternary(live && !c.is_true())?;
        let unsigned = a.unsigned || b.unsigned;
        let pick = if c.is_true() { a } else { b };
        Ok(Num {
            bits: pick.bits,
            unsigned,
        })
    }

    fn binary(&mut self, min_prec: u8, live: bool) -> Result<Num, EvalError> {
        let mut lhs = self.unary(live)?;
        while let Some(op) = self.peek_op() {
            let prec = match op {
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
            if prec < min_prec || prec == 0 {
                break;
            }
            self.pos += 1;
            let rhs_live = match op {
                "||" => live && !lhs.is_true(),
                "&&" => live && lhs.is_true(),
                _ => live,
            };
            let rhs = self.binary(prec + 1, rhs_live)?;
            lhs = self.apply(op, lhs, rhs, live)?;
        }
        Ok(lhs)
    }

    fn apply(&self, op: &str, a: Num, b: Num, live: bool) -> Result<Num, EvalError> {
        let u = arith(a, b);
        let mk = |bits: u64| Num { bits, unsigned: u };
        Ok(match op {
            "||" => bool_num(a.is_true() || b.is_true()),
            "&&" => bool_num(a.is_true() && b.is_true()),
            "|" => mk(a.bits | b.bits),
            "^" => mk(a.bits ^ b.bits),
            "&" => mk(a.bits & b.bits),
            "==" | "!=" | "<" | ">" | "<=" | ">=" => bool_num(compare(a, b, op)),
            "<<" => shift_left(a, b.as_i64()),
            ">>" => shift_right(a, b.as_i64()),
            "+" => mk(a.bits.wrapping_add(b.bits)),
            "-" => mk(a.bits.wrapping_sub(b.bits)),
            "*" => mk(a.bits.wrapping_mul(b.bits)),
            "/" | "%" => {
                if b.bits == 0 {
                    if live {
                        return Err(EvalError("division by zero in #if".to_string()));
                    }
                    return Ok(mk(0));
                }
                if u {
                    mk(if op == "/" {
                        a.bits / b.bits
                    } else {
                        a.bits % b.bits
                    })
                } else {
                    let (x, y) = (a.bits as i64, b.bits as i64);
                    let r = if op == "/" {
                        x.wrapping_div(y)
                    } else {
                        x.wrapping_rem(y)
                    };
                    mk(r as u64)
                }
            }
            _ => return Err(EvalError(format!("unsupported operator '{op}'"))),
        })
    }

    fn unary(&mut self, live: bool) -> Result<Num, EvalError> {
        let Some(t) = self.toks.get(self.pos) else {
            return Err(EvalError(
                "expected value in preprocessor expression".to_string(),
            ));
        };
        self.pos += 1;
        match t.kind {
            TokenKind::Number => parse_int(self.it.get(t.sym)),
            TokenKind::Char => parse_char(self.it.get(t.sym), self.unsigned_char),
            TokenKind::Ident => Ok(Num::signed(0)),
            TokenKind::Punct => match self.it.get(t.sym) {
                "(" => {
                    let v = self.expr(live)?;
                    self.expect(")")?;
                    Ok(v)
                }
                "+" => self.unary(live),
                "-" => {
                    let v = self.unary(live)?;
                    Ok(Num {
                        bits: v.bits.wrapping_neg(),
                        unsigned: v.unsigned,
                    })
                }
                "~" => {
                    let v = self.unary(live)?;
                    Ok(Num {
                        bits: !v.bits,
                        unsigned: v.unsigned,
                    })
                }
                "!" => {
                    let v = self.unary(live)?;
                    Ok(bool_num(!v.is_true()))
                }
                other => Err(EvalError(format!(
                    "token \"{other}\" is not valid in preprocessor expressions"
                ))),
            },
            TokenKind::Str => Err(EvalError(
                "string literal in preprocessor expression".to_string(),
            )),
            _ => Err(EvalError(format!(
                "token \"{}\" is not valid in preprocessor expressions",
                self.it.get(t.sym)
            ))),
        }
    }
}

pub fn eval(tokens: &[Token], it: &Interner, unsigned_char: bool) -> Result<Num, EvalError> {
    let toks: Vec<Token> = tokens
        .iter()
        .copied()
        .filter(|t| t.kind != TokenKind::Placemarker)
        .collect();
    if toks.is_empty() {
        return Err(EvalError("#if with no expression".to_string()));
    }
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        it,
        unsigned_char,
    };
    let v = p.expr(true)?;
    if p.pos != toks.len() {
        let t = toks[p.pos];
        let what = if t.kind == TokenKind::Punct && t.sym == sym::RPAREN {
            "missing '(' in expression".to_string()
        } else {
            format!("missing binary operator before token \"{}\"", it.get(t.sym))
        };
        return Err(EvalError(what));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::lex::lex_all;

    fn ev(s: &str) -> Result<String, String> {
        let mut it = Interner::new();
        let toks = lex_all(s.as_bytes(), false, &mut it);
        eval(&toks, &it, true).map(Num::display).map_err(|e| e.0)
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(ev("1 + 2 * 3").unwrap(), "7");
        assert_eq!(ev("(1 + 2) * 3").unwrap(), "9");
        assert_eq!(ev("1 << 12").unwrap(), "4096");
        assert_eq!(ev("10 / 3 % 2").unwrap(), "1");
        assert_eq!(ev("1 ? 2 : 3").unwrap(), "2");
        assert_eq!(ev("0 || 0 && 1 | 4").unwrap(), "0");
        assert_eq!(ev("UNDEFINED_THING + 1").unwrap(), "1");
    }

    #[test]
    fn signedness() {
        assert_eq!(ev("-1 < 0").unwrap(), "1");
        assert_eq!(ev("-1 < 0u").unwrap(), "0");
        assert_eq!(ev("~0UL >> 1").unwrap(), i64::MAX.to_string());
        assert_eq!(ev("0xffffffffffffffff").unwrap(), u64::MAX.to_string());
        assert_eq!(ev("-1 >> 63").unwrap(), "-1");
        assert_eq!(ev("0x10 == 16 && 010 == 8 && 0b101 == 5").unwrap(), "1");
    }

    #[test]
    fn chars_and_errors() {
        assert_eq!(ev("'A'").unwrap(), "65");
        assert_eq!(ev("'\\n' + '\\x41'").unwrap(), "75");
        assert_eq!(ev("'\\377'").unwrap(), "255");
        assert!(ev("1 / 0").is_err());
        assert_eq!(ev("0 && (1 / 0)").unwrap(), "0");
        assert!(ev("1 +").is_err());
        assert!(ev("1 2").is_err());
        assert!(ev("1.5").is_err());
        assert!(ev("").is_err());
    }

    #[test]
    fn int_parsing() {
        assert_eq!(parse_int("4096UL").unwrap(), Num::unsigned(4096));
        assert_eq!(parse_int("0").unwrap(), Num::signed(0));
        assert_eq!(parse_int("0x1fLL").unwrap(), Num::signed(31));
        assert!(parse_int("12abc").is_err());
    }
}
