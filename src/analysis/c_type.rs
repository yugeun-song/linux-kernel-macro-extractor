use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use super::value::IntTy;
use crate::pp::sim::Sim;
use crate::types::{DefKind, DefView, DerivedKind, NameKind, TRef, Table};
use crate::workspace::Workspace;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Base<'a> {
    Basic(Vec<&'a str>),
    Tag(DefKind, &'a str),
    Typedef(&'a str),
    Typeof(usize, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Pointer,
    Array(Option<u64>),
    Function,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeName<'a> {
    pub base: Base<'a>,
    pub ops: Vec<Op>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step<'a> {
    Field(&'a str),
    Index(u64),
}

const QUALIFIERS: &[&str] = &[
    "const",
    "volatile",
    "restrict",
    "__restrict",
    "__restrict__",
    "__volatile__",
    "__volatile",
    "__const",
    "__const__",
    "__extension__",
];

const BASIC: &[&str] = &[
    "void",
    "char",
    "short",
    "int",
    "long",
    "float",
    "double",
    "signed",
    "unsigned",
    "_Bool",
    "bool",
    "__int128",
    "__signed__",
    "__signed",
];

fn closing(t: &[&str], open: usize) -> Option<usize> {
    let (o, c) = match t[open] {
        "(" => ("(", ")"),
        "[" => ("[", "]"),
        _ => return None,
    };
    let mut depth = 0usize;
    for (i, w) in t.iter().enumerate().skip(open) {
        if *w == o {
            depth += 1;
        } else if *w == c {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

fn word(w: &str) -> bool {
    w.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn array_len(t: &[&str]) -> Option<Option<u64>> {
    match t {
        [] => Some(None),
        [n] => {
            let k = super::c_const::literal(n)?;
            u64::try_from(k.v).ok().map(Some)
        }
        _ => None,
    }
}

fn declarator(t: &[&str]) -> Option<Vec<Op>> {
    let mut i = 0;
    let mut pointers = 0;
    while let Some(&w) = t.get(i) {
        if w == "*" {
            pointers += 1;
        } else if !QUALIFIERS.contains(&w) {
            break;
        }
        i += 1;
    }
    let rest = &t[i..];
    let mut ops = vec![Op::Pointer; pointers];
    let (inner, suffix_from) = match rest.first() {
        Some(&"(") if rest.get(1).is_some_and(|w| matches!(*w, "*" | "(" | "[")) => {
            let close = closing(rest, 0)?;
            (Some(declarator(&rest[1..close])?), close + 1)
        }
        _ => (None, 0),
    };
    let mut suffixes = Vec::new();
    let mut j = suffix_from;
    while j < rest.len() {
        match rest[j] {
            "[" => {
                let close = closing(rest, j)?;
                suffixes.push(Op::Array(array_len(&rest[j + 1..close])?));
                j = close + 1;
            }
            "(" => {
                let close = closing(rest, j)?;
                suffixes.push(Op::Function);
                j = close + 1;
            }
            _ => return None,
        }
    }
    ops.extend(suffixes.into_iter().rev());
    if let Some(inner) = inner {
        ops.extend(inner);
    }
    Some(ops)
}

pub fn parse_type_name<'a>(t: &[&'a str]) -> Option<TypeName<'a>> {
    let mut i = 0;
    let mut basic: Vec<&'a str> = Vec::new();
    let mut base: Option<Base<'a>> = None;
    while let Some(&w) = t.get(i) {
        if QUALIFIERS.contains(&w) {
            i += 1;
        } else if matches!(w, "__attribute__" | "__attribute") {
            let close = closing(t, i + 1)?;
            i = close + 1;
        } else if matches!(w, "struct" | "union" | "enum") {
            let tag = t.get(i + 1).copied().filter(|n| word(n))?;
            if base.is_some() || !basic.is_empty() || t.get(i + 2) == Some(&"{") {
                return None;
            }
            let kind = match w {
                "struct" => DefKind::Struct,
                "union" => DefKind::Union,
                _ => DefKind::Enum,
            };
            base = Some(Base::Tag(kind, tag));
            i += 2;
        } else if matches!(w, "typeof" | "__typeof__" | "__typeof") {
            if base.is_some() || !basic.is_empty() || t.get(i + 1) != Some(&"(") {
                return None;
            }
            let close = closing(t, i + 1)?;
            base = Some(Base::Typeof(i + 2, close));
            i = close + 1;
        } else if BASIC.contains(&w) {
            if base.is_some() {
                return None;
            }
            basic.push(w);
            i += 1;
        } else if word(w) && base.is_none() && basic.is_empty() {
            base = Some(Base::Typedef(w));
            i += 1;
        } else {
            break;
        }
    }
    let base = match (base, basic.is_empty()) {
        (Some(b), true) => b,
        (None, false) => Base::Basic(basic),
        _ => return None,
    };
    let ops = declarator(&t[i..])?;
    Some(TypeName { base, ops })
}

pub fn basic_int(words: &[&str]) -> Option<IntTy> {
    let (mut unsigned, mut signed, mut longs, mut short, mut char_, mut int, mut boolean) =
        (false, false, 0, false, false, false, false);
    for w in words {
        match *w {
            "unsigned" if !unsigned => unsigned = true,
            "signed" | "__signed__" | "__signed" if !signed => signed = true,
            "long" => longs += 1,
            "short" if !short => short = true,
            "char" if !char_ => char_ = true,
            "int" if !int => int = true,
            "_Bool" | "bool" if !boolean => boolean = true,
            _ => return None,
        }
    }
    if (unsigned && signed) || longs > 2 {
        return None;
    }
    Some(match (boolean, char_, short, longs) {
        (true, false, false, 0) if !unsigned && !signed && !int => IntTy::Bool,
        (false, true, false, 0) if !int => match (unsigned, signed) {
            (true, _) => IntTy::UChar,
            (_, true) => IntTy::SChar,
            _ => IntTy::Char,
        },
        (false, false, true, 0) => {
            if unsigned {
                IntTy::UShort
            } else {
                IntTy::Short
            }
        }
        (false, false, false, 0) => {
            if unsigned {
                IntTy::UInt
            } else {
                IntTy::Int
            }
        }
        (false, false, false, 1) => {
            if unsigned {
                IntTy::ULong
            } else {
                IntTy::Long
            }
        }
        (false, false, false, 2) => {
            if unsigned {
                IntTy::ULongLong
            } else {
                IntTy::LongLong
            }
        }
        _ => return None,
    })
}

fn gcc_int(name: &str, size: u64) -> Option<IntTy> {
    let t = match name {
        "char" => IntTy::Char,
        "signed char" => IntTy::SChar,
        "unsigned char" => IntTy::UChar,
        "short int" => IntTy::Short,
        "short unsigned int" => IntTy::UShort,
        "int" => IntTy::Int,
        "unsigned int" => IntTy::UInt,
        "long int" => IntTy::Long,
        "long unsigned int" => IntTy::ULong,
        "long long int" => IntTy::LongLong,
        "long long unsigned int" => IntTy::ULongLong,
        "_Bool" => IntTy::Bool,
        _ => return None,
    };
    (lp64_bytes(t) == Some(size)).then_some(t)
}

pub fn lp64_bytes(t: IntTy) -> Option<u64> {
    Some(match t {
        IntTy::Bool | IntTy::Char | IntTy::SChar | IntTy::UChar => 1,
        IntTy::Short | IntTy::UShort => 2,
        IntTy::Int | IntTy::UInt => 4,
        IntTy::Long | IntTy::ULong | IntTy::LongLong | IntTy::ULongLong => 8,
    })
}

fn basic_size(words: &[&str]) -> Option<u64> {
    if let Some(t) = basic_int(words) {
        return lp64_bytes(t);
    }
    match words {
        ["float"] => Some(4),
        ["double"] => Some(8),
        ["__int128"]
        | ["signed", "__int128"]
        | ["unsigned", "__int128"]
        | ["__int128", "unsigned"] => Some(16),
        _ => None,
    }
}

pub trait TypeEnv {
    fn lp64(&self) -> bool;

    fn char_unsigned(&self) -> bool {
        false
    }

    fn named_size(&self, _base: &Base<'_>) -> Option<u64> {
        None
    }

    fn typedef_int(&self, _name: &str) -> Option<IntTy> {
        None
    }

    fn offset(&self, _base: &Base<'_>, _path: &[Step<'_>]) -> Option<(u64, Option<u64>)> {
        None
    }

    fn fn_return(&self, _name: &str) -> Option<IntTy> {
        None
    }

    fn enumerator(&self, _name: &str) -> Option<i128> {
        None
    }

    fn is_typedef(&self, _name: &str) -> bool {
        false
    }

    fn size_of(&self, t: &TypeName<'_>, typeof_size: Option<u64>) -> Option<u64> {
        if !self.lp64() {
            return None;
        }
        let mut size = match &t.base {
            Base::Basic(words) => basic_size(words),
            Base::Typeof(..) => typeof_size,
            other => self.named_size(other),
        };
        for op in &t.ops {
            size = match op {
                Op::Pointer => Some(8),
                Op::Array(Some(n)) => size.and_then(|s| s.checked_mul(*n)),
                Op::Array(None) | Op::Function => None,
            };
        }
        size
    }

    fn int_of(&self, t: &TypeName<'_>) -> Option<IntTy> {
        if !t.ops.is_empty() || !self.lp64() {
            return None;
        }
        match &t.base {
            Base::Basic(words) => basic_int(words),
            Base::Typedef(name) => self.typedef_int(name),
            _ => None,
        }
    }
}

pub struct Basic {
    pub lp64: bool,
}

impl TypeEnv for Basic {
    fn lp64(&self) -> bool {
        self.lp64
    }
}

#[derive(Clone, Debug)]
pub enum Used {
    Type { spelled: String, def: u32 },
    Enumerator { name: String, def: u32, value: i128 },
}

pub struct TypeCx<'a, 'w> {
    pub ws: &'w Workspace,
    pub sim: &'a Sim<'w>,
    pub char_unsigned: bool,
    pub used: RefCell<Vec<Used>>,
    pub wanted: Cell<bool>,
}

impl<'a, 'w> TypeCx<'a, 'w> {
    pub fn new(ws: &'w Workspace, sim: &'a Sim<'w>, char_unsigned: bool) -> Self {
        TypeCx {
            ws,
            sim,
            char_unsigned,
            used: RefCell::new(Vec::new()),
            wanted: Cell::new(false),
        }
    }

    pub fn table(&self) -> Option<&'w Table> {
        self.wanted.set(true);
        self.ws.types().table.as_ref()
    }

    pub fn abs(&self, file: &str) -> PathBuf {
        if file.starts_with('/') {
            PathBuf::from(file)
        } else {
            self.ws.layout.objtree.join(file)
        }
    }

    fn visible(&self, file: &str, line: u32) -> bool {
        self.sim
            .files
            .lookup(&self.abs(file))
            .is_some_and(|f| self.sim.reached(f, line))
    }

    pub fn def_named(&self, kind: DefKind, name: &str) -> Option<DefView<'w>> {
        let table = self.table()?;
        let mut found: Option<DefView<'w>> = None;
        for (k, i) in table.named(name) {
            if k != NameKind::Def(kind) {
                continue;
            }
            let d = table.def(i)?;
            if !self.visible(d.file, d.line) {
                continue;
            }
            match &found {
                None => found = Some(d),
                Some(f) if f.size == d.size && f.size.is_some() && kind != DefKind::Enum => {}
                Some(_) => return None,
            }
        }
        found
    }

    fn record(&self, u: Used) {
        self.used.borrow_mut().push(u);
    }

    pub fn aggregate(&self, table: &'w Table, t: u32, depth: u32) -> Option<DefView<'w>> {
        if depth > 32 {
            return None;
        }
        match table.tref(t) {
            TRef::Named(DefKind::Typedef, n) => {
                let d = self.def_named(DefKind::Typedef, n)?;
                if !d.types_agree() {
                    return None;
                }
                self.aggregate(table, d.target, depth + 1)
            }
            TRef::Named(kind, n) => self.def_named(kind, n),
            TRef::Anon(i) => table.def(i),
            TRef::Derived {
                kind: DerivedKind::Const | DerivedKind::Volatile | DerivedKind::Restrict,
                target,
                ..
            } => self.aggregate(table, target, depth + 1),
            _ => None,
        }
    }

    fn tref_size(&self, table: &'w Table, t: u32, depth: u32) -> Option<u64> {
        if depth > 32 {
            return None;
        }
        match table.tref(t) {
            TRef::Base { size, .. } => Some(size),
            TRef::Named(kind, n) => self.def_named(kind, n)?.size,
            TRef::Anon(i) => table.def(i)?.size,
            TRef::Derived {
                kind,
                target,
                count,
            } => match kind {
                DerivedKind::Pointer => Some(8),
                DerivedKind::Const | DerivedKind::Volatile | DerivedKind::Restrict => {
                    self.tref_size(table, target, depth + 1)
                }
                DerivedKind::Array => {
                    count?.checked_mul(self.tref_size(table, target, depth + 1)?)
                }
                DerivedKind::Atomic => None,
            },
            _ => None,
        }
    }

    fn tref_int(&self, table: &'w Table, t: u32, depth: u32) -> Option<IntTy> {
        if depth > 32 {
            return None;
        }
        match table.tref(t) {
            TRef::Base { name, size, .. } => gcc_int(name, size),
            TRef::Named(DefKind::Typedef, n) => {
                let d = self.def_named(DefKind::Typedef, n)?;
                if !d.types_agree() {
                    return None;
                }
                self.tref_int(table, d.target, depth + 1)
            }
            TRef::Named(DefKind::Enum, n) => {
                let d = self.def_named(DefKind::Enum, n)?;
                self.tref_int(table, d.under, depth + 1)
            }
            TRef::Anon(i) => {
                let d = table.def(i)?;
                (d.kind == DefKind::Enum)
                    .then(|| self.tref_int(table, d.under, depth + 1))
                    .flatten()
            }
            TRef::Derived {
                kind: DerivedKind::Const | DerivedKind::Volatile,
                target,
                ..
            } => self.tref_int(table, target, depth + 1),
            _ => None,
        }
    }

    fn base_def(&self, base: &Base<'_>) -> Option<(DefView<'w>, String)> {
        match base {
            Base::Tag(kind, name) => Some((
                self.def_named(*kind, name)?,
                format!("{} {name}", kind.keyword()),
            )),
            Base::Typedef(name) => {
                Some((self.def_named(DefKind::Typedef, name)?, name.to_string()))
            }
            _ => None,
        }
    }
}

impl TypeEnv for TypeCx<'_, '_> {
    fn lp64(&self) -> bool {
        self.ws.config.is_y("64BIT")
    }

    fn char_unsigned(&self) -> bool {
        self.char_unsigned
    }

    fn named_size(&self, base: &Base<'_>) -> Option<u64> {
        let (d, spelled) = self.base_def(base)?;
        let size = d.size?;
        self.record(Used::Type {
            spelled,
            def: d.index,
        });
        Some(size)
    }

    fn is_typedef(&self, name: &str) -> bool {
        self.def_named(DefKind::Typedef, name).is_some()
    }

    fn typedef_int(&self, name: &str) -> Option<IntTy> {
        let table = self.table()?;
        let d = self.def_named(DefKind::Typedef, name)?;
        if !d.types_agree() {
            return None;
        }
        self.tref_int(table, d.target, 0)
    }

    fn offset(&self, base: &Base<'_>, path: &[Step<'_>]) -> Option<(u64, Option<u64>)> {
        let table = self.table()?;
        let (start, spelled) = self.base_def(base)?;
        let mut def = match start.kind {
            DefKind::Typedef if start.types_agree() => self.aggregate(table, start.target, 0)?,
            DefKind::Struct | DefKind::Union => start,
            _ => return None,
        };
        let mut offset = 0u64;
        let mut size = def.size;
        let mut current: Option<u32> = None;
        for step in path {
            match *step {
                Step::Field(name) => {
                    if let Some(t) = current {
                        def = self.aggregate(table, t, 0)?;
                    }
                    let (extra, member) = self.find_member(table, &def, name, 0)?;
                    if member.bit_size != 0 {
                        return None;
                    }
                    offset = offset.checked_add(extra)?.checked_add(member.offset?)?;
                    size = member.size;
                    current = Some(member.ty);
                }
                Step::Index(k) => {
                    let t = current?;
                    let TRef::Derived {
                        kind: DerivedKind::Array,
                        target,
                        count,
                    } = table.tref(t)
                    else {
                        return None;
                    };
                    let elem = self.tref_size(table, target, 0)?;
                    if count.is_some_and(|c| k >= c) {
                        return None;
                    }
                    offset = offset.checked_add(k.checked_mul(elem)?)?;
                    size = Some(elem);
                    current = Some(target);
                }
            }
        }
        if path.is_empty() {
            return None;
        }
        self.record(Used::Type {
            spelled,
            def: start.index,
        });
        Some((offset, size))
    }

    fn fn_return(&self, name: &str) -> Option<IntTy> {
        let table = self.table()?;
        let mut found = None;
        for (k, i) in table.named(name) {
            if k != NameKind::Function {
                continue;
            }
            let e = table.entity(i)?;
            if !self.visible(e.file, e.line) {
                continue;
            }
            let t = self.tref_int(table, e.ty, 0)?;
            match found {
                None => found = Some(t),
                Some(f) if f == t => {}
                Some(_) => return None,
            }
        }
        found
    }

    fn enumerator(&self, name: &str) -> Option<i128> {
        let (def, value) = self.enumerator_at(name)?;
        self.record(Used::Enumerator {
            name: name.to_string(),
            def,
            value,
        });
        Some(value)
    }
}

impl<'w> TypeCx<'_, 'w> {
    pub fn enumerator_at(&self, name: &str) -> Option<(u32, i128)> {
        let known = self
            .ws
            .index
            .lookup(name)
            .iter()
            .any(|p| p.kind == crate::index::DefKind::Enumerator);
        if !known {
            return None;
        }
        let table = self.table()?;
        let mut found: Option<(u32, i128)> = None;
        for (k, i) in table.named(name) {
            if k != NameKind::Enumerator {
                continue;
            }
            let e = table.enumerator(i)?;
            let d = table.def(e.def)?;
            if !self.visible(d.file, d.line) {
                continue;
            }
            if !d.layout_agrees() {
                return None;
            }
            match found {
                None => found = Some((e.def, e.value)),
                Some((_, v)) if v == e.value => {}
                Some(_) => return None,
            }
        }
        let (def, value) = found?;
        if !(i128::from(i32::MIN)..=i128::from(i32::MAX)).contains(&value) {
            return None;
        }
        Some((def, value))
    }

    fn find_member(
        &self,
        table: &'w Table,
        def: &DefView<'w>,
        name: &str,
        depth: u32,
    ) -> Option<(u64, crate::types::MemberView<'w>)> {
        if depth > 16 || !def.layout_agrees() {
            return None;
        }
        let mut hit = None;
        for m in table.members(def) {
            if m.name == name {
                if hit.is_some() {
                    return None;
                }
                hit = Some((0, m));
            } else if m.name.is_empty()
                && let Some(inner) = self.aggregate(table, m.ty, 0)
                && let Some((extra, found)) = self.find_member(table, &inner, name, depth + 1)
            {
                if hit.is_some() {
                    return None;
                }
                hit = Some((m.offset?.checked_add(extra)?, found));
            }
        }
        hit
    }
}

pub fn c_base_name(gcc: &str) -> String {
    match gcc {
        "long unsigned int" => "unsigned long".into(),
        "long int" => "long".into(),
        "short unsigned int" => "unsigned short".into(),
        "short int" => "short".into(),
        "long long unsigned int" => "unsigned long long".into(),
        "long long int" => "long long".into(),
        "__int128 unsigned" => "unsigned __int128".into(),
        other => other.into(),
    }
}

pub fn declaration(table: &Table, t: u32, inner: &str) -> String {
    spell(table, t, inner.to_string(), 0)
}

fn join(base: &str, inner: &str) -> String {
    if inner.is_empty() {
        base.to_string()
    } else {
        format!("{base} {inner}")
    }
}

fn spell(table: &Table, t: u32, inner: String, depth: u32) -> String {
    if depth > 32 {
        return join("?", &inner);
    }
    match table.tref(t) {
        TRef::Void => join("void", &inner),
        TRef::Base { name, .. } => join(&c_base_name(name), &inner),
        TRef::Named(DefKind::Typedef, n) => join(n, &inner),
        TRef::Named(kind, n) => join(&format!("{} {n}", kind.keyword()), &inner),
        TRef::Anon(i) => {
            let kw = table.def(i).map_or("struct", |d| d.kind.keyword());
            join(&format!("{kw} {{...}}"), &inner)
        }
        TRef::Derived {
            kind,
            target,
            count,
        } => match kind {
            DerivedKind::Pointer => {
                let wrapped = matches!(
                    table.tref(target),
                    TRef::Derived {
                        kind: DerivedKind::Array,
                        ..
                    } | TRef::Function(_)
                );
                let next = if wrapped {
                    format!("(*{inner})")
                } else {
                    format!("*{inner}")
                };
                spell(table, target, next, depth + 1)
            }
            DerivedKind::Const | DerivedKind::Volatile => {
                let q = if kind == DerivedKind::Const {
                    "const"
                } else {
                    "volatile"
                };
                match table.tref(target) {
                    TRef::Derived {
                        kind: DerivedKind::Pointer,
                        ..
                    } => spell(
                        table,
                        target,
                        format!("{q} {inner}").trim_end().to_string(),
                        depth + 1,
                    ),
                    _ => format!("{q} {}", spell(table, target, inner, depth + 1)),
                }
            }
            DerivedKind::Restrict => spell(table, target, inner, depth + 1),
            DerivedKind::Atomic => format!("_Atomic {}", spell(table, target, inner, depth + 1)),
            DerivedKind::Array => {
                let dim = count.map_or(String::new(), |c| c.to_string());
                spell(table, target, format!("{inner}[{dim}]"), depth + 1)
            }
        },
        TRef::Function(f) => {
            let params: Vec<String> = table
                .params(&f)
                .map(|p| spell(table, p, String::new(), depth + 1))
                .collect();
            let mut list = params.join(", ");
            if f.variadic {
                list = if list.is_empty() {
                    "...".into()
                } else {
                    format!("{list}, ...")
                };
            } else if list.is_empty() && f.prototyped {
                list = "void".into();
            }
            spell(table, f.ret, format!("{inner}({list})"), depth + 1)
        }
        TRef::Unknown => join("?", &inner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<&str> {
        s.split_whitespace().collect()
    }

    #[test]
    fn type_names_and_abstract_declarators() {
        let cases: &[(&str, Option<u64>)] = &[
            ("unsigned long", Some(8)),
            ("long unsigned int", Some(8)),
            ("const char *", Some(8)),
            ("int [ 4 ]", Some(16)),
            ("int * [ 4 ]", Some(32)),
            ("int ( * ) [ 4 ]", Some(8)),
            ("short [ 2 ] [ 3 ]", Some(12)),
            ("char", Some(1)),
            ("_Bool", Some(1)),
            ("long long", Some(8)),
            ("unsigned __int128", Some(16)),
            ("void", None),
            ("void *", Some(8)),
            ("int ( int )", None),
            ("int ( * ) ( int )", Some(8)),
            ("int [ ]", None),
            ("long double", None),
            ("unsigned signed int", None),
        ];
        let env = Basic { lp64: true };
        for (src, want) in cases {
            let w = words(src);
            let t = parse_type_name(&w).unwrap_or_else(|| panic!("{src}"));
            assert_eq!(env.size_of(&t, None), *want, "{src}");
        }
        assert_eq!(
            Basic { lp64: false }.size_of(&parse_type_name(&["long"]).unwrap(), None),
            None
        );
        assert!(
            parse_type_name(&words("struct page"))
                .is_some_and(|t| t.base == Base::Tag(DefKind::Struct, "page"))
        );
        assert!(parse_type_name(&words("struct { int x ; }")).is_none());
        assert!(parse_type_name(&words("pte_t *")).is_some_and(|t| t.ops == vec![Op::Pointer]));
        assert!(parse_type_name(&words("x + 1")).is_none());
        assert!(parse_type_name(&words("unsigned long x")).is_none());
    }
}
