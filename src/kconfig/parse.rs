use std::cell::RefCell;
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

use super::model::{
    Choice, CmpOp, Default, Definition, Expr, Kconfig, Loc, Prompt, Range, Select, Stand, SymId,
    SymType,
};

#[derive(Clone, Debug)]
pub struct Env {
    pub arch: String,
    pub srcarch: String,
    pub kernelversion: String,
    pub srctree: PathBuf,
}

#[derive(Clone, Debug)]
struct Var {
    value: String,
    recursive: bool,
    opaque: bool,
}

enum Frame {
    If(Expr),
    Menu { deps: Expr, visible: Expr },
    Choice(u32),
}

enum Current {
    None,
    Config { sym: SymId, def: Box<Definition> },
    Choice(u32),
    Menu,
    Comment(Expr),
}

enum ChoiceItem {
    Config {
        sym: SymId,
        deps: Expr,
        prompt: Option<Expr>,
    },
    Comment {
        deps: Expr,
    },
    If {
        cond: Expr,
        items: Vec<ChoiceItem>,
    },
}

struct OpenChoice {
    id: u32,
    levels: Vec<(Expr, Vec<ChoiceItem>)>,
}

impl ChoiceItem {
    fn dep(&self) -> Expr {
        match self {
            ChoiceItem::Config {
                deps,
                prompt: Some(p),
                ..
            } => Expr::and(deps.clone(), p.clone()),
            ChoiceItem::Config { deps, .. } | ChoiceItem::Comment { deps } => deps.clone(),
            ChoiceItem::If { cond, .. } => cond.clone(),
        }
    }
}

fn submenu_end(items: &[ChoiceItem], i: usize) -> usize {
    let ChoiceItem::Config {
        sym,
        prompt: Some(_),
        ..
    } = &items[i]
    else {
        return i + 1;
    };
    let base = items[i].dep().conjuncts();
    let mut j = i + 1;
    while j < items.len() {
        let dep = items[j].dep();
        if !dep.contains_sym(*sym) {
            break;
        }
        if !dep.requires_sym(*sym) {
            let terms = dep.conjuncts();
            if !base.iter().all(|t| terms.contains(t)) {
                break;
            }
        }
        j = submenu_end(items, j);
    }
    j
}

fn choice_values(items: &[ChoiceItem], out: &mut Vec<SymId>) {
    let mut i = 0;
    while i < items.len() {
        match &items[i] {
            ChoiceItem::Config { sym, .. } => out.push(*sym),
            ChoiceItem::If { items: inner, .. } => choice_values(inner, out),
            ChoiceItem::Comment { .. } => {}
        }
        i = submenu_end(items, i);
    }
}

const OPAQUE_OPEN: char = '\u{2}';
const OPAQUE_CLOSE: char = '\u{3}';

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Word(String),
    Str(String),
    Op(&'static str),
}

const MAX_SOURCE_DEPTH: usize = 64;

pub struct Parser<'e> {
    env: &'e Env,
    k: Kconfig,
    vars: FxHashMap<String, Var>,
    frames: Vec<Frame>,
    cur: Current,
    file: u32,
    line: u32,
    depth: usize,
    open_choices: Vec<OpenChoice>,
    opaque: RefCell<Vec<String>>,
}

fn indent_of(s: &str) -> usize {
    let mut w = 0;
    for c in s.chars() {
        match c {
            ' ' => w += 1,
            '\t' => w = (w / 8 + 1) * 8,
            _ => break,
        }
    }
    w
}

fn tokenize(s: &str) -> Vec<Tok> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'#' {
            break;
        }
        if c == b'"' || c == b'\'' {
            let mut v = String::new();
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 1;
                }
                v.push(b[i] as char);
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(v));
            continue;
        }
        let two = if i + 1 < b.len() { &s[i..i + 2] } else { "" };
        let op2 = match two {
            "&&" => Some("&&"),
            "||" => Some("||"),
            "!=" => Some("!="),
            "<=" => Some("<="),
            ">=" => Some(">="),
            _ => None,
        };
        if let Some(o) = op2 {
            out.push(Tok::Op(o));
            i += 2;
            continue;
        }
        let op1 = match c {
            b'=' => Some("="),
            b'<' => Some("<"),
            b'>' => Some(">"),
            b'!' => Some("!"),
            b'(' => Some("("),
            b')' => Some(")"),
            _ => None,
        };
        if let Some(o) = op1 {
            out.push(Tok::Op(o));
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !b"\"'=<>!()&|#".contains(&b[i]) {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        out.push(Tok::Word(s[start..i].to_string()));
    }
    out
}

fn is_const_word(w: &str) -> bool {
    matches!(w, "y" | "m" | "n")
        || w.parse::<i64>().is_ok()
        || (w.starts_with("0x") || w.starts_with("0X")) && u64::from_str_radix(&w[2..], 16).is_ok()
        || w.starts_with('-') && w[1..].parse::<u64>().is_ok()
}

