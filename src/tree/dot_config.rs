use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tristate {
    No,
    Module,
    Yes,
}

impl Tristate {
    pub fn parse(s: &str) -> Option<Tristate> {
        match s {
            "y" => Some(Tristate::Yes),
            "m" => Some(Tristate::Module),
            "n" => Some(Tristate::No),
            _ => None,
        }
    }

    pub fn as_char(self) -> char {
        match self {
            Tristate::No => 'n',
            Tristate::Module => 'm',
            Tristate::Yes => 'y',
        }
    }

    pub fn and(self, other: Tristate) -> Tristate {
        self.min(other)
    }

    pub fn or(self, other: Tristate) -> Tristate {
        self.max(other)
    }
}

impl std::ops::Not for Tristate {
    type Output = Tristate;

    fn not(self) -> Tristate {
        match self {
            Tristate::No => Tristate::Yes,
            Tristate::Module => Tristate::Module,
            Tristate::Yes => Tristate::No,
        }
    }
}

impl fmt::Display for Tristate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char(self.as_char())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigValue {
    Tristate(Tristate),
    Str(String),
    Num(String),
}

impl ConfigValue {
    pub fn tristate(&self) -> Option<Tristate> {
        match self {
            ConfigValue::Tristate(t) => Some(*t),
            _ => None,
        }
    }

    pub fn as_config_text(&self) -> String {
        match self {
            ConfigValue::Tristate(t) => t.to_string(),
            ConfigValue::Str(s) => format!("\"{}\"", escape(s)),
            ConfigValue::Num(n) => n.clone(),
        }
    }
}

impl fmt::Display for ConfigValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigValue::Tristate(Tristate::No) => f.write_str("is not set"),
            other => f.write_str(&other.as_config_text()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub value: ConfigValue,
    pub line: u32,
}

#[derive(Clone, Debug, Default)]
pub struct DotConfig {
    pub path: PathBuf,
    pub header_arch: Option<String>,
    pub header_release: Option<String>,
    items: Vec<(Arc<str>, Entry)>,
    index: FxHashMap<Arc<str>, u32>,
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn strip_prefix(name: &str) -> &str {
    name.strip_prefix("CONFIG_").unwrap_or(name)
}

impl DotConfig {
    pub fn load(path: &Path) -> std::io::Result<DotConfig> {
        let text = std::fs::read_to_string(path)?;
        let mut cfg = DotConfig::parse(&text);
        cfg.path = path.to_path_buf();
        Ok(cfg)
    }

    pub fn parse(text: &str) -> DotConfig {
        let lines = text.len() / 32;
        let mut cfg = DotConfig {
            items: Vec::with_capacity(lines),
            index: FxHashMap::with_capacity_and_hasher(lines, Default::default()),
            ..DotConfig::default()
        };
        for (idx, raw) in text.lines().enumerate() {
            let line = idx as u32 + 1;
            let l = raw.trim_end();
            if let Some(rest) = l.strip_prefix("# Linux/") {
                if let Some((arch, tail)) = rest.split_once(' ') {
                    cfg.header_arch = Some(arch.to_string());
                    cfg.header_release = tail.split_whitespace().next().map(str::to_string);
                }
                continue;
            }
            if let Some(rest) = l.strip_prefix("# CONFIG_") {
                if let Some(name) = rest.strip_suffix(" is not set") {
                    cfg.insert(name, ConfigValue::Tristate(Tristate::No), line);
                }
                continue;
            }
            let Some(rest) = l.strip_prefix("CONFIG_") else {
                continue;
            };
            let Some((name, val)) = rest.split_once('=') else {
                continue;
            };
            let value = if let Some(t) = Tristate::parse(val) {
                ConfigValue::Tristate(t)
            } else if let Some(inner) = val.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
                ConfigValue::Str(unescape(inner))
            } else {
                ConfigValue::Num(val.to_string())
            };
            cfg.insert(name, value, line);
        }
        cfg
    }

