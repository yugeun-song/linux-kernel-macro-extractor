use super::value::{IntTy, common, promote};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Konst {
    pub v: i128,
    pub ty: IntTy,
    pub hex: bool,
}

const SUFFIXES: &[(&str, bool, u8)] = &[
    ("", false, 0),
    ("u", true, 0),
    ("U", true, 0),
    ("l", false, 1),
    ("L", false, 1),
    ("ul", true, 1),
    ("uL", true, 1),
    ("Ul", true, 1),
    ("UL", true, 1),
    ("lu", true, 1),
    ("lU", true, 1),
    ("Lu", true, 1),
    ("LU", true, 1),
    ("ll", false, 2),
    ("LL", false, 2),
    ("ull", true, 2),
    ("uLL", true, 2),
    ("Ull", true, 2),
    ("ULL", true, 2),
    ("llu", true, 2),
    ("llU", true, 2),
    ("LLu", true, 2),
    ("LLU", true, 2),
];

fn signed(t: IntTy) -> bool {
    t.signed(false)
}

fn modulus(t: IntTy) -> i128 {
    1i128 << t.bits()
}

fn min(t: IntTy) -> i128 {
    if signed(t) {
        -(1i128 << (t.bits() - 1))
    } else {
        0
    }
}

fn max(t: IntTy) -> i128 {
    if signed(t) {
        (1i128 << (t.bits() - 1)) - 1
    } else {
        modulus(t) - 1
    }
}

fn fits(v: i128, t: IntTy) -> bool {
    min(t) <= v && v <= max(t)
}

fn convert(v: i128, t: IntTy) -> Option<i128> {
    if fits(v, t) {
        Some(v)
    } else if signed(t) {
        None
    } else {
        Some(v.rem_euclid(modulus(t)))
    }
}

fn integer(t: IntTy) -> bool {
    matches!(
        t,
        IntTy::Int | IntTy::UInt | IntTy::Long | IntTy::ULong | IntTy::LongLong | IntTy::ULongLong
    )
}

pub fn literal(text: &str) -> Option<Konst> {
    let (radix, from) = match text.as_bytes() {
        [b'0', b'x' | b'X', ..] => (16, 2),
        [b'0', b'b' | b'B', ..] => (2, 2),
        [b'0', b'0'..=b'9', ..] => (8, 1),
        _ => (10, 0),
    };
    let rest = &text[from..];
    let split = rest
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(rest.len());
    let (digits, suffix) = rest.split_at(split);
    if digits.is_empty() || (radix == 10 && !digits.as_bytes()[0].is_ascii_digit()) {
        return None;
    }
    let &(_, unsigned, longs) = SUFFIXES.iter().find(|(s, ..)| *s == suffix)?;
    let v = u128::from_str_radix(digits, radix).ok()?;
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
    let ty = candidates.iter().copied().find(|&t| v <= max(t) as u128)?;
    Some(Konst {
        v: v as i128,
        ty,
        hex: radix == 16 || radix == 2,
    })
}

fn truth(b: bool) -> Konst {
    Konst {
        v: i128::from(b),
        ty: IntTy::Int,
        hex: false,
    }
}

pub fn unary(op: &str, a: Konst) -> Option<Konst> {
    if !integer(a.ty) {
        return None;
    }
    let t = promote(a.ty);
    let v = match op {
        "+" => a.v,
        "-" if signed(t) => {
            let v = -a.v;
            if !fits(v, t) {
                return None;
            }
            v
        }
        "-" => (-a.v).rem_euclid(modulus(t)),
        "~" if signed(t) => !a.v,
        "~" => (!a.v).rem_euclid(modulus(t)),
        "!" => return Some(truth(a.v == 0)),
        _ => return None,
    };
    Some(Konst {
        v,
        ty: t,
        hex: a.hex || op == "~",
    })
}

