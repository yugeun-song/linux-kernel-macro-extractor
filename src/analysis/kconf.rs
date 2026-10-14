use std::fs::File;
use std::path::Path;

use memmap2::Mmap;

use super::report::{ExpNode, KconfigDefault, KconfigInfo, Report};
use crate::index::FileClass;
use crate::index::format::rd_u32;
use crate::kconfig::eval::number;
use crate::kconfig::{Eval, Kconfig, Source, SymId, SymType};
use crate::store::{hash_hex, slot_lock, write_slot};
use crate::tree::dot_config::Tristate;
use crate::workspace::Workspace;

const TABLE_MAGIC: &[u8; 8] = b"KMXKCI\0\x01";
const TABLE_HEAD: usize = 12;
const TABLE_ENTRY: usize = 16;

enum Bytes {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl std::ops::Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Bytes::Mapped(m) => m,
            Bytes::Owned(v) => v,
        }
    }
}

pub struct InfoTable {
    bytes: Bytes,
    count: usize,
}

impl InfoTable {
    fn new(bytes: Bytes) -> Option<InfoTable> {
        if bytes.len() < TABLE_HEAD || &bytes[..8] != TABLE_MAGIC {
            return None;
        }
        let count = rd_u32(&bytes, 8) as usize;
        if TABLE_HEAD + count * TABLE_ENTRY > bytes.len() {
            return None;
        }
        Some(InfoTable { bytes, count })
    }

    fn open(path: &Path) -> Option<InfoTable> {
        let f = File::open(path).ok()?;
        let map = unsafe { Mmap::map(&f) }.ok()?;
        InfoTable::new(Bytes::Mapped(map))
    }

    fn encode(mut rows: Vec<(String, Vec<u8>)>) -> Vec<u8> {
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows.dedup_by(|a, b| a.0 == b.0);
        let mut blob = Vec::new();
        let mut table = Vec::with_capacity(rows.len() * TABLE_ENTRY);
        let base = TABLE_HEAD + rows.len() * TABLE_ENTRY;
        for (name, data) in &rows {
            for part in [name.as_bytes(), data.as_slice()] {
                table.extend_from_slice(&((base + blob.len()) as u32).to_le_bytes());
                table.extend_from_slice(&(part.len() as u32).to_le_bytes());
                blob.extend_from_slice(part);
            }
        }
        let mut out = Vec::with_capacity(base + blob.len());
        out.extend_from_slice(TABLE_MAGIC);
        out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
        out.extend_from_slice(&table);
        out.extend_from_slice(&blob);
        out
    }

    fn slice(&self, at: usize) -> Option<&[u8]> {
        let off = rd_u32(&self.bytes, at) as usize;
        let len = rd_u32(&self.bytes, at + 4) as usize;
        self.bytes.get(off..off.checked_add(len)?)
    }

    fn find(&self, name: &str) -> Option<&[u8]> {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let at = TABLE_HEAD + mid * TABLE_ENTRY;
            match self.slice(at)?.cmp(name.as_bytes()) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return self.slice(at + 8),
            }
        }
        None
    }

    pub fn names_with_prefix(&self, prefix: &str, limit: usize) -> Vec<String> {
        let p = prefix.as_bytes();
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.slice(TABLE_HEAD + mid * TABLE_ENTRY) {
                Some(n) if n < p => lo = mid + 1,
                _ => hi = mid,
            }
        }
        let mut out = Vec::new();
        for i in lo..self.count {
            let at = TABLE_HEAD + i * TABLE_ENTRY;
            let Some(name) = self.slice(at) else { break };
            if !name.starts_with(p) || out.len() >= limit {
                break;
            }
            let defined = self.slice(at + 8).is_some_and(|d| !d.is_empty());
            if defined && let Ok(n) = std::str::from_utf8(name) {
                out.push(n.to_string());
            }
        }
        out
    }

    pub fn get(&self, name: &str) -> Option<KconfigInfo> {
        let bare = name.strip_prefix("CONFIG_").unwrap_or(name);
        let key = bare
            .strip_suffix("_MODULE")
            .filter(|b| self.find(b).is_some())
            .unwrap_or(bare);
        let data = self.find(key).filter(|d| !d.is_empty())?;
        serde_json::from_slice(data).ok()
    }
}

fn table_key(ws: &Workspace) -> String {
    let mut text = format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\n",
        ws.layout.srctree.display(),
        ws.target.arch.srcarch(),
        ws.tree.version.release(),
        ws.manifest.config.hash
    );
    for f in ws.index.files().filter(|f| f.class() == FileClass::Kconfig) {
        text.push_str(f.path);
        text.push_str(&format!("\u{1f}{:032x}\n", f.hash));
    }
    hash_hex(text.as_bytes())
}

