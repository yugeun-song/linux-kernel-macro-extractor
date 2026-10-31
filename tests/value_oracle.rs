use std::collections::BTreeMap;
use std::path::PathBuf;

use kmx::analysis::candidates::Reach;
use kmx::analysis::context::{ContextChoice, sim_for};
use kmx::analysis::expansion::{Alternatives, TreeOpts};
use kmx::analysis::show::{ShowOptions, show};
use kmx::pp::kbuild::CompileModel;
use kmx::pp::sim::Main;
use kmx::workspace::{OpenOptions, Workspace};

#[allow(dead_code)]
mod support {
    pub mod oracle;
}

use support::oracle::Oracle;

const HEADERS: &[&str] = &[
    "include/linux/mmzone.h",
    "include/linux/mm_types.h",
    "include/linux/mm.h",
    "include/linux/page-flags-layout.h",
    "include/linux/pageblock-flags.h",
    "include/linux/skbuff.h",
    "include/linux/slab.h",
    "include/linux/sched.h",
    "include/linux/fs.h",
    "include/linux/bpf.h",
    "ARCH/include/asm/page.h",
    "ARCH/include/asm/pgtable.h",
    "ARCH/include/asm/memory.h",
    "ARCH/include/asm/processor.h",
];

fn literal(display: &str, ty: &str) -> Option<String> {
    let n = display.split_whitespace().next()?;
    n.parse::<i128>().ok()?;
    Some(match ty {
        "int" => format!("({n})"),
        "unsigned int" => format!("({n}U)"),
        "long" => format!("({n}L)"),
        "unsigned long" => format!("({n}UL)"),
        "long long" => format!("({n}LL)"),
        "unsigned long long" => format!("({n}ULL)"),
        "_Bool" | "char" | "signed char" | "unsigned char" | "short" | "unsigned short" => {
            format!("(({ty}){n})")
        }
        _ => return None,
    })
}

