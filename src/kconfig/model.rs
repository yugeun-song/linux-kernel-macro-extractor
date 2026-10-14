use rustc_hash::FxHashMap;

pub type SymId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymType {
    Unknown,
    Bool,
    Tristate,
    String,
    Int,
    Hex,
}

impl SymType {
    pub fn name(self) -> &'static str {
        match self {
            SymType::Unknown => "unknown",
            SymType::Bool => "bool",
            SymType::Tristate => "tristate",
            SymType::String => "string",
            SymType::Int => "int",
            SymType::Hex => "hex",
        }
    }

    pub fn is_tristateish(self) -> bool {
        matches!(self, SymType::Bool | SymType::Tristate)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn text(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stand {
    Unknown,
    Owner(SymId),
    Target(SymId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    Yes,
    Sym(SymId),
    Const(String),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    Opaque(u32, Stand),
    Mod,
}

impl Expr {
    pub fn and(a: Expr, b: Expr) -> Expr {
        match (a, b) {
            (Expr::Yes, x) | (x, Expr::Yes) => x,
            (a, b) => Expr::And(Box::new(a), Box::new(b)),
        }
    }

    pub fn or(a: Expr, b: Expr) -> Expr {
        Expr::Or(Box::new(a), Box::new(b))
    }

    pub fn is_yes(&self) -> bool {
        matches!(self, Expr::Yes)
    }

    pub fn equals_n(self) -> Expr {
        let n = || Box::new(Expr::Const("n".into()));
        match self {
            Expr::And(a, b) => Expr::Or(Box::new(a.equals_n()), Box::new(b.equals_n())),
            Expr::Or(a, b) => Expr::And(Box::new(a.equals_n()), Box::new(b.equals_n())),
            Expr::Not(a) => a.differs_from_n(),
            c @ Expr::Cmp(..) => Expr::Not(Box::new(c)),
            other => Expr::Cmp(CmpOp::Eq, Box::new(other), n()),
        }
    }

    pub fn differs_from_n(self) -> Expr {
        let n = || Box::new(Expr::Const("n".into()));
        match self {
            Expr::And(a, b) => Expr::Not(Box::new(Expr::Or(
                Box::new(a.equals_n()),
                Box::new(b.equals_n()),
            ))),
            Expr::Or(a, b) => Expr::Not(Box::new(Expr::And(
                Box::new(a.equals_n()),
                Box::new(b.equals_n()),
            ))),
            Expr::Not(a) => a.equals_n(),
            c @ Expr::Cmp(..) => c,
            other => Expr::Cmp(CmpOp::Ne, Box::new(other), n()),
        }
    }

    pub fn rewrite_m(self) -> Expr {
        match self {
            Expr::Const(c) if c == "m" => Expr::Mod,
            Expr::Not(a) => Expr::Not(Box::new(a.rewrite_m())),
            Expr::And(a, b) => Expr::And(Box::new(a.rewrite_m()), Box::new(b.rewrite_m())),
            Expr::Or(a, b) => Expr::Or(Box::new(a.rewrite_m()), Box::new(b.rewrite_m())),
            other => other,
        }
    }

    pub fn symbols(&self, out: &mut Vec<SymId>) {
        match self {
            Expr::Sym(s) => out.push(*s),
            Expr::Not(a) => a.symbols(out),
            Expr::And(a, b) | Expr::Or(a, b) | Expr::Cmp(_, a, b) => {
                a.symbols(out);
                b.symbols(out);
            }
            _ => {}
        }
    }

    pub fn claim_opaque(&mut self, stand: Stand) -> bool {
        match self {
            Expr::Opaque(_, s) => {
                if *s == Stand::Unknown {
                    *s = stand;
                }
                true
            }
            Expr::Not(a) => a.claim_opaque(stand),
            Expr::And(a, b) | Expr::Or(a, b) | Expr::Cmp(_, a, b) => {
                let x = a.claim_opaque(stand);
                b.claim_opaque(stand) || x
            }
            _ => false,
        }
    }

    pub fn contains_sym(&self, sym: SymId) -> bool {
        match self {
            Expr::Sym(s) => *s == sym,
            Expr::Not(a) => a.contains_sym(sym),
            Expr::And(a, b) | Expr::Or(a, b) | Expr::Cmp(_, a, b) => {
                a.contains_sym(sym) || b.contains_sym(sym)
            }
            _ => false,
        }
    }

    pub fn requires_sym(&self, sym: SymId) -> bool {
        let is = |e: &Expr| matches!(e, Expr::Sym(s) if *s == sym);
        let constant =
            |e: &Expr, v: &[&str]| matches!(e, Expr::Const(c) if v.contains(&c.as_str()));
        match self {
            Expr::And(a, b) => a.requires_sym(sym) || b.requires_sym(sym),
            Expr::Sym(s) => *s == sym,
            Expr::Cmp(CmpOp::Eq, a, b) => is(a) && constant(b, &["y", "m"]),
            Expr::Cmp(CmpOp::Ne, a, b) => is(a) && constant(b, &["n"]),
            _ => false,
        }
    }

    pub fn conjuncts(&self) -> Vec<Expr> {
        fn go(e: &Expr, out: &mut Vec<Expr>) {
            match e {
                Expr::Yes => {}
                Expr::And(a, b) => {
                    go(a, out);
                    go(b, out);
                }
                Expr::Cmp(CmpOp::Eq, a, b) if matches!(&**b, Expr::Const(c) if c == "y") => {
                    out.push((**a).clone())
                }
                Expr::Cmp(CmpOp::Ne, a, b) if matches!(&**b, Expr::Const(c) if c == "n") => {
                    out.push((**a).clone())
                }
                other => out.push(other.clone()),
            }
        }
        let mut out = Vec::new();
        go(self, &mut out);
        out
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Loc {
    pub file: u32,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub struct Prompt {
    pub text: String,
    pub cond: Expr,
    pub loc: Loc,
}

#[derive(Clone, Debug)]
pub struct Default {
    pub value: Expr,
    pub cond: Expr,
    pub loc: Loc,
    pub opaque: bool,
}

#[derive(Clone, Debug)]
pub struct Select {
    pub target: SymId,
    pub cond: Expr,
    pub loc: Loc,
}

#[derive(Clone, Debug)]
pub struct Range {
    pub lo: Expr,
    pub hi: Expr,
    pub cond: Expr,
}

#[derive(Clone, Debug)]
pub struct Definition {
    pub loc: Loc,
    pub own_deps: Expr,
    pub deps: Expr,
    pub visible_if: Expr,
    pub prompts: Vec<Prompt>,
    pub defaults: Vec<Default>,
    pub selects: Vec<Select>,
    pub implies: Vec<Select>,
    pub ranges: Vec<Range>,
    pub help: Option<String>,
    pub opaque: bool,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub ty: SymType,
    pub defs: Vec<Definition>,
    pub selected_by: Vec<(SymId, Expr, Loc)>,
    pub implied_by: Vec<(SymId, Expr, Loc)>,
    pub choice: Option<u32>,
    pub modules: bool,
    pub transitional: bool,
    pub env: Option<String>,
}

impl Symbol {
    pub fn new(name: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            ty: SymType::Unknown,
            defs: Vec::new(),
            selected_by: Vec::new(),
            implied_by: Vec::new(),
            choice: None,
            modules: false,
            transitional: false,
            env: None,
        }
    }

    pub fn is_defined(&self) -> bool {
        !self.defs.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct Choice {
    pub loc: Loc,
    pub prompt: Option<Prompt>,
    pub deps: Expr,
    pub defaults: Vec<(SymId, Expr, Loc)>,
    pub members: Vec<SymId>,
    pub optional: bool,
    pub tristate: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Kconfig {
    pub symbols: Vec<Symbol>,
    pub by_name: FxHashMap<String, SymId>,
    pub choices: Vec<Choice>,
    pub files: Vec<String>,
    pub modules_sym: Option<SymId>,
    pub warnings: Vec<String>,
    pub opaque: Vec<String>,
}

impl Kconfig {
    pub fn sym(&self, s: SymId) -> &Symbol {
        &self.symbols[s as usize]
    }

    pub fn lookup(&self, name: &str) -> Option<SymId> {
        self.by_name
            .get(name.strip_prefix("CONFIG_").unwrap_or(name))
            .copied()
    }

    pub fn intern(&mut self, name: &str) -> SymId {
        if let Some(&s) = self.by_name.get(name) {
            return s;
        }
        let id = self.symbols.len() as SymId;
        self.symbols.push(Symbol::new(name));
        self.by_name.insert(name.to_string(), id);
        id
    }

    pub fn loc_text(&self, l: Loc) -> String {
        format!(
            "{}:{}",
            self.files.get(l.file as usize).map_or("?", String::as_str),
            l.line
        )
    }

    pub fn expr_text(&self, e: &Expr) -> String {
        fn go(k: &Kconfig, e: &Expr, parent: u8, out: &mut String) {
            match e {
                Expr::Yes => out.push('y'),
                Expr::Mod => out.push('m'),
                Expr::Sym(s) => out.push_str(&k.sym(*s).name),
                Expr::Const(c) => {
                    if c == "y"
                        || c == "m"
                        || c == "n"
                        || c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                    {
                        out.push_str(c);
                    } else {
                        out.push('"');
                        out.push_str(c);
                        out.push('"');
                    }
                }
                Expr::Not(a) => {
                    out.push('!');
                    go(k, a, 4, out);
                }
                Expr::Cmp(op, a, b) => {
                    go(k, a, 3, out);
                    out.push_str(op.text());
                    go(k, b, 3, out);
                }
                Expr::Opaque(id, _) => {
                    out.push_str(k.opaque.get(*id as usize).map_or("$(...)", String::as_str))
                }
                Expr::And(a, b) => {
                    if parent > 2 {
                        out.push('(');
                    }
                    go(k, a, 2, out);
                    out.push_str(" && ");
                    go(k, b, 2, out);
                    if parent > 2 {
                        out.push(')');
                    }
                }
                Expr::Or(a, b) => {
                    if parent > 1 {
                        out.push('(');
                    }
                    go(k, a, 1, out);
                    out.push_str(" || ");
                    go(k, b, 1, out);
                    if parent > 1 {
                        out.push(')');
                    }
                }
            }
        }
        let mut s = String::new();
        go(self, e, 0, &mut s);
        s
    }
}