const KEYWORDS: &[&str] = &[
    "config",
    "menuconfig",
    "choice",
    "endchoice",
    "comment",
    "menu",
    "endmenu",
    "if",
    "endif",
    "source",
    "rsource",
    "osource",
    "orsource",
    "mainmenu",
    "bool",
    "boolean",
    "tristate",
    "string",
    "hex",
    "int",
    "def_bool",
    "def_tristate",
    "prompt",
    "default",
    "depends",
    "select",
    "imply",
    "range",
    "visible",
    "help",
    "---help---",
    "option",
    "modules",
    "transitional",
    "optional",
    "requires",
];

impl<'e> Parser<'e> {
    pub fn new(env: &'e Env) -> Parser<'e> {
        Parser {
            env,
            k: Kconfig::default(),
            vars: FxHashMap::default(),
            frames: Vec::new(),
            cur: Current::None,
            file: 0,
            line: 0,
            depth: 0,
            open_choices: Vec::new(),
            opaque: RefCell::new(Vec::new()),
        }
    }

    pub fn finish(mut self) -> Kconfig {
        self.finish_entry();
        while let Some(open) = self.open_choices.pop() {
            self.close_choice(open);
        }
        if self.k.modules_sym.is_none() {
            self.k.modules_sym = self.k.lookup("MODULES");
        }
        self.k.opaque = self.opaque.into_inner();
        self.k
    }

    fn opaque_token(&self, text: &str) -> String {
        let mut table = self.opaque.borrow_mut();
        table.push(text.to_string());
        format!("{OPAQUE_OPEN}{}{OPAQUE_CLOSE}", table.len() - 1)
    }

    fn opaque_atom(&self, word: &str) -> Expr {
        let id = word
            .strip_prefix(OPAQUE_OPEN)
            .and_then(|w| w.strip_suffix(OPAQUE_CLOSE))
            .and_then(|n| n.parse::<u32>().ok());
        if let Some(id) = id {
            return Expr::Opaque(id, Stand::Unknown);
        }
        let mut text = String::new();
        let mut rest = word;
        while let Some(at) = rest.find(OPAQUE_OPEN) {
            text.push_str(&rest[..at]);
            let tail = &rest[at + OPAQUE_OPEN.len_utf8()..];
            let end = tail.find(OPAQUE_CLOSE).unwrap_or(tail.len());
            if let Some(t) = tail[..end]
                .parse::<usize>()
                .ok()
                .and_then(|i| self.opaque.borrow().get(i).cloned())
            {
                text.push_str(&t);
            }
            rest = tail.get(end + OPAQUE_CLOSE.len_utf8()..).unwrap_or("");
        }
        text.push_str(rest);
        let mut table = self.opaque.borrow_mut();
        table.push(text);
        Expr::Opaque(table.len() as u32 - 1, Stand::Unknown)
    }

    fn log_choice_item(&mut self, item: ChoiceItem) {
        if let Some(open) = self.open_choices.last_mut()
            && let Some((_, items)) = open.levels.last_mut()
        {
            items.push(item);
        }
    }

    fn close_choice_level(&mut self) {
        if let Some(open) = self.open_choices.last_mut()
            && open.levels.len() > 1
            && let Some((cond, items)) = open.levels.pop()
            && let Some((_, parent)) = open.levels.last_mut()
        {
            parent.push(ChoiceItem::If { cond, items });
        }
    }

    fn close_choice(&mut self, mut open: OpenChoice) {
        while open.levels.len() > 1 {
            if let Some((cond, items)) = open.levels.pop()
                && let Some((_, parent)) = open.levels.last_mut()
            {
                parent.push(ChoiceItem::If { cond, items });
            }
        }
        let Some((_, items)) = open.levels.pop() else {
            return;
        };
        let mut values = Vec::new();
        choice_values(&items, &mut values);
        for sym in values {
            let ty = self.k.symbols[sym as usize].ty;
            if !(ty.is_tristateish() || ty == SymType::Unknown) {
                continue;
            }
            let ch = &mut self.k.choices[open.id as usize];
            if !ch.members.contains(&sym) {
                ch.members.push(sym);
            }
            self.k.symbols[sym as usize].choice = Some(open.id);
        }
    }

    fn loc(&self) -> Loc {
        Loc {
            file: self.file,
            line: self.line,
        }
    }

    fn warn(&mut self, msg: String) {
        let l = self.k.loc_text(self.loc());
        self.k.warnings.push(format!("{l}: {msg}"));
    }

    fn call(&self, name: &str, args: &[String], opaque: &mut bool) -> String {
        match name {
            "shell" | "success" | "failure" => {
                *opaque = true;
                String::new()
            }
            "info" | "warning-if" | "error-if" => String::new(),
            "filename" => self
                .k
                .files
                .get(self.file as usize)
                .cloned()
                .unwrap_or_default(),
            "lineno" => self.line.to_string(),
            "ARCH" => self.env.arch.clone(),
            "SRCARCH" => self.env.srcarch.clone(),
            "KERNELVERSION" => self.env.kernelversion.clone(),
            "srctree" | "abs_srctree" => self.env.srctree.display().to_string(),
            _ => match self.vars.get(name) {
                Some(v) => {
                    let mut body = v.value.clone();
                    if v.recursive {
                        for (i, a) in args.iter().enumerate() {
                            body = body.replace(&format!("$({})", i + 1), a);
                        }
                        body = self.expand(&body, opaque);
                    }
                    if v.opaque {
                        *opaque = true;
                    }
                    body
                }
                None => {
                    *opaque = true;
                    String::new()
                }
            },
        }
    }

    fn expand(&self, s: &str, opaque: &mut bool) -> String {
        if !s.contains('$') {
            return s.to_string();
        }
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'$' && b.get(i + 1) == Some(&b'(') {
                let mut depth = 0;
                let mut j = i + 1;
                while j < b.len() {
                    match b[j] {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                let inner = &s[i + 2..j.min(s.len())];
                let mut parts = Vec::new();
                let mut d = 0;
                let mut start = 0;
                for (k, ch) in inner.char_indices() {
                    match ch {
                        '(' => d += 1,
                        ')' => d -= 1,
                        ',' if d == 0 => {
                            parts.push(&inner[start..k]);
                            start = k + 1;
                        }
                        _ => {}
                    }
                }
                parts.push(&inner[start..]);
                let mut inner = false;
                let name = self.expand(parts[0].trim(), &mut inner);
                let args: Vec<String> = parts[1..]
                    .iter()
                    .map(|a| self.expand(a, &mut inner))
                    .collect();
                let value = self.call(&name, &args, &mut inner);
                let end = (j + 1).min(s.len());
                if inner && !matches!(name.as_str(), "info" | "warning-if" | "error-if") {
                    *opaque = true;
                    out.push_str(&self.opaque_token(&s[i..end]));
                } else {
                    out.push_str(&value);
                }
                i = j + 1;
                continue;
            }
            if b[i] == b'$' && b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                let mut inner = false;
                let value = self.call(&s[i + 1..j], &[], &mut inner);
                if inner {
                    *opaque = true;
                    out.push_str(&self.opaque_token(&s[i..j]));
                } else {
                    out.push_str(&value);
                }
                i = j;
                continue;
            }
            out.push(b[i] as char);
            i += 1;
        }
        out
    }

    fn parse_expr(&mut self, toks: &[Tok], pos: &mut usize) -> Expr {
        let mut e = self.parse_and(toks, pos);
        while toks.get(*pos) == Some(&Tok::Op("||")) {
            *pos += 1;
            let r = self.parse_and(toks, pos);
            e = Expr::or(e, r);
        }
        e
    }

    fn parse_and(&mut self, toks: &[Tok], pos: &mut usize) -> Expr {
        let mut e = self.parse_not(toks, pos);
        while toks.get(*pos) == Some(&Tok::Op("&&")) {
            *pos += 1;
            let r = self.parse_not(toks, pos);
            e = Expr::And(Box::new(e), Box::new(r));
        }
        e
    }

    fn parse_not(&mut self, toks: &[Tok], pos: &mut usize) -> Expr {
        if toks.get(*pos) == Some(&Tok::Op("!")) {
            *pos += 1;
            let e = self.parse_not(toks, pos);
            return Expr::Not(Box::new(e));
        }
        let lhs = self.parse_primary(toks, pos);
        let op = match toks.get(*pos) {
            Some(Tok::Op("=")) => CmpOp::Eq,
            Some(Tok::Op("!=")) => CmpOp::Ne,
            Some(Tok::Op("<")) => CmpOp::Lt,
            Some(Tok::Op("<=")) => CmpOp::Le,
            Some(Tok::Op(">")) => CmpOp::Gt,
            Some(Tok::Op(">=")) => CmpOp::Ge,
            _ => return lhs,
        };
        *pos += 1;
        let rhs = self.parse_primary(toks, pos);
        Expr::Cmp(op, Box::new(lhs), Box::new(rhs))
    }

    fn parse_primary(&mut self, toks: &[Tok], pos: &mut usize) -> Expr {
        match toks.get(*pos).cloned() {
            Some(Tok::Op("(")) => {
                *pos += 1;
                let e = self.parse_expr(toks, pos);
                if toks.get(*pos) == Some(&Tok::Op(")")) {
                    *pos += 1;
                }
                e
            }
            Some(Tok::Word(w) | Tok::Str(w)) if w.contains(OPAQUE_OPEN) => {
                *pos += 1;
                self.opaque_atom(&w)
            }
            Some(Tok::Word(w)) => {
                *pos += 1;
                if is_const_word(&w) {
                    Expr::Const(w)
                } else {
                    Expr::Sym(self.k.intern(&w))
                }
            }
            Some(Tok::Str(s)) => {
                *pos += 1;
                Expr::Const(s)
            }
            _ => {
                *pos += 1;
                Expr::Const("n".into())
            }
        }
    }

    fn opt_if(&mut self, toks: &[Tok], pos: &mut usize) -> Expr {
        if toks.get(*pos) == Some(&Tok::Word("if".into())) {
            *pos += 1;
            self.parse_expr(toks, pos).rewrite_m()
        } else {
            Expr::Yes
        }
    }

    fn inherited(&self) -> (Expr, Expr) {
        let mut deps = Expr::Yes;
        let mut visible = Expr::Yes;
        for f in &self.frames {
            match f {
                Frame::If(e) => deps = Expr::and(deps, e.clone()),
                Frame::Menu {
                    deps: d,
                    visible: v,
                } => {
                    deps = Expr::and(deps, d.clone());
                    visible = Expr::and(visible, v.clone());
                }
                Frame::Choice(c) => {
                    deps = Expr::and(deps, self.k.choices[*c as usize].deps.clone())
                }
            }
        }
        (deps, visible)
    }

    fn finish_entry(&mut self) {
        let cur = std::mem::replace(&mut self.cur, Current::None);
        let (sym, mut def) = match cur {
            Current::Config { sym, def } => (sym, def),
            Current::Comment(deps) => {
                self.log_choice_item(ChoiceItem::Comment { deps });
                return;
            }
            _ => return,
        };
        let (deps, visible) = self.inherited();
        def.deps = Expr::and(deps, def.own_deps.clone());
        def.visible_if = visible;
        let owner = Stand::Owner(sym);
        let mut opaque = def.deps.claim_opaque(owner);
        opaque |= def.visible_if.claim_opaque(owner);
        for p in &mut def.prompts {
            opaque |= p.cond.claim_opaque(owner);
        }
        for d in &mut def.defaults {
            opaque |= d.value.claim_opaque(owner);
            opaque |= d.cond.claim_opaque(owner);
        }
        for r in &mut def.ranges {
            opaque |= r.lo.claim_opaque(owner);
            opaque |= r.hi.claim_opaque(owner);
            opaque |= r.cond.claim_opaque(owner);
        }
        def.opaque |= opaque;
        self.log_choice_item(ChoiceItem::Config {
            sym,
            deps: def.own_deps.clone(),
            prompt: def.prompts.first().map(|p| p.cond.clone()),
        });
        for s in &def.selects {
            let cond = Expr::and(s.cond.clone(), def.deps.clone());
            self.k.symbols[s.target as usize]
                .selected_by
                .push((sym, cond, s.loc));
        }
        for s in &def.implies {
            let cond = Expr::and(s.cond.clone(), def.deps.clone());
            self.k.symbols[s.target as usize]
                .implied_by
                .push((sym, cond, s.loc));
        }
        self.k.symbols[sym as usize].defs.push(*def);
    }

    fn set_type(&mut self, sym: SymId, ty: SymType) {
        let s = &mut self.k.symbols[sym as usize];
        if s.ty == SymType::Unknown {
            s.ty = ty;
        }
    }

    fn pop_until(&mut self, pred: impl Fn(&Frame) -> bool, what: &str) {
        while let Some(f) = self.frames.pop() {
            if pred(&f) {
                return;
            }
        }
        self.warn(format!("unbalanced {what}"));
    }

    pub fn parse_root(&mut self, rel: &str) {
        let path = self.env.srctree.join(rel);
        self.parse_file(&path, false);
    }

    fn resolve_source(&self, spec: &str, relative: bool) -> Vec<PathBuf> {
        let base = if relative {
            self.k
                .files
                .get(self.file as usize)
                .and_then(|f| Path::new(f).parent().map(|p| self.env.srctree.join(p)))
                .unwrap_or_else(|| self.env.srctree.clone())
        } else {
            self.env.srctree.clone()
        };
        let full = if Path::new(spec).is_absolute() {
            PathBuf::from(spec)
        } else {
            base.join(spec)
        };
        if !spec.contains(['*', '?']) {
            return vec![full];
        }
        let Some(pattern) = full
            .to_str()
            .and_then(|s| globset::Glob::new(s).ok())
            .map(|g| g.compile_matcher())
        else {
            return Vec::new();
        };
        let mut found = Vec::new();
        let mut dir = full.clone();
        while dir.to_string_lossy().contains(['*', '?']) {
            if !dir.pop() {
                break;
            }
        }
        fn walk(d: &Path, m: &globset::GlobMatcher, out: &mut Vec<PathBuf>, depth: usize) {
            let Ok(rd) = std::fs::read_dir(d) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if m.is_match(&p) && p.is_file() {
                    out.push(p.clone());
                }
                if depth < 3 && e.file_type().is_ok_and(|t| t.is_dir()) {
                    walk(&p, m, out, depth + 1);
                }
            }
        }
        walk(&dir, &pattern, &mut found, 0);
        found.sort();
        found
    }

