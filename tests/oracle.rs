use std::path::PathBuf;

use kmx::workspace::{OpenOptions, Workspace};

mod support {
    pub mod oracle;
}

use support::oracle::Oracle;

#[test]
fn simulator_matches_the_compiler() {
    let Some(tree) = std::env::var_os("KMX_ORACLE_TREE") else {
        eprintln!("KMX_ORACLE_TREE is not set; skipping the compiler oracle");
        return;
    };
    let opts = OpenOptions {
        build_dir: std::env::var_os("KMX_ORACLE_BUILD").map(PathBuf::from),
        refresh: true,
        compiler: None,
        rebuild: false,
    };
    let ws = Workspace::open(&PathBuf::from(tree), &opts, &mut |m| eprintln!("{m}"))
        .unwrap_or_else(|e| panic!("{e}"));
    let files = std::env::var("KMX_ORACLE_FILES").unwrap_or_else(|_| "init/main.c".into());
    let mut failures = Vec::new();
    for rel in files.split(',').map(str::trim).filter(|f| !f.is_empty()) {
        let Some(o) = Oracle::new(&ws, rel) else {
            failures.push(format!("{rel}: no .cmd file in the build directory"));
            continue;
        };
        let (m, mut sim) = o.macros();
        let (only_gcc, only_ours, differ) = (m.only_gcc(), m.only_ours(), m.differ());
        if !(only_gcc.is_empty() && only_ours.is_empty() && differ.is_empty()) {
            failures.push(format!(
                "{rel}: macro tables differ: only gcc {:?}, only kmx {:?}, different {:?}",
                &only_gcc[..only_gcc.len().min(5)],
                &only_ours[..only_ours.len().min(5)],
                &differ[..differ.len().min(5)]
            ));
        }
        let e = o.expansions(&mut sim);
        if !e.mismatches.is_empty() {
            let names: Vec<&str> = e.mismatches.iter().take(5).map(|m| m.0.as_str()).collect();
            failures.push(format!(
                "{rel}: {} of {} expansions differ, e.g. {names:?}",
                e.mismatches.len(),
                e.compared
            ));
        }
        eprintln!(
            "{rel}: {} macros from {} files (simulated in {:?}, {} diagnostics, compiler {}) \
             and {} expansions (compiler {}) checked",
            m.gcc.len(),
            m.files,
            m.sim_time,
            m.diags.len(),
            m.gcc_status,
            e.compared,
            e.gcc_status
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
