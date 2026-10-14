use super::lex::{is_ident_cont, is_ident_start, splice};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirLine {
    pub off: u32,
    pub end: u32,
    pub line: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DirKind {
    Define,
    Undef,
    If,
    Ifdef,
    Ifndef,
    Elif,
    Elifdef,
    Elifndef,
    Else,
    Endif,
    Include,
    IncludeNext,
    Error,
    Warning,
    Pragma,
    Line,
    Ident,
    Assert,
    Unassert,
    Null,
    Unknown,
}

impl DirKind {
    pub fn from_word(w: &[u8]) -> DirKind {
        match w {
            b"define" => DirKind::Define,
            b"undef" => DirKind::Undef,
            b"if" => DirKind::If,
            b"ifdef" => DirKind::Ifdef,
            b"ifndef" => DirKind::Ifndef,
            b"elif" => DirKind::Elif,
            b"elifdef" => DirKind::Elifdef,
            b"elifndef" => DirKind::Elifndef,
            b"else" => DirKind::Else,
            b"endif" => DirKind::Endif,
            b"include" | b"import" => DirKind::Include,
            b"include_next" => DirKind::IncludeNext,
            b"error" => DirKind::Error,
            b"warning" => DirKind::Warning,
            b"pragma" => DirKind::Pragma,
            b"line" => DirKind::Line,
            b"ident" | b"sccs" => DirKind::Ident,
            b"assert" => DirKind::Assert,
            b"unassert" => DirKind::Unassert,
            _ => DirKind::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DirKind::Define => "define",
            DirKind::Undef => "undef",
            DirKind::If => "if",
            DirKind::Ifdef => "ifdef",
            DirKind::Ifndef => "ifndef",
            DirKind::Elif => "elif",
            DirKind::Elifdef => "elifdef",
            DirKind::Elifndef => "elifndef",
            DirKind::Else => "else",
            DirKind::Endif => "endif",
            DirKind::Include => "include",
            DirKind::IncludeNext => "include_next",
            DirKind::Error => "error",
            DirKind::Warning => "warning",
            DirKind::Pragma => "pragma",
            DirKind::Line => "line",
            DirKind::Ident => "ident",
            DirKind::Assert => "assert",
            DirKind::Unassert => "unassert",
            DirKind::Null => "",
            DirKind::Unknown => "?",
        }
    }
}

static INTERESTING: [bool; 256] = {
    let mut t = [false; 256];
    t[b'\n' as usize] = true;
    t[b'\\' as usize] = true;
    t[b'/' as usize] = true;
    t[b'"' as usize] = true;
    t[b'\'' as usize] = true;
    t
};

fn simd_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let forced_scalar = std::env::var_os("KMX_SCAN").is_some_and(|v| v == "scalar");
        !forced_scalar && avx2_available()
    })
}

#[cfg(target_arch = "x86_64")]
fn avx2_available() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
}

#[cfg(not(target_arch = "x86_64"))]
fn avx2_available() -> bool {
    false
}

#[inline]
fn skip_plain_scalar(src: &[u8], mut i: usize) -> usize {
    while i < src.len() && !INTERESTING[src[i] as usize] {
        i += 1;
    }
    i
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn skip_plain_avx2(src: &[u8], mut i: usize) -> usize {
    use std::arch::x86_64::*;
    let n = src.len();
    let nl = _mm256_set1_epi8(b'\n' as i8);
    let bs = _mm256_set1_epi8(b'\\' as i8);
    let sl = _mm256_set1_epi8(b'/' as i8);
    let dq = _mm256_set1_epi8(b'"' as i8);
    let sq = _mm256_set1_epi8(b'\'' as i8);
    while i + 32 <= n {
        let v = unsafe { _mm256_loadu_si256(src.as_ptr().add(i).cast()) };
        let a = _mm256_or_si256(_mm256_cmpeq_epi8(v, nl), _mm256_cmpeq_epi8(v, bs));
        let b = _mm256_or_si256(_mm256_cmpeq_epi8(v, sl), _mm256_cmpeq_epi8(v, dq));
        let m = _mm256_or_si256(_mm256_or_si256(a, b), _mm256_cmpeq_epi8(v, sq));
        let mask = _mm256_movemask_epi8(m) as u32;
        if mask != 0 {
            return i + mask.trailing_zeros() as usize;
        }
        i += 32;
    }
    skip_plain_scalar(src, i)
}

#[inline]
fn skip_plain(src: &[u8], i: usize) -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        if simd_enabled() {
            return unsafe { skip_plain_avx2(src, i) };
        }
    }
    skip_plain_scalar(src, i)
}

