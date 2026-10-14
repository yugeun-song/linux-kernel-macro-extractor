use rustc_hash::FxHashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Sym(pub u32);

macro_rules! predefined {
    ($($name:ident = $text:expr),* $(,)?) => {
        pub mod sym {
            use super::Sym;
            predefined!(@consts 0u32; $($name),*);
        }
        const PREDEFINED: &[&str] = &[$($text),*];
    };
    (@consts $n:expr; $first:ident $(, $rest:ident)*) => {
        pub const $first: Sym = Sym($n);
        predefined!(@consts $n + 1u32; $($rest),*);
    };
    (@consts $n:expr;) => {};
}

predefined! {
    EMPTY = "",
    HASH = "#",
    HASHHASH = "##",
    LPAREN = "(",
    RPAREN = ")",
    COMMA = ",",
    ELLIPSIS = "...",
    LT = "<",
    GT = ">",
    LSHIFT = "<<",
    RSHIFT = ">>",
    LE = "<=",
    GE = ">=",
    EQEQ = "==",
    NE = "!=",
    ANDAND = "&&",
    OROR = "||",
    PLUS = "+",
    MINUS = "-",
    STAR = "*",
    SLASH = "/",
    PERCENT = "%",
    AMP = "&",
    PIPE = "|",
    CARET = "^",
    TILDE = "~",
    BANG = "!",
    QUESTION = "?",
    COLON = ":",
    SEMI = ";",
    EQ = "=",
    LBRACKET = "[",
    RBRACKET = "]",
    LBRACE = "{",
    RBRACE = "}",
    DOT = ".",
    ARROW = "->",
    DEFINED = "defined",
    VA_ARGS = "__VA_ARGS__",
    VA_OPT = "__VA_OPT__",
    HAS_INCLUDE = "__has_include",
    HAS_INCLUDE_NEXT = "__has_include_next",
    HAS_ATTRIBUTE = "__has_attribute",
    HAS_C_ATTRIBUTE = "__has_c_attribute",
    HAS_CPP_ATTRIBUTE = "__has_cpp_attribute",
    HAS_BUILTIN = "__has_builtin",
    HAS_FEATURE = "__has_feature",
    HAS_EXTENSION = "__has_extension",
    HAS_EMBED = "__has_embed",
    FILE_ = "__FILE__",
    LINE_ = "__LINE__",
    COUNTER_ = "__COUNTER__",
    BASE_FILE_ = "__BASE_FILE__",
    INCLUDE_LEVEL_ = "__INCLUDE_LEVEL__",
    DATE_ = "__DATE__",
    TIME_ = "__TIME__",
    TIMESTAMP_ = "__TIMESTAMP__",
    PRAGMA_OP = "_Pragma",
    TRUE_ = "true",
    FALSE_ = "false",
}

pub struct Interner {
    map: FxHashMap<&'static str, Sym>,
    strs: Vec<&'static str>,
    chunk: String,
    full: Vec<String>,
}

const CHUNK: usize = 64 << 10;

impl Default for Interner {
    fn default() -> Self {
        Self::new()
    }
}

impl Interner {
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    pub fn with_capacity(n: usize) -> Self {
        let n = n.max(PREDEFINED.len());
        let mut it = Interner {
            map: FxHashMap::with_capacity_and_hasher(n, Default::default()),
            strs: Vec::with_capacity(n),
            chunk: String::new(),
            full: Vec::new(),
        };
        for s in PREDEFINED {
            it.intern(s);
        }
        it
    }

    pub fn intern(&mut self, s: &str) -> Sym {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = Sym(self.strs.len() as u32);
        let stored = self.store(s);
        self.strs.push(stored);
        self.map.insert(stored, id);
        id
    }

    fn store(&mut self, s: &str) -> &'static str {
        if self.chunk.capacity() - self.chunk.len() < s.len() {
            let fresh = String::with_capacity(CHUNK.max(s.len()));
            self.full.push(std::mem::replace(&mut self.chunk, fresh));
        }
        let start = self.chunk.len();
        self.chunk.push_str(s);
        let stored: *const str = &self.chunk[start..];
        unsafe { &*stored }
    }

    pub fn intern_bytes(&mut self, b: &[u8]) -> Sym {
        match std::str::from_utf8(b) {
            Ok(s) => self.intern(s),
            Err(_) => self.intern(&String::from_utf8_lossy(b)),
        }
    }

    pub fn lookup(&self, s: &str) -> Option<Sym> {
        self.map.get(s).copied()
    }

    #[inline]
    pub fn get(&self, s: Sym) -> &str {
        self.strs[s.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.strs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predefined_ids_are_stable() {
        let it = Interner::new();
        assert_eq!(it.get(sym::HASHHASH), "##");
        assert_eq!(it.get(sym::DEFINED), "defined");
        assert_eq!(it.lookup("__VA_ARGS__"), Some(sym::VA_ARGS));
    }

    #[test]
    fn strings_survive_chunk_changes() {
        let mut it = Interner::with_capacity(4);
        let words: Vec<String> = (0..20_000)
            .map(|i| format!("NAME_{i}_{}", "x".repeat(i % 40)))
            .collect();
        let syms: Vec<Sym> = words.iter().map(|w| it.intern(w)).collect();
        let big = "y".repeat(CHUNK * 2);
        let b = it.intern(&big);
        for (w, s) in words.iter().zip(&syms) {
            assert_eq!(it.get(*s), w);
            assert_eq!(it.intern(w), *s);
        }
        assert_eq!(it.get(b), big);
        assert_eq!(it.get(sym::DEFINED), "defined");
    }

    #[test]
    fn intern_dedups() {
        let mut it = Interner::new();
        let a = it.intern("PAGE_SIZE");
        let b = it.intern("PAGE_SIZE");
        assert_eq!(a, b);
        assert_eq!(it.get(a), "PAGE_SIZE");
    }
}
