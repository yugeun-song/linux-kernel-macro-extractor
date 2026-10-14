use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const FILES: &[(&str, &str)] = &[
    (
        "Makefile",
        "VERSION = 6\nPATCHLEVEL = 12\nSUBLEVEL = 0\nEXTRAVERSION =\nNAME = Test\n\n\
         KERNELRELEASE = $(shell cat include/config/kernel.release 2> /dev/null)\n\
         KERNELVERSION = $(VERSION).$(PATCHLEVEL).$(SUBLEVEL)$(EXTRAVERSION)\n\n\
         vmlinux: vmlinux.a\n",
    ),
    (".gitignore", ".config\n"),
    (
        "Kconfig",
        "mainmenu \"Linux/$(ARCH) $(KERNELVERSION) Kernel Configuration\"\n\n\
         source \"arch/$(SRCARCH)/Kconfig\"\n\n\
         config MODULES\n\tbool \"Enable loadable module support\"\n\n\
         config BAR\n\tbool \"Bar support\"\n\tdefault y\n\n\
         config SHIFT\n\tint\n\tdefault 12 if BAR\n\tdefault 13\n",
    ),
    (
        "arch/x86/Kconfig",
        "config X86\n\tdef_bool y\n\nconfig X86_64\n\tdef_bool y\n\nconfig 64BIT\n\tdef_bool y\n",
    ),
    (
        "arch/x86/include/asm/page.h",
        "#ifndef _ASM_PAGE_H\n#define _ASM_PAGE_H\n\
         #define PAGE_SHIFT CONFIG_SHIFT\n\
         #define PAGE_SIZE (1UL << PAGE_SHIFT)\n\
         #endif\n",
    ),
    ("arch/arm64/include/asm/page.h", "#define PAGE_SIZE 65536\n"),
    (
        "include/linux/kconfig.h",
        "#ifndef __LINUX_KCONFIG_H\n#define __LINUX_KCONFIG_H\n\
         #include <generated/autoconf.h>\n\
         #define IS_ENABLED(option) (option)\n#endif\n",
    ),
    (
        "include/linux/foo.h",
        "#ifndef _LINUX_FOO_H\n#define _LINUX_FOO_H\n\
         #ifdef CONFIG_BAR\n#define FOO_MODE 1\n#else\n#define FOO_MODE 2\n#endif\n\
         #define FOO_TWICE(x) ((x) * 2)\n\
         enum foo_slot {\n\tFOO_A,\n#ifdef CONFIG_BAR\n\tFOO_B,\n#endif\n\tFOO_C = 10,\n\tFOO_D,\n};\n\
         #endif\n",
    ),
    ("kernel/Makefile", "obj-y += fork.o\n"),
    (
        "kernel/fork.c",
        "#include <linux/foo.h>\n#define LOCAL_ONLY 7\nint fork_mode = FOO_MODE;\n",
    ),
    (
        "init/main.c",
        "#include <linux/foo.h>\nvoid start_kernel(void)\n{\n}\n",
    ),
    ("mm/Makefile", "\n"),
    ("scripts/kconfig/Makefile", "\n"),
    (
        "scripts/kconfig/conf.c",
        "int main(void)\n{\n\treturn 0;\n}\n",
    ),
    (
        "scripts/Kbuild.include",
        "if_changed = $(if $(newer-prereqs)$(cmd-check),$(cmd))\n",
    ),
    ("tools/include/linux/tools_only.h", "#define TOOLS_ONLY 1\n"),
];

const DOT_CONFIG: &str = "#\n# Automatically generated file; DO NOT EDIT.\n\
# Linux/x86 6.12.0 Kernel Configuration\n#\n\
CONFIG_X86=y\nCONFIG_X86_64=y\nCONFIG_64BIT=y\n\
# CONFIG_MODULES is not set\nCONFIG_BAR=y\nCONFIG_SHIFT=12\n";

struct Fixture {
    root: PathBuf,
    tree: PathBuf,
    home: PathBuf,
}

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

impl Fixture {
    fn new() -> Option<Fixture> {
        if !git_available() {
            eprintln!("git is not available; skipping");
            return None;
        }
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "kmx-cli-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let tree = root.join("linux");
        for (path, text) in FILES {
            write(&tree.join(path), text);
        }
        write(&tree.join(".config"), DOT_CONFIG);
        let f = Fixture {
            home: root.join("kmx-home"),
            tree,
            root,
        };
        f.git(&["init", "-q", "-b", "main"]);
        f.commit("initial import");
        Some(f)
    }

