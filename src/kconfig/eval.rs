use rustc_hash::FxHashMap;

use super::model::{CmpOp, Expr, Kconfig, Stand, SymId, SymType};
use crate::tree::dot_config::{ConfigValue, DotConfig, Tristate};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Val {
    Tri(Tristate),
    Str(String),
}

impl Val {
    pub fn text(&self) -> String {
        match self {
            Val::Tri(t) => t.to_string(),
            Val::Str(s) => s.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    User,
    Default { def: usize, idx: usize },
    Select,
    Imply,
    Choice,
    Environment { def: usize, idx: usize },
    Transitional,
    Unset,
}

pub struct Eval<'k> {
    pub k: &'k Kconfig,
    user: FxHashMap<SymId, String>,
    dotcfg: &'k DotConfig,
    vals: Vec<Option<(Val, Source)>>,
    busy: Vec<bool>,
}

fn tri_of_str(s: &str) -> Option<Tristate> {
    Tristate::parse(s)
}

fn compared(text: &str, ty: Option<SymType>) -> Option<i128> {
    let digits = |s: &str, radix: u32| -> Option<i128> {
        if s.is_empty() || !s.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        i128::from_str_radix(s, radix).ok()
    };
    let (neg, body) = match text.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let hex = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X"));
    let v = match ty {
        Some(SymType::Bool | SymType::Tristate) => {
            return Some(match text {
                "n" => 0,
                "m" => 1,
                "y" => 2,
                _ => -1,
            });
        }
        Some(SymType::Int) => digits(body, 10)?,
        Some(SymType::Hex) => digits(hex.unwrap_or(body), 16)?,
        _ => match hex {
            Some(h) => digits(h, 16)?,
            None if body.len() > 1 && body.starts_with('0') => digits(&body[1..], 8)?,
            None => digits(body, 10)?,
        },
    };
    Some(if neg { -v } else { v })
}

pub fn number(s: &str) -> Option<i128> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return i128::from_str_radix(h, 16).ok();
    }
    s.parse::<i128>().ok()
}