fn build_table(ws: &Workspace) -> Vec<u8> {
    let k = load(ws);
    let mut ev = Eval::new(&k, &ws.config);
    let rows: Vec<(String, Vec<u8>)> = (0..k.symbols.len() as SymId)
        .map(|s| {
            let data = info_of(&k, &mut ev, s)
                .and_then(|info| serde_json::to_vec(&info).ok())
                .unwrap_or_default();
            (k.sym(s).name.clone(), data)
        })
        .collect();
    InfoTable::encode(rows)
}

impl Workspace {
    pub fn kconfig_table(&self) -> Option<&InfoTable> {
        self.kconfig_table
            .get_or_init(|| {
                let dir = self.cache_dir();
                let name = format!("kconfig-{}.tbl", table_key(self));
                if let Some(t) = InfoTable::open(&dir.join(&name)) {
                    return Some(t);
                }
                let _guard = slot_lock(dir, ".kconfig.lock");
                if let Some(t) = InfoTable::open(&dir.join(&name)) {
                    return Some(t);
                }
                let bytes = build_table(self);
                if let Ok(path) = write_slot(dir, "kconfig-", &name, &bytes)
                    && let Some(t) = InfoTable::open(&path)
                {
                    return Some(t);
                }
                InfoTable::new(Bytes::Owned(bytes))
            })
            .as_ref()
    }
}

pub fn load(ws: &Workspace) -> Kconfig {
    let env = crate::kconfig::Env {
        arch: ws
            .config
            .header_arch
            .clone()
            .unwrap_or_else(|| ws.target.arch.name().to_string()),
        srcarch: ws.target.arch.srcarch().to_string(),
        kernelversion: ws.tree.version.release(),
        srctree: ws.layout.srctree.clone(),
    };
    crate::kconfig::load(&env)
}

fn with_values(k: &Kconfig, ev: &mut Eval<'_>, e: &crate::kconfig::model::Expr) -> String {
    let mut syms = Vec::new();
    e.symbols(&mut syms);
    syms.sort_unstable();
    syms.dedup();
    let vals: Vec<String> = syms
        .iter()
        .take(6)
        .map(|s| format!("{}={}", k.sym(*s).name, ev.value(*s).0.text()))
        .collect();
    vals.join(", ")
}

pub fn info(k: &Kconfig, ev: &mut Eval<'_>, name: &str) -> Option<KconfigInfo> {
    let s = k.lookup(
        name.strip_suffix("_MODULE")
            .filter(|b| k.lookup(b).is_some())
            .unwrap_or(name),
    )?;
    info_of(k, ev, s)
}

