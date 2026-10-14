use std::path::{Component, Path, PathBuf};

use crate::tree::Layout;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefineOp {
    Define(String, String),
    Undef(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelSource {
    Cmd(PathBuf),
    Representative(PathBuf),
    Synthesized,
}

#[derive(Clone, Debug)]
pub struct CompileModel {
    pub compiler: Option<String>,
    pub quote_dirs: Vec<PathBuf>,
    pub dirs: Vec<PathBuf>,
    pub system_dirs: Vec<PathBuf>,
    pub force_includes: Vec<PathBuf>,
    pub ops: Vec<DefineOp>,
    pub flags: Vec<String>,
    pub asm: bool,
    pub source: ModelSource,
}

pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn split_define(s: &str) -> DefineOp {
    match s.split_once('=') {
        Some((n, v)) => DefineOp::Define(n.to_string(), v.to_string()),
        None => DefineOp::Define(s.to_string(), "1".to_string()),
    }
}

pub fn read_cmd_line(cmd_file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(cmd_file).ok()?;
    let first = text.lines().next()?;
    let (lhs, rhs) = first.split_once(":=")?;
    let lhs = lhs.trim();
    (lhs.starts_with("savedcmd_") || lhs.starts_with("cmd_")).then(|| rhs.trim().to_string())
}

impl CompileModel {
    pub fn parse_command(cmd: &str, cwd: &Path, source: ModelSource) -> Option<CompileModel> {
        let argv = shlex::split(cmd)?;
        let mut m = CompileModel {
            compiler: argv.first().cloned(),
            quote_dirs: Vec::new(),
            dirs: Vec::new(),
            system_dirs: Vec::new(),
            force_includes: Vec::new(),
            ops: Vec::new(),
            flags: Vec::new(),
            asm: false,
            source,
        };
        let abs = |p: &str| normalize(&cwd.join(p));
        let mut i = 1;
        while i < argv.len() {
            let a = &argv[i];
            let next = argv.get(i + 1);
            let take = |prefix: &str| -> Option<(String, bool)> {
                if a == prefix {
                    next.map(|n| (n.clone(), true))
                } else {
                    a.strip_prefix(prefix).map(|r| (r.to_string(), false))
                }
            };
            if let Some((v, two)) = take("-iquote") {
                m.quote_dirs.push(abs(&v));
                i += 1 + usize::from(two);
            } else if let Some((v, two)) = take("-isystem") {
                m.system_dirs.push(abs(&v));
                i += 1 + usize::from(two);
            } else if let Some((v, two)) = take("-include") {
                m.force_includes.push(abs(&v));
                i += 1 + usize::from(two);
            } else if let Some((v, two)) = take("-I") {
                m.dirs.push(abs(&v));
                i += 1 + usize::from(two);
            } else if let Some((v, two)) = take("-D") {
                m.ops.push(split_define(&v));
                i += 1 + usize::from(two);
            } else if let Some((v, two)) = take("-U") {
                m.ops.push(DefineOp::Undef(v));
                i += 1 + usize::from(two);
            } else if a == "-x" {
                if next.is_some_and(|n| n == "assembler-with-cpp") {
                    m.asm = true;
                }
                i += 2;
            } else if a == "-o" || a == "-MF" || a == "-MT" || a == "-MQ" {
                i += 2;
            } else {
                if a.starts_with('-') && !a.starts_with("-Wp,") && !a.starts_with("-W") {
                    m.flags.push(a.clone());
                }
                i += 1;
            }
        }
        if m.ops
            .iter()
            .any(|o| matches!(o, DefineOp::Define(n, _) if n == "__ASSEMBLY__"))
        {
            m.asm = true;
        }
        Some(m)
    }

    pub fn from_cmd(cmd_file: &Path, layout: &Layout, source: ModelSource) -> Option<CompileModel> {
        let line = read_cmd_line(cmd_file)?;
        CompileModel::parse_command(&line, &layout.objtree, source)
    }

    pub fn cmd_path(layout: &Layout, rel_source: &str) -> PathBuf {
        let (dir, file) = rel_source.rsplit_once('/').unwrap_or(("", rel_source));
        let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
        let mut p = layout.objtree.clone();
        if !dir.is_empty() {
            p.push(dir);
        }
        p.push(format!(".{stem}.o.cmd"));
        p
    }

    pub fn for_file(layout: &Layout, rel_source: &str, fallback: &CompileModel) -> CompileModel {
        let cmd = CompileModel::cmd_path(layout, rel_source);
        if let Some(m) = CompileModel::from_cmd(&cmd, layout, ModelSource::Cmd(cmd.clone())) {
            return m;
        }
        let mut m = fallback.without_per_file();
        let (dir, file) = rel_source.rsplit_once('/').unwrap_or(("", rel_source));
        let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
        if !layout.in_tree() {
            m.dirs.push(normalize(&layout.srctree.join(dir)));
            m.dirs.push(normalize(&layout.objtree.join(dir)));
        }
        let modfile = if dir.is_empty() {
            stem.to_string()
        } else {
            format!("{dir}/{stem}")
        };
        m.ops.push(DefineOp::Define(
            "KBUILD_MODFILE".into(),
            format!("\"{modfile}\""),
        ));
        m.ops.push(DefineOp::Define(
            "KBUILD_BASENAME".into(),
            format!("\"{stem}\""),
        ));
        m.ops.push(DefineOp::Define(
            "KBUILD_MODNAME".into(),
            format!("\"{stem}\""),
        ));
        m.ops.push(DefineOp::Define(
            "__KBUILD_MODNAME".into(),
            format!("kmod_{stem}"),
        ));
        if rel_source.ends_with(".S") {
            m.make_asm(layout);
        }
        m.source = match &fallback.source {
            ModelSource::Cmd(p) | ModelSource::Representative(p) => {
                ModelSource::Representative(p.clone())
            }
            ModelSource::Synthesized => ModelSource::Synthesized,
        };
        m
    }

    pub fn make_asm(&mut self, layout: &Layout) {
        if self.asm {
            return;
        }
        self.asm = true;
        let ct = layout.srctree.join("include/linux/compiler_types.h");
        self.force_includes.retain(|f| *f != ct);
        self.ops
            .push(DefineOp::Define("__ASSEMBLY__".into(), "1".into()));
    }

    pub fn without_per_file(&self) -> CompileModel {
        const PER_FILE: [&str; 4] = [
            "KBUILD_MODFILE",
            "KBUILD_BASENAME",
            "KBUILD_MODNAME",
            "__KBUILD_MODNAME",
        ];
        let mut m = self.clone();
        m.ops.retain(|o| match o {
            DefineOp::Define(n, _) | DefineOp::Undef(n) => {
                !PER_FILE.contains(&n.as_str()) && n != "KBUILD_STR"
            }
        });
        m
    }

    pub fn representative(layout: &Layout) -> Option<CompileModel> {
        const CANDIDATES: [&str; 4] = [
            "init/.main.o.cmd",
            "kernel/.fork.o.cmd",
            "mm/.slub.o.cmd",
            "kernel/.sys.o.cmd",
        ];
        for c in CANDIDATES {
            let p = layout.objtree.join(c);
            if let Some(m) =
                CompileModel::from_cmd(&p, layout, ModelSource::Representative(p.clone()))
                && !m.asm
            {
                return Some(m.without_per_file_for(layout, c));
            }
        }
        None
    }

    fn without_per_file_for(self, layout: &Layout, rel_cmd: &str) -> CompileModel {
        let dir = rel_cmd.rsplit_once('/').map_or("", |(d, _)| d);
        let src_dir = normalize(&layout.srctree.join(dir));
        let obj_dir = normalize(&layout.objtree.join(dir));
        let mut m = self.without_per_file();
        m.dirs.retain(|d| *d != src_dir && *d != obj_dir);
        m.quote_dirs.retain(|d| *d != src_dir && *d != obj_dir);
        m
    }

    pub fn synthesized(layout: &Layout) -> CompileModel {
        let s = &layout.srctree;
        let o = &layout.objtree;
        let arch = &layout.srcarch;
        let mut dirs = vec![
            s.join(format!("arch/{arch}/include")),
            o.join(format!("arch/{arch}/include/generated")),
        ];
        if !layout.in_tree() {
            dirs.push(s.join("include"));
        }
        dirs.extend([
            o.join("include"),
            s.join(format!("arch/{arch}/include/uapi")),
            o.join(format!("arch/{arch}/include/generated/uapi")),
            s.join("include/uapi"),
            o.join("include/generated/uapi"),
        ]);
        let mut force = Vec::new();
        for f in [
            "include/linux/compiler-version.h",
            "include/linux/kconfig.h",
            "include/linux/compiler_types.h",
        ] {
            let p = s.join(f);
            if p.is_file() {
                force.push(p);
            }
        }
        CompileModel {
            compiler: None,
            quote_dirs: Vec::new(),
            dirs: dirs.iter().map(|d| normalize(d)).collect(),
            system_dirs: Vec::new(),
            force_includes: force,
            ops: vec![DefineOp::Define("__KERNEL__".into(), "1".into())],
            flags: vec!["-std=gnu11".into(), "-O2".into()],
            asm: false,
            source: ModelSource::Synthesized,
        }
    }

    pub fn default_for(layout: &Layout) -> CompileModel {
        CompileModel::representative(layout).unwrap_or_else(|| CompileModel::synthesized(layout))
    }

    pub fn std(&self) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find_map(|f| f.strip_prefix("-std="))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout {
            srctree: PathBuf::from("/src/linux"),
            objtree: PathBuf::from("/build/x86"),
            srcarch: "x86".into(),
        }
    }

    #[test]
    fn parses_kbuild_command() {
        let cmd = r#"gcc-16 -Wp,-MMD,mm/.slub.o.d -nostdinc -I/src/linux/arch/x86/include -I./arch/x86/include/generated -I/src/linux/include -I./include -include /src/linux/include/linux/kconfig.h -D__KERNEL__ -std=gnu11 -mno-sse -O2 -DKBUILD_MODNAME='"slub"' -DCONFIG_X=0x1 -UFOO -c -o mm/slub.o /src/linux/mm/slub.c"#;
        let m = CompileModel::parse_command(cmd, Path::new("/build/x86"), ModelSource::Synthesized)
            .unwrap();
        assert_eq!(m.compiler.as_deref(), Some("gcc-16"));
        assert_eq!(
            m.dirs,
            vec![
                PathBuf::from("/src/linux/arch/x86/include"),
                PathBuf::from("/build/x86/arch/x86/include/generated"),
                PathBuf::from("/src/linux/include"),
                PathBuf::from("/build/x86/include"),
            ]
        );
        assert_eq!(
            m.force_includes,
            vec![PathBuf::from("/src/linux/include/linux/kconfig.h")]
        );
        assert!(m.ops.contains(&DefineOp::Define(
            "KBUILD_MODNAME".into(),
            "\"slub\"".into()
        )));
        assert!(
            m.ops
                .contains(&DefineOp::Define("__KERNEL__".into(), "1".into()))
        );
        assert!(m.ops.contains(&DefineOp::Undef("FOO".into())));
        assert_eq!(m.std(), Some("gnu11"));
        assert!(m.flags.contains(&"-mno-sse".to_string()));
        assert!(!m.asm);
        let stripped = m.without_per_file();
        assert!(
            !stripped
                .ops
                .iter()
                .any(|o| matches!(o, DefineOp::Define(n, _) if n == "KBUILD_MODNAME"))
        );
    }

    #[test]
    fn synthesized_model_out_of_tree() {
        let m = CompileModel::synthesized(&layout());
        assert_eq!(m.dirs[0], PathBuf::from("/src/linux/arch/x86/include"));
        assert_eq!(
            m.dirs[1],
            PathBuf::from("/build/x86/arch/x86/include/generated")
        );
        assert_eq!(m.dirs[2], PathBuf::from("/src/linux/include"));
    }

    #[test]
    fn cmd_paths() {
        assert_eq!(
            CompileModel::cmd_path(&layout(), "mm/slub.c"),
            PathBuf::from("/build/x86/mm/.slub.o.cmd")
        );
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }
}