    fn git(&self, args: &[&str]) {
        let st = Command::new("git")
            .arg("-C")
            .arg(&self.tree)
            .args([
                "-c",
                "user.name=kmx test",
                "-c",
                "user.email=kmx@test.invalid",
            ])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?} failed");
    }

    fn commit(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--no-gpg-sign", "-m", msg]);
    }

    fn kmx_at(&self, tree: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kmx"))
            .arg(tree)
            .args(args)
            .env("KMX_HOME", &self.home)
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap()
    }

    fn kmx(&self, args: &[&str]) -> Output {
        self.kmx_at(&self.tree, args)
    }

    fn bare(&self, args: &[&str]) -> Output {
        self.bare_with(Path::new(env!("CARGO_BIN_EXE_kmx")), args)
    }

    fn bare_with(&self, bin: &Path, args: &[&str]) -> Output {
        let run = || {
            Command::new(bin)
                .args(args)
                .env("KMX_HOME", &self.home)
                .env("NO_COLOR", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
        };
        for _ in 0..50 {
            match run() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                other => return other.unwrap(),
            }
        }
        run().unwrap()
    }

    fn stdout(&self, args: &[&str]) -> String {
        let out = self.kmx(args);
        assert!(
            out.status.success(),
            "kmx {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut all = args.to_vec();
        all.push("--json");
        let text = self.stdout(&all);
        serde_json::from_str(text.lines().next().unwrap()).unwrap()
    }

    fn tracked_state(&self) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.tree)
            .args([
                "status",
                "--porcelain",
                "--ignored",
                "--untracked-files=all",
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn conclusion(text: &str) -> &str {
    let line = text.lines().next().unwrap_or_default();
    line.split("  ").next().unwrap_or(line)
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

#[test]
fn resolves_value_through_layers() {
    let Some(f) = Fixture::new() else { return };
    assert_eq!(
        conclusion(&f.stdout(&["PAGE_SIZE", "--brief"])),
        "PAGE_SIZE = 4096 (0x1000, 4 KiB)"
    );
    let r = f.json(&["PAGE_SIZE"]);
    assert_eq!(r["active"]["path"], "arch/x86/include/asm/page.h");
    let rivals: Vec<&str> = r["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["status"].as_str().unwrap())
        .collect();
    assert_eq!(rivals, ["other_arch"]);
    let tree = f.stdout(&["PAGE_SIZE"]);
    assert!(tree.contains("PAGE_SHIFT"), "{tree}");
    assert!(tree.contains("CONFIG_SHIFT"), "{tree}");
}

#[test]
fn explains_the_losing_branch() {
    let Some(f) = Fixture::new() else { return };
    assert_eq!(
        conclusion(&f.stdout(&["FOO_MODE", "--brief"])),
        "FOO_MODE = 1"
    );
    let r = f.json(&["FOO_MODE"]);
    let lost = &r["candidates"][0];
    assert_eq!(lost["status"], "condition_false");
    assert_eq!(lost["line"], 6);
    assert_eq!(r["active_conditions"][0]["atoms"][0]["text"], "CONFIG_BAR");
}

#[test]
fn what_if_reevaluates_kconfig() {
    let Some(f) = Fixture::new() else { return };
    assert_eq!(
        conclusion(&f.stdout(&["FOO_MODE", "--set", "BAR=n", "--brief"])),
        "FOO_MODE = 2"
    );
    assert_eq!(
        conclusion(&f.stdout(&["PAGE_SIZE", "--set", "BAR=n", "--brief"])),
        "PAGE_SIZE = 8192 (0x2000, 8 KiB)"
    );
    let out = f.kmx(&["PAGE_SIZE", "--set", "NO_SUCH_SYMBOL=y"]);
    assert_eq!(code(&out), 2);
}

#[test]
fn evaluates_enum_constants() {
    let Some(f) = Fixture::new() else { return };
    assert_eq!(conclusion(&f.stdout(&["FOO_A", "--brief"])), "FOO_A = 0");
    assert_eq!(conclusion(&f.stdout(&["FOO_B", "--brief"])), "FOO_B = 1");
    assert_eq!(
        conclusion(&f.stdout(&["FOO_D", "--brief"])),
        "FOO_D = 11 (0xb)"
    );
    let r = f.json(&["FOO_C", "--set", "BAR=n"]);
    let members = r["enum_info"]["members"].as_array().unwrap();
    let excluded: Vec<&str> = members
        .iter()
        .filter(|m| m["active"] == false)
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(excluded, ["FOO_B"]);
}

#[test]
fn explains_config_symbols() {
    let Some(f) = Fixture::new() else { return };
    let r = f.json(&["CONFIG_SHIFT"]);
    assert_eq!(r["kconfig"]["value"], "12");
    let defaults = r["kconfig"]["defaults"].as_array().unwrap();
    assert_eq!(defaults.len(), 2);
    assert_eq!(defaults[0]["chosen"], true);
}

#[test]
fn expands_text_and_lists_names() {
    let Some(f) = Fixture::new() else { return };
    assert_eq!(
        conclusion(&f.stdout(&["expand", "FOO_TWICE(PAGE_SIZE)", "--brief"])),
        "FOO_TWICE(PAGE_SIZE) = 8192 (0x2000, 8 KiB)"
    );
    let list = f.stdout(&["list", "FOO_*"]);
    let names: Vec<&str> = list
        .lines()
        .map(|l| l.split('\t').next().unwrap())
        .collect();
    assert_eq!(
        names,
        ["FOO_A", "FOO_B", "FOO_C", "FOO_D", "FOO_MODE", "FOO_TWICE"]
    );
}

#[test]
fn local_definitions_are_tu_dependent() {
    let Some(f) = Fixture::new() else { return };
    let r = f.json(&["LOCAL_ONLY"]);
    assert!(r["tu_dependent"].as_str().is_some());
    assert_eq!(r["active"]["path"], "kernel/fork.c");
    assert!(
        r["context"]["mode"]
            .as_str()
            .unwrap()
            .contains("kernel/fork.c"),
        "{}",
        r["context"]["mode"]
    );
    assert_eq!(
        conclusion(&f.stdout(&["LOCAL_ONLY", "--brief"])),
        "LOCAL_ONLY = 7"
    );
}

#[test]
fn unknown_names_are_errors() {
    let Some(f) = Fixture::new() else { return };
    let out = f.kmx(&["PAGE_SIZ"]);
    assert_eq!(code(&out), 1);
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("PAGE_SIZ: no macro, enum constant or Kconfig symbol"),
        "{err}"
    );
    let out = f.kmx(&["PAGE_SIZE", "PAGE_SIZ", "--brief"]);
    assert_eq!(code(&out), 1);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "PAGE_SIZE = 4096 (0x1000, 4 KiB)  unsigned long\n"
    );
    let out = f.kmx(&["TOOLS_ONLY"]);
    assert_eq!(code(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("--scope all"));
    assert_eq!(
        f.stdout(&["TOOLS_ONLY", "--scope", "all", "--brief"]),
        "TOOLS_ONLY is not defined in this context\n"
    );
    let r = f.json(&["TOOLS_ONLY", "--scope", "all"]);
    assert_eq!(r["candidates"][0]["status"], "host");
    assert_eq!(
        r["candidates"][0]["path"],
        "tools/include/linux/tools_only.h"
    );
    let out = f.kmx(&["list", "ZZZ_*"]);
    assert_eq!(code(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("no macro, enum or Kconfig name"));
}

#[test]
fn invalid_inputs_are_errors() {
    let Some(f) = Fixture::new() else { return };
    let cases: &[(&[&str], &str)] = &[
        (&["line", "kernel/fork.c:99"], "has 3 lines"),
        (&["line", "kernel/nope.c:1"], "is not a file"),
        (&["FOO_MODE", "--in", "kernel/nope.c"], "is not a file"),
        (&["FOO_MODE", "--include", "linux/nope.h"], "no such header"),
        (&["FOO_MODE", "--set", "BAR=maybe"], "must be y or n"),
        (
            &["FOO_MODE", "--set", "SHIFT=abc"],
            "must be a decimal number",
        ),
        (&["FOO_MODE", "--set", "X86=n"], "Kconfig keeps X86 at y"),
        (&["expand", " "], "needs some text"),
        (&["list", "FOO_*", "--set", "BAR=n"], "has no effect"),
        (
            &[
                "FOO_MODE",
                "--in",
                "kernel/fork.c",
                "--include",
                "linux/foo.h",
            ],
            "only one",
        ),
    ];
    for (args, needle) in cases {
        let out = f.kmx(args);
        assert_eq!(code(&out), 2, "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(needle), "{args:?}: {err}");
    }
    assert_eq!(
        conclusion(&f.stdout(&["FOO_MODE", "--include", "linux/foo.h", "--brief"])),
        "FOO_MODE = 1"
    );
}

#[test]
fn completes_names_options_and_values() {
    let Some(f) = Fixture::new() else { return };
    f.stdout(&["FOO_MODE", "--brief"]);
    let tree = f.tree.display().to_string();
    let complete = |words: &[&str]| -> Vec<String> {
        let cword = (words.len() - 1).to_string();
        let out = Command::new(env!("CARGO_BIN_EXE_kmx"))
            .arg("--complete")
            .arg(&cword)
            .arg("--")
            .args(words)
            .env("KMX_HOME", &f.home)
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    };
    let first = complete(&["kmx", ""]);
    assert_eq!(first[..5], [":dirs", "ls", "rm", "prune", "inspect"]);
    assert!(
        first.len() == 6 && first[5].len() == 12 && first[5].bytes().all(|b| b.is_ascii_hexdigit()),
        "{first:?}"
    );
    assert_eq!(complete(&["kmx", "in"]), [":dirs", "inspect"]);
    let ids = complete(&["kmx", "rm", ""]);
    let canonical = f.tree.canonicalize().unwrap().display().to_string();
    assert!(ids.contains(&canonical), "{ids:?}");
    assert!(
        ids.iter()
            .any(|c| c.len() == 12 && c.bytes().all(|b| b.is_ascii_hexdigit())),
        "{ids:?}"
    );
    assert_eq!(complete(&["kmx", "ls", ""]), Vec::<String>::new());
    assert_eq!(complete(&["kmx", &tree, "FOO_M"]), ["FOO_MODE"]);
    assert_eq!(complete(&["kmx", &tree, "lis"]), ["list"]);
    assert_eq!(complete(&["kmx", &tree, "TOOLS_"]), Vec::<String>::new());
    assert_eq!(
        complete(&["kmx", &tree, "--scope", "all", "TOOLS_"]),
        ["TOOLS_ONLY"]
    );
    assert_eq!(complete(&["kmx", &tree, "CONFIG_BA"]), ["CONFIG_BAR"]);
    assert_eq!(complete(&["kmx", &tree, "x", "--set", "BA"]), ["BAR="]);
    assert_eq!(
        complete(&["kmx", &tree, "x", "--set", "BAR="]),
        ["BAR=y", "BAR=n"]
    );
    assert_eq!(complete(&["kmx", &tree, "x", "--style", "st"]), ["steps"]);
    assert_eq!(
        complete(&["kmx", &tree, "line", "kernel/fo"]),
        ["kernel/fork.c"]
    );
    assert_eq!(
        complete(&["kmx", &tree, "x", "--include", "linux/fo"]),
        ["linux/foo.h"]
    );
    assert!(complete(&["kmx", &tree, "x", "--bri"]).contains(&"--brief".to_string()));
    let script = Command::new(env!("CARGO_BIN_EXE_kmx"))
        .args(["--completions", "zsh"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&script.stdout).starts_with("#compdef kmx"));
}

#[test]
fn edits_refresh_the_index() {
    let Some(f) = Fixture::new() else { return };
    assert_eq!(
        conclusion(&f.stdout(&["FOO_MODE", "--brief"])),
        "FOO_MODE = 1"
    );
    let header = f.tree.join("include/linux/foo.h");
    let text = std::fs::read_to_string(&header).unwrap();
    std::fs::write(
        &header,
        text.replace("#define FOO_MODE 1", "#define FOO_MODE 3"),
    )
    .unwrap();
    std::fs::write(
        f.tree.join("include/linux/added.h"),
        "#define ADDED_LATER 5\n",
    )
    .unwrap();
    let out = f.kmx(&["--norefresh", "FOO_MODE"]);
    assert_eq!(code(&out), 3);
    assert!(String::from_utf8_lossy(&out.stderr).contains("include/linux/foo.h"));
    assert_eq!(
        conclusion(&f.stdout(&["FOO_MODE", "--brief"])),
        "FOO_MODE = 3"
    );
    assert_eq!(
        conclusion(&f.stdout(&["ADDED_LATER", "--brief"])),
        "ADDED_LATER = 5"
    );
}

#[test]
fn moved_heads_and_new_configs_refresh_the_index() {
    let Some(f) = Fixture::new() else { return };
    f.stdout(&["FOO_MODE", "--brief"]);
    let header = f.tree.join("include/linux/foo.h");
    let text = std::fs::read_to_string(&header).unwrap();
    std::fs::write(
        &header,
        text.replace("#define FOO_MODE 1", "#define FOO_MODE 4"),
    )
    .unwrap();
    f.commit("second");
    let frozen = f.kmx(&["--norefresh", "FOO_MODE"]);
    assert_eq!(code(&frozen), 3);
    assert!(String::from_utf8_lossy(&frozen.stderr).contains("HEAD moved"));
    let out = f.kmx(&["FOO_MODE", "--brief"]);
    assert_eq!(code(&out), 0);
    assert_eq!(
        conclusion(&String::from_utf8_lossy(&out.stdout)),
        "FOO_MODE = 4"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("HEAD moved") && err.contains("index updated"),
        "{err}"
    );
    f.git(&[
        "commit",
        "-q",
        "--amend",
        "--no-gpg-sign",
        "-m",
        "second, reworded",
    ]);
    let out = f.kmx(&["FOO_MODE", "--brief"]);
    assert_eq!(
        conclusion(&String::from_utf8_lossy(&out.stdout)),
        "FOO_MODE = 4"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("HEAD moved"));
    let config = f.tree.join(".config");
    let dot = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        dot.replace("CONFIG_BAR=y", "# CONFIG_BAR is not set"),
    )
    .unwrap();
    let frozen = f.kmx(&["--norefresh", "FOO_MODE"]);
    assert_eq!(code(&frozen), 3);
    assert!(String::from_utf8_lossy(&frozen.stderr).contains("CONFIG_BAR"));
    let out = f.kmx(&["FOO_MODE", "--brief"]);
    assert_eq!(
        conclusion(&String::from_utf8_lossy(&out.stdout)),
        "FOO_MODE = 2"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains(".config changed"));
    let quiet = f.kmx(&["FOO_MODE", "--brief"]);
    assert!(
        quiet.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&quiet.stderr)
    );
}

#[test]
fn rejects_unusable_trees() {
    let Some(f) = Fixture::new() else { return };
    let out = f.kmx_at(&f.tree.join("include"), &["FOO_MODE"]);
    assert_eq!(code(&out), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("top level"));
    let nested = f.tree.join("drivers/vendor");
    std::fs::create_dir_all(&nested).unwrap();
    let st = Command::new("git")
        .arg("-C")
        .arg(&nested)
        .args(["init", "-q"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap();
    assert!(st.success());
    let out = f.kmx(&["FOO_MODE"]);
    assert_eq!(code(&out), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("nested"));
}

#[test]
fn rejects_trees_that_only_look_like_a_kernel() {
    let Some(f) = Fixture::new() else { return };
    write(
        &f.tree.join("Kconfig"),
        "mainmenu \"U-Boot $(UBOOTVERSION) Configuration\"\n",
    );
    std::fs::remove_file(f.tree.join("init/main.c")).unwrap();
    let out = f.kmx(&["FOO_MODE"]);
    assert_eq!(code(&out), 2);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not a Linux kernel tree"), "{err}");
    assert!(
        err.contains("\"U-Boot $(UBOOTVERSION) Configuration\""),
        "{err}"
    );
    assert!(err.contains("no init/main.c"), "{err}");
    let Some(g) = Fixture::new() else { return };
    g.git(&["rm", "-q", "--cached", "scripts/Kbuild.include"]);
    g.git(&["commit", "-q", "--no-gpg-sign", "-m", "untrack kbuild"]);
    let out = g.kmx(&["FOO_MODE"]);
    assert_eq!(code(&out), 2);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("git does not track scripts/Kbuild.include"),
        "{err}"
    );
}

#[test]
fn manages_indexes_without_touching_the_tree() {
    let Some(f) = Fixture::new() else { return };
    let before = f.tracked_state();
    f.stdout(&["FOO_MODE", "--brief"]);
    f.stdout(&["PAGE_SIZE"]);
    let manage = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kmx"))
            .args(args)
            .env("KMX_HOME", &f.home)
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    };
    let canonical = f.tree.canonicalize().unwrap().display().to_string();
    let listing = String::from_utf8_lossy(&manage(&["ls"]).stdout).into_owned();
    let mut lines = listing.lines();
    let header: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(
        header,
        [
            "ID", "KERNEL", "COMMIT", "HASH", "CONTENT", "ARCH", "STATE", "INDEXED", "SIZE", "TREE"
        ]
    );
    let row = lines.next().unwrap();
    assert!(
        row.contains("6.12.0") && row.contains("x86_64"),
        "{listing}"
    );
    assert!(
        row.contains("up to date") && row.contains("just now"),
        "{listing}"
    );
    assert!(
        lines.next().unwrap().starts_with("1 kernel tree, "),
        "{listing}"
    );
    let id = row.split_whitespace().next().unwrap().to_string();
    let narrow = Command::new(env!("CARGO_BIN_EXE_kmx"))
        .arg("ls")
        .env("KMX_HOME", &f.home)
        .env("NO_COLOR", "1")
        .env("COLUMNS", "60")
        .output()
        .unwrap();
    let narrow = String::from_utf8_lossy(&narrow.stdout).into_owned();
    assert!(
        narrow.starts_with(&format!("{id}  6.12.0  x86_64  up to date\n")),
        "{narrow}"
    );
    assert!(
        narrow.contains("├─ commit hash  ") && narrow.contains("└─ tree         "),
        "{narrow}"
    );
    let json: serde_json::Value =
        serde_json::from_slice(&manage(&["ls", "--json"]).stdout).unwrap();
    assert_eq!(json["srctree"], canonical.as_str());
    assert_eq!(json["state"], "up_to_date");
    let inspect = String::from_utf8_lossy(&manage(&["inspect", &id[..6]]).stdout).into_owned();
    assert!(inspect.contains(&canonical), "{inspect}");
    assert!(inspect.contains("content   "), "{inspect}");
    assert!(inspect.contains("state     up to date"), "{inspect}");
    assert_eq!(f.tracked_state(), before);
    let missing = manage(&["rm", "ffffffffffff"]);
    assert_eq!(code(&missing), 2);
    let gone = f.root.join("moved-away");
    std::fs::rename(&f.tree, &gone).unwrap();
    let listing = String::from_utf8_lossy(&manage(&["ls"]).stdout).into_owned();
    assert!(
        listing.contains("missing") && listing.contains("kmx prune"),
        "{listing}"
    );
    let pruned = String::from_utf8_lossy(&manage(&["prune"]).stdout).into_owned();
    assert!(pruned.contains(&format!("removed {id}")), "{pruned}");
    std::fs::rename(&gone, &f.tree).unwrap();
    f.stdout(&["FOO_MODE", "--brief"]);
    assert!(manage(&["rm", f.tree.to_str().unwrap()]).status.success());
    let listing = String::from_utf8_lossy(&manage(&["ls"]).stdout).into_owned();
    assert!(listing.contains("0 kernel trees"), "{listing}");
}

#[test]
fn grep_lists_every_definition_with_status() {
    let Some(f) = Fixture::new() else { return };
    let out = f.stdout(&["grep", "-w", "FOO_MODE"]);
    assert_eq!(
        out,
        "linux/include/linux/foo.h:4: #define FOO_MODE 1  [active = 1]\n\
         linux/include/linux/foo.h:6: #define FOO_MODE 2  [condition false: #ifdef CONFIG_BAR is true]\n"
    );
    let names: Vec<String> = f
        .stdout(&["grep", "FOO"])
        .lines()
        .map(|l| l.split(": ").nth(1).unwrap_or("").to_string())
        .collect();
    assert!(
        names.contains(&"#define FOO_TWICE(x) ((x) * 2)  [active]".to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&"FOO_C = 10  [active = 10 (0xa)]".to_string()),
        "{names:?}"
    );
    let globbed = f.stdout(&["grep", "-g", "FOO_?"]);
    assert_eq!(globbed.lines().count(), 4, "{globbed}");
    let out = f.kmx(&["grep", "ZZZ_NOTHING"]);
    assert_eq!(code(&out), 1);
    assert!(out.stdout.is_empty());
    let out = f.kmx(&["grep", "-w", "-g", "FOO"]);
    assert_eq!(code(&out), 2);
}

#[test]
fn uses_finds_definitions_that_reference_a_name() {
    let Some(f) = Fixture::new() else { return };
    assert_eq!(
        f.stdout(&["uses", "CONFIG_SHIFT"]),
        "linux/arch/x86/include/asm/page.h:3: #define PAGE_SHIFT CONFIG_SHIFT  [active = 12 (0xc)]\n"
    );
    let out = f.stdout(&["uses", "PAGE_SHIFT"]);
    assert!(
        out.starts_with(
            "linux/arch/x86/include/asm/page.h:4: #define PAGE_SIZE (1UL << PAGE_SHIFT)  [active = 4096"
        ),
        "{out}"
    );
    let out = f.kmx(&["uses", "FOO_MODE"]);
    assert_eq!(code(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("no macro definition"));
}

#[test]
fn vimgrep_keeps_openable_paths_and_output_shortens_them() {
    let Some(f) = Fixture::new() else { return };
    let tree = f.tree.display().to_string();
    assert_eq!(
        f.stdout(&["grep", "-w", "FOO_MODE", "--vimgrep"])
            .lines()
            .next(),
        Some(format!("{tree}/include/linux/foo.h:4:9:#define FOO_MODE 1  [active = 1]").as_str())
    );
    let out = Command::new(env!("CARGO_BIN_EXE_kmx"))
        .current_dir(&f.root)
        .args(["linux", "grep", "-w", "FOO_MODE"])
        .env("KMX_HOME", &f.home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("linux/include/linux/foo.h:4: "), "{text}");
    let shown = f.stdout(&["FOO_MODE"]);
    assert!(shown.contains("linux/include/linux/foo.h:4"), "{shown}");
    assert!(!shown.contains(&format!("{tree}/include")), "{shown}");
    let canonical = f.tree.canonicalize().unwrap().display().to_string();
    assert!(
        shown
            .lines()
            .any(|l| l.trim_start() == format!("tree     {canonical}")),
        "{shown}"
    );
    assert!(shown.contains(" \u{f126} "), "{shown}");
    assert!(f.stdout(&["FOO_MODE", "--ascii"]).contains("6.12.0 @ "));
    let json = f.json(&["FOO_MODE"]);
    assert_eq!(json["active"]["path"], "include/linux/foo.h");
    let out = f.kmx(&["FOO_MODE", "--vimgrep"]);
    assert_eq!(code(&out), 2);
}

#[test]
fn missing_compiler_headers_come_from_the_host_compiler() {
    let Some(f) = Fixture::new() else { return };
    let host = Command::new("gcc")
        .arg("-print-file-name=include")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|d| Path::new(d).is_absolute() && Path::new(d).join("stdarg.h").is_file());
    let Some(host) = host else {
        eprintln!("gcc with its own headers is not available; skipping");
        return;
    };
    write(&f.tree.join(".gitignore"), ".config\n.*.cmd\n");
    f.commit("ignore build records");
    write(
        &f.tree.join("init/.main.o.cmd"),
        "cmd_init/main.o := gcc -nostdinc -isystem /kmx-absent/gcc/include -I./arch/x86/include \
         -I./include -include ./include/linux/kconfig.h -D__KERNEL__ -c -o init/main.o init/main.c\n\n\
         source_init/main.o := init/main.c\n\n\
         deps_init/main.o := \\\n  include/linux/foo.h \\\n\ninit/main.o: $(deps_init/main.o)\n",
    );
    let out = f.stdout(&["expand", "va_arg(ap, int)", "--include", "<stdarg.h>"]);
    assert!(
        out.starts_with("va_arg(ap, int) → __builtin_va_arg(ap, int)"),
        "{out}"
    );
    assert!(out.contains(&format!("compiler headers: {host}")), "{out}");
    assert!(
        out.contains("(the build's /kmx-absent/gcc/include is absent here)"),
        "{out}"
    );
    let r = f.json(&["FOO_MODE"]);
    assert_eq!(
        r["context"]["compiler_headers"][0]["recorded"],
        "/kmx-absent/gcc/include"
    );
    assert_eq!(r["context"]["compiler_headers"][0]["used"], host.as_str());
}

const DEFINE_VARIANTS: &str = "#define        SPACED_NAME          (1 + 2)\n\
#define\tTABBED_NAME\t3\n\
#define \\\nSPLIT_BEFORE_NAME (4)\n\
#  define INDENTED_HASH 5\n\
\t#\tdefine\tTAB_HASH\t6\n\
#/* c */define /* c */ COMMENTED 7\n\
/* lead */ #define AFTER_COMMENT 8\n\
#de\\\nfine SPLIT_KEYWORD 9\n\
#define SPLIT_IN_NAME_\\\nTAIL 10\n\
#define SPLIT_PAREN\\\n(x) ((x) * 11)\n\
#define NOT_FUNCTION/**/(x) 12\n\
#define /* multi\n   line comment */ AFTER_MULTILINE_COMMENT 14\n\
#define MULTI_LINE_BODY(a, b) \\\n\t((a) + \\\n\t (b))\n\
#define CRLF_REAL 16\r\n\
#define \\\r\nCRLF_SPLIT 17\r\n\
#define USES_SPLIT (SPLIT_BEFORE_NAME + SPLIT_PAREN(1))\n";

#[test]
fn define_variants_follow_c_rules() {
    let Some(f) = Fixture::new() else { return };
    write(&f.tree.join("include/linux/variants.h"), DEFINE_VARIANTS);
    write(
        &f.tree.join("include/linux/broken.h"),
        "#define\nNOT_A_MACRO_LINE (9)\n",
    );
    f.commit("define variants");
    for (name, value) in [
        ("SPACED_NAME", "3"),
        ("TABBED_NAME", "3"),
        ("SPLIT_BEFORE_NAME", "4"),
        ("INDENTED_HASH", "5"),
        ("TAB_HASH", "6"),
        ("COMMENTED", "7"),
        ("AFTER_COMMENT", "8"),
        ("SPLIT_KEYWORD", "9"),
        ("SPLIT_IN_NAME_TAIL", "10"),
        ("AFTER_MULTILINE_COMMENT", "14"),
        ("CRLF_REAL", "16"),
        ("CRLF_SPLIT", "17"),
    ] {
        let out = f.stdout(&[name, "--brief"]);
        let head = format!("{name} = {value}");
        assert!(
            out.strip_prefix(&head)
                .is_some_and(|rest| rest.starts_with(['\n', ' '])),
            "{name}: {out}"
        );
    }
    for (text, value) in [
        ("SPLIT_PAREN(2)", "22"),
        ("MULTI_LINE_BODY(1, 2)", "3"),
        ("USES_SPLIT", "15"),
    ] {
        let out = f.stdout(&["expand", text, "--brief"]);
        assert!(out.contains(&format!("= {value}")), "{text}: {out}");
    }
    let out = f.stdout(&["expand", "NOT_FUNCTION(3)", "--brief"]);
    assert!(out.contains("(x)12(3)"), "{out}");
    let vim = f.stdout(&["grep", "-w", "SPLIT_BEFORE_NAME", "--vimgrep"]);
    assert!(vim.contains("include/linux/variants.h:3:"), "{vim}");
    let uses = f.stdout(&["uses", "SPLIT_PAREN", "--vimgrep"]);
    assert!(uses.contains("#define USES_SPLIT"), "{uses}");
    let out = f.kmx(&["NOT_A_MACRO_LINE"]);
    assert_eq!(code(&out), 1, "{}", String::from_utf8_lossy(&out.stdout));
}

const SELF_NAMED: &str = "#ifndef _LINUX_SELF_H\n#define _LINUX_SELF_H\n\
#define HELPER_BIAS 5\n\
#define helper helper\n\
static inline int helper(int x)\n{\n\treturn x + HELPER_BIAS;\n}\n\
#ifdef CONFIG_NOT_SET_ANYWHERE\n\
static inline int pick(void) { return 1; }\n\
#else\n\
static inline int pick(void)\n{\n\treturn 2;\n}\n\
#endif\n\
#define pick pick\n\
enum self_enum {\n\tSELF_A = 3,\n#define SELF_A SELF_A\n};\n\
#define LONELY LONELY\n\
#endif\n";

#[test]
fn self_named_macros_lead_to_the_c_definition() {
    let Some(f) = Fixture::new() else { return };
    write(&f.tree.join("include/linux/self.h"), SELF_NAMED);
    f.commit("self-named macros");
    let out = f.stdout(&["expand", "helper", "--include", "<linux/self.h>"]);
    assert!(out.starts_with("helper(x) = x + 5\n\n"), "{out}");
    let brief = f.stdout(&["expand", "helper", "--include", "<linux/self.h>", "-b"]);
    assert_eq!(brief, "helper(x) = x + 5\n");
    assert!(
        out.contains("names itself: the C static inline function below applies"),
        "{out}"
    );
    assert!(out.contains("static inline int helper(int x)"), "{out}");
    assert!(out.contains("        return x + HELPER_BIAS;"), "{out}");
    assert!(out.contains("HELPER_BIAS"), "{out}");
    let json = f.json(&["helper"]);
    let child = &json["expansion"]["children"][0];
    assert_eq!(child["target"]["kind"], "static inline function", "{json}");
    let pick = f.stdout(&["expand", "pick", "--include", "<linux/self.h>"]);
    assert!(
        pick.contains("return 2;") && !pick.contains("return 1;"),
        "{pick}"
    );
    assert!(pick.starts_with("pick() = 2\n"), "{pick}");
    assert_eq!(conclusion(&f.stdout(&["SELF_A", "--brief"])), "SELF_A = 3");
    let sum = f.stdout(&[
        "expand",
        "SELF_A * 2",
        "--include",
        "<linux/self.h>",
        "--brief",
    ]);
    assert!(sum.contains("= 6"), "{sum}");
    let lonely = f.stdout(&["LONELY"]);
    assert!(
        lonely.contains("LONELY names itself, but no function or enum constant named LONELY"),
        "{lonely}"
    );
}

const REFINE: &str = "#ifndef _LINUX_REFINE_H\n#define _LINUX_REFINE_H\n\
#define VA_BITS 39\n\
#define BIT(nr) (1UL << (nr))\n\
#define MASK (~((0xffffffffffffffffUL) << ((VA_BITS) - 1)))\n\
#define __force\n\
#define BUILD_BUG_ON_INVALID(e) ((void)(sizeof((__force long)(e))))\n\
#define OFFSET ({ BUILD_BUG_ON_INVALID(start & 1); start; })\n\
#define TO_PHYS(x) ({ unsigned long __x = (unsigned long)(x); __x & BIT(VA_BITS - 1) ? (__x & MASK) + OFFSET : (__x - voffset_of_the_kernel_image); })\n\
#endif\n";

#[test]
fn conclusions_are_refined_and_every_node_leads_with_its_result() {
    let Some(f) = Fixture::new() else { return };
    write(&f.tree.join("include/linux/refine.h"), REFINE);
    f.commit("refinement");
    let out = f.stdout(&["TO_PHYS", "--include", "<linux/refine.h>", "--ascii"]);
    assert!(
        out.starts_with(
            "TO_PHYS(x) -> ({\n        unsigned long __x = (unsigned long)x;\n        __x & 0x4000000000UL\n                ? (__x & 0x3fffffffffUL) + ({ start; })\n                : __x - voffset_of_the_kernel_image;\n})\n\n"
        ),
        "{out}"
    );
    assert!(
        out.contains("BIT(VA_BITS - 1) = 274877906944 (0x4000000000, 256 GiB)\n"),
        "{out}"
    );
    assert!(out.contains("nr = VA_BITS - 1 -> 38\n"), "{out}");
    assert!(out.contains("OFFSET = ({ start; })\n"), "{out}");
    assert!(out.contains("__force = (expands to nothing)\n"), "{out}");
    let define = out
        .lines()
        .find(|l| l.contains("#define BIT(nr) (1UL << (nr))"))
        .unwrap_or_default();
    assert!(define.ends_with("include/linux/refine.h:4"), "{out}");
    let header = out
        .lines()
        .position(|l| l.contains("BIT(VA_BITS - 1) = "))
        .unwrap_or_default();
    assert!(
        out.lines()
            .nth(header + 1)
            .unwrap_or_default()
            .contains("#define BIT(nr)"),
        "{out}"
    );
    let width = |text: &str| text.chars().count();
    for line in out
        .lines()
        .filter(|l| l.contains("0x3fffffffffUL") || l.contains("#define TO_PHYS"))
    {
        assert!(width(line) <= 80, "{line}\n{out}");
    }
    let brief = f.stdout(&[
        "TO_PHYS",
        "--include",
        "<linux/refine.h>",
        "--ascii",
        "--brief",
    ]);
    let headline = out.split("\n\n").next().unwrap_or_default();
    assert_eq!(brief, format!("{headline}\n"));
    let both = f.stdout(&[
        "BIT",
        "VA_BITS",
        "--include",
        "<linux/refine.h>",
        "--ascii",
        "-b",
    ]);
    assert_eq!(
        both, "BIT(nr) -> 1UL << nr\nVA_BITS = 39 (0x27)  int\n",
        "{both}"
    );
    let json = f.json(&["TO_PHYS", "--include", "<linux/refine.h>"]);
    let to_phys = &json["expansion"];
    assert_eq!(
        to_phys["refined"],
        "({ unsigned long __x = (unsigned long)x; __x & 0x4000000000UL ? (__x & 0x3fffffffffUL) + ({ start; }) : __x - voffset_of_the_kernel_image; })",
        "{json}"
    );
}

#[test]
fn default_output_peels_only_the_active_chain() {
    let Some(f) = Fixture::new() else { return };
    let plain = f.stdout(&["FOO_MODE"]);
    assert!(!plain.contains('✗'), "{plain}");
    assert!(!plain.contains("other definitions"), "{plain}");
    assert!(plain.contains("✓ #ifdef CONFIG_BAR"), "{plain}");
    let all = f.stdout(&["FOO_MODE", "--all"]);
    assert!(
        all.contains('✗') && all.contains("other definitions of FOO_MODE"),
        "{all}"
    );
    let two = f.stdout(&["FOO_MODE", "--candidates", "2"]);
    assert!(two.contains("other definitions of FOO_MODE"), "{two}");
    let grep = f.stdout(&["grep", "-w", "FOO_MODE"]);
    assert_eq!(grep.lines().count(), 2, "{grep}");
}

#[test]
fn concurrent_queries_survive_edits_and_refreshes() {
    let Some(f) = Fixture::new() else { return };
    f.stdout(&["FOO_MODE", "--brief"]);
    let header = f.tree.join("include/linux/foo.h");
    let original = std::fs::read_to_string(&header).unwrap();
    let variants: Vec<String> = (1..=4)
        .map(|v| original.replace("#define FOO_MODE 1", &format!("#define FOO_MODE {v}")))
        .collect();
    let spawn = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_kmx"))
            .arg(&f.tree)
            .args(args)
            .env("KMX_HOME", &f.home)
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    for round in 0..6 {
        let children: Vec<_> = (0..6)
            .map(|k| match k % 3 {
                0 => spawn(&["FOO_MODE", "--brief"]),
                1 => spawn(&["grep", "^FOO_"]),
                _ => spawn(&["FOO_C", "--brief", "--no-memo"]),
            })
            .collect();
        std::fs::write(&header, &variants[round % variants.len()]).unwrap();
        for child in children {
            let out = child.wait_with_output().unwrap();
            assert_eq!(
                code(&out),
                0,
                "round {round}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(
                text.starts_with("FOO_MODE = ")
                    || text.starts_with("FOO_C = 10")
                    || text.contains("FOO_MODE"),
                "round {round}: {text}"
            );
        }
    }
    let last = 5 % variants.len() + 1;
    assert_eq!(
        conclusion(&f.stdout(&["FOO_MODE", "--brief"])),
        format!("FOO_MODE = {last}")
    );
}

#[test]
fn index_ids_stand_in_for_the_tree() {
    let Some(f) = Fixture::new() else { return };
    let by_path = f.stdout(&["FOO_MODE", "--brief"]);
    let listed: serde_json::Value =
        serde_json::from_slice(&f.bare(&["ls", "--json"]).stdout).unwrap();
    let id = listed["id"].as_str().unwrap().to_string();
    for arg in [&id[..12], &id[..6]] {
        let out = f.bare(&[arg, "FOO_MODE", "--brief"]);
        assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout), by_path);
    }
    let unknown = f.bare(&["ffffffffffff", "FOO_MODE"]);
    assert_eq!(code(&unknown), 2);
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("no index has this ID"));
    let other_build = f.bare(&[&id[..12], "-O", f.root.to_str().unwrap(), "FOO_MODE"]);
    assert_eq!(code(&other_build), 2);
    assert!(String::from_utf8_lossy(&other_build.stderr).contains("drop -O"));
}

