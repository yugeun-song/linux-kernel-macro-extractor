use std::fmt;

use serde::{Deserialize, Serialize};

use super::dot_config::DotConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    X86_64,
    Arm64,
    Riscv64,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchError {
    #[error("unsupported architecture `{0}` (kmx supports x86_64, arm64 and riscv64)")]
    Unsupported(String),
    #[error(
        "32-bit {0} configuration is not supported (kmx supports 64-bit x86_64, arm64 and riscv64)"
    )]
    ThirtyTwoBit(&'static str),
    #[error("cannot tell the architecture from .config")]
    Unknown,
}

impl Arch {
    pub const ALL: [Arch; 3] = [Arch::X86_64, Arch::Arm64, Arch::Riscv64];

    pub fn srcarch(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86",
            Arch::Arm64 => "arm64",
            Arch::Riscv64 => "riscv",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Arm64 => "arm64",
            Arch::Riscv64 => "riscv64",
        }
    }

    pub fn detect(cfg: &DotConfig) -> Result<Arch, ArchError> {
        let is64 = cfg.is_y("64BIT");
        let family = match cfg.header_arch.as_deref() {
            Some("x86_64" | "x86" | "i386") => "x86",
            Some("arm64") => "arm64",
            Some("riscv") => "riscv",
            Some(other) => return Err(ArchError::Unsupported(other.to_string())),
            None if cfg.is_y("X86") => "x86",
            None if cfg.is_y("ARM64") => "arm64",
            None if cfg.is_y("RISCV") => "riscv",
            None => return Err(ArchError::Unknown),
        };
        match family {
            "arm64" => Ok(Arch::Arm64),
            "x86" if is64 || cfg.is_y("X86_64") => Ok(Arch::X86_64),
            "x86" => Err(ArchError::ThirtyTwoBit("x86")),
            _ if is64 => Ok(Arch::Riscv64),
            _ => Err(ArchError::ThirtyTwoBit("riscv")),
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub arch: Arch,
    pub big_endian: bool,
}

impl Target {
    pub fn detect(cfg: &DotConfig) -> Result<Target, ArchError> {
        let arch = Arch::detect(cfg)?;
        let big_endian = arch == Arch::Arm64 && cfg.is_y("CPU_BIG_ENDIAN");
        Ok(Target { arch, big_endian })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompilerKind {
    Gcc,
    Clang,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compiler {
    pub kind: CompilerKind,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub text: Option<String>,
    pub assumed: bool,
}

impl Compiler {
    pub const ASSUMED_GCC: (u32, u32, u32) = (16, 2, 1);

    pub fn from_config(cfg: &DotConfig) -> Compiler {
        let text = cfg.string("CC_VERSION_TEXT").map(str::to_string);
        let split = |v: u64| {
            (
                (v / 10000) as u32,
                ((v / 100) % 100) as u32,
                (v % 100) as u32,
            )
        };
        let found = if cfg.is_y("CC_IS_CLANG") {
            cfg.num("CLANG_VERSION")
                .map(|v| (CompilerKind::Clang, split(v)))
        } else if cfg.is_y("CC_IS_GCC") {
            cfg.num("GCC_VERSION")
                .map(|v| (CompilerKind::Gcc, split(v)))
        } else {
            None
        };
        let assumed = found.is_none();
        let (kind, (major, minor, patch)) = found.unwrap_or((CompilerKind::Gcc, Self::ASSUMED_GCC));
        Compiler {
            kind,
            major,
            minor,
            patch,
            text,
            assumed,
        }
    }

    pub fn from_compile_h(text: &str) -> Option<Compiler> {
        let value = text
            .lines()
            .find_map(|l| l.trim_start().strip_prefix("#define LINUX_COMPILER"))
            .filter(|rest| rest.starts_with([' ', '\t']))?
            .trim()
            .trim_matches('"')
            .trim();
        [
            ("gcc version ", CompilerKind::Gcc),
            ("clang version ", CompilerKind::Clang),
        ]
        .into_iter()
        .find_map(|(word, kind)| {
            let at = value.find(word)? + word.len();
            let ver = value[at..].split_whitespace().next()?;
            let mut nums = ver.split('.').map(|p| p.parse::<u32>().ok());
            let major = nums.next().flatten()?;
            Some(Compiler {
                kind,
                major,
                minor: nums.next().flatten().unwrap_or(0),
                patch: nums.next().flatten().unwrap_or(0),
                text: Some(value.to_string()),
                assumed: false,
            })
        })
    }

    pub fn parse_spec(spec: &str) -> Option<Compiler> {
        let (name, ver) = spec.split_once(['-', ' ', '@']).unwrap_or((spec, ""));
        let kind = match name {
            "gcc" => CompilerKind::Gcc,
            "clang" => CompilerKind::Clang,
            _ => return None,
        };
        let mut nums = ver.split('.').map(|p| p.parse::<u32>().ok());
        let major = nums.next().flatten()?;
        let minor = nums.next().flatten().unwrap_or(0);
        let patch = nums.next().flatten().unwrap_or(0);
        Some(Compiler {
            kind,
            major,
            minor,
            patch,
            text: Some(spec.to_string()),
            assumed: false,
        })
    }

    pub fn version_code(&self) -> u32 {
        self.major * 10000 + self.minor * 100 + self.patch
    }
}

impl fmt::Display for Compiler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.kind {
            CompilerKind::Gcc => "gcc",
            CompilerKind::Clang => "clang",
        };
        write!(f, "{name} {}.{}.{}", self.major, self.minor, self.patch)?;
        if self.assumed {
            f.write_str(" (assumed)")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(s: &str) -> DotConfig {
        DotConfig::parse(s)
    }

    #[test]
    fn detects_supported_arches() {
        let x = cfg("# Linux/x86_64 6.12.112 Kernel Configuration\nCONFIG_64BIT=y\nCONFIG_X86=y\n");
        assert_eq!(Arch::detect(&x).unwrap(), Arch::X86_64);
        let a = cfg("# Linux/arm64 6.12.112 Kernel Configuration\nCONFIG_64BIT=y\n");
        assert_eq!(Arch::detect(&a).unwrap(), Arch::Arm64);
        let r = cfg("# Linux/riscv 6.12.112 Kernel Configuration\nCONFIG_64BIT=y\n");
        assert_eq!(Arch::detect(&r).unwrap(), Arch::Riscv64);
        let old = cfg("CONFIG_ARM64=y\nCONFIG_64BIT=y\n");
        assert_eq!(Arch::detect(&old).unwrap(), Arch::Arm64);
    }

    #[test]
    fn rejects_others() {
        let p = cfg("# Linux/powerpc 6.12.112 Kernel Configuration\n");
        assert!(matches!(Arch::detect(&p), Err(ArchError::Unsupported(_))));
        let i = cfg("# Linux/i386 6.12.112 Kernel Configuration\nCONFIG_X86=y\nCONFIG_X86_32=y\n");
        assert!(matches!(
            Arch::detect(&i),
            Err(ArchError::ThirtyTwoBit("x86"))
        ));
        let r32 = cfg("# Linux/riscv 6.12.112 Kernel Configuration\nCONFIG_32BIT=y\n");
        assert!(matches!(
            Arch::detect(&r32),
            Err(ArchError::ThirtyTwoBit("riscv"))
        ));
    }

    #[test]
    fn compiler_from_config() {
        let cc = Compiler::from_config(&cfg("CONFIG_CC_IS_GCC=y\nCONFIG_GCC_VERSION=160201\n"));
        assert_eq!(
            (cc.kind, cc.major, cc.minor, cc.patch),
            (CompilerKind::Gcc, 16, 2, 1)
        );
        assert!(!cc.assumed);
        assert_eq!(cc.version_code(), 160201);
        assert!(Compiler::from_config(&cfg("CONFIG_ARM64=y\n")).assumed);
        let old = Compiler::from_compile_h(
            "#define LINUX_COMPILE_BY \"root\"\n#define LINUX_COMPILER \"gcc version 16.0.1 20260322 (experimental) (Ubuntu 16-20260322-1ubuntu1) \"\n",
        )
        .unwrap();
        assert_eq!(
            (old.kind, old.major, old.minor, old.patch, old.assumed),
            (CompilerKind::Gcc, 16, 0, 1, false)
        );
        let clang = Compiler::from_compile_h(
            "#define LINUX_COMPILER\t\t\"clang version 3.8.0 (tags/RELEASE_380/final)\"\n",
        )
        .unwrap();
        assert_eq!(
            (clang.kind, clang.major, clang.minor),
            (CompilerKind::Clang, 3, 8)
        );
        assert!(
            Compiler::from_compile_h("#define LINUX_COMPILE_HOST \"gcc version 1.2.3\"\n")
                .is_none()
        );
        let spec = Compiler::parse_spec("clang-19.1").unwrap();
        assert_eq!(
            (spec.kind, spec.major, spec.minor),
            (CompilerKind::Clang, 19, 1)
        );
    }
}