fn info_of(k: &Kconfig, ev: &mut Eval<'_>, s: SymId) -> Option<KconfigInfo> {
    let sym = k.sym(s);
    if !sym.is_defined() {
        return None;
    }
    let (val, source) = ev.value(s);
    let ty = ev.sym_type(s);
    let vis = ev.visibility(s);
    let dir = ev.dir_dep(s);
    let rev = ev.rev_dep(s);
    let prompt = sym
        .defs
        .iter()
        .flat_map(|d| d.prompts.first())
        .next()
        .map(|p| p.text.clone());
    let deps_text: Vec<String> = sym
        .defs
        .iter()
        .filter(|d| !d.deps.is_yes())
        .map(|d| k.expr_text(&d.deps))
        .collect();
    let mut defaults = Vec::new();
    for (di, d) in sym.defs.iter().enumerate() {
        for (i, df) in d.defaults.iter().enumerate() {
            let cond_full = crate::kconfig::model::Expr::and(df.cond.clone(), d.deps.clone());
            let cv = ev.eval(&cond_full);
            let chosen = matches!(source, Source::Default { def, idx } | Source::Environment { def, idx } if def == di && idx == i);
            defaults.push(KconfigDefault {
                value: if df.opaque {
                    format!("{} (toolchain test)", k.expr_text(&df.value))
                } else {
                    let v = k.expr_text(&df.value);
                    match &df.value {
                        crate::kconfig::model::Expr::Sym(x) => {
                            format!("{v} (={})", ev.value(*x).0.text())
                        }
                        _ => v,
                    }
                },
                condition: (!df.cond.is_yes()).then(|| k.expr_text(&df.cond)),
                condition_value: (!df.cond.is_yes()).then(|| {
                    let vals = with_values(k, ev, &df.cond);
                    if vals.is_empty() {
                        cv.to_string()
                    } else {
                        format!("{cv}: {vals}")
                    }
                }),
                chosen,
                site: k.loc_text(df.loc),
            });
        }
    }
    let selected_by: Vec<(String, String)> = sym
        .selected_by
        .iter()
        .map(|(by, cond, loc)| {
            let v = ev.tri(*by);
            let c = ev.eval(cond);
            (
                k.sym(*by).name.clone(),
                format!("{v} (condition {c}, {})", k.loc_text(*loc)),
            )
        })
        .collect();
    let implied_by: Vec<(String, String)> = sym
        .implied_by
        .iter()
        .map(|(by, cond, loc)| {
            let v = ev.tri(*by);
            let c = ev.eval(cond);
            (
                k.sym(*by).name.clone(),
                format!("{v} (condition {c}, {})", k.loc_text(*loc)),
            )
        })
        .collect();
    let choice = sym.choice.map(|c| {
        k.choices[c as usize]
            .members
            .iter()
            .map(|&m| (k.sym(m).name.clone(), ev.value(m).0.text()))
            .collect::<Vec<_>>()
    });
    let range = sym
        .defs
        .iter()
        .flat_map(|d| d.ranges.first())
        .next()
        .map(|r| format!("{} .. {}", k.expr_text(&r.lo), k.expr_text(&r.hi)));
    let reason = match source {
        Source::User => format!(
            "set to {} in .config; the prompt is visible ({vis}), so the user value applies",
            val.text()
        ),
        Source::Default { def, idx } => {
            let d = &sym.defs[def].defaults[idx];
            let cond = if d.cond.is_yes() {
                String::new()
            } else {
                format!(" if {}", k.expr_text(&d.cond))
            };
            let hidden = if vis == Tristate::No && prompt.is_some() {
                " (the prompt is hidden, so the .config value is not consulted)"
            } else if prompt.is_none() {
                " (no prompt: the value is always computed)"
            } else {
                ""
            };
            format!("default {}{cond} applies{hidden}", k.expr_text(&d.value))
        }
        Source::Select => {
            let mut who = Vec::new();
            for (by, cond, _) in &sym.selected_by {
                let v = ev.tri(*by);
                if v.and(ev.eval(cond)) != Tristate::No {
                    who.push(format!("{}={v}", k.sym(*by).name));
                }
            }
            format!("forced to {} by `select` from {}", val.text(), who.join(", "))
        }
        Source::Imply => format!("raised to {} by `imply`", val.text()),
        Source::Choice => {
            let c = sym.choice.unwrap_or(0);
            match ev.choice_selection(c) {
                Some((m, why)) if m == s => format!("selected in its choice ({why})"),
                Some((m, why)) => format!("not selected: the choice picked {} ({why})", k.sym(m).name),
                None => "the choice has no visible member".to_string(),
            }
        }
        Source::Environment { .. } => {
            "decided by a toolchain test inside Kconfig ($(shell,...)/$(cc-option,...)); the value is taken from .config".to_string()
        }
        Source::Transitional => "transitional symbol: it only reads values from older .config files".to_string(),
        Source::Unset => {
            if dir == Tristate::No {
                "its dependencies are not met, so it stays n".to_string()
            } else {
                "no default applies and nothing selects it, so it stays n".to_string()
            }
        }
    };
    let mut reason = reason;
    if rev > dir && matches!(ty, SymType::Bool | SymType::Tristate) {
        reason.push_str("; note: selected although its own dependencies are not met");
    }
    Some(KconfigInfo {
        symbol: format!("CONFIG_{}", sym.name),
        kind: if sym.choice.is_some() {
            format!("{} (choice member)", ty.name())
        } else {
            ty.name().to_string()
        },
        value: val.text(),
        sites: sym.defs.iter().map(|d| k.loc_text(d.loc)).collect(),
        prompt,
        visible: Some(vis.to_string()),
        depends: (!deps_text.is_empty()).then(|| deps_text.join(" || ")),
        depends_value: Some(dir.to_string()),
        defaults,
        selected_by,
        implied_by,
        choice,
        range,
        reason,
    })
}

fn type_error(name: &str, ty: SymType, value: &str) -> Option<String> {
    let ok = match ty {
        SymType::Bool => matches!(value, "y" | "n"),
        SymType::Tristate => matches!(value, "y" | "m" | "n"),
        SymType::Int => value.parse::<i128>().is_ok(),
        SymType::Hex => value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
            .is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit())),
        SymType::String => true,
        SymType::Unknown => false,
    };
    let want = match ty {
        SymType::Bool => "y or n",
        SymType::Tristate => "y, m or n",
        SymType::Int => "a decimal number",
        SymType::Hex => "a 0x-prefixed hexadecimal number",
        SymType::String => "any string",
        SymType::Unknown => "a symbol with a type",
    };
    let article = if ty == SymType::Int { "an" } else { "a" };
    (!ok).then(|| {
        format!(
            "{name} is {article} {} symbol, so the value must be {want}",
            ty.name()
        )
    })
}

