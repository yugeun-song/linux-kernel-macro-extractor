use anstyle::{AnsiColor, Effects, Style};

use crate::intern::Interner;
use crate::syntax::lex::{self, TokenKind};

pub const NAME: Style = AnsiColor::Cyan.on_default().effects(Effects::BOLD);
pub const QUERY: Style = AnsiColor::BrightCyan.on_default().effects(Effects::BOLD);
pub const VALUE: Style = AnsiColor::Yellow.on_default().effects(Effects::BOLD);
pub const TYPE: Style = Style::new().effects(Effects::DIMMED);
pub const PATH: Style = AnsiColor::Magenta.on_default();
pub const LINE: Style = AnsiColor::Green.on_default();
pub const OK: Style = AnsiColor::Green.on_default().effects(Effects::BOLD);
pub const BAD: Style = AnsiColor::Red.on_default().effects(Effects::BOLD);
pub const MAYBE: Style = AnsiColor::Yellow.on_default().effects(Effects::BOLD);
pub const DIM: Style = Style::new().effects(Effects::DIMMED);
pub const HEADING: Style = Style::new().effects(Effects::BOLD.insert(Effects::UNDERLINE));
pub const LABEL: Style = AnsiColor::BrightBlue.on_default();
pub const WARN: Style = AnsiColor::Yellow.on_default();
pub const KEYWORD: Style = AnsiColor::Blue.on_default();
pub const DIRECTIVE: Style = AnsiColor::BrightBlue.on_default().effects(Effects::BOLD);
pub const NUMBER: Style = AnsiColor::Yellow.on_default();
pub const STRING: Style = AnsiColor::Green.on_default();
pub const CONFIG: Style = AnsiColor::BrightMagenta.on_default();
pub const GUIDE: Style = Style::new().effects(Effects::DIMMED);

pub fn paint(s: &str, style: Style) -> String {
    if s.is_empty() {
        return String::new();
    }
    format!("{style}{s}{style:#}")
}

const C_KEYWORDS: &[&str] = &[
    "auto",
    "break",
    "case",
    "char",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "float",
    "for",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "register",
    "restrict",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "typedef",
    "union",
    "unsigned",
    "void",
    "volatile",
    "while",
    "_Bool",
    "_Static_assert",
    "_Generic",
    "__attribute__",
    "__typeof__",
    "typeof",
    "asm",
    "__asm__",
    "__auto_type",
    "_Alignof",
    "__alignof__",
    "__builtin_choose_expr",
    "__builtin_constant_p",
    "__builtin_expect",
    "__builtin_types_compatible_p",
    "__builtin_unreachable",
    "__builtin_offsetof",
];

pub fn code(text: &str) -> String {
    let mut it = Interner::new();
    let toks = lex::lex_all(text.as_bytes(), false, &mut it);
    let mut out = String::with_capacity(text.len() * 2);
    let mut prev: Option<String> = None;
    let mut after_hash = false;
    for (i, t) in toks.iter().enumerate() {
        let s = lex::spell(t, &it).to_string();
        if let Some(p) = &prev
            && (t.space() || lex::avoid_paste(p, &s))
        {
            out.push(' ');
        }
        let styled = match t.kind {
            TokenKind::Punct if s == "#" && i == 0 => {
                after_hash = true;
                paint(&s, DIRECTIVE)
            }
            TokenKind::Ident if after_hash => {
                after_hash = false;
                paint(&s, DIRECTIVE)
            }
            TokenKind::Ident if s.starts_with("CONFIG_") => paint(&s, CONFIG),
            TokenKind::Ident if C_KEYWORDS.contains(&s.as_str()) => paint(&s, KEYWORD),
            TokenKind::Ident
                if i == 2 && toks.first().is_some_and(|f| lex::spell(f, &it) == "#") =>
            {
                paint(&s, NAME)
            }
            TokenKind::Number | TokenKind::Char => paint(&s, NUMBER),
            TokenKind::Str => paint(&s, STRING),
            _ => s.clone(),
        };
        out.push_str(&styled);
        prev = Some(s);
    }
    out
}

pub fn width(s: &str) -> usize {
    let mut w = 0;
    let mut in_esc = false;
    for ch in s.chars() {
        if in_esc {
            if ch.is_ascii_alphabetic() {
                in_esc = false;
            }
            continue;
        }
        if ch == '\u{1b}' {
            in_esc = true;
            continue;
        }
        w += match ch as u32 {
            0x1100..=0x115f
            | 0x2e80..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe30..=0xfe4f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6 => 2,
            _ => 1,
        };
    }
    w
}

#[derive(Clone, Copy)]
pub struct Glyphs {
    pub branch: &'static str,
    pub last: &'static str,
    pub pipe: &'static str,
    pub blank: &'static str,
    pub ok: &'static str,
    pub bad: &'static str,
    pub maybe: &'static str,
    pub arrow: &'static str,
    pub git: &'static str,
}

pub const UNICODE: Glyphs = Glyphs {
    branch: "├─ ",
    last: "└─ ",
    pipe: "│  ",
    blank: "   ",
    ok: "✓",
    bad: "✗",
    maybe: "?",
    arrow: "→",
    git: "\u{f126}",
};

pub const ASCII: Glyphs = Glyphs {
    branch: "|- ",
    last: "`- ",
    pipe: "|  ",
    blank: "   ",
    ok: "+",
    bad: "x",
    maybe: "?",
    arrow: "->",
    git: "@",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_ignores_escapes() {
        assert_eq!(width(&paint("abc", NAME)), 3);
        assert_eq!(width("├─ x"), 4);
        assert_eq!(width("\u{ff21}\u{ff22}"), 4);
    }

    #[test]
    fn highlighting_keeps_text() {
        let raw = "#define PAGE_SIZE (_AC(1,UL) << CONFIG_PAGE_SHIFT)";
        let painted = code(raw);
        let plain: String = {
            let mut s = String::new();
            let mut esc = false;
            for c in painted.chars() {
                if esc {
                    if c.is_ascii_alphabetic() {
                        esc = false;
                    }
                    continue;
                }
                if c == '\u{1b}' {
                    esc = true;
                    continue;
                }
                s.push(c);
            }
            s
        };
        assert_eq!(plain, raw);
    }
}
