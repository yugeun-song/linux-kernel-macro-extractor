use std::path::PathBuf;
use std::process::Command;

use kmx::kconfig::{self, Eval};
use kmx::tree::KernelTree;
use kmx::tree::arch::Arch;
use kmx::tree::dot_config::DotConfig;

fn first_line(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(str::to_string)
        })
        .unwrap_or_default()
}

#[test]
fn evaluator_matches_kconfig_randconfig() {
    let Some(tree) = std::env::var_os("KMX_KCONFIG_ORACLE_TREE").map(PathBuf::from) else {
        eprintln!("KMX_KCONFIG_ORACLE_TREE is not set; skipping the Kconfig oracle");
        return;
    };
    let conf = tree.join("scripts/kconfig/conf");
    assert!(
        conf.is_file(),
        "{}: build scripts/kconfig/conf first",
        conf.display()
    );
    let kernel = KernelTree::open(&tree).unwrap_or_else(|e| panic!("{e}"));
    let base = DotConfig::load(&tree.join(".config")).unwrap_or_else(|e| panic!("{e}"));
    let arch = Arch::detect(&base).unwrap_or_else(|e| panic!("{e}"));
    let (make_arch, cross, pin) = match arch {
        Arch::X86_64 => ("x86_64", "", ""),
        Arch::Arm64 => ("arm64", "aarch64-linux-gnu-", ""),
        Arch::Riscv64 => ("riscv", "riscv64-linux-gnu-", "CONFIG_ARCH_RV64I=y\n"),
    };
    let cross = std::env::var("KMX_KCONFIG_ORACLE_CROSS").unwrap_or_else(|_| cross.to_string());
    let seeds: u64 = std::env::var("KMX_KCONFIG_ORACLE_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let first: u64 = std::env::var("KMX_KCONFIG_ORACLE_FIRST")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let env = kconfig::Env {
        arch: make_arch.to_string(),
        srcarch: arch.srcarch().to_string(),
        kernelversion: kernel.version.release(),
        srctree: kernel.root.clone(),
    };
    let k = kconfig::load(&env);
    let tool = |name: &str| format!("{cross}{name}");
    let cc_version = first_line(&tool("gcc"), &["--version"]);
    let scratch = std::env::temp_dir().join(format!("kmx-kconfig-oracle-{}", std::process::id()));
    let mut failures = Vec::new();
    for seed in first..first + seeds {
        let dir = scratch.join(seed.to_string());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join(".config");
        let allconfig = dir.join("allconfig");
        std::fs::write(&allconfig, pin).unwrap();
        let out = Command::new("sh")
            .args(["-c", "ulimit -c 0; exec \"$@\"", "sh"])
            .arg(&conf)
            .args(["-s", "--randconfig", "Kconfig"])
            .current_dir(&dir)
            .env("srctree", &kernel.root)
            .env("ARCH", make_arch)
            .env("SRCARCH", arch.srcarch())
            .env("CROSS_COMPILE", &cross)
            .env("CC", tool("gcc"))
            .env("LD", tool("ld"))
            .env("AR", tool("ar"))
            .env("NM", tool("nm"))
            .env("STRIP", tool("strip"))
            .env("OBJCOPY", tool("objcopy"))
            .env("OBJDUMP", tool("objdump"))
            .env("READELF", tool("readelf"))
            .env("HOSTCC", "gcc")
            .env("KERNELVERSION", kernel.version.release())
            .env("CC_VERSION_TEXT", &cc_version)
            .env("KCONFIG_SEED", seed.to_string())
            .env("KCONFIG_PROBABILITY", ((seed % 3) * 30 + 20).to_string())
            .env("KCONFIG_CONFIG", &config)
            .env("KCONFIG_ALLCONFIG", &allconfig)
            .output()
            .unwrap();
        let Ok(cfg) = DotConfig::load(&config) else {
            failures.push(format!(
                "seed {seed}: conf produced no .config: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
            continue;
        };
        let mut ev = Eval::new(&k, &cfg);
        let mismatches = ev.mismatches();
        if mismatches.is_empty() {
            let _ = std::fs::remove_dir_all(&dir);
        } else {
            let shown: Vec<String> = mismatches
                .iter()
                .take(5)
                .map(|(s, ours, theirs)| format!("{s}: kmx {ours}, conf {theirs}"))
                .collect();
            failures.push(format!(
                "seed {seed}: {} mismatches, e.g. {shown:?} (config kept in {})",
                mismatches.len(),
                dir.display()
            ));
        }
    }
    if failures.is_empty() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    eprintln!(
        "{}: {seeds} random configurations checked, {} failed",
        tree.display(),
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
