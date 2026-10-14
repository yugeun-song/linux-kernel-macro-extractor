use super::{DefKind, ENUM_TAG_PREFIX, FileClass, USE_PREFIX};
use crate::syntax::enums::{find_enums, merged_skips};
use crate::syntax::scan::{DirKind, DirLine, parse_head, scan_full};

#[derive(Default)]
pub(super) struct Extracted {
    pub(super) names: String,
    pub(super) posts: Vec<RawPost>,
    pub(super) defs: u64,
    pub(super) stored: rustc_hash::FxHashMap<u64, (u32, u32)>,
}

impl Extracted {
    fn store(&mut self, prefix: &str, name: &str) -> (u32, u32) {
        use std::hash::BuildHasher;
        let hash = rustc_hash::FxBuildHasher.hash_one((prefix, name));
        let len = (prefix.len() + name.len()) as u32;
        if let Some(&(at, n)) = self.stored.get(&hash)
            && n == len
            && self.names[at as usize..(at + n) as usize]
                .strip_prefix(prefix)
                .is_some_and(|rest| rest == name)
        {
            return (at, n);
        }
        let at = self.names.len() as u32;
        self.names.push_str(prefix);
        self.names.push_str(name);
        self.stored.entry(hash).or_insert((at, len));
        (at, len)
    }
}

#[derive(Clone, Copy)]
pub(super) struct RawPost {
    pub(super) name_at: u32,
    pub(super) name_len: u32,
    pub(super) line: u32,
    pub(super) off: u32,
    pub(super) kd: u8,
}

fn depth_byte(kind: DefKind, depth: u32) -> u8 {
    (kind as u8) | ((depth.min(31) as u8) << 3)
}

struct LineCounter<'a> {
    src: &'a [u8],
    at: usize,
    line: u32,
}

impl LineCounter<'_> {
    fn line_of(&mut self, off: usize) -> u32 {
        if off < self.at {
            self.at = 0;
            self.line = 1;
        }
        let end = off.min(self.src.len());
        self.line += memchr::memchr_iter(b'\n', &self.src[self.at..end]).count() as u32;
        self.at = end;
        self.line
    }
}