#[inline]
fn count_nl(s: &[u8]) -> u32 {
    memchr::memchr_iter(b'\n', s).count() as u32
}

#[inline]
fn skip_block_comment(src: &[u8], from: usize, line: &mut u32) -> usize {
    match memchr::memmem::find(&src[from..], b"*/") {
        Some(j) => {
            *line += count_nl(&src[from..from + j]);
            from + j + 2
        }
        None => {
            *line += count_nl(&src[from..]);
            src.len()
        }
    }
}

#[inline]
fn skip_line_comment(src: &[u8], mut from: usize, line: &mut u32) -> usize {
    loop {
        match memchr::memchr(b'\n', &src[from..]) {
            None => return src.len(),
            Some(j) => {
                let nl = from + j;
                let mut k = nl;
                if k > 0 && src[k - 1] == b'\r' {
                    k -= 1;
                }
                if k > 0 && src[k - 1] == b'\\' {
                    *line += 1;
                    from = nl + 1;
                    continue;
                }
                return nl;
            }
        }
    }
}

#[inline]
fn skip_literal(src: &[u8], at: usize, line: &mut u32) -> usize {
    let q = src[at];
    let mut i = at + 1;
    let mut extra = 0u32;
    while i < src.len() {
        let c = src[i];
        if c == b'\\' {
            if src.get(i + 1) == Some(&b'\n') {
                extra += 1;
            } else if src.get(i + 1) == Some(&b'\r') && src.get(i + 2) == Some(&b'\n') {
                extra += 1;
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == q {
            *line += extra;
            return i + 1;
        }
        if c == b'\n' {
            return at + 1;
        }
        i += 1;
    }
    at + 1
}

fn directive_end(src: &[u8], mut i: usize, line: &mut u32) -> usize {
    let n = src.len();
    while i < n {
        match src[i] {
            b'\n' => return i,
            b'\\' => {
                if src.get(i + 1) == Some(&b'\n') {
                    *line += 1;
                    i += 2;
                } else if src.get(i + 1) == Some(&b'\r') && src.get(i + 2) == Some(&b'\n') {
                    *line += 1;
                    i += 3;
                } else {
                    i += 1;
                }
            }
            b'/' => match src.get(i + 1) {
                Some(b'*') => i = skip_block_comment(src, i + 2, line),
                Some(b'/') => i = skip_line_comment(src, i + 2, line),
                _ => i += 1,
            },
            b'"' | b'\'' => i = skip_literal(src, i, line),
            _ => i = skip_plain(src, i + 1),
        }
    }
    n
}

pub fn directive_at(src: &[u8], off: usize) -> Option<std::ops::Range<usize>> {
    (src.get(off) == Some(&b'#')).then(|| {
        let mut line = 0;
        off..directive_end(src, off + 1, &mut line)
    })
}

pub fn scan(src: &[u8], out: &mut Vec<DirLine>) {
    scan_full(src, out, None);
}

pub fn scan_full(src: &[u8], out: &mut Vec<DirLine>, mut skipped: Option<&mut Vec<(u32, u32)>>) {
    let n = src.len();
    let mut i = if src.starts_with(&[0xef, 0xbb, 0xbf]) {
        3
    } else {
        0
    };
    let mut line: u32 = 1;
    let mut bol = true;
    let mut note = |a: usize, b: usize| {
        if let Some(v) = skipped.as_deref_mut() {
            v.push((a as u32, b as u32));
        }
    };
    while i < n {
        let c = src[i];
        match c {
            b'\n' => {
                line += 1;
                bol = true;
                i += 1;
            }
            b' ' | b'\t' | b'\r' | 0x0b | 0x0c => i += 1,
            b'\\' => {
                if src.get(i + 1) == Some(&b'\n') {
                    line += 1;
                    i += 2;
                } else if src.get(i + 1) == Some(&b'\r') && src.get(i + 2) == Some(&b'\n') {
                    line += 1;
                    i += 3;
                } else {
                    bol = false;
                    i += 1;
                }
            }
            b'/' => match src.get(i + 1) {
                Some(b'*') => {
                    let end = skip_block_comment(src, i + 2, &mut line);
                    note(i, end);
                    i = end;
                }
                Some(b'/') => {
                    let end = skip_line_comment(src, i + 2, &mut line);
                    note(i, end);
                    i = end;
                }
                _ => {
                    bol = false;
                    i += 1;
                }
            },
            b'#' if bol => {
                let start_line = line;
                let end = directive_end(src, i + 1, &mut line);
                out.push(DirLine {
                    off: i as u32,
                    end: end as u32,
                    line: start_line,
                });
                i = end;
                bol = false;
            }
            b'"' | b'\'' => {
                let end = skip_literal(src, i, &mut line);
                if end > i + 1 {
                    note(i, end);
                }
                i = end;
                bol = false;
            }
            _ => {
                bol = false;
                i = skip_plain(src, i + 1);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head<'a> {
    pub kind: DirKind,
    pub name: Option<std::borrow::Cow<'a, [u8]>>,
    pub function_like: bool,
}

fn skip_hspace(s: &[u8], mut i: usize, end: usize) -> usize {
    while i < end {
        match s[i] {
            b' ' | b'\t' | b'\r' | 0x0b | 0x0c => i += 1,
            b'/' if i + 1 < end && s[i + 1] == b'*' => {
                match memchr::memmem::find(&s[i + 2..end], b"*/") {
                    Some(j) => i = i + 2 + j + 2,
                    None => return end,
                }
            }
            b'/' if i + 1 < end && s[i + 1] == b'/' => return end,
            _ => break,
        }
    }
    i
}

pub fn parse_head(src: &[u8], d: DirLine) -> Head<'_> {
    let raw = &src[d.off as usize..d.end as usize];
    if memchr::memchr(b'\\', raw).is_some()
        && let std::borrow::Cow::Owned(v) = splice(raw)
    {
        let (kind, name) = parse_head_plain(&v);
        let function_like = name.as_ref().is_some_and(|r| v.get(r.end) == Some(&b'('));
        return Head {
            kind,
            name: name.map(|r| std::borrow::Cow::Owned(v[r].to_vec())),
            function_like,
        };
    }
    let (kind, name) = parse_head_plain(raw);
    let function_like = name.as_ref().is_some_and(|r| raw.get(r.end) == Some(&b'('));
    Head {
        kind,
        name: name.map(|r| std::borrow::Cow::Borrowed(&raw[r])),
        function_like,
    }
}

fn parse_head_plain(s: &[u8]) -> (DirKind, Option<std::ops::Range<usize>>) {
    let end = s.len();
    let mut i = skip_hspace(s, 1, end);
    let ws = i;
    while i < end && is_ident_cont(s[i], false) {
        i += 1;
    }
    let word = &s[ws..i];
    let kind = if word.is_empty() {
        if skip_hspace(s, i, end) >= end {
            DirKind::Null
        } else {
            DirKind::Unknown
        }
    } else if word[0].is_ascii_digit() {
        DirKind::Line
    } else {
        DirKind::from_word(word)
    };
    if matches!(
        kind,
        DirKind::Define
            | DirKind::Undef
            | DirKind::Ifdef
            | DirKind::Ifndef
            | DirKind::Elifdef
            | DirKind::Elifndef
    ) {
        let j = skip_hspace(s, i, end);
        if j < end && is_ident_start(s[j], false) {
            let mut k = j + 1;
            while k < end && is_ident_cont(s[k], false) {
                k += 1;
            }
            return (kind, Some(j..k));
        }
    }
    (kind, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(s: &str) -> Vec<(u32, String)> {
        let mut v = Vec::new();
        scan(s.as_bytes(), &mut v);
        v.iter()
            .map(|d| (d.line, s[d.off as usize..d.end as usize].to_string()))
            .collect()
    }

    #[test]
    fn finds_simple_directives() {
        let d = dirs("#define A 1\nint x;\n  #  if B\n#endif\n");
        assert_eq!(
            d,
            vec![
                (1, "#define A 1".into()),
                (3, "#  if B".into()),
                (4, "#endif".into())
            ]
        );
    }

    #[test]
    fn hash_inside_block_comment_is_not_directive() {
        let d = dirs("/* x\n#define NO 1\n*/\n#define YES 2\n");
        assert_eq!(d, vec![(4, "#define YES 2".into())]);
    }

    #[test]
    fn directive_after_comment_on_same_line() {
        let d = dirs("/* c */ #define A 1\n");
        assert_eq!(d, vec![(1, "#define A 1".into())]);
    }

    #[test]
    fn mid_line_hash_not_directive() {
        let d = dirs("x = 1; #define NO\nint /* a\n */ #define NO2\n");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn continuation_and_spanning_comment() {
        let src =
            "#define M(a) \\\n  ((a) + 1)\n#define C 1 /* spans\n lines */ + 2\nnext\n#define D\n";
        let d = dirs(src);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].0, 1);
        assert_eq!(d[1].0, 3);
        assert!(d[1].1.ends_with("+ 2"));
        assert_eq!(d[2].0, 6);
    }

    #[test]
    fn quotes_hide_comment_markers() {
        let d = dirs("char *s = \"/*\";\n#define A 1\nchar c = '\"';\n#define B 2\n");
        assert_eq!(d.len(), 2);
        assert_eq!(d[1].0, 4);
    }

    #[test]
    fn unterminated_apostrophe_in_asm_comment() {
        let d = dirs("# don't panic\n#define X 1\n");
        assert_eq!(d.len(), 2);
        assert_eq!(d[1], (2, "#define X 1".into()));
    }

    #[test]
    fn line_comment_continuation() {
        let d = dirs("// comment \\\n#define HIDDEN 1\n#define SEEN 2\n");
        assert_eq!(d, vec![(3, "#define SEEN 2".into())]);
    }

    #[test]
    fn simd_and_scalar_agree() {
        let mut text = String::new();
        for i in 0..400 {
            text.push_str(&format!(
                "int v{i} = 1; /* c{i} */ char *s = \"#x\"; // tail {i}\n"
            ));
            if i % 7 == 0 {
                text.push_str(&format!("#define M{i}(x) \\\n  ((x) + '\\'' + {i})\n"));
            }
        }
        let b = text.as_bytes();
        for start in 0..b.len().min(200) {
            assert_eq!(skip_plain(b, start), skip_plain_scalar(b, start));
        }
        let mut d = Vec::new();
        scan(b, &mut d);
        assert_eq!(d.len(), 58);
    }

    #[test]
    fn records_skipped_ranges() {
        let src = b"a /* c */ \"s\" // x\nb\n";
        let mut d = Vec::new();
        let mut r = Vec::new();
        scan_full(src, &mut d, Some(&mut r));
        let parts: Vec<&[u8]> = r
            .iter()
            .map(|&(a, b)| &src[a as usize..b as usize])
            .collect();
        assert_eq!(parts, vec![&b"/* c */"[..], &b"\"s\""[..], &b"// x"[..]]);
    }

    #[test]
    fn heads() {
        let src = b"#define PAGE_SIZE (1)\n#  ifndef  FOO\n# include <x.h>\n#\n# 12 \"f\"\n#if X\n";
        let mut v = Vec::new();
        scan(src, &mut v);
        let h: Vec<_> = v.iter().map(|d| parse_head(src, *d)).collect();
        assert_eq!(h[0].kind, DirKind::Define);
        assert_eq!(h[0].name.as_deref(), Some(&b"PAGE_SIZE"[..]));
        assert!(!h[0].function_like);
        assert_eq!(h[1].kind, DirKind::Ifndef);
        assert_eq!(h[1].name.as_deref(), Some(&b"FOO"[..]));
        assert_eq!(h[2].kind, DirKind::Include);
        assert_eq!(h[3].kind, DirKind::Null);
        assert_eq!(h[4].kind, DirKind::Line);
        assert_eq!(h[5].kind, DirKind::If);
    }

    #[test]
    fn head_with_splice_in_name_position() {
        let src = b"#define \\\n  LONG_NAME 1\n";
        let mut v = Vec::new();
        scan(src, &mut v);
        let h = parse_head(src, v[0]);
        assert_eq!(h.kind, DirKind::Define);
        assert_eq!(h.name.as_deref(), Some(&b"LONG_NAME"[..]));
    }
}
