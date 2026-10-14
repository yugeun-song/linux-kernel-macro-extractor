use super::kbuild::CompileModel;
use crate::tree::arch::{Arch, Compiler, CompilerKind};

const X86_64_GCC: &str = include_str!("../../data/predef/x86_64-gcc.h");
const X86_64_CLANG: &str = include_str!("../../data/predef/x86_64-clang.h");
const ARM64_GCC: &str = include_str!("../../data/predef/arm64-gcc.h");
const ARM64_CLANG: &str = include_str!("../../data/predef/arm64-clang.h");
const RISCV64_GCC: &str = include_str!("../../data/predef/riscv64-gcc.h");
const RISCV64_CLANG: &str = include_str!("../../data/predef/riscv64-clang.h");

pub fn base_table(arch: Arch, kind: CompilerKind) -> &'static str {
    match (arch, kind) {
        (Arch::X86_64, CompilerKind::Gcc) => X86_64_GCC,
        (Arch::X86_64, CompilerKind::Clang) => X86_64_CLANG,
        (Arch::Arm64, CompilerKind::Gcc) => ARM64_GCC,
        (Arch::Arm64, CompilerKind::Clang) => ARM64_CLANG,
        (Arch::Riscv64, CompilerKind::Gcc) => RISCV64_GCC,
        (Arch::Riscv64, CompilerKind::Clang) => RISCV64_CLANG,
    }
}

struct Table {
    entries: Vec<(String, String)>,
}

impl Table {
    fn parse(text: &str) -> Table {
        let entries = text
            .lines()
            .filter_map(|l| l.strip_prefix("#define "))
            .map(|rest| match rest.split_once(' ') {
                Some((n, v)) => (n.to_string(), v.to_string()),
                None => (rest.to_string(), String::new()),
            })
            .collect();
        Table { entries }
    }

    fn set(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        match self.entries.iter_mut().find(|(n, _)| n == name) {
            Some(e) => e.1 = value,
            None => self.entries.push((name.to_string(), value)),
        }
    }

    fn remove(&mut self, name: &str) {
        self.entries.retain(|(n, _)| n != name);
    }

    fn render(&self) -> String {
        let mut s = String::with_capacity(self.entries.len() * 32);
        for (n, v) in &self.entries {
            s.push_str("#define ");
            s.push_str(n);
            if !v.is_empty() {
                s.push(' ');
                s.push_str(v);
            }
            s.push('\n');
        }
        s
    }
}

pub fn with_compiler_version(text: &str, cc: &Compiler) -> String {
    let mut t = Table::parse(text);
    set_version(&mut t, cc);
    t.render()
}

