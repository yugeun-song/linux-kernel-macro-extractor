use crate::pp::macros::SrcFile;
use crate::pp::sim::Sim;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub line: u32,
    pub start: usize,
    pub start_line: u32,
    pub signature_end: usize,
    pub body: Option<(usize, usize)>,
    pub end_line: u32,
}

pub struct Function {
    pub file: SrcFile,
    pub found: Found,
    pub signature: String,
    pub kind: &'static str,
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
    line: u32,
}

impl Cursor<'_> {
    fn peek(&self, k: usize) -> Option<u8> {
        self.s.get(self.i + k).copied()
    }

    fn skip_gap(&mut self) {
        while let Some(c) = self.peek(0) {
            match c {
                b'\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                b' ' | b'\t' | b'\r' | 0x0b | 0x0c => self.i += 1,
                b'\\' if self.peek(1) == Some(b'\n') => {
                    self.line += 1;
                    self.i += 2;
                }
                b'/' if self.peek(1) == Some(b'*') => self.block_comment(),
                b'/' if self.peek(1) == Some(b'/') => self.line_comment(),
                _ => return,
            }
        }
    }

    fn block_comment(&mut self) {
        self.i += 2;
        while let Some(c) = self.peek(0) {
            if c == b'*' && self.peek(1) == Some(b'/') {
                self.i += 2;
                return;
            }
            if c == b'\n' {
                self.line += 1;
            }
            self.i += 1;
        }
    }

    fn line_comment(&mut self) {
        while let Some(c) = self.peek(0) {
            if c == b'\\' && self.peek(1) == Some(b'\n') {
                self.line += 1;
                self.i += 2;
                continue;
            }
            if c == b'\n' {
                return;
            }
            self.i += 1;
        }
    }

    fn literal(&mut self, quote: u8) {
        self.i += 1;
        while let Some(c) = self.peek(0) {
            match c {
                b'\\' => {
                    if self.peek(1) == Some(b'\n') {
                        self.line += 1;
                    }
                    self.i += 2;
                }
                b'\n' => return,
                c if c == quote => {
                    self.i += 1;
                    return;
                }
                _ => self.i += 1,
            }
        }
    }

    fn directive(&mut self) {
        while let Some(c) = self.peek(0) {
            match c {
                b'\\' if self.peek(1) == Some(b'\n') => {
                    self.line += 1;
                    self.i += 2;
                }
                b'/' if self.peek(1) == Some(b'*') => self.block_comment(),
                b'\n' => return,
                _ => self.i += 1,
            }
        }
    }

    fn ident(&mut self) -> &[u8] {
        let start = self.i;
        while self
            .peek(0)
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.i += 1;
        }
        &self.s[start..self.i]
    }

    fn matching(&mut self, open: u8, close: u8) -> bool {
        let mut depth = 0usize;
        while let Some(c) = self.peek(0) {
            match c {
                b'"' | b'\'' => {
                    self.literal(c);
                    continue;
                }
                b'/' if matches!(self.peek(1), Some(b'*' | b'/')) => {
                    self.skip_gap();
                    continue;
                }
                b'\n' => self.line += 1,
                c if c == open => depth += 1,
                c if c == close => {
                    depth -= 1;
                    if depth == 0 {
                        self.i += 1;
                        return true;
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
        false
    }
}

pub fn scan(src: &[u8], name: &str) -> Vec<Found> {
    let name = name.as_bytes();
    let mut c = Cursor {
        s: src,
        i: 0,
        line: 1,
    };
    let mut out = Vec::new();
    let mut braces = 0usize;
    let mut start = 0usize;
    let mut start_line = 1u32;
    let mut fresh = true;
    let mut initializer = false;
    let mut bol = true;
    while let Some(ch) = c.peek(0) {
        match ch {
            b'\n' => {
                c.line += 1;
                c.i += 1;
                bol = true;
                continue;
            }
            b' ' | b'\t' | b'\r' | 0x0b | 0x0c => {
                c.i += 1;
                continue;
            }
            b'\\' if c.peek(1) == Some(b'\n') => {
                c.line += 1;
                c.i += 2;
                continue;
            }
            b'/' if matches!(c.peek(1), Some(b'*' | b'/')) => {
                c.skip_gap();
                continue;
            }
            b'#' if bol => {
                c.directive();
                fresh = true;
                continue;
            }
            _ => {}
        }
        bol = false;
        if fresh && braces == 0 {
            start = c.i;
            start_line = c.line;
            fresh = false;
        }
        match ch {
            b'"' | b'\'' => c.literal(ch),
            b'{' => {
                braces += 1;
                c.i += 1;
            }
            b'}' => {
                braces = braces.saturating_sub(1);
                c.i += 1;
                if braces == 0 {
                    fresh = true;
                    initializer = false;
                }
            }
            b';' => {
                c.i += 1;
                if braces == 0 {
                    fresh = true;
                    initializer = false;
                }
            }
            b'=' if braces == 0 => {
                initializer = true;
                c.i += 1;
            }
            ch if ch.is_ascii_alphabetic() || ch == b'_' => {
                let line = c.line;
                let word_start = c.i;
                let word = c.ident();
                let candidate = word == name && braces == 0 && !initializer;
                let preceded = src[..word_start]
                    .iter()
                    .rev()
                    .find(|b| !b.is_ascii_whitespace())
                    .is_some_and(|b| *b != b'.' && *b != b'>');
                if !(candidate && preceded) {
                    continue;
                }
                let save = (c.i, c.line);
                c.skip_gap();
                if c.peek(0) != Some(b'(') || !c.matching(b'(', b')') {
                    (c.i, c.line) = save;
                    continue;
                }
                let signature_end = c.i;
                loop {
                    c.skip_gap();
                    match c.peek(0) {
                        Some(b'{') => {
                            let open = c.i;
                            if c.matching(b'{', b'}') {
                                out.push(Found {
                                    line,
                                    start,
                                    start_line,
                                    signature_end,
                                    body: Some((open, c.i - 1)),
                                    end_line: c.line,
                                });
                                fresh = true;
                                initializer = false;
                            }
                            break;
                        }
                        Some(b';') => {
                            out.push(Found {
                                line,
                                start,
                                start_line,
                                signature_end,
                                body: None,
                                end_line: c.line,
                            });
                            break;
                        }
                        Some(ch) if ch.is_ascii_alphabetic() || ch == b'_' => {
                            c.ident();
                            c.skip_gap();
                            if c.peek(0) == Some(b'(') {
                                c.matching(b'(', b')');
                            }
                        }
                        _ => break,
                    }
                }
            }
            _ => c.i += 1,
        }
    }
    out
}

fn collapse(text: &[u8]) -> String {
    let mut out = String::new();
    let mut c = Cursor {
        s: text,
        i: 0,
        line: 1,
    };
    let mut gap = false;
    while let Some(ch) = c.peek(0) {
        if ch.is_ascii_whitespace()
            || (ch == b'/' && matches!(c.peek(1), Some(b'*' | b'/')))
            || (ch == b'\\' && c.peek(1) == Some(b'\n'))
        {
            c.skip_gap();
            gap = true;
            continue;
        }
        if gap && !out.is_empty() && !out.ends_with('(') && ch != b')' && ch != b',' {
            out.push(' ');
        }
        gap = false;
        out.push(ch as char);
        c.i += 1;
    }
    out
}

pub fn kind_of(signature: &str, body: bool) -> &'static str {
    let words: Vec<&str> = signature
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .collect();
    let has = |w: &str| words.contains(&w);
    let inline = has("inline") || has("__always_inline") || has("__inline__") || has("__inline");
    match (body, has("static"), inline) {
        (false, ..) => "function declaration",
        (true, true, true) => "static inline function",
        (true, true, false) => "static function",
        (true, false, _) => "function",
    }
}

pub fn find(sim: &Sim<'_>, name: &str, prefer: Option<SrcFile>) -> Option<Function> {
    let mut order: Vec<SrcFile> = prefer.into_iter().collect();
    order.extend(
        (0..sim.files.len() as u32)
            .map(SrcFile)
            .filter(|f| Some(*f) != prefer && sim.visited(*f)),
    );
    let mut declaration = None;
    for file in order {
        let content = sim.files.content(file);
        if memchr::memmem::find(&content, name.as_bytes()).is_none() {
            continue;
        }
        for found in scan(&content, name) {
            if sim.skip_at(file, found.line).is_some() {
                continue;
            }
            let signature = collapse(&content[found.start..found.signature_end]);
            let kind = kind_of(&signature, found.body.is_some());
            let f = Function {
                file,
                found,
                signature,
                kind,
            };
            if f.found.body.is_some() {
                return Some(f);
            }
            declaration.get_or_insert(f);
        }
    }
    declaration
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "#define phys_to_virt phys_to_virt\n\
/* static inline void *phys_to_virt(int) { } */\n\
static inline void *phys_to_virt(phys_addr_t x)\n\
{\n\
\treturn (void *)(__phys_to_virt(x));\n\
}\n\
struct ops o = { .phys_to_virt = phys_to_virt, };\n\
int use(void) { return phys_to_virt(1) != 0; }\n\
extern void *phys_to_virt(phys_addr_t x) __attribute__((pure));\n\
EXPORT_SYMBOL(phys_to_virt);\n";

    #[test]
    fn finds_definitions_and_declarations_at_file_scope() {
        let found = scan(SRC.as_bytes(), "phys_to_virt");
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(
            (found[0].line, found[0].start_line, found[0].end_line),
            (3, 3, 6)
        );
        assert!(found[0].body.is_some());
        let sig = collapse(&SRC.as_bytes()[found[0].start..found[0].signature_end]);
        assert_eq!(sig, "static inline void *phys_to_virt(phys_addr_t x)");
        assert_eq!(kind_of(&sig, true), "static inline function");
        assert_eq!(found[1].line, 9);
        assert!(found[1].body.is_none());
    }

    #[test]
    fn multi_line_signatures_and_attributes() {
        let src = "static __always_inline unsigned long\nfoo(unsigned long a,\n    int b) __must_check\n{\n\treturn a;\n}\n";
        let found = scan(src.as_bytes(), "foo");
        assert_eq!(found.len(), 1);
        let sig = collapse(&src.as_bytes()[found[0].start..found[0].signature_end]);
        assert_eq!(
            sig,
            "static __always_inline unsigned long foo(unsigned long a, int b)"
        );
        assert_eq!(kind_of(&sig, true), "static inline function");
        assert_eq!(found[0].start_line, 1);
    }
}