pub(super) fn extract(src: &[u8], class: FileClass, out: &mut Extracted) -> bool {
    let mut dirs: Vec<DirLine> = Vec::new();
    let mut skipped = Vec::new();
    let want_enums = matches!(class, FileClass::CSource | FileClass::Header);
    scan_full(src, &mut dirs, want_enums.then_some(&mut skipped));
    let mut depth: u32 = 0;
    let mut first_frame_closer = None;
    let mut depth_after: Vec<(u32, u32)> = Vec::with_capacity(dirs.len());
    let heads: Vec<_> = dirs.iter().map(|d| parse_head(src, *d)).collect();
    for (i, (d, h)) in dirs.iter().zip(&heads).enumerate() {
        match h.kind {
            DirKind::Define | DirKind::Undef => {
                if let Some(name) = &h.name {
                    let kind = match (h.kind, h.function_like) {
                        (DirKind::Undef, _) => DefKind::Undef,
                        (_, true) => DefKind::Function,
                        _ => DefKind::Object,
                    };
                    push_post(out, name, d.line, d.off, depth_byte(kind, depth));
                    if kind != DefKind::Undef {
                        let raw = &src[d.off as usize..d.end as usize];
                        push_uses(out, raw, d.line, d.off, depth_byte(kind, depth));
                    }
                }
            }
            DirKind::If | DirKind::Ifdef | DirKind::Ifndef => depth += 1,
            DirKind::Endif => {
                if depth == 1 && first_frame_closer.is_none() {
                    first_frame_closer = Some(i);
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        depth_after.push((d.off, depth));
    }
    let guarded = heads.len() >= 3
        && heads[0].kind == DirKind::Ifndef
        && heads[1].kind == DirKind::Define
        && heads[0].name.is_some()
        && heads[0].name == heads[1].name
        && first_frame_closer == Some(heads.len() - 1);
    if guarded {
        for p in &mut out.posts {
            let d = p.kd >> 3;
            p.kd = (p.kd & 7) | (d.saturating_sub(1) << 3);
        }
    }
    if want_enums {
        let skips = merged_skips(&dirs, &skipped);
        let depth_at = |off: u32| -> u32 {
            let idx = depth_after.partition_point(|&(o, _)| o < off);
            let raw = if idx == 0 { 0 } else { depth_after[idx - 1].1 };
            raw.saturating_sub(u32::from(guarded))
        };
        let mut lines = LineCounter {
            src,
            at: 0,
            line: 1,
        };
        for decl in find_enums(src, &skips) {
            let kline = lines.line_of(decl.keyword as usize);
            if let Some((a, b)) = decl.tag {
                let mut key = Vec::with_capacity(ENUM_TAG_PREFIX.len() + (b - a) as usize);
                key.extend_from_slice(ENUM_TAG_PREFIX.as_bytes());
                key.extend_from_slice(&src[a as usize..b as usize]);
                let kd = depth_byte(DefKind::EnumTag, depth_at(decl.keyword));
                push_post(out, &key, kline, decl.keyword, kd);
            }
            for m in &decl.members {
                let line = lines.line_of(m.start as usize);
                let kd = depth_byte(DefKind::Enumerator, depth_at(m.start));
                push_post(
                    out,
                    &src[m.start as usize..m.end as usize],
                    line,
                    decl.keyword,
                    kd,
                );
            }
        }
    }
    guarded
}

const KEYWORDS: &[&[u8]] = &[
    b"auto",
    b"break",
    b"case",
    b"char",
    b"const",
    b"continue",
    b"default",
    b"do",
    b"double",
    b"else",
    b"enum",
    b"extern",
    b"float",
    b"for",
    b"goto",
    b"if",
    b"inline",
    b"int",
    b"long",
    b"register",
    b"restrict",
    b"return",
    b"short",
    b"signed",
    b"sizeof",
    b"static",
    b"struct",
    b"switch",
    b"typedef",
    b"union",
    b"unsigned",
    b"void",
    b"volatile",
    b"while",
    b"_Bool",
    b"_Complex",
    b"_Static_assert",
    b"_Alignof",
    b"_Alignas",
    b"_Generic",
    b"_Thread_local",
    b"typeof",
    b"__typeof__",
    b"__typeof",
    b"asm",
    b"__asm__",
    b"__asm",
    b"__attribute__",
    b"__attribute",
    b"__extension__",
    b"__volatile__",
    b"__inline__",
    b"__inline",
    b"__VA_ARGS__",
    b"__VA_OPT__",
];

fn ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn skip_literal(t: &[u8], mut i: usize) -> usize {
    let q = t[i];
    i += 1;
    while i < t.len() && t[i] != q {
        i += if t[i] == b'\\' { 2 } else { 1 };
    }
    i + 1
}

fn push_uses(out: &mut Extracted, raw: &[u8], line: u32, off: u32, kd: u8) {
    let text = crate::syntax::lex::splice(raw);
    let t: &[u8] = &text;
    let blank = |i: &mut usize| {
        while *i < t.len() && matches!(t[*i], b' ' | b'\t') {
            *i += 1;
        }
    };
    let mut i = 0;
    blank(&mut i);
    if t.get(i) != Some(&b'#') {
        return;
    }
    i += 1;
    blank(&mut i);
    if !t[i..].starts_with(b"define") {
        return;
    }
    i += 6;
    blank(&mut i);
    let start = i;
    while i < t.len() && ident(t[i]) {
        i += 1;
    }
    let name = &t[start..i];
    if name.is_empty() {
        return;
    }
    let mut params: Vec<&[u8]> = Vec::new();
    if t.get(i) == Some(&b'(') {
        i += 1;
        while i < t.len() && t[i] != b')' {
            if ident_start(t[i]) {
                let s = i;
                while i < t.len() && ident(t[i]) {
                    i += 1;
                }
                params.push(&t[s..i]);
            } else {
                i += 1;
            }
        }
        i += 1;
    }
    let mut seen: Vec<&[u8]> = Vec::new();
    while i < t.len() {
        let c = t[i];
        if c == b'"' || c == b'\'' {
            i = skip_literal(t, i);
        } else if c == b'/' && t.get(i + 1) == Some(&b'*') {
            i = memchr::memmem::find(&t[i + 2..], b"*/").map_or(t.len(), |e| i + 2 + e + 2);
        } else if c == b'/' && t.get(i + 1) == Some(&b'/') {
            break;
        } else if c.is_ascii_digit() || (c == b'.' && t.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            i += 1;
            while i < t.len()
                && (ident(t[i])
                    || t[i] == b'.'
                    || (matches!(t[i], b'+' | b'-')
                        && matches!(t[i - 1], b'e' | b'E' | b'p' | b'P')))
            {
                i += 1;
            }
        } else if ident_start(c) {
            let s = i;
            while i < t.len() && ident(t[i]) {
                i += 1;
            }
            let id = &t[s..i];
            if id != name && !params.contains(&id) && !KEYWORDS.contains(&id) && !seen.contains(&id)
            {
                seen.push(id);
            }
        } else {
            i += 1;
        }
    }
    for id in seen {
        let Ok(id) = std::str::from_utf8(id) else {
            continue;
        };
        let (name_at, name_len) = out.store(USE_PREFIX, id);
        out.posts.push(RawPost {
            name_at,
            name_len,
            line,
            off,
            kd,
        });
    }
}

fn push_post(out: &mut Extracted, name: &[u8], line: u32, off: u32, kd: u8) {
    let Ok(name) = std::str::from_utf8(name) else {
        return;
    };
    let (name_at, name_len) = out.store("", name);
    out.posts.push(RawPost {
        name_at,
        name_len,
        line,
        off,
        kd,
    });
    out.defs += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_defines_with_depth_and_guard() {
        let src = b"#ifndef _X_H\n#define _X_H\n#define A 1\n#ifdef CONFIG_B\n#define F(x) (x)\n#undef A\n#endif\nenum e { E1, E2 = 3 };\n#endif\n";
        let mut p = Extracted::default();
        let guarded = extract(src, FileClass::Header, &mut p);
        assert!(guarded);
        let got: Vec<(String, u32, u8, u8)> = p
            .posts
            .iter()
            .map(|r| {
                let n = p.names[r.name_at as usize..(r.name_at + r.name_len) as usize].to_string();
                (n, r.line, r.kd & 7, r.kd >> 3)
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("_X_H".into(), 2, DefKind::Object as u8, 0),
                ("A".into(), 3, DefKind::Object as u8, 0),
                ("F".into(), 5, DefKind::Function as u8, 1),
                ("A".into(), 6, DefKind::Undef as u8, 1),
                ("enum e".into(), 8, DefKind::EnumTag as u8, 0),
                ("E1".into(), 8, DefKind::Enumerator as u8, 0),
                ("E2".into(), 8, DefKind::Enumerator as u8, 0),
            ]
        );
    }
}
