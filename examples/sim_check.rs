use std::path::PathBuf;

use kmx::workspace::{OpenOptions, Workspace};

#[path = "../tests/support/oracle.rs"]
mod oracle;

fn clip(s: &str) -> &str {
    &s[..s.len().min(300)]
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: sim_check TREE BUILD_DIR|- FILE [--expand]");
        std::process::exit(2);
    }
    let opts = OpenOptions {
        build_dir: (a[2] != "-").then(|| PathBuf::from(&a[2])),
        refresh: true,
        compiler: None,
        rebuild: false,
    };
    let ws = Workspace::open(&PathBuf::from(&a[1]), &opts, &mut |m| eprintln!("{m}"))
        .unwrap_or_else(|e| panic!("{e}"));
    let rel = &a[3];
    let Some(o) = oracle::Oracle::new(&ws, rel) else {
        eprintln!("{rel}: no .cmd file in the build directory");
        std::process::exit(1);
    };
    let (m, mut sim) = o.macros();
    println!(
        "{rel}: sim {:?}, files {}, ours {} gcc {} | only_gcc {} only_ours {} differ {} | diags {} (gcc rc {})",
        m.sim_time,
        m.files,
        m.ours.len(),
        m.gcc.len(),
        m.only_gcc().len(),
        m.only_ours().len(),
        m.differ().len(),
        m.diags.len(),
        m.gcc_status
    );
    for k in m.only_gcc().into_iter().take(12) {
        println!("  only gcc : {}", m.gcc[k]);
    }
    for k in m.only_ours().into_iter().take(12) {
        println!("  only ours: {}", m.ours[k]);
    }
    for k in m.differ().into_iter().take(12) {
        println!(
            "  differ   : gcc  {}\n             ours {}",
            m.gcc[k], m.ours[k]
        );
    }
    for d in m.diags.iter().take(8) {
        println!("  diag {d}");
    }
    if a.get(4).map(String::as_str) != Some("--expand") {
        return;
    }
    let e = o.expansions(&mut sim);
    for (name, gcc, ours) in e.mismatches.iter().take(8) {
        match gcc {
            Some(g) => println!(
                "  EXPAND DIFF {name}\n    gcc : {}\n    ours: {}",
                clip(g),
                clip(ours)
            ),
            None => println!("  EXPAND MISSING {name}"),
        }
    }
    println!(
        "  expansions compared: {} mismatches: {} (gcc rc {})",
        e.compared,
        e.mismatches.len(),
        e.gcc_status
    );
}