impl<'k> Eval<'k> {
    pub fn new(k: &'k Kconfig, dotcfg: &'k DotConfig) -> Eval<'k> {
        let mut user = FxHashMap::default();
        for (name, e) in dotcfg.iter() {
            if let Some(s) = k.lookup(name) {
                let v = match &e.value {
                    ConfigValue::Tristate(t) => t.to_string(),
                    ConfigValue::Str(s) => s.clone(),
                    ConfigValue::Num(n) => n.clone(),
                };
                user.insert(s, v);
            }
        }
        Eval {
            k,
            user,
            dotcfg,
            vals: vec![None; k.symbols.len()],
            busy: vec![false; k.symbols.len()],
        }
    }

    pub fn set_user(&mut self, s: SymId, value: &str) {
        if let Some(c) = self.k.sym(s).choice
            && value == "y"
        {
            for &m in &self.k.choices[c as usize].members {
                if m != s {
                    self.user.insert(m, "n".into());
                }
            }
        }
        self.user.insert(s, value.to_string());
        self.vals.iter_mut().for_each(|v| *v = None);
    }

    pub fn user_value(&self, s: SymId) -> Option<&str> {
        self.user.get(&s).map(String::as_str)
    }

    pub fn modules_enabled(&mut self) -> bool {
        match self.k.modules_sym {
            Some(m) => self.tri(m) == Tristate::Yes,
            None => true,
        }
    }

    pub fn sym_type(&mut self, s: SymId) -> SymType {
        let ty = self.k.sym(s).ty;
        if ty == SymType::Tristate && Some(s) != self.k.modules_sym && !self.modules_enabled() {
            SymType::Bool
        } else {
            ty
        }
    }

    pub fn tri(&mut self, s: SymId) -> Tristate {
        match self.value(s).0 {
            Val::Tri(t) => t,
            Val::Str(_) => Tristate::No,
        }
    }

    pub fn value(&mut self, s: SymId) -> (Val, Source) {
        if let Some(v) = &self.vals[s as usize] {
            return v.clone();
        }
        if self.busy[s as usize] {
            return (Val::Tri(Tristate::No), Source::Unset);
        }
        self.busy[s as usize] = true;
        let v = self.compute(s);
        self.busy[s as usize] = false;
        self.vals[s as usize] = Some(v.clone());
        v
    }

    pub fn eval(&mut self, e: &Expr) -> Tristate {
        match e {
            Expr::Yes => Tristate::Yes,
            Expr::Mod => {
                if self.modules_enabled() {
                    Tristate::Module
                } else {
                    Tristate::No
                }
            }
            Expr::Sym(s) => self.tri(*s),
            Expr::Const(c) => tri_of_str(c).unwrap_or(Tristate::No),
            Expr::Not(a) => !self.eval(a),
            Expr::And(a, b) => {
                let x = self.eval(a);
                if x == Tristate::No {
                    return x;
                }
                x.and(self.eval(b))
            }
            Expr::Or(a, b) => {
                let x = self.eval(a);
                if x == Tristate::Yes {
                    return x;
                }
                x.or(self.eval(b))
            }
            Expr::Cmp(op, a, b) => {
                let (x, tx) = self.operand(a);
                let (y, ty) = self.operand(b);
                let both_strings = tx == Some(SymType::String) && ty == Some(SymType::String);
                let ord = match (both_strings, compared(&x, tx), compared(&y, ty)) {
                    (false, Some(p), Some(q)) => p.cmp(&q),
                    _ => x.cmp(&y),
                };
                let r = match op {
                    CmpOp::Eq => ord.is_eq(),
                    CmpOp::Ne => !ord.is_eq(),
                    CmpOp::Lt => ord.is_lt(),
                    CmpOp::Le => ord.is_le(),
                    CmpOp::Gt => ord.is_gt(),
                    CmpOp::Ge => ord.is_ge(),
                };
                if r { Tristate::Yes } else { Tristate::No }
            }
            Expr::Opaque(_, stand) => match *stand {
                Stand::Owner(s) => self.tri(s),
                Stand::Target(t) => match self.dotcfg_val(t) {
                    Some(Val::Tri(v)) => v,
                    _ => Tristate::No,
                },
                Stand::Unknown => Tristate::No,
            },
        }
    }

    fn operand(&mut self, e: &Expr) -> (String, Option<SymType>) {
        match e {
            Expr::Sym(s) => {
                let ty = if self.k.sym(*s).is_defined() {
                    self.sym_type(*s)
                } else {
                    SymType::Unknown
                };
                (self.string(e), Some(ty))
            }
            Expr::Const(c) if matches!(c.as_str(), "y" | "m" | "n") => {
                (c.clone(), Some(SymType::Tristate))
            }
            Expr::Const(c) => (c.clone(), None),
            other => (self.eval(other).to_string(), Some(SymType::Tristate)),
        }
    }

    pub fn string(&mut self, e: &Expr) -> String {
        match e {
            Expr::Yes => "y".into(),
            Expr::Const(c) => c.clone(),
            Expr::Sym(s) => {
                if !self.k.sym(*s).is_defined() && !self.user.contains_key(s) {
                    return match self.k.sym(*s).ty {
                        SymType::Unknown => String::new(),
                        _ => "n".into(),
                    };
                }
                self.value(*s).0.text()
            }
            other => self.eval(other).to_string(),
        }
    }

    pub fn visibility(&mut self, s: SymId) -> Tristate {
        let k = self.k;
        let mut t = Tristate::No;
        for d in &k.sym(s).defs {
            for p in &d.prompts {
                let v = self
                    .eval(&p.cond)
                    .and(self.eval(&d.deps))
                    .and(self.eval(&d.visible_if));
                t = t.or(v);
            }
        }
        if t == Tristate::Module
            && (self.sym_type(s) != SymType::Tristate || !self.modules_enabled())
        {
            t = Tristate::Yes;
        }
        t
    }

    pub fn dir_dep(&mut self, s: SymId) -> Tristate {
        let k = self.k;
        let defs = &k.sym(s).defs;
        if defs.is_empty() {
            return Tristate::Yes;
        }
        let mut t = Tristate::No;
        for d in defs {
            t = t.or(self.eval(&d.deps));
        }
        self.bool_fix(s, t)
    }

    pub fn rev_dep(&mut self, s: SymId) -> Tristate {
        let k = self.k;
        let mut t = Tristate::No;
        for (by, cond, _) in &k.sym(s).selected_by {
            let v = self.tri(*by).and(self.eval(cond));
            t = t.or(v);
        }
        self.bool_fix(s, t)
    }

    pub fn implied(&mut self, s: SymId) -> Tristate {
        let k = self.k;
        let mut t = Tristate::No;
        for (by, cond, _) in &k.sym(s).implied_by {
            let v = self.tri(*by).and(self.eval(cond));
            t = t.or(v);
        }
        self.bool_fix(s, t)
    }

    fn bool_fix(&mut self, s: SymId, t: Tristate) -> Tristate {
        if t == Tristate::Module && self.sym_type(s) == SymType::Bool {
            Tristate::Yes
        } else {
            t
        }
    }

    pub fn choice_visibility(&mut self, c: u32) -> Tristate {
        let k = self.k;
        let ch = &k.choices[c as usize];
        match &ch.prompt {
            Some(p) => self.eval(&p.cond).and(self.eval(&ch.deps)),
            None => Tristate::No,
        }
    }

    pub fn choice_mode(&mut self, c: u32) -> Tristate {
        let k = self.k;
        let ch = &k.choices[c as usize];
        if !ch.tristate && !ch.optional {
            return Tristate::Yes;
        }
        let users: Vec<Option<Tristate>> = ch
            .members
            .iter()
            .map(|m| self.user.get(m).and_then(|u| tri_of_str(u)))
            .collect();
        if users.contains(&Some(Tristate::Yes)) {
            Tristate::Yes
        } else if users.contains(&Some(Tristate::Module)) && ch.tristate && self.modules_enabled() {
            Tristate::Module
        } else if ch.optional || users.iter().any(Option::is_some) {
            Tristate::No
        } else {
            Tristate::Yes
        }
    }

    pub fn choice_selection(&mut self, c: u32) -> Option<(SymId, &'static str)> {
        let k = self.k;
        let ch = &k.choices[c as usize];
        for &m in &ch.members {
            if self.user.get(&m).map(String::as_str) == Some("y")
                && self.visibility(m) != Tristate::No
            {
                return Some((m, "chosen in .config"));
            }
        }
        for (m, cond, _) in &ch.defaults {
            if self.eval(cond) != Tristate::No && self.visibility(*m) != Tristate::No {
                return Some((*m, "choice default"));
            }
        }
        ch.members
            .iter()
            .copied()
            .find(|&m| self.visibility(m) != Tristate::No)
            .map(|m| (m, "first visible member"))
    }

    pub fn first_default(&mut self, s: SymId) -> Option<(usize, usize, Tristate)> {
        let k = self.k;
        for (di, d) in k.sym(s).defs.iter().enumerate() {
            for (i, df) in d.defaults.iter().enumerate() {
                let c = self.eval(&df.cond).and(self.eval(&d.deps));
                if c != Tristate::No {
                    return Some((di, i, c));
                }
            }
        }
        None
    }

    fn dotcfg_val(&self, s: SymId) -> Option<Val> {
        let name = &self.k.sym(s).name;
        self.dotcfg.value(name).map(|v| match v {
            ConfigValue::Tristate(t) => Val::Tri(*t),
            ConfigValue::Str(s) => Val::Str(s.clone()),
            ConfigValue::Num(n) => Val::Str(n.clone()),
        })
    }

    fn compute(&mut self, s: SymId) -> (Val, Source) {
        let k = self.k;
        let sym = k.sym(s);
        if !sym.is_defined() {
            return match self.user.get(&s) {
                Some(v) => (
                    tri_of_str(v)
                        .map(Val::Tri)
                        .unwrap_or_else(|| Val::Str(v.clone())),
                    Source::User,
                ),
                None => (Val::Tri(Tristate::No), Source::Unset),
            };
        }
        if sym.transitional {
            let v = self
                .user
                .get(&s)
                .and_then(|v| tri_of_str(v))
                .unwrap_or(Tristate::No);
            return (Val::Tri(v), Source::Transitional);
        }
        if let Some(def) = sym.defs.iter().position(|d| d.opaque) {
            let v = self.dotcfg_val(s).unwrap_or(match sym.ty {
                SymType::String | SymType::Int | SymType::Hex => Val::Str(String::new()),
                _ => Val::Tri(Tristate::No),
            });
            return (v, Source::Environment { def, idx: 0 });
        }
        let ty = self.sym_type(s);
        match ty {
            SymType::Bool | SymType::Tristate => {
                if let Some(c) = sym.choice {
                    let vis = self.choice_visibility(c);
                    if vis != Tristate::No {
                        match self.choice_mode(c) {
                            Tristate::Yes => {
                                let sel = self.choice_selection(c).map(|(m, _)| m);
                                let v = if sel == Some(s) {
                                    Tristate::Yes
                                } else {
                                    Tristate::No
                                };
                                return (Val::Tri(v), Source::Choice);
                            }
                            Tristate::Module => {
                                let u = self
                                    .user
                                    .get(&s)
                                    .and_then(|u| tri_of_str(u))
                                    .unwrap_or(Tristate::No);
                                let v = u.min(Tristate::Module).and(self.visibility(s));
                                return (Val::Tri(v), Source::Choice);
                            }
                            Tristate::No => return (Val::Tri(Tristate::No), Source::Choice),
                        }
                    }
                }
                let vis = self.visibility(s);
                let mut src = Source::Unset;
                let mut v = Tristate::No;
                let user = if vis != Tristate::No {
                    self.user.get(&s).and_then(|u| tri_of_str(u))
                } else {
                    None
                };
                if let Some(u) = user {
                    v = u.and(vis);
                    src = Source::User;
                } else {
                    if let Some((di, idx, cond)) = self.first_default(s) {
                        let df = &sym.defs[di].defaults[idx];
                        if df.opaque {
                            v = self
                                .dotcfg_val(s)
                                .and_then(|x| match x {
                                    Val::Tri(t) => Some(t),
                                    _ => None,
                                })
                                .unwrap_or(Tristate::No);
                            src = Source::Environment { def: di, idx };
                        } else {
                            v = self.eval(&df.value).and(cond);
                            src = Source::Default { def: di, idx };
                        }
                    }
                    let imp = self.implied(s);
                    if imp != Tristate::No {
                        let before = v;
                        v = v.or(imp).and(self.dir_dep(s));
                        if v != before {
                            src = Source::Imply;
                        }
                    }
                }
                let rev = self.rev_dep(s);
                if rev > v {
                    v = v.or(rev);
                    src = Source::Select;
                }
                if v == Tristate::Module && ty == SymType::Bool {
                    v = Tristate::Yes;
                }
                (Val::Tri(v), src)
            }
            SymType::String | SymType::Int | SymType::Hex => {
                let vis = self.visibility(s);
                let mut out: Option<(String, Source)> = None;
                if vis != Tristate::No
                    && let Some(u) = self.user.get(&s)
                {
                    out = Some((u.clone(), Source::User));
                }
                if out.is_none() {
                    if let Some((di, idx, _)) = self.first_default(s) {
                        let df = &sym.defs[di].defaults[idx];
                        out = Some(if df.opaque {
                            (
                                self.dotcfg_val(s).map(|v| v.text()).unwrap_or_default(),
                                Source::Environment { def: di, idx },
                            )
                        } else {
                            (self.string(&df.value), Source::Default { def: di, idx })
                        });
                    } else if sym.env.is_some() {
                        out = self
                            .dotcfg_val(s)
                            .map(|v| (v.text(), Source::Environment { def: 0, idx: 0 }));
                    }
                }
                let (mut text, src) = out.unwrap_or((String::new(), Source::Unset));
                if matches!(ty, SymType::Int | SymType::Hex) && !text.is_empty() {
                    text = self.clamp(s, text);
                }
                (Val::Str(text), src)
            }
            SymType::Unknown => (Val::Tri(Tristate::No), Source::Unset),
        }
    }

    pub fn range(&mut self, s: SymId) -> Option<(i128, i128)> {
        let k = self.k;
        for d in &k.sym(s).defs {
            for r in &d.ranges {
                if self.eval(&r.cond) == Tristate::No {
                    continue;
                }
                let lo = number(&self.string(&r.lo))?;
                let hi = number(&self.string(&r.hi))?;
                return Some((lo, hi));
            }
        }
        None
    }

    fn clamp(&mut self, s: SymId, text: String) -> String {
        let k = self.k;
        let Some(v) = number(&text) else { return text };
        for d in &k.sym(s).defs {
            for r in &d.ranges {
                if self.eval(&r.cond) == Tristate::No {
                    continue;
                }
                let (lo, hi) = (number(&self.string(&r.lo)), number(&self.string(&r.hi)));
                let hex = text.starts_with("0x") || text.starts_with("0X");
                let fmt = |n: i128| {
                    if hex {
                        format!("0x{n:x}")
                    } else {
                        n.to_string()
                    }
                };
                if let Some(lo) = lo.filter(|lo| v < *lo) {
                    return fmt(lo);
                }
                if let Some(hi) = hi.filter(|hi| v > *hi) {
                    return fmt(hi);
                }
                return text;
            }
        }
        text
    }

    pub fn autoconf_h(&mut self) -> String {
        let k = self.k;
        let mut out = String::from("#define AUTOCONF_INCLUDED\n");
        for (i, sym) in k.symbols.iter().enumerate() {
            if !sym.is_defined() || sym.transitional {
                continue;
            }
            let s = i as SymId;
            match self.value(s).0 {
                Val::Tri(Tristate::Yes) => {
                    out.push_str(&format!("#define CONFIG_{} 1\n", sym.name))
                }
                Val::Tri(Tristate::Module) => {
                    out.push_str(&format!("#define CONFIG_{}_MODULE 1\n", sym.name))
                }
                Val::Tri(Tristate::No) => {}
                Val::Str(v) => {
                    let written = !v.is_empty() || self.dotcfg.get(&sym.name).is_some();
                    if !written {
                        continue;
                    }
                    match sym.ty {
                        SymType::String => {
                            let esc = v.replace('\\', "\\\\").replace('"', "\\\"");
                            out.push_str(&format!("#define CONFIG_{} \"{esc}\"\n", sym.name));
                        }
                        SymType::Hex if !v.starts_with("0x") && !v.starts_with("0X") => {
                            out.push_str(&format!("#define CONFIG_{} 0x{v}\n", sym.name));
                        }
                        _ => out.push_str(&format!("#define CONFIG_{} {v}\n", sym.name)),
                    }
                }
            }
        }
        out
    }

    pub fn mismatches(&mut self) -> Vec<(String, String, String)> {
        let k = self.k;
        let mut out = Vec::new();
        for (i, sym) in k.symbols.iter().enumerate() {
            if !sym.is_defined() || sym.transitional {
                continue;
            }
            let ours = self.value(i as SymId).0;
            let theirs = self.dotcfg_val(i as SymId);
            let same = match (&ours, &theirs) {
                (Val::Tri(Tristate::No), None) => true,
                (Val::Str(s), None) => s.is_empty(),
                (a, Some(b)) => {
                    a == b
                        || matches!((a, b), (Val::Str(x), Val::Str(y)) if number(x).is_some() && number(x) == number(y))
                }
                _ => false,
            };
            if !same {
                out.push((
                    sym.name.clone(),
                    ours.text(),
                    theirs
                        .map(|t| t.text())
                        .unwrap_or_else(|| "(absent)".into()),
                ));
            }
        }
        out
    }
}
