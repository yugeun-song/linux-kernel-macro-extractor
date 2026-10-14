use std::path::PathBuf;
use std::time::Instant;

use kmx::kconfig::{self, Eval};
use kmx::tree::KernelTree;
use kmx::tree::arch::Arch;
use kmx::tree::dot_config::DotConfig;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let tree = KernelTree::open(&PathBuf::from(&a[1])).unwrap();
    let objtree = a
        .get(2)
        .filter(|s| *s != "-")
        .map(PathBuf::from)
        .unwrap_or(tree.root.clone());
    let cfg = DotConfig::load(&objtree.join(".config")).unwrap();
    let arch = Arch::detect(&cfg).unwrap();
    let env = kconfig::Env {
        arch: cfg
            .header_arch
            .clone()
            .unwrap_or_else(|| arch.name().to_string()),
        srcarch: arch.srcarch().to_string(),
        kernelversion: tree.version.release(),
        srctree: tree.root.clone(),
    };
    let t = Instant::now();
    let k = kconfig::load(&env);
    let parse = t.elapsed();
    let mut ev = Eval::new(&k, &cfg);
    let t2 = Instant::now();
    let mm = ev.mismatches();
    println!(
        "{}: files {} symbols {} choices {} warnings {} | parse {:?} eval {:?} | mismatches {} / .config entries {}",
        tree.root.display(),
        k.files.len(),
        k.symbols.len(),
        k.choices.len(),
        k.warnings.len(),
        parse,
        t2.elapsed(),
        mm.len(),
        cfg.len()
    );
    for w in k.warnings.iter().take(5) {
        println!("  warning: {w}");
    }
    for (n, ours, theirs) in mm.iter().take(15) {
        println!("  {n}: ours={ours} .config={theirs}");
    }
    if let Ok(name) = std::env::var("KMX_DEBUG_SYM") {
        let s = k.lookup(&name).unwrap();
        println!(
            "debug {name}: value={:?} vis={} dir={} rev={} user={:?}",
            ev.value(s),
            ev.visibility(s),
            ev.dir_dep(s),
            ev.rev_dep(s),
            ev.user_value(s)
        );
        for (by, cond, loc) in &k.sym(s).selected_by {
            let v = ev.tri(*by);
            let c = ev.eval(cond);
            println!(
                "  selected by {} = {v} cond {} = {c}  ({})",
                k.sym(*by).name,
                k.expr_text(cond),
                k.loc_text(*loc)
            );
        }
        for d in &k.sym(s).defs {
            println!(
                "  def {} deps {} = {}",
                k.loc_text(d.loc),
                k.expr_text(&d.deps),
                ev.eval(&d.deps)
            );
        }
    }
}