#[test]
fn indexes_survive_reinstalls_and_older_manifests() {
    let Some(f) = Fixture::new() else { return };
    let first = f.stdout(&["FOO_MODE"]);
    let listed: serde_json::Value =
        serde_json::from_slice(&f.bare(&["ls", "--json"]).stdout).unwrap();
    let project = f.home.join("projects").join(listed["id"].as_str().unwrap());
    let manifest = project.join("current/manifest.json");
    let mut old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    let content = old["content"].as_str().unwrap().to_string();
    old.as_object_mut().unwrap().remove("content");
    std::fs::write(&manifest, serde_json::to_vec(&old).unwrap()).unwrap();
    let listing = String::from_utf8_lossy(&f.bare(&["ls"]).stdout).into_owned();
    assert!(listing.contains(&content[..12]), "{listing}");
    let upgraded: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(upgraded["content"], content.as_str());
    let installed = f.root.join("bin/kmx");
    std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_kmx"), &installed).unwrap();
    let tree = f.tree.to_str().unwrap();
    let again = f.bare_with(&installed, &[tree, "FOO_MODE"]);
    assert_eq!(String::from_utf8_lossy(&again.stdout), first);
    let tags = std::fs::read_dir(project.join("current/memo"))
        .unwrap()
        .count();
    assert_eq!(
        tags, 1,
        "a copied binary must reuse the memo of the same build"
    );
}