fn set_version(t: &mut Table, cc: &Compiler) {
    let (maj, min, pat) = (cc.major, cc.minor, cc.patch);
    let current = |t: &Table, name: &str| {
        t.entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let same = match cc.kind {
        CompilerKind::Gcc => {
            current(t, "__GNUC__") == Some(maj.to_string())
                && current(t, "__GNUC_MINOR__") == Some(min.to_string())
                && current(t, "__GNUC_PATCHLEVEL__") == Some(pat.to_string())
        }
        CompilerKind::Clang => {
            current(t, "__clang_major__") == Some(maj.to_string())
                && current(t, "__clang_minor__") == Some(min.to_string())
                && current(t, "__clang_patchlevel__") == Some(pat.to_string())
        }
    };
    if same {
        return;
    }
    match cc.kind {
        CompilerKind::Gcc => {
            t.set("__GNUC__", maj.to_string());
            t.set("__GNUC_MINOR__", min.to_string());
            t.set("__GNUC_PATCHLEVEL__", pat.to_string());
            t.set("__VERSION__", format!("\"{maj}.{min}.{pat}\""));
        }
        CompilerKind::Clang => {
            t.set("__clang_major__", maj.to_string());
            t.set("__clang_minor__", min.to_string());
            t.set("__clang_patchlevel__", pat.to_string());
            t.set("__clang_version__", format!("\"{maj}.{min}.{pat} \""));
            t.set("__VERSION__", format!("\"Clang {maj}.{min}.{pat}\""));
        }
    }
}

pub fn predefined_source(arch: Arch, cc: &Compiler, model: &CompileModel) -> String {
    let mut t = Table::parse(base_table(arch, cc.kind));
    set_version(&mut t, cc);
    for f in &model.flags {
        match f.as_str() {
            "-O0" => {
                t.remove("__OPTIMIZE__");
                t.remove("__OPTIMIZE_SIZE__");
            }
            "-O" | "-O1" | "-O2" | "-O3" | "-Ofast" | "-Og" => {
                t.set("__OPTIMIZE__", "1");
                t.remove("__OPTIMIZE_SIZE__");
            }
            "-Os" | "-Oz" => {
                t.set("__OPTIMIZE__", "1");
                t.set("__OPTIMIZE_SIZE__", "1");
            }
            "-fsigned-char" => t.remove("__CHAR_UNSIGNED__"),
            "-funsigned-char" => t.set("__CHAR_UNSIGNED__", "1"),
            "-fno-stack-protector" => {
                for m in [
                    "__SSP__",
                    "__SSP_STRONG__",
                    "__SSP_ALL__",
                    "__SSP_EXPLICIT__",
                ] {
                    t.remove(m);
                }
            }
            "-fstack-protector" => t.set("__SSP__", "1"),
            "-fstack-protector-strong" => t.set("__SSP_STRONG__", "3"),
            "-fstack-protector-all" => t.set("__SSP_ALL__", "2"),
            "-fstack-protector-explicit" => t.set("__SSP_EXPLICIT__", "4"),
            "-fno-asynchronous-unwind-tables" => t.remove("__GCC_HAVE_DWARF2_CFI_ASM"),
            "-fasynchronous-unwind-tables" | "-funwind-tables" => {
                t.set("__GCC_HAVE_DWARF2_CFI_ASM", "1")
            }
            "-fcf-protection=none" => t.remove("__CET__"),
            "-fcf-protection=branch" => t.set("__CET__", "1"),
            "-fcf-protection=return" => t.set("__CET__", "2"),
            "-fcf-protection" | "-fcf-protection=full" => t.set("__CET__", "3"),
            other => {
                if let Some(list) = other.strip_prefix("-fsanitize=") {
                    for s in list.split(',') {
                        match s {
                            "address" | "kernel-address" => t.set("__SANITIZE_ADDRESS__", "1"),
                            "hwaddress" | "kernel-hwaddress" => {
                                t.set("__SANITIZE_HWADDRESS__", "1")
                            }
                            "thread" => t.set("__SANITIZE_THREAD__", "1"),
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    if model
        .flags
        .iter()
        .any(|f| f.starts_with("-g") && f != "-g0")
    {
        t.set("__GCC_HAVE_DWARF2_CFI_ASM", "1");
    }
    match model.std() {
        Some("gnu89" | "gnu90" | "c89" | "c90") => {
            t.remove("__STDC_VERSION__");
            t.remove("__GNUC_STDC_INLINE__");
            t.set("__GNUC_GNU_INLINE__", "1");
        }
        Some("gnu99" | "c99") => t.set("__STDC_VERSION__", "199901L"),
        Some("gnu11" | "c11") => t.set("__STDC_VERSION__", "201112L"),
        Some("gnu17" | "gnu18" | "c17" | "c18") => t.set("__STDC_VERSION__", "201710L"),
        Some("gnu2x" | "gnu23" | "c2x" | "c23") => t.set("__STDC_VERSION__", "202311L"),
        _ => {}
    }
    if model.asm {
        t.set("__ASSEMBLER__", "1");
    }
    t.render()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pp::kbuild::ModelSource;

    fn model(flags: &[&str], asm: bool) -> CompileModel {
        CompileModel {
            compiler: None,
            quote_dirs: Vec::new(),
            dirs: Vec::new(),
            system_dirs: Vec::new(),
            force_includes: Vec::new(),
            ops: Vec::new(),
            flags: flags.iter().map(|s| s.to_string()).collect(),
            asm,
            source: ModelSource::Synthesized,
        }
    }

    #[test]
    fn version_and_flag_adjustments() {
        let cc = Compiler::parse_spec("gcc-13.2.0").unwrap();
        let s = predefined_source(
            Arch::X86_64,
            &cc,
            &model(&["-Os", "-std=gnu89", "-fsanitize=kernel-address"], true),
        );
        assert!(s.contains("#define __GNUC__ 13\n"));
        assert!(s.contains("#define __GNUC_MINOR__ 2\n"));
        assert!(s.contains("#define __OPTIMIZE_SIZE__ 1\n"));
        assert!(!s.contains("__STDC_VERSION__"));
        assert!(s.contains("#define __SANITIZE_ADDRESS__ 1\n"));
        assert!(s.contains("#define __ASSEMBLER__ 1\n"));
        assert!(s.contains("#define __x86_64__ 1\n"));
        assert!(!s.contains("#define __SSE__"));
    }

    #[test]
    fn every_table_has_the_basics() {
        for arch in Arch::ALL {
            for kind in [CompilerKind::Gcc, CompilerKind::Clang] {
                let t = base_table(arch, kind);
                assert!(t.contains("#define __SIZEOF_LONG__ 8"), "{arch} {kind:?}");
                assert!(t.contains("#define __CHAR_BIT__ 8"), "{arch} {kind:?}");
            }
        }
    }
}