pub fn binary(op: &str, a: Konst, b: Konst) -> Option<Konst> {
    if !integer(a.ty) || !integer(b.ty) {
        return None;
    }
    match op {
        "<<" | ">>" => {
            let t = promote(a.ty);
            if b.v < 0 || b.v >= i128::from(t.bits()) {
                return None;
            }
            let v = if op == ">>" {
                if signed(t) && a.v < 0 {
                    return None;
                }
                a.v >> b.v
            } else if signed(t) {
                if a.v < 0 {
                    return None;
                }
                let v = a.v.checked_mul(1i128 << b.v)?;
                if !fits(v, t) {
                    return None;
                }
                v
            } else {
                (((a.v as u128) << b.v) % (modulus(t) as u128)) as i128
            };
            Some(Konst {
                v,
                ty: t,
                hex: true,
            })
        }
        "&&" => Some(truth(a.v != 0 && b.v != 0)),
        "||" => Some(truth(a.v != 0 || b.v != 0)),
        "==" | "!=" | "<" | ">" | "<=" | ">=" => {
            let t = common(a.ty, b.ty, false);
            let (x, y) = (convert(a.v, t)?, convert(b.v, t)?);
            Some(truth(match op {
                "==" => x == y,
                "!=" => x != y,
                "<" => x < y,
                ">" => x > y,
                "<=" => x <= y,
                _ => x >= y,
            }))
        }
        "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" => {
            let t = common(a.ty, b.ty, false);
            let (x, y) = (convert(a.v, t)?, convert(b.v, t)?);
            let r = match op {
                "+" => x + y,
                "-" => x - y,
                "*" if signed(t) => x.checked_mul(y)?,
                "*" => ((x as u128).wrapping_mul(y as u128) % (modulus(t) as u128)) as i128,
                "/" | "%" => {
                    if y == 0 || !fits(x / y, t) {
                        return None;
                    }
                    if op == "/" { x / y } else { x % y }
                }
                "&" => x & y,
                "|" => x | y,
                _ => x ^ y,
            };
            let v = if signed(t) {
                if !fits(r, t) {
                    return None;
                }
                r
            } else {
                r.rem_euclid(modulus(t))
            };
            Some(Konst {
                v,
                ty: t,
                hex: a.hex || b.hex || matches!(op, "&" | "|" | "^"),
            })
        }
        _ => None,
    }
}

pub fn cast(words: &[&str], a: Konst) -> Option<Konst> {
    if !integer(a.ty) || words.is_empty() {
        return None;
    }
    let (mut unsigned, mut signed_word, mut longs, mut ints) = (false, false, 0, 0);
    for w in words {
        match *w {
            "unsigned" if !unsigned => unsigned = true,
            "signed" | "__signed__" | "__signed" if !signed_word => signed_word = true,
            "long" => longs += 1,
            "int" => ints += 1,
            _ => return None,
        }
    }
    if (unsigned && signed_word) || longs > 2 || ints > 1 {
        return None;
    }
    let ty = match (unsigned, longs) {
        (false, 0) => IntTy::Int,
        (true, 0) => IntTy::UInt,
        (false, 1) => IntTy::Long,
        (true, 1) => IntTy::ULong,
        (false, _) => IntTy::LongLong,
        (true, _) => IntTy::ULongLong,
    };
    Some(Konst {
        v: convert(a.v, ty)?,
        ty,
        hex: a.hex,
    })
}

pub fn cond(c: Konst, t: Konst, f: Konst) -> Option<Konst> {
    if !integer(c.ty) || !integer(t.ty) || !integer(f.ty) {
        return None;
    }
    let ty = common(t.ty, f.ty, false);
    let pick = if c.v != 0 { t } else { f };
    Some(Konst {
        v: convert(pick.v, ty)?,
        ty,
        hex: pick.hex,
    })
}

