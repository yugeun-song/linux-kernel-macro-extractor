use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use kmx::types::{DefKind, NameKind, Table};
use kmx::workspace::{OpenOptions, Workspace};

fn open(tree: &std::ffi::OsStr) -> Workspace {
    let opts = OpenOptions {
        build_dir: std::env::var_os("KMX_TYPES_BUILD").map(PathBuf::from),
        refresh: true,
        compiler: None,
        rebuild: false,
    };
    Workspace::open(&PathBuf::from(tree), &opts, &mut |m| eprintln!("{m}"))
        .unwrap_or_else(|e| panic!("{e}"))
}

fn unique_size(table: &Table, name: &str, kinds: &[DefKind]) -> Option<u64> {
    let mut size = None;
    for (k, i) in table.named(name) {
        let NameKind::Def(kind) = k else { continue };
        if !kinds.contains(&kind) {
            continue;
        }
        let d = table.def(i)?;
        if !d.layout_agrees() {
            return None;
        }
        match (size, d.size) {
            (None, s) => size = Some(s?),
            (Some(a), Some(b)) if a == b => {}
            _ => return None,
        }
    }
    size
}

fn gdb(vmlinux: &std::path::Path, cmds: &[String]) -> Vec<Option<String>> {
    let dir = std::env::temp_dir().join(format!("kmx-types-oracle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let list = dir.join("exprs.txt");
    std::fs::write(&list, cmds.join("\n")).unwrap();
    let script = dir.join("eval.py");
    std::fs::write(
        &script,
        format!(
            "import gdb\nfor i, e in enumerate(open({:?}).read().split('\\n')):\n    try:\n        print('@%d %d' % (i, int(gdb.parse_and_eval(e))))\n    except Exception:\n        pass\n",
            list.display().to_string()
        ),
    )
    .unwrap();
    let out = Command::new("gdb")
        .args(["-nx", "-batch", "-x"])
        .arg(&script)
        .arg(vmlinux)
        .output()
        .expect("gdb");
    let _ = std::fs::remove_dir_all(&dir);
    let mut got = vec![None; cmds.len()];
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((i, v)) = l.strip_prefix('@').and_then(|r| r.split_once(' '))
            && let Ok(i) = i.parse::<usize>()
            && i < got.len()
        {
            got[i] = Some(v.to_string());
        }
    }
    got
}

#[test]
fn type_table_matches_pahole_and_gdb() {
    let Some(tree) = std::env::var_os("KMX_TYPES_ORACLE") else {
        eprintln!("KMX_TYPES_ORACLE is not set; skipping the DWARF type oracle");
        return;
    };
    let ws = open(&tree);
    let types = ws.types();
    if let Some(n) = types.note() {
        panic!("{n}");
    }
    let table = types.table.as_ref().expect("a type table");
    let m = &table.meta;
    eprintln!(
        "{} defs, {} units ({} failed), {} products ({} failed), {} conflicts, {} ignored relocations, {} ms",
        table.def_count(),
        m.units,
        m.failed_units,
        m.products.len(),
        m.failed_products.len(),
        m.conflicts,
        m.ignored_relocations,
        m.elapsed_ms
    );
    assert!(m.failed_products.is_empty(), "{:?}", m.failed_products);
    assert_eq!(m.failed_units, 0);
    let mut shown = 0;
    for i in 0..table.def_count() as u32 {
        let Some(d) = table.def(i).filter(|d| d.conflicts != 0) else {
            continue;
        };
        if shown < 12 {
            eprintln!(
                "  conflict {:#x}: {} {} {}:{}",
                d.conflicts,
                d.kind.keyword(),
                d.name,
                d.file,
                d.line
            );
        }
        shown += 1;
    }
    let vmlinux = ws.layout.objtree.join("vmlinux");
    let mut products = vec![vmlinux.clone()];
    products.extend(
        kmx::types::module_paths(&ws.layout.objtree)
            .into_iter()
            .filter(|p| m.products.iter().any(|q| p.ends_with(q))),
    );
    let jobs: usize = std::env::var("KMX_ORACLE_JOBS")
        .ok()
        .and_then(|j| j.parse().ok())
        .unwrap_or(1);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let sizes = std::sync::Mutex::new(BTreeMap::<String, std::collections::BTreeSet<u64>>::new());
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(p) = products.get(i) else { break };
                    let out = Command::new("pahole")
                        .args(["-F", "dwarf", "--sizes"])
                        .arg(p)
                        .output()
                        .expect("pahole");
                    let mut local = Vec::new();
                    for line in String::from_utf8_lossy(&out.stdout).lines() {
                        let mut f = line.split('\t');
                        if let (Some(name), Some(size)) = (f.next(), f.next())
                            && let Ok(size) = size.parse::<u64>()
                        {
                            local.push((name.to_string(), size));
                        }
                    }
                    let mut all = sizes.lock().unwrap();
                    for (n, sz) in local {
                        all.entry(n).or_default().insert(sz);
                    }
                }
            });
        }
    });
    let mut compared = 0;
    let mut differ = Vec::new();
    let mut missing = Vec::new();
    for (name, set) in sizes.into_inner().unwrap() {
        if table.named(&name).next().is_none() {
            missing.push(name);
            continue;
        }
        let ours = unique_size(table, &name, &[DefKind::Struct, DefKind::Union]);
        match (ours, set.len()) {
            (Some(o), 1) => {
                compared += 1;
                if !set.contains(&o) {
                    differ.push(format!("{name}: kmx {o}, pahole {set:?}"));
                }
            }
            (Some(o), _) => differ.push(format!("{name}: kmx {o} only, pahole {set:?}")),
            (None, _) => {}
        }
    }
    eprintln!(
        "pahole over {} products: {compared} struct/union sizes compared, {} differ, {} missing",
        products.len(),
        differ.len(),
        missing.len()
    );
    let mut cmds: Vec<String> = Vec::new();
    let mut expect: Vec<(String, String)> = Vec::new();
    let mut picked: BTreeMap<String, ()> = BTreeMap::new();
    let step = (table.def_count() / 1500).max(1);
    for i in (0..table.def_count() as u32).step_by(step) {
        let Some(d) = table.def(i) else { continue };
        if d.name.is_empty() || !d.types_agree() || picked.contains_key(d.name) {
            continue;
        }
        let kinds: &[DefKind] = match d.kind {
            DefKind::Typedef => &[DefKind::Typedef],
            k => &[k],
        };
        let others = table
            .named(d.name)
            .filter(|(k, _)| matches!(k, NameKind::Def(x) if kinds.contains(x)))
            .count();
        if others != 1 {
            continue;
        }
        picked.insert(d.name.to_string(), ());
        let spelled = match d.kind {
            DefKind::Typedef => d.name.to_string(),
            k => format!("{} {}", k.keyword(), d.name),
        };
        if let Some(s) = d.size {
            cmds.push(format!("sizeof({spelled})"));
            expect.push((format!("sizeof({spelled})"), s.to_string()));
        }
        if d.kind == DefKind::Struct {
            for mv in table.members(&d).take(4) {
                if mv.name.is_empty() || mv.bit_size != 0 {
                    continue;
                }
                let (Some(off), Some(sz)) = (mv.offset, mv.size) else {
                    continue;
                };
                cmds.push(format!("(unsigned long)&(({spelled} *)0)->{}", mv.name));
                cmds.push(format!("sizeof((({spelled} *)0)->{})", mv.name));
                expect.push((format!("offsetof({spelled}, {})", mv.name), off.to_string()));
                expect.push((
                    format!("sizeof(({spelled} *)0)->{}", mv.name),
                    sz.to_string(),
                ));
            }
        }
        if d.kind == DefKind::Enum {
            for e in table.enumerators(&d).take(3) {
                let alone = table
                    .named(e.name)
                    .filter(|(k, _)| *k == NameKind::Enumerator)
                    .count()
                    == 1;
                if alone {
                    cmds.push(e.name.to_string());
                    expect.push((e.name.to_string(), e.value.to_string()));
                }
            }
        }
    }
    let got = gdb(&vmlinux, &cmds);
    let mut gdb_differ = Vec::new();
    let mut gdb_compared = 0;
    for ((what, want), have) in expect.iter().zip(&got) {
        let Some(have) = have else { continue };
        gdb_compared += 1;
        if want != have {
            gdb_differ.push(format!("{what}: kmx {want}, gdb {have}"));
        }
    }
    eprintln!(
        "gdb: {gdb_compared} of {} sizes, offsets and enumerator values compared, {} differ",
        expect.len(),
        gdb_differ.len()
    );
    assert!(
        gdb_compared * 3 >= expect.len(),
        "gdb answered only {gdb_compared} of {}",
        expect.len()
    );
    assert!(
        differ.is_empty(),
        "pahole differs: {:?}",
        &differ[..differ.len().min(20)]
    );
    assert!(
        gdb_differ.is_empty(),
        "gdb differs: {:?}",
        &gdb_differ[..gdb_differ.len().min(20)]
    );
    assert!(
        missing.len() * 1000 <= compared,
        "missing from kmx: {:?}",
        &missing[..missing.len().min(20)]
    );
}
