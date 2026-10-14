use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use rustc_hash::FxHashMap;

struct Roots {
    src_given: PathBuf,
    src_abs: PathBuf,
    src_label: String,
    obj_given: PathBuf,
    obj_abs: PathBuf,
    obj_label: String,
}

static ROOTS: OnceLock<Roots> = OnceLock::new();

fn label(abs: &Path) -> String {
    abs.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| ".".to_string())
}

pub fn configure(src_given: &Path, src_abs: &Path, obj_given: Option<&Path>, obj_abs: &Path) {
    let _ = ROOTS.set(Roots {
        src_given: src_given.to_path_buf(),
        src_abs: src_abs.to_path_buf(),
        src_label: label(src_abs),
        obj_given: obj_given.unwrap_or(src_given).to_path_buf(),
        obj_abs: obj_abs.to_path_buf(),
        obj_label: label(obj_abs),
    });
}

fn relative(rel: &str) -> bool {
    !(rel.is_empty() || Path::new(rel).is_absolute() || rel.starts_with("../"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Under {
    Src,
    Obj,
    Neither,
}

fn under(r: &Roots, rel: &str) -> Under {
    static KNOWN: OnceLock<Mutex<FxHashMap<String, Under>>> = OnceLock::new();
    let known = KNOWN.get_or_init(Mutex::default);
    if let Some(&u) = known
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(rel)
    {
        return u;
    }
    let u = if r.src_abs.join(rel).exists() {
        Under::Src
    } else if r.obj_abs.join(rel).exists() {
        Under::Obj
    } else {
        Under::Neither
    };
    known
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(rel.to_string(), u);
    u
}

pub fn shown(rel: &str) -> Cow<'_, str> {
    let Some(r) = ROOTS.get() else {
        return Cow::Borrowed(rel);
    };
    if !relative(rel) {
        return Cow::Borrowed(rel);
    }
    match under(r, rel) {
        Under::Src => Cow::Owned(format!("{}/{rel}", r.src_label)),
        Under::Obj => Cow::Owned(format!("{}/{rel}", r.obj_label)),
        Under::Neither => Cow::Borrowed(rel),
    }
}

pub fn openable(rel: &str) -> Cow<'_, str> {
    let Some(r) = ROOTS.get() else {
        return Cow::Borrowed(rel);
    };
    if !relative(rel) {
        return Cow::Borrowed(rel);
    }
    match under(r, rel) {
        Under::Src => Cow::Owned(r.src_given.join(rel).display().to_string()),
        Under::Obj => Cow::Owned(r.obj_given.join(rel).display().to_string()),
        Under::Neither => Cow::Borrowed(rel),
    }
}

pub fn roots() -> Option<(PathBuf, Option<PathBuf>)> {
    let r = ROOTS.get()?;
    let obj = (r.obj_abs != r.src_abs).then(|| r.obj_abs.clone());
    Some((r.src_abs.clone(), obj))
}

fn path_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'.' | b'-')
}

fn path_tokens(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let b = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < b.len() {
            if !path_char(b[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < b.len() && path_char(b[i]) {
                i += 1;
            }
            let mut end = i;
            while end + 1 < b.len() && b[end] == b'/' && path_char(b[end + 1]) {
                end += 1;
                while end < b.len() && path_char(b[end]) {
                    end += 1;
                }
            }
            if end > i {
                i = end;
                return Some((start, end));
            }
        }
        None
    })
}

fn escapes(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let b = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while let Some(at) = memchr::memchr(0x1b, &b[i..]) {
            let start = i + at;
            i = start + 1;
            if b.get(i) != Some(&b'[') {
                continue;
            }
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';') {
                j += 1;
            }
            if b.get(j) == Some(&b'm') {
                i = j + 1;
                return Some((start, i));
            }
        }
        i = b.len();
        None
    })
}

fn rewrite(segment: &str, out: &mut String) -> bool {
    let mut changed = false;
    let mut last = 0;
    for (start, end) in path_tokens(segment) {
        let before = segment[..start].chars().next_back();
        if matches!(before, Some('/' | '<' | '"' | '.')) {
            continue;
        }
        if let Cow::Owned(s) = shown(&segment[start..end]) {
            out.push_str(&segment[last..start]);
            out.push_str(&s);
            last = end;
            changed = true;
        }
    }
    out.push_str(&segment[last..]);
    changed
}

pub fn in_text(text: &str) -> Cow<'_, str> {
    if ROOTS.get().is_none() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 256);
    let mut changed = false;
    let mut pos = 0;
    for (start, end) in escapes(text) {
        changed |= rewrite(&text[pos..start], &mut out);
        out.push_str(&text[start..end]);
        pos = end;
    }
    changed |= rewrite(&text[pos..], &mut out);
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_regex(re: &str, text: &str) -> Vec<(usize, usize)> {
        regex::Regex::new(re)
            .unwrap()
            .find_iter(text)
            .map(|m| (m.start(), m.end()))
            .collect()
    }

    #[test]
    fn scanners_match_the_patterns_they_replace() {
        let texts = [
            "include/linux/mm.h:12 and arch/arm64/include/asm/page.h",
            "a/ b// c//d e/f/ /g/h ./x/y ../p/q <linux/types.h> \"x/y.h\"",
            "tail/end",
            "\x1b[1mbold\x1b[0m \x1b[38;5;2mx\x1b[ \x1b[12 \x1b[m",
            "",
            "/",
            "a-b+c.d_e/f-g+h.i_j",
            "x\u{e9}/y and z/\u{e9}w",
        ];
        for t in texts {
            assert_eq!(
                path_tokens(t).collect::<Vec<_>>(),
                by_regex(r"[A-Za-z0-9_+.\-]+(?:/[A-Za-z0-9_+.\-]+)+", t),
                "{t:?}"
            );
            assert_eq!(
                escapes(t).collect::<Vec<_>>(),
                by_regex(r"\x1b\[[0-9;]*m", t),
                "{t:?}"
            );
        }
    }
}
