pub mod arch;
pub mod dot_config;
pub mod git;
mod layout;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use git::{GitDirs, HeadInfo, StatusEntry};
pub use layout::{Layout, Root};

#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("{0}: no such directory")]
    NotFound(PathBuf),
    #[error("{0}: not a directory")]
    NotDir(PathBuf),
    #[error("{given}: not the top level of a git work tree (it starts at {top})")]
    NotTopLevel { given: PathBuf, top: PathBuf },
    #[error("{0}: not a git work tree (no usable .git here)")]
    NotGit(PathBuf),
    #[error("{path}: not a Linux kernel tree: {}", .problems.join("; "))]
    NotKernel {
        path: PathBuf,
        problems: Vec<String>,
    },
    #[error("{path}: unusable git HEAD: {why}")]
    BadHead { path: PathBuf, why: String },
    #[error("{0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelVersion {
    pub version: u32,
    pub patchlevel: u32,
    pub sublevel: u32,
    pub extraversion: String,
    pub name: String,
}

impl KernelVersion {
    pub fn parse(makefile: &str) -> Option<KernelVersion> {
        let mut v = KernelVersion::default();
        let (mut has_version, mut has_patchlevel) = (false, false);
        for line in makefile.lines().take(40) {
            let Some((key, val)) = line.split_once('=') else {
                continue;
            };
            let val = val.trim();
            match key.trim() {
                "VERSION" => {
                    v.version = val.parse().ok()?;
                    has_version = true;
                }
                "PATCHLEVEL" => {
                    v.patchlevel = val.parse().ok()?;
                    has_patchlevel = true;
                }
                "SUBLEVEL" => v.sublevel = val.parse().unwrap_or(0),
                "EXTRAVERSION" => v.extraversion = val.to_string(),
                "NAME" => v.name = val.to_string(),
                _ => {}
            }
        }
        (has_version && has_patchlevel).then_some(v)
    }

    pub fn release(&self) -> String {
        format!(
            "{}.{}.{}{}",
            self.version, self.patchlevel, self.sublevel, self.extraversion
        )
    }

    pub fn at_least(&self, version: u32, patchlevel: u32) -> bool {
        (self.version, self.patchlevel) >= (version, patchlevel)
    }
}

#[derive(Clone, Debug)]
pub struct KernelTree {
    pub root: PathBuf,
    pub git: GitDirs,
    pub version: KernelVersion,
    pub head: HeadInfo,
}

pub const EVIDENCE: [&str; 6] = [
    "Makefile",
    "Kconfig",
    "init/main.c",
    "include/linux/kconfig.h",
    "scripts/Kbuild.include",
    "scripts/kconfig/conf.c",
];

const DIRECTORIES: [&str; 5] = ["arch", "include/linux", "kernel", "mm", "scripts/kconfig"];

