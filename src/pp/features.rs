use std::io::Write as _;
use std::process::{Command, Stdio};

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::tree::arch::{Arch, Compiler, CompilerKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Probe {
    Attribute,
    Builtin,
    Feature,
    CAttribute,
}

impl Probe {
    pub fn operator(self) -> &'static str {
        match self {
            Probe::Attribute => "__has_attribute",
            Probe::Builtin => "__has_builtin",
            Probe::Feature => "__has_feature",
            Probe::CAttribute => "__has_c_attribute",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Probed,
    Table,
    Assumed,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Probed {
    pub compiler: String,
    pub answers: Vec<(Probe, String, bool)>,
}

const KNOWN: &[(Probe, &str)] = &[
    (Probe::Attribute, "btf_type_tag"),
    (Probe::Attribute, "__copy__"),
    (Probe::Attribute, "__counted_by__"),
    (Probe::Attribute, "__designated_init__"),
    (Probe::Attribute, "__diagnose_as_builtin__"),
    (Probe::Attribute, "disable_sanitizer_instrumentation"),
    (Probe::Attribute, "__error__"),
    (Probe::Attribute, "__externally_visible__"),
    (Probe::Attribute, "__fallthrough__"),
    (Probe::Attribute, "__no_caller_saved_registers__"),
    (Probe::Attribute, "__noclone__"),
    (Probe::Attribute, "noipa"),
    (Probe::Attribute, "__nonstring__"),
    (Probe::Attribute, "__no_profile_instrument_function__"),
    (Probe::Attribute, "__no_sanitize_coverage__"),
    (Probe::Attribute, "__no_stack_protector__"),
    (Probe::Attribute, "__overloadable__"),
    (Probe::Attribute, "__pass_dynamic_object_size__"),
    (Probe::Attribute, "__pass_object_size__"),
    (Probe::Attribute, "__preserve_most__"),
    (Probe::Attribute, "__retain__"),
    (Probe::Attribute, "__uninitialized__"),
    (Probe::Attribute, "__warning__"),
    (Probe::Builtin, "__builtin_alloca_uninitialized"),
    (Probe::Builtin, "__builtin_counted_by_ref"),
    (Probe::Builtin, "__builtin_dynamic_object_size"),
    (Probe::Builtin, "__builtin_has_attribute"),
    (Probe::Builtin, "__builtin_isdigit"),
    (Probe::Builtin, "__builtin_unreachable"),
    (Probe::Feature, "address_sanitizer"),
    (Probe::Feature, "coverage_sanitizer"),
    (Probe::Feature, "hwaddress_sanitizer"),
    (Probe::Feature, "kcfi"),
    (Probe::Feature, "memory_sanitizer"),
    (Probe::Feature, "shadow_call_stack"),
    (Probe::Feature, "thread_sanitizer"),
    (Probe::Feature, "undefined_behavior_sanitizer"),
];

const GCC_ATTRIBUTES: &[(&str, u32)] = &[
    ("aligned", 3),
    ("alias", 3),
    ("alloc_size", 4),
    ("always_inline", 3),
    ("assume", 13),
    ("assume_aligned", 4),
    ("cleanup", 3),
    ("cold", 4),
    ("const", 3),
    ("constructor", 3),
    ("copy", 9),
    ("counted_by", 15),
    ("deprecated", 3),
    ("designated_init", 5),
    ("destructor", 3),
    ("error", 4),
    ("externally_visible", 4),
    ("fallthrough", 7),
    ("flatten", 4),
    ("format", 3),
    ("gnu_inline", 4),
    ("hot", 4),
    ("malloc", 3),
    ("may_alias", 3),
    ("mode", 3),
    ("naked", 8),
    ("no_instrument_function", 3),
    ("no_profile_instrument_function", 7),
    ("no_reorder", 5),
    ("no_sanitize", 8),
    ("no_sanitize_address", 4),
    ("no_sanitize_coverage", 12),
    ("no_sanitize_undefined", 5),
    ("no_stack_protector", 11),
    ("noclone", 4),
    ("noinline", 3),
    ("noipa", 8),
    ("nonnull", 3),
    ("nonstring", 8),
    ("noreturn", 3),
    ("optimize", 4),
    ("packed", 3),
    ("pure", 3),
    ("retain", 11),
    ("returns_nonnull", 5),
    ("section", 3),
    ("strict_flex_array", 13),
    ("target", 4),
    ("unavailable", 12),
    ("uninitialized", 12),
    ("unused", 3),
    ("used", 3),
    ("visibility", 4),
    ("warn_unused_result", 4),
    ("warning", 4),
    ("weak", 3),
    ("zero_call_used_regs", 11),
];

const GCC_X86_ONLY_ATTRIBUTES: &[(&str, u32)] = &[
    ("no_caller_saved_registers", 7),
    ("nocf_check", 8),
    ("indirect_branch", 8),
    ("function_return", 8),
    ("interrupt", 7),
];

const CLANG_ONLY_ATTRIBUTES: &[&str] = &[
    "overloadable",
    "pass_object_size",
    "pass_dynamic_object_size",
    "diagnose_as_builtin",
    "disable_sanitizer_instrumentation",
    "preserve_most",
    "preserve_all",
    "enable_if",
    "diagnose_if",
    "btf_type_tag",
];

const CLANG_ONLY_BUILTINS: &[&str] = &[
    "__builtin_alloca_uninitialized",
    "__builtin_preserve_access_index",
    "__builtin_assume",
];

const GCC_BUILTINS: &[(&str, u32)] = &[
    ("__builtin_counted_by_ref", 15),
    ("__builtin_dynamic_object_size", 12),
    ("__builtin_has_attribute", 9),
    ("__builtin_isdigit", 10),
    ("__builtin_unreachable", 10),
    ("__builtin_constant_p", 10),
    ("__builtin_expect", 10),
    ("__builtin_object_size", 10),
];

fn bare(name: &str) -> &str {
    let n = name.rsplit("::").next().unwrap_or(name);
    n.strip_prefix("__")
        .and_then(|n| n.strip_suffix("__"))
        .unwrap_or(n)
}

#[derive(Clone)]
pub struct Features {
    compiler: Compiler,
    arch: Arch,
    sanitizers: Vec<String>,
    probed: FxHashMap<(Probe, String), bool>,
    probed_by: Option<String>,
}

impl Features {
    pub fn new(
        compiler: &Compiler,
        arch: Arch,
        flags: &[String],
        probed: Option<&Probed>,
    ) -> Features {
        let sanitizers = flags
            .iter()
            .filter_map(|f| f.strip_prefix("-fsanitize="))
            .flat_map(|l| l.split(',').map(str::to_string))
            .collect();
        let mut map = FxHashMap::default();
        if let Some(p) = probed {
            for (k, n, v) in &p.answers {
                map.insert((*k, n.clone()), *v);
            }
        }
        Features {
            compiler: compiler.clone(),
            arch,
            sanitizers,
            probed: map,
            probed_by: probed.map(|p| p.compiler.clone()),
        }
    }

    pub fn probed_by(&self) -> Option<&str> {
        self.probed_by.as_deref()
    }

    pub fn operator_defined(&self, op: Probe, asm: bool) -> bool {
        let c = &self.compiler;
        match (c.kind, op) {
            (CompilerKind::Clang, _) => true,
            (CompilerKind::Gcc, Probe::Feature) => c.major >= 14,
            (CompilerKind::Gcc, _) if asm => false,
            (CompilerKind::Gcc, Probe::Attribute) => c.major >= 5,
            (CompilerKind::Gcc, Probe::Builtin) => c.major >= 10,
            (CompilerKind::Gcc, Probe::CAttribute) => c.major >= 11,
        }
    }

    pub fn query(&self, op: Probe, name: &str) -> (bool, Answer) {
        if let Some(&v) = self.probed.get(&(op, name.to_string())) {
            return (v, Answer::Probed);
        }
        let c = &self.compiler;
        let n = bare(name);
        match (c.kind, op) {
            (CompilerKind::Gcc, Probe::Attribute) => {
                if let Some(&(_, v)) = GCC_ATTRIBUTES.iter().find(|(a, _)| *a == n) {
                    return (c.major >= v, Answer::Table);
                }
                if let Some(&(_, v)) = GCC_X86_ONLY_ATTRIBUTES.iter().find(|(a, _)| *a == n) {
                    return (self.arch == Arch::X86_64 && c.major >= v, Answer::Table);
                }
                if CLANG_ONLY_ATTRIBUTES.contains(&n) {
                    return (false, Answer::Table);
                }
                (false, Answer::Assumed)
            }
            (CompilerKind::Gcc, Probe::Builtin) => {
                match GCC_BUILTINS.iter().find(|(b, _)| *b == name) {
                    Some(&(_, v)) => (c.major >= v, Answer::Table),
                    None if CLANG_ONLY_BUILTINS.contains(&name) => (false, Answer::Table),
                    None => (false, Answer::Assumed),
                }
            }
            (_, Probe::Feature) => {
                let on = |s: &[&str]| self.sanitizers.iter().any(|x| s.contains(&x.as_str()));
                let v = match n {
                    "address_sanitizer" => on(&["address", "kernel-address"]),
                    "hwaddress_sanitizer" => on(&["hwaddress", "kernel-hwaddress"]),
                    "thread_sanitizer" => on(&["thread"]),
                    "memory_sanitizer" => on(&["memory", "kernel-memory"]),
                    "undefined_behavior_sanitizer" => on(&["undefined"]),
                    "shadow_call_stack" => on(&["shadow-call-stack"]),
                    "kcfi" => on(&["kcfi"]),
                    _ => false,
                };
                (
                    v && self.operator_defined(Probe::Feature, false),
                    Answer::Table,
                )
            }
            (CompilerKind::Clang, Probe::Attribute) => {
                let gcc_only = ["copy", "externally_visible", "noclone", "noipa"];
                (!gcc_only.contains(&n), Answer::Assumed)
            }
            (CompilerKind::Clang, Probe::Builtin) => {
                (name != "__builtin_has_attribute", Answer::Assumed)
            }
            (_, Probe::CAttribute) => (false, Answer::Assumed),
        }
    }
}

fn binary_version(bin: &str) -> Option<u32> {
    let out = Command::new(bin)
        .arg("-dumpversion")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().split('.').next()?.parse().ok()
}

fn candidates(compiler: &Compiler, arch: Arch, hint: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if let Some(h) = hint {
        v.push(h.to_string());
        if let Some((base, _)) = h.rsplit_once('-')
            && (base.ends_with("gcc") || base.ends_with("clang"))
        {
            v.push(base.to_string());
        }
    }
    match compiler.kind {
        CompilerKind::Gcc => {
            let triple = match arch {
                Arch::X86_64 => "x86_64-linux-gnu",
                Arch::Arm64 => "aarch64-linux-gnu",
                Arch::Riscv64 => "riscv64-linux-gnu",
            };
            v.push(format!("{triple}-gcc-{}", compiler.major));
            v.push(format!("{triple}-gcc"));
            if arch == Arch::X86_64 {
                v.push(format!("gcc-{}", compiler.major));
                v.push("gcc".to_string());
            }
        }
        CompilerKind::Clang => {
            v.push(format!("clang-{}", compiler.major));
            v.push("clang".to_string());
        }
    }
    v.dedup();
    v
}

pub fn find_compiler(compiler: &Compiler, arch: Arch, hint: Option<&str>) -> Option<String> {
    candidates(compiler, arch, hint)
        .into_iter()
        .find(|b| binary_version(b) == Some(compiler.major))
}

pub fn resolve_in_path(bin: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(bin);
    if p.components().count() > 1 {
        return Some(p.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|d| d.join(bin))
        .find(|c| c.is_file())
}

pub fn target_flag(compiler: &Compiler, arch: Arch) -> Option<&'static str> {
    (compiler.kind == CompilerKind::Clang).then_some(match arch {
        Arch::X86_64 => "--target=x86_64-linux-gnu",
        Arch::Arm64 => "--target=aarch64-linux-gnu",
        Arch::Riscv64 => "--target=riscv64-linux-gnu",
    })
}

pub fn run_preprocessor(
    bin: &str,
    args: &[String],
    lang: &str,
    input: &str,
    cwd: &std::path::Path,
) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
        .args(["-E", "-x", lang, "-"])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn probe(bin: &str, args: &[String], cwd: &std::path::Path) -> Option<Probed> {
    let mut src = String::new();
    for (i, (op, name)) in KNOWN.iter().enumerate() {
        src.push_str(&format!(
            "#if defined({0}) && {0}({1})\nkmx_probe_{i}\n#endif\n",
            op.operator(),
            name
        ));
    }
    let mut a = args.to_vec();
    a.push("-P".to_string());
    let text = run_preprocessor(bin, &a, "c", &src, cwd)?;
    let hits: rustc_hash::FxHashSet<usize> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("kmx_probe_")?.parse().ok())
        .collect();
    Some(Probed {
        compiler: bin.to_string(),
        answers: KNOWN
            .iter()
            .enumerate()
            .map(|(i, (op, name))| (*op, name.to_string(), hits.contains(&i)))
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_answers() {
        let gcc9 = Compiler::parse_spec("gcc-9.4").unwrap();
        let f = Features::new(&gcc9, Arch::Arm64, &[], None);
        assert_eq!(f.query(Probe::Attribute, "__copy__"), (true, Answer::Table));
        assert_eq!(
            f.query(Probe::Attribute, "__no_stack_protector__"),
            (false, Answer::Table)
        );
        assert_eq!(
            f.query(Probe::Attribute, "__no_caller_saved_registers__"),
            (false, Answer::Table)
        );
        assert_eq!(
            f.query(Probe::Attribute, "__overloadable__"),
            (false, Answer::Table)
        );
        assert!(!f.operator_defined(Probe::Builtin, false));
        let gcc16 = Compiler::parse_spec("gcc-16.2").unwrap();
        let g = Features::new(&gcc16, Arch::X86_64, &[], None);
        assert!(g.operator_defined(Probe::Builtin, false));
        assert!(!g.operator_defined(Probe::Builtin, true));
        assert!(g.operator_defined(Probe::Feature, true));
        let x = Features::new(
            &gcc9,
            Arch::X86_64,
            &["-fsanitize=kernel-address".into()],
            None,
        );
        assert!(x.query(Probe::Attribute, "__no_caller_saved_registers__").0);
        assert!(!x.query(Probe::Feature, "address_sanitizer").0);
    }
}