fn same_value(ty: SymType, got: &str, want: &str) -> bool {
    match ty {
        SymType::Int | SymType::Hex => match (number(got), number(want)) {
            (Some(a), Some(b)) => a == b,
            _ => got == want,
        },
        _ => got == want,
    }
}

pub fn apply_overrides(k: &Kconfig, ev: &mut Eval<'_>, sets: &[String]) -> Result<(), String> {
    let mut requests: Vec<(&str, SymId, String, &str)> = Vec::new();
    for s in sets {
        let (name, value) = s
            .split_once('=')
            .ok_or_else(|| format!("--set {s}: expected SYMBOL=VALUE"))?;
        let name = name.trim().strip_prefix("CONFIG_").unwrap_or(name.trim());
        let id = k
            .lookup(name)
            .filter(|id| k.sym(*id).is_defined())
            .ok_or_else(|| format!("--set {s}: no Kconfig symbol named {name}"))?;
        let value = value.trim().trim_matches('"');
        if let Some(e) = type_error(name, ev.sym_type(id), value) {
            return Err(format!("--set {s}: {e}"));
        }
        ev.set_user(id, value);
        requests.push((name, id, value.to_string(), s));
    }
    let mut errors = Vec::new();
    for (name, id, want, raw) in &requests {
        let ty = ev.sym_type(*id);
        if let (Some((lo, hi)), Some(v)) = (ev.range(*id), number(want))
            && (v < lo || v > hi)
        {
            errors.push(format!("--set {raw}: {name} must be within {lo}..{hi}"));
            continue;
        }
        let got = ev.value(*id).0.text();
        if !same_value(ty, &got, want) {
            let why = info_of(k, ev, *id).map(|i| i.reason).unwrap_or_default();
            errors.push(format!("--set {raw}: Kconfig keeps {name} at {got}: {why}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn walk_config(n: &mut ExpNode, f: &mut dyn FnMut(&mut ExpNode)) {
    if n.name.starts_with("CONFIG_") {
        f(n);
    }
    for c in &mut n.children {
        walk_config(c, f);
    }
}

fn has_config(n: &ExpNode) -> bool {
    n.name.starts_with("CONFIG_") || n.children.iter().any(has_config)
}

pub struct WhatIf {
    pub autoconf: String,
    pub notes: Vec<String>,
}

pub fn what_if(ws: &Workspace, sets: &[String]) -> Result<WhatIf, String> {
    let k = load(ws);
    let mut ev = Eval::new(&k, &ws.config);
    apply_overrides(&k, &mut ev, sets)?;
    let mut notes = Vec::new();
    let changes = ev.mismatches();
    let list: Vec<String> = changes
        .iter()
        .take(12)
        .map(|(n, new, old)| format!("{n}: {old} -> {new}"))
        .collect();
    let more = changes.len().saturating_sub(12);
    let summary = match (changes.len(), more) {
        (0, _) => "what-if: no Kconfig symbol changes its value".to_string(),
        (n, 0) => format!("what-if: {n} Kconfig symbol(s) change: {}", list.join(", ")),
        (n, m) => format!(
            "what-if: {n} Kconfig symbol(s) change: {}, +{m} more",
            list.join(", ")
        ),
    };
    notes.insert(0, summary);
    Ok(WhatIf {
        autoconf: ev.autoconf_h(),
        notes,
    })
}

pub fn attach(ws: &Workspace, report: &mut Report, sets: &[String]) {
    let wants =
        report.query.starts_with("CONFIG_") || report.expansion.as_ref().is_some_and(has_config);
    if !wants {
        return;
    }
    if sets.is_empty()
        && let Some(table) = ws.kconfig_table()
    {
        if let Some(root) = report.expansion.as_mut() {
            walk_config(root, &mut |n| n.kconfig = table.get(&n.name));
        }
        if report.query.starts_with("CONFIG_") {
            report.kconfig = table.get(&report.query);
        }
        return;
    }
    let k = load(ws);
    let mut ev = Eval::new(&k, &ws.config);
    if !sets.is_empty() {
        let _ = apply_overrides(&k, &mut ev, sets);
    }
    if let Some(root) = report.expansion.as_mut() {
        walk_config(root, &mut |n| n.kconfig = info(&k, &mut ev, &n.name));
    }
    if report.query.starts_with("CONFIG_") {
        report.kconfig = info(&k, &mut ev, &report.query);
    }
}