fn has_word(text: &str, word: &str) -> bool {
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

fn main_menu(kconfig: &str) -> Option<String> {
    kconfig.lines().find_map(|l| {
        let rest = l.trim_start().strip_prefix("mainmenu")?;
        let rest = rest.trim_start();
        let quoted = rest.strip_prefix('"')?;
        Some(quoted.split('"').next().unwrap_or(quoted).to_string())
    })
}

pub fn kernel_problems(root: &Path) -> (Option<KernelVersion>, Vec<String>) {
    let read = |rel: &str| {
        std::fs::read(root.join(rel))
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    };
    let mut problems = Vec::new();
    let mut version = None;
    match read("Makefile") {
        None => problems.push("no top-level Makefile".to_string()),
        Some(mk) => {
            version = KernelVersion::parse(&mk);
            if version.is_none() {
                problems.push("the Makefile does not start with VERSION and PATCHLEVEL".into());
            }
            if !(has_word(&mk, "KERNELRELEASE") && has_word(&mk, "KERNELVERSION")) {
                problems.push("the Makefile defines no KERNELRELEASE/KERNELVERSION".into());
            }
            if !has_word(&mk, "vmlinux") {
                problems.push("the Makefile has no vmlinux target".into());
            }
        }
    }
    match read("Kconfig").map(|k| main_menu(&k)) {
        None => problems.push("no top-level Kconfig".into()),
        Some(None) => problems.push("the top-level Kconfig has no mainmenu".into()),
        Some(Some(menu)) if !menu.starts_with("Linux/") => problems.push(format!(
            "the Kconfig main menu is \"{menu}\" instead of \"Linux/...\""
        )),
        Some(Some(_)) => {}
    }
    match read("init/main.c") {
        None => problems.push("no init/main.c".into()),
        Some(c) if !c.contains("start_kernel(void)") => {
            problems.push("init/main.c does not define start_kernel()".into())
        }
        Some(_) => {}
    }
    match read("include/linux/kconfig.h") {
        None => problems.push("no include/linux/kconfig.h".into()),
        Some(h) if !has_word(&h, "IS_ENABLED") => {
            problems.push("include/linux/kconfig.h does not define IS_ENABLED".into())
        }
        Some(_) => {}
    }
    match read("scripts/Kbuild.include") {
        None => problems.push("no scripts/Kbuild.include".into()),
        Some(k) if !has_word(&k, "if_changed") => {
            problems.push("scripts/Kbuild.include does not define if_changed".into())
        }
        Some(_) => {}
    }
    if !root.join("scripts/kconfig/conf.c").is_file() {
        problems.push("no scripts/kconfig/conf.c".into());
    }
    let missing: Vec<&str> = DIRECTORIES
        .iter()
        .copied()
        .filter(|d| !root.join(d).is_dir())
        .collect();
    if !missing.is_empty() {
        problems.push(format!("missing directories: {}", missing.join(", ")));
    }
    (version, problems)
}

impl KernelTree {
    pub fn open(given: &Path) -> Result<KernelTree, TreeError> {
        if !given.exists() {
            return Err(TreeError::NotFound(given.to_path_buf()));
        }
        if !given.is_dir() {
            return Err(TreeError::NotDir(given.to_path_buf()));
        }
        let root = given
            .canonicalize()
            .map_err(|e| TreeError::Io(given.to_path_buf(), e))?;
        if !root.join(".git").exists() {
            return Err(match enclosing_git_top(&root) {
                Some(top) => TreeError::NotTopLevel {
                    given: given.to_path_buf(),
                    top,
                },
                None => TreeError::NotGit(given.to_path_buf()),
            });
        }
        let git = GitDirs::discover(&root).ok_or_else(|| TreeError::NotGit(root.clone()))?;
        let (version, problems) = kernel_problems(&root);
        let Some(version) = version.filter(|_| problems.is_empty()) else {
            return Err(TreeError::NotKernel {
                path: root,
                problems,
            });
        };
        let head = git.read_head().map_err(|why| TreeError::BadHead {
            path: root.clone(),
            why,
        })?;
        Ok(KernelTree {
            root,
            git,
            version,
            head,
        })
    }
}

fn enclosing_git_top(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .skip(1)
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing() {
        let mk = "# SPDX\nVERSION = 6\nPATCHLEVEL = 12\nSUBLEVEL = 112\nEXTRAVERSION =\nNAME = Baby Opossum Posse\n";
        let v = KernelVersion::parse(mk).unwrap();
        assert_eq!(v.release(), "6.12.112");
        assert_eq!(v.name, "Baby Opossum Posse");
        let rc = KernelVersion::parse(
            "VERSION = 7\nPATCHLEVEL = 3\nSUBLEVEL = 0\nEXTRAVERSION = -rc6\n",
        )
        .unwrap();
        assert_eq!(rc.release(), "7.3.0-rc6");
        assert!(rc.at_least(6, 12));
        assert!(KernelVersion::parse("all:\n\techo hi\n").is_none());
    }

    #[test]
    fn words_and_menus() {
        assert!(has_word("KERNELVERSION = $(VERSION)", "KERNELVERSION"));
        assert!(!has_word("UBOOTVERSION = 1", "VERSION"));
        assert!(!has_word("MY_KERNELVERSION_X", "KERNELVERSION"));
        assert_eq!(
            main_menu("# c\nmainmenu \"Linux/$(ARCH) $(KERNELVERSION) Kernel Configuration\"\n")
                .as_deref(),
            Some("Linux/$(ARCH) $(KERNELVERSION) Kernel Configuration")
        );
        assert_eq!(
            main_menu("mainmenu \"U-Boot $(UBOOTVERSION) Configuration\"\n").as_deref(),
            Some("U-Boot $(UBOOTVERSION) Configuration")
        );
        assert_eq!(main_menu("config A\n"), None);
    }
}