    fn parse_file(&mut self, path: &Path, optional: bool) {
        if self.depth > MAX_SOURCE_DEPTH {
            self.warn("source nesting too deep".into());
            return;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            if !optional {
                self.warn(format!("cannot read {}", path.display()));
            }
            return;
        };
        let rel = path
            .strip_prefix(&self.env.srctree)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());
        let saved = (self.file, self.line);
        self.file = self.k.files.len() as u32;
        self.k.files.push(rel);
        self.depth += 1;
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            self.line = i as u32 + 1;
            let mut logical = lines[i].to_string();
            while logical.trim_end().ends_with('\\') && i + 1 < lines.len() {
                let t = logical.trim_end();
                logical = format!("{} {}", &t[..t.len() - 1], lines[i + 1].trim_start());
                i += 1;
            }
            i += 1;
            let trimmed = logical.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let first = trimmed.split_whitespace().next().unwrap_or("");
            if first == "help" || first == "---help---" {
                let (help, next) = Self::take_help(&lines, i);
                if let Current::Config { def, .. } = &mut self.cur {
                    def.help = Some(help);
                }
                i = next;
                continue;
            }
            if !KEYWORDS.contains(&first)
                && let Some(assign) = Self::assignment(trimmed)
            {
                self.assign(assign);
                continue;
            }
            self.statement(trimmed);
        }
        self.finish_entry();
        self.depth -= 1;
        (self.file, self.line) = saved;
    }

    fn take_help(lines: &[&str], from: usize) -> (String, usize) {
        let mut j = from;
        let mut first_indent = None;
        let mut text = Vec::new();
        while j < lines.len() {
            let l = lines[j];
            if l.trim().is_empty() {
                text.push(String::new());
                j += 1;
                continue;
            }
            let ind = indent_of(l);
            if ind == 0 {
                break;
            }
            let want = *first_indent.get_or_insert(ind);
            if ind < want {
                break;
            }
            text.push(l.trim().to_string());
            j += 1;
        }
        while text.last().is_some_and(String::is_empty) {
            text.pop();
        }
        (text.join("\n"), j)
    }

    fn assignment(line: &str) -> Option<(String, &'static str, String)> {
        let name_end = line.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))?;
        if name_end == 0 {
            return None;
        }
        let name = &line[..name_end];
        let rest = line[name_end..].trim_start();
        for op in [":=", "+=", "="] {
            if let Some(v) = rest.strip_prefix(op) {
                return Some((name.to_string(), op, v.trim().to_string()));
            }
        }
        None
    }

    fn assign(&mut self, (name, op, value): (String, &'static str, String)) {
        match op {
            ":=" => {
                let mut opaque = false;
                let v = self.expand(&value, &mut opaque);
                self.vars.insert(
                    name,
                    Var {
                        value: v,
                        recursive: false,
                        opaque,
                    },
                );
            }
            "=" => {
                self.vars.insert(
                    name,
                    Var {
                        value,
                        recursive: true,
                        opaque: false,
                    },
                );
            }
            _ => {
                let mut opaque = false;
                let add = self.expand(&value, &mut opaque);
                let e = self.vars.entry(name).or_insert(Var {
                    value: String::new(),
                    recursive: false,
                    opaque: false,
                });
                if !e.value.is_empty() {
                    e.value.push(' ');
                }
                e.value.push_str(&add);
                e.opaque |= opaque;
            }
        }
    }

    fn statement(&mut self, line: &str) {
        let mut opaque = false;
        let expanded = self.expand(line, &mut opaque);
        let toks = tokenize(&expanded);
        let Some(Tok::Word(kw)) = toks.first().cloned() else {
            return;
        };
        let mut pos = 1;
        let loc = self.loc();
        match kw.as_str() {
            "config" | "menuconfig" => {
                self.finish_entry();
                let Some(Tok::Word(name)) = toks.get(1) else {
                    self.warn("config without a name".into());
                    return;
                };
                let sym = self.k.intern(name);
                self.cur = Current::Config {
                    sym,
                    def: Box::new(Definition {
                        loc,
                        own_deps: Expr::Yes,
                        deps: Expr::Yes,
                        visible_if: Expr::Yes,
                        prompts: Vec::new(),
                        defaults: Vec::new(),
                        selects: Vec::new(),
                        implies: Vec::new(),
                        ranges: Vec::new(),
                        help: None,
                        opaque: false,
                    }),
                };
            }
            "choice" => {
                self.finish_entry();
                let id = self.k.choices.len() as u32;
                self.k.choices.push(Choice {
                    loc,
                    prompt: None,
                    deps: Expr::Yes,
                    defaults: Vec::new(),
                    members: Vec::new(),
                    optional: false,
                    tristate: false,
                });
                let (deps, _) = self.inherited();
                self.k.choices[id as usize].deps = deps;
                self.frames.push(Frame::Choice(id));
                self.open_choices.push(OpenChoice {
                    id,
                    levels: vec![(Expr::Yes, Vec::new())],
                });
                self.cur = Current::Choice(id);
            }
            "endchoice" => {
                self.finish_entry();
                if let Some(open) = self.open_choices.pop() {
                    self.close_choice(open);
                }
                self.pop_until(|f| matches!(f, Frame::Choice(_)), "endchoice");
            }
            "comment" => {
                self.finish_entry();
                self.cur = Current::Comment(Expr::Yes);
            }
            "menu" => {
                self.finish_entry();
                self.frames.push(Frame::Menu {
                    deps: Expr::Yes,
                    visible: Expr::Yes,
                });
                self.cur = Current::Menu;
            }
            "endmenu" => {
                self.finish_entry();
                self.pop_until(|f| matches!(f, Frame::Menu { .. }), "endmenu");
            }
            "if" => {
                self.finish_entry();
                let e = self.parse_expr(&toks, &mut pos).rewrite_m();
                if let Some(open) = self.open_choices.last_mut() {
                    open.levels.push((e.clone(), Vec::new()));
                }
                self.frames.push(Frame::If(e));
            }
            "endif" => {
                self.finish_entry();
                self.close_choice_level();
                self.pop_until(|f| matches!(f, Frame::If(_)), "endif");
            }
            "source" | "rsource" | "osource" | "orsource" => {
                self.finish_entry();
                let spec = match toks.get(1) {
                    Some(Tok::Str(s)) | Some(Tok::Word(s)) => s.clone(),
                    _ => return,
                };
                let relative = kw.starts_with('r') || kw == "orsource";
                let optional = kw.starts_with('o');
                let paths = self.resolve_source(&spec, relative);
                if paths.is_empty() && !optional {
                    self.warn(format!("source {spec}: no such file"));
                }
                for p in paths {
                    self.parse_file(&p, optional);
                }
            }
            "mainmenu" => {}
            _ => self.attribute(&kw, &toks, &mut pos, loc, opaque),
        }
    }

    fn attribute(&mut self, kw: &str, toks: &[Tok], pos: &mut usize, loc: Loc, opaque: bool) {
        if opaque
            && !matches!(kw, "select" | "imply")
            && let Current::Config { def, .. } = &mut self.cur
        {
            def.opaque = true;
        }
        let ty = match kw {
            "bool" | "boolean" | "def_bool" => Some(SymType::Bool),
            "tristate" | "def_tristate" => Some(SymType::Tristate),
            "string" => Some(SymType::String),
            "hex" => Some(SymType::Hex),
            "int" => Some(SymType::Int),
            _ => None,
        };
        match kw {
            "bool" | "boolean" | "tristate" | "string" | "hex" | "int" | "prompt" => {
                let text = match toks.get(*pos) {
                    Some(Tok::Str(s)) => {
                        *pos += 1;
                        Some(s.clone())
                    }
                    _ => None,
                };
                let cond = self.opt_if(toks, pos);
                if let Current::Config { sym, def } = &mut self.cur {
                    let sym = *sym;
                    if let Some(text) = text {
                        def.prompts.push(Prompt { text, cond, loc });
                    }
                    if let Some(t) = ty {
                        self.set_type(sym, t);
                    }
                } else if let Current::Choice(c) = &self.cur {
                    let ch = &mut self.k.choices[*c as usize];
                    if kw == "tristate" {
                        ch.tristate = true;
                    }
                    if let Some(text) = text {
                        ch.prompt = Some(Prompt { text, cond, loc });
                    }
                }
            }
            "def_bool" | "def_tristate" | "default" => {
                let value = self.parse_expr(toks, pos);
                let cond = self.opt_if(toks, pos);
                match &mut self.cur {
                    Current::Config { sym, def } => {
                        let sym = *sym;
                        def.defaults.push(Default {
                            value,
                            cond,
                            loc,
                            opaque,
                        });
                        if let Some(t) = ty {
                            self.set_type(sym, t);
                        }
                    }
                    Current::Choice(c) => {
                        if let Expr::Sym(s) = value {
                            self.k.choices[*c as usize].defaults.push((s, cond, loc));
                        }
                    }
                    _ => {}
                }
            }
            "depends" | "requires" => {
                if toks.get(*pos) == Some(&Tok::Word("on".into())) {
                    *pos += 1;
                }
                let mut e = self.parse_expr(toks, pos);
                if toks.get(*pos) == Some(&Tok::Word("if".into())) {
                    *pos += 1;
                    let cond = self.parse_expr(toks, pos);
                    e = Expr::or(e, cond.equals_n());
                }
                let e = e.rewrite_m();
                match &mut self.cur {
                    Current::Config { def, .. } => {
                        def.own_deps = Expr::and(def.own_deps.clone(), e)
                    }
                    Current::Choice(c) => {
                        let ch = &mut self.k.choices[*c as usize];
                        ch.deps = Expr::and(ch.deps.clone(), e);
                    }
                    Current::Menu => {
                        if let Some(Frame::Menu { deps, .. }) = self.frames.last_mut() {
                            *deps = Expr::and(deps.clone(), e);
                        }
                    }
                    Current::Comment(deps) => *deps = Expr::and(deps.clone(), e),
                    _ => {}
                }
            }
            "visible" => {
                if toks.get(*pos) == Some(&Tok::Word("if".into())) {
                    *pos += 1;
                }
                let e = self.parse_expr(toks, pos).rewrite_m();
                if let (Current::Menu, Some(Frame::Menu { visible, .. })) =
                    (&self.cur, self.frames.last_mut())
                {
                    *visible = Expr::and(visible.clone(), e);
                }
            }
            "select" | "imply" => {
                let Some(Tok::Word(target)) = toks.get(*pos).cloned() else {
                    return;
                };
                *pos += 1;
                let target = self.k.intern(&target);
                let mut cond = self.opt_if(toks, pos);
                cond.claim_opaque(Stand::Target(target));
                if let Current::Config { def, .. } = &mut self.cur {
                    let s = Select { target, cond, loc };
                    if kw == "select" {
                        def.selects.push(s);
                    } else {
                        def.implies.push(s);
                    }
                }
            }
            "range" => {
                let lo = self.parse_primary(toks, pos);
                let hi = self.parse_primary(toks, pos);
                let cond = self.opt_if(toks, pos);
                if let Current::Config { def, .. } = &mut self.cur {
                    def.ranges.push(Range { lo, hi, cond });
                }
            }
            "option" => {
                let rest: Vec<String> = toks[*pos..]
                    .iter()
                    .map(|t| match t {
                        Tok::Word(w) | Tok::Str(w) => w.clone(),
                        Tok::Op(o) => o.to_string(),
                    })
                    .collect();
                if let Current::Config { sym, .. } = &self.cur {
                    let sym = *sym;
                    match rest.first().map(String::as_str) {
                        Some("modules") => self.k.modules_sym = Some(sym),
                        Some("env") => self.k.symbols[sym as usize].env = rest.get(2).cloned(),
                        _ => {}
                    }
                }
            }
            "modules" => {
                if let Current::Config { sym, .. } = &self.cur {
                    self.k.modules_sym = Some(*sym);
                }
            }
            "transitional" => {
                if let Current::Config { sym, .. } = &self.cur {
                    self.k.symbols[*sym as usize].transitional = true;
                }
            }
            "optional" => {
                if let Current::Choice(c) = &self.cur {
                    self.k.choices[*c as usize].optional = true;
                }
            }
            other => self.warn(format!("unknown keyword `{other}`")),
        }
    }
}