    fn insert(&mut self, name: &str, value: ConfigValue, line: u32) {
        let entry = Entry { value, line };
        match self.index.get(name) {
            Some(&i) => self.items[i as usize].1 = entry,
            None => {
                let key: Arc<str> = name.into();
                self.index.insert(key.clone(), self.items.len() as u32);
                self.items.push((key, entry));
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        let i = *self.index.get(strip_prefix(name))?;
        Some(&self.items[i as usize].1)
    }

    pub fn value(&self, name: &str) -> Option<&ConfigValue> {
        self.get(name).map(|e| &e.value)
    }

    pub fn tristate(&self, name: &str) -> Tristate {
        self.value(name)
            .and_then(ConfigValue::tristate)
            .unwrap_or(Tristate::No)
    }

    pub fn is_y(&self, name: &str) -> bool {
        self.tristate(name) == Tristate::Yes
    }

    pub fn num(&self, name: &str) -> Option<u64> {
        match self.value(name)? {
            ConfigValue::Num(n) => parse_num(n),
            _ => None,
        }
    }

    pub fn string(&self, name: &str) -> Option<&str> {
        match self.value(name)? {
            ConfigValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn set(&mut self, name: &str, value: ConfigValue) {
        let name = strip_prefix(name);
        match self.index.get(name) {
            Some(&i) => self.items[i as usize].1.value = value,
            None => self.insert(name, value, 0),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.items.iter().map(|(n, e)| (&**n, e))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn autoconf_h(&self) -> String {
        let mut out = String::with_capacity(self.items.len() * 40);
        out.push_str("#define AUTOCONF_INCLUDED\n");
        for (name, e) in self.iter() {
            let _ = match &e.value {
                ConfigValue::Tristate(Tristate::Yes) => writeln!(out, "#define CONFIG_{name} 1"),
                ConfigValue::Tristate(Tristate::Module) => {
                    writeln!(out, "#define CONFIG_{name}_MODULE 1")
                }
                ConfigValue::Tristate(Tristate::No) => Ok(()),
                ConfigValue::Str(s) => writeln!(out, "#define CONFIG_{name} \"{}\"", escape(s)),
                ConfigValue::Num(n) => writeln!(out, "#define CONFIG_{name} {n}"),
            };
        }
        out
    }
}

pub fn parse_num(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()
    } else if let Some(neg) = s.strip_prefix('-') {
        neg.parse::<u64>().ok().map(|v| v.wrapping_neg())
    } else {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "#\n# Automatically generated file; DO NOT EDIT.\n# Linux/x86_64 6.12.112 Kernel Configuration\n#\nCONFIG_CC_VERSION_TEXT=\"gcc (GCC) 16.2.1 20260810\"\nCONFIG_CC_IS_GCC=y\nCONFIG_GCC_VERSION=160201\nCONFIG_64BIT=y\n# CONFIG_X86_32 is not set\nCONFIG_EXT4_FS=m\nCONFIG_PHYSICAL_START=0x1000000\nCONFIG_CMDLINE=\"a \\\"b\\\"\"\n";

    #[test]
    fn parses_values() {
        let c = DotConfig::parse(SAMPLE);
        assert_eq!(c.header_arch.as_deref(), Some("x86_64"));
        assert_eq!(c.header_release.as_deref(), Some("6.12.112"));
        assert!(c.is_y("CONFIG_64BIT"));
        assert!(c.is_y("64BIT"));
        assert_eq!(c.tristate("X86_32"), Tristate::No);
        assert!(c.get("X86_32").is_some());
        assert_eq!(c.tristate("EXT4_FS"), Tristate::Module);
        assert_eq!(c.num("GCC_VERSION"), Some(160201));
        assert_eq!(c.num("PHYSICAL_START"), Some(0x1000000));
        assert_eq!(c.string("CMDLINE"), Some("a \"b\""));
        assert_eq!(c.get("GCC_VERSION").unwrap().line, 7);
    }

    #[test]
    fn autoconf_synthesis() {
        let c = DotConfig::parse(SAMPLE);
        let h = c.autoconf_h();
        assert!(h.contains("#define CONFIG_64BIT 1\n"));
        assert!(h.contains("#define CONFIG_EXT4_FS_MODULE 1\n"));
        assert!(!h.contains("X86_32"));
        assert!(h.contains("#define CONFIG_CMDLINE \"a \\\"b\\\"\"\n"));
        assert!(h.contains("#define CONFIG_PHYSICAL_START 0x1000000\n"));
    }

    #[test]
    fn tristate_logic() {
        use Tristate::*;
        assert_eq!(Yes.and(Module), Module);
        assert_eq!(No.or(Module), Module);
        assert_eq!(!Module, Module);
        assert_eq!(!Yes, No);
    }
}