pub fn spell(k: Konst) -> Option<(bool, String)> {
    let suffix = match k.ty {
        IntTy::Int => "",
        IntTy::UInt => "U",
        IntTy::Long => "L",
        IntTy::ULong => "UL",
        IntTy::LongLong => "LL",
        IntTy::ULongLong => "ULL",
        _ => return None,
    };
    if !fits(k.v, k.ty) || (k.v < 0 && k.v == min(k.ty)) {
        return None;
    }
    if k.v < 0 {
        return Some((true, format!("{}{suffix}", -k.v)));
    }
    let digits = if k.hex && k.v != 0 {
        format!("0x{:x}", k.v)
    } else {
        k.v.to_string()
    };
    Some((false, format!("{digits}{suffix}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(text: &str) -> Konst {
        literal(text).unwrap()
    }

    fn shown(k: Option<Konst>) -> Option<String> {
        let (neg, s) = spell(k?)?;
        Some(if neg { format!("-{s}") } else { s })
    }

    #[test]
    fn literal_types_follow_the_c_table() {
        assert_eq!(k("2147483647").ty, IntTy::Int);
        assert_eq!(k("2147483648").ty, IntTy::Long);
        assert_eq!(k("0x7fffffff").ty, IntTy::Int);
        assert_eq!(k("0x80000000").ty, IntTy::UInt);
        assert_eq!(k("0xffffffffffffffff").ty, IntTy::ULong);
        assert_eq!(k("0xffffffffffffffffUL").ty, IntTy::ULong);
        assert_eq!(k("4294967296u").ty, IntTy::ULong);
        assert_eq!(k("1LL").ty, IntTy::LongLong);
        assert_eq!(k("1llu").ty, IntTy::ULongLong);
        assert_eq!(k("0777").v, 511);
        assert_eq!(k("0b101").v, 5);
        assert_eq!(k("0").ty, IntTy::Int);
        for bad in [
            "18446744073709551615",
            "08",
            "0x",
            "1lL",
            "1.5",
            "1e5",
            "0x1p3",
            "1i",
            "1uu",
            "1wb",
        ] {
            assert_eq!(literal(bad), None, "{bad}");
        }
    }

    #[test]
    fn folding_refuses_undefined_and_implementation_defined_results() {
        let int_max = k("2147483647");
        assert_eq!(binary("+", int_max, k("1")), None);
        assert_eq!(binary("<<", k("1"), k("31")), None);
        assert_eq!(binary("<<", k("1"), k("32")), None);
        assert_eq!(binary("<<", k("1UL"), k("64")), None);
        assert_eq!(
            unary("-", k("1")).and_then(|m| binary(">>", m, k("1"))),
            None
        );
        assert_eq!(binary("/", k("1"), k("0")), None);
        assert_eq!(binary("%", k("1"), k("0")), None);
        let int_min = binary("-", unary("-", int_max).unwrap(), k("1")).unwrap();
        assert_eq!(binary("/", int_min, unary("-", k("1")).unwrap()), None);
        assert_eq!(binary("%", int_min, unary("-", k("1")).unwrap()), None);
        assert_eq!(cast(&["int"], k("4294967295U")), None);
        assert_eq!(shown(Some(int_min)), None);
    }

    #[test]
    fn folding_keeps_value_and_type() {
        assert_eq!(
            shown(binary(
                "<<",
                k("1UL"),
                binary("-", k("39"), k("1")).unwrap()
            )),
            Some("0x4000000000UL".into())
        );
        let mask = binary("<<", k("0xffffffffffffffffUL"), k("38")).unwrap();
        assert_eq!(shown(unary("~", mask)), Some("0x3fffffffffUL".into()));
        assert_eq!(shown(binary("-", k("39"), k("1"))), Some("38".into()));
        assert_eq!(
            shown(binary("-", k("0U"), k("1"))),
            Some("4294967295U".into())
        );
        assert_eq!(
            shown(unary("-", k("1UL"))),
            Some("18446744073709551615UL".into())
        );
        assert_eq!(shown(unary("-", k("5L"))), Some("-5L".into()));
        assert_eq!(shown(binary("*", k("3"), k("4L"))), Some("12L".into()));
        assert_eq!(shown(binary("+", k("1"), k("1U"))), Some("2U".into()));
        assert_eq!(shown(binary("+", k("1L"), k("1U"))), Some("2L".into()));
        assert_eq!(shown(binary("+", k("0x10"), k("1"))), Some("0x11".into()));
        assert_eq!(
            shown(binary("<", unary("-", k("1")).unwrap(), k("0U"))),
            Some("0".into())
        );
        assert_eq!(
            shown(binary("%", unary("-", k("7")).unwrap(), k("2"))),
            Some("-1".into())
        );
        assert_eq!(
            shown(binary("/", unary("-", k("7")).unwrap(), k("2"))),
            Some("-3".into())
        );
        assert_eq!(
            shown(binary(
                "*",
                k("0xffffffffffffffffUL"),
                k("0xffffffffffffffffUL")
            )),
            Some("0x1UL".into())
        );
        assert_eq!(
            shown(cast(&["unsigned", "long"], k("5"))),
            Some("5UL".into())
        );
        assert_eq!(
            shown(cast(&["long", "unsigned"], unary("-", k("1")).unwrap())),
            Some("18446744073709551615UL".into())
        );
        assert_eq!(shown(cond(k("1"), k("2"), k("3L"))), Some("2L".into()));
        assert_eq!(shown(unary("~", k("0"))), Some("-1".into()));
        assert_eq!(shown(unary("!", k("5UL"))), Some("0".into()));
        assert_eq!(cast(&["char"], k("1")), None);
        assert_eq!(cast(&["u64"], k("1")), None);
    }
}