pub fn load(env: &Env) -> Kconfig {
    let mut p = Parser::new(env);
    p.parse_root("Kconfig");
    p.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_text(files: &[(&str, &str)]) -> Kconfig {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "kmx-kconfig-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, text) in files {
            let p = dir.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let env = Env {
            arch: "x86_64".into(),
            srcarch: "x86".into(),
            kernelversion: "6.12.0".into(),
            srctree: dir.clone(),
        };
        let k = load(&env);
        let _ = std::fs::remove_dir_all(&dir);
        k
    }

    #[test]
    fn parses_core_constructs() {
        let k = parse_text(&[
            (
                "Kconfig",
                "mainmenu \"Linux/$(ARCH)\"\nCC_HAS := $(success,true)\nsource \"arch/$(SRCARCH)/Kconfig\"\n\nconfig MODULES\n\tbool \"Modules\"\n\tmodules\n\nmenu \"M\"\n\tdepends on X86\n\nconfig A\n\ttristate \"A\" if B\n\tdefault m if C\n\tselect D\n\thelp\n\t  Some help\n\n\t  more\n\nendmenu\n\nchoice\n\tprompt \"size\"\n\tdefault S4K\nconfig S4K\n\tbool \"4K\"\nconfig S64K\n\tbool \"64K\"\nendchoice\n\nconfig CCX\n\tdef_bool $(CC_HAS)\n\nif S4K\nconfig SHIFT\n\tint\n\tdefault 12\n\trange 12 16\nendif\n",
            ),
            ("arch/x86/Kconfig", "config X86\n\tdef_bool y\n"),
        ]);
        let a = &k.symbols[k.lookup("A").unwrap() as usize];
        assert_eq!(a.ty, SymType::Tristate);
        assert_eq!(a.defs[0].help.as_deref(), Some("Some help\n\nmore"));
        assert_eq!(k.expr_text(&a.defs[0].deps), "X86");
        let d = &k.symbols[k.lookup("D").unwrap() as usize];
        assert_eq!(d.selected_by.len(), 1);
        let c = &k.choices[0];
        assert_eq!(c.members.len(), 2);
        assert_eq!(c.defaults.len(), 1);
        assert!(k.symbols[k.lookup("CCX").unwrap() as usize].defs[0].defaults[0].opaque);
        assert!(k.lookup("X86").is_some());
        assert_eq!(k.modules_sym, k.lookup("MODULES"));
        let shift = &k.symbols[k.lookup("SHIFT").unwrap() as usize];
        assert_eq!(k.expr_text(&shift.defs[0].deps), "S4K");
        assert_eq!(shift.defs[0].ranges.len(), 1);
        assert!(k.warnings.is_empty(), "{:?}", k.warnings);
    }

    fn values(k: &Kconfig, config: &str, names: &[&str]) -> Vec<String> {
        let cfg = crate::tree::dot_config::DotConfig::parse(config);
        let mut ev = crate::kconfig::Eval::new(k, &cfg);
        names
            .iter()
            .map(|n| ev.value(k.lookup(n).unwrap()).0.text())
            .collect()
    }

    #[test]
    fn entries_depending_on_a_choice_value_leave_the_choice() {
        let k = parse_text(&[(
            "Kconfig",
            "choice\n\tprompt \"gadget\"\nconfig A\n\ttristate \"A\"\nconfig A_SUB\n\tbool \"A sub\"\n\tdepends on A\nconfig A_NOT\n\tbool \"not A\"\n\tdepends on !A\nconfig B\n\ttristate \"B\"\nif B\nconfig B_SUB\n\tbool \"B sub\"\nendif\ncomment \"plain\"\nconfig C\n\ttristate \"C\"\nconfig C_LATE\n\tbool \"late\"\n\tdepends on A\nendchoice\n",
        )]);
        let names: Vec<&str> = k.choices[0]
            .members
            .iter()
            .map(|&m| k.sym(m).name.as_str())
            .collect();
        assert_eq!(names, ["A", "B", "C", "C_LATE"]);
        assert!(k.sym(k.lookup("A_SUB").unwrap()).choice.is_none());
    }

    #[test]
    fn toolchain_tests_follow_the_recorded_config() {
        let k = parse_text(&[(
            "Kconfig",
            "config CFI\n\tdef_bool y\n\tbool \"cfi\"\n\tdepends on X && $(success,false)\n\tselect SHADOW\n\tselect EXTRA if $(success,true)\nconfig X\n\tdef_bool y\nconfig SHADOW\n\tbool\nconfig EXTRA\n\tbool\nif $(success,true)\nconfig GATED\n\tbool \"gated\"\nendif\n$(error-if,$(success,true),never shown)\n",
        )]);
        assert!(k.warnings.is_empty(), "{:?}", k.warnings);
        let names = ["CFI", "SHADOW", "EXTRA", "GATED"];
        assert_eq!(
            values(
                &k,
                "CONFIG_CFI=y\nCONFIG_X=y\nCONFIG_SHADOW=y\nCONFIG_GATED=y\n",
                &names
            ),
            ["y", "y", "n", "y"]
        );
        assert_eq!(values(&k, "CONFIG_X=y\n", &names), ["n", "n", "n", "n"]);
        let cfi = k.sym(k.lookup("CFI").unwrap());
        assert_eq!(k.expr_text(&cfi.defs[0].deps), "X && $(success,false)");
    }

    #[test]
    fn comparisons_follow_symbol_types() {
        let k = parse_text(&[(
            "Kconfig",
            "config MODULES\n\tbool \"modules\"\n\tmodules\nconfig TEE\n\ttristate \"tee\"\nconfig KEYS\n\ttristate \"keys\"\nconfig KEYS_TEE\n\tbool \"tee keys\"\n\tdepends on TEE >= KEYS\n\tdefault y\nconfig BASE\n\thex \"base\"\n\tdefault 0x1000\nconfig BIG\n\tdef_bool BASE > 0x800\n",
        )]);
        assert_eq!(
            values(
                &k,
                "CONFIG_MODULES=y\nCONFIG_KEYS=m\n",
                &["KEYS_TEE", "BIG"]
            ),
            ["n", "y"]
        );
        assert_eq!(
            values(
                &k,
                "CONFIG_MODULES=y\nCONFIG_TEE=y\nCONFIG_KEYS=m\nCONFIG_KEYS_TEE=y\n",
                &["KEYS_TEE"]
            ),
            ["y"]
        );
    }

    #[test]
    fn bare_m_in_a_condition_needs_modules() {
        let k = parse_text(&[(
            "Kconfig",
            "config MODULES\n\tbool \"modules\"\n\tmodules\nconfig TEST\n\ttristate \"test\"\n\tselect EXPORTS if m\nconfig EXPORTS\n\tbool\n",
        )]);
        assert_eq!(
            values(&k, "CONFIG_TEST=y\n", &["TEST", "EXPORTS"]),
            ["y", "n"]
        );
        assert_eq!(
            values(
                &k,
                "CONFIG_MODULES=y\nCONFIG_TEST=y\n",
                &["TEST", "EXPORTS"]
            ),
            ["y", "y"]
        );
    }

    #[test]
    fn conditional_dependencies() {
        let k = parse_text(&[(
            "Kconfig",
            "config MODULES\n\tbool \"modules\"\n\tmodules\nconfig USB\n\ttristate \"usb\"\nconfig GADGET\n\ttristate \"gadget\"\nconfig CDNS\n\ttristate \"cdns\"\n\tdepends on USB || GADGET\n\tdepends on USB if !GADGET\n\tdepends on GADGET if !USB\n",
        )]);
        let cdns = k.sym(k.lookup("CDNS").unwrap());
        assert_eq!(
            k.expr_text(&cdns.defs[0].deps),
            "(USB || GADGET) && (USB || GADGET!=n) && (GADGET || USB!=n)"
        );
        let with = |extra: &str| {
            values(
                &k,
                &format!("CONFIG_MODULES=y\nCONFIG_CDNS=y\n{extra}"),
                &["CDNS"],
            )
        };
        assert_eq!(with("CONFIG_USB=y\nCONFIG_GADGET=m\n"), ["y"]);
        assert_eq!(with("CONFIG_USB=m\nCONFIG_GADGET=y\n"), ["y"]);
        assert_eq!(with("CONFIG_USB=m\n"), ["m"]);
    }
}