#[test]
fn folded_values_match_the_compiler() {
    let Some(tree) = std::env::var_os("KMX_VALUE_ORACLE") else {
        eprintln!("KMX_VALUE_ORACLE is not set; skipping the macro value oracle");
        return;
    };
    let opts = OpenOptions {
        build_dir: None,
        refresh: true,
        compiler: None,
        rebuild: false,
    };
    let ws = Workspace::open(&PathBuf::from(tree), &opts, &mut |m| eprintln!("{m}"))
        .unwrap_or_else(|e| panic!("{e}"));
    let oracle = Oracle::new(&ws, "init/main.c").expect("init/main.c was compiled");
    let arch = format!("arch/{}", ws.layout.srcarch);
    let headers: Vec<String> = match std::env::var("KMX_VALUE_HEADERS") {
        Ok(list) => list.split(',').map(str::to_string).collect(),
        Err(_) => HEADERS.iter().map(|h| h.replace("ARCH", &arch)).collect(),
    };
    let show_opts = ShowOptions {
        context: ContextChoice::Auto,
        tree: TreeOpts {
            max_depth: Some(1),
            alternatives: Alternatives::None,
            reach: Reach::Kernel,
            full_bodies: false,
        },
        autoconf: None,
        what_if: Vec::new(),
        census: false,
    };
    let model = CompileModel::default_for(&ws.layout);
    let mut groups: BTreeMap<Vec<String>, Vec<(String, String, String)>> = BTreeMap::new();
    let (mut names_seen, mut valued, mut skipped, mut newer) = (0, 0, 0, 0);
    for rel in &headers {
        let abs = ws.layout.srctree.join(rel);
        if !abs.is_file() {
            continue;
        }
        let (mut sim, _) = sim_for(&ws, model.clone(), None);
        sim.run(Main::File(abs.clone()));
        let syms: Vec<_> = sim.macro_names().collect();
        let mut names: Vec<String> = syms
            .into_iter()
            .filter_map(|s| {
                let d = sim.def_of(s)?;
                let site = d.site?;
                (!d.is_function() && d.builtin.is_none() && sim.files.display(site.file) == rel)
                    .then(|| sim.it.get(s).to_string())
            })
            .collect();
        names.sort();
        for name in names {
            names_seen += 1;
            let r = show(&ws, &name, &show_opts);
            let Some(v) = &r.value else { continue };
            let Some(lit) = literal(&v.display, v.ty.as_deref().unwrap_or("")) else {
                skipped += 1;
                continue;
            };
            let Some(header) = r
                .context
                .mode
                .strip_prefix("virtual TU: ")
                .and_then(|m| m.split_whitespace().next())
                .filter(|h| h.starts_with('<') && r.context.mode.contains("init/.main.o.cmd"))
            else {
                skipped += 1;
                continue;
            };
            let mut includes = vec![header.to_string()];
            for n in &r.notes {
                if let Some(rest) = n
                    .strip_prefix("auto-included to define the macros used: ")
                    .or_else(|| n.strip_prefix("auto-included to define the types used: "))
                {
                    includes.extend(rest.split_whitespace().map(str::to_string));
                }
            }
            valued += 1;
            if r.expansion.as_ref().is_some_and(|n| {
                ["sizeof", "offsetof", "__builtin_"]
                    .iter()
                    .any(|w| n.result.contains(w))
            }) {
                newer += 1;
            }
            groups
                .entry(includes)
                .or_default()
                .push((name, lit, v.ty.clone().unwrap_or_default()));
        }
    }
    if let Some((includes, probes)) = groups.iter().next() {
        let (name, lit, _) = &probes[0];
        let head: String = includes.iter().map(|h| format!("#include {h}\n")).collect();
        let (wrong_ok, _) = oracle.check(&format!(
            "{head}_Static_assert(({name}) == {lit} + 1, \"sanity\");\n"
        ));
        assert!(!wrong_ok, "the compiler accepted a wrong value for {name}");
    }
    let mut failures = Vec::new();
    let mut unverified = 0;
    let mut uncompilable = 0;
    for (includes, probes) in &groups {
        let mut src: String = includes.iter().map(|h| format!("#include {h}\n")).collect();
        let first = includes.len() + 1;
        for (name, lit, ty) in probes {
            src.push_str(&format!(
                "_Static_assert(({name}) == {lit}, \"kmx-value\");\n\
                 _Static_assert(__builtin_types_compatible_p(__typeof__({name}), {ty}), \"kmx-type\");\n"
            ));
        }
        let (ok, errors) = oracle.check(&src);
        if ok {
            continue;
        }
        let mut noisy = false;
        let mut lost = std::collections::BTreeSet::new();
        for line in errors.lines().filter(|l| l.contains("error")) {
            let Some(at) = line
                .strip_prefix("<stdin>:")
                .and_then(|r| r.split(':').next())
                .and_then(|n| n.parse::<usize>().ok())
            else {
                noisy = true;
                continue;
            };
            let Some(i) = at
                .checked_sub(first)
                .map(|k| k / 2)
                .filter(|i| *i < probes.len())
            else {
                failures.push(format!("{includes:?}: {}", line.trim()));
                continue;
            };
            let (name, lit, ty) = &probes[i];
            if line.contains("static assertion failed") {
                failures.push(format!(
                    "{name}: kmx says {lit} of type {ty} ({})",
                    line.trim()
                ));
            } else {
                lost.insert(i);
            }
        }
        unverified += lost.len();
        if noisy {
            uncompilable += 1;
        }
    }
    eprintln!(
        "{} headers: {names_seen} object-like macros, {valued} with a kmx value checked by the compiler \
         ({newer} through sizeof, offsetof or builtins), \
         {skipped} values not expressible as a literal or outside a header context, \
         {unverified} not checkable in gcc's context, {uncompilable} contexts with errors inside headers, {} failures",
        headers.len(),
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "{}",
        failures[..failures.len().min(30)].join("\n")
    );
}
