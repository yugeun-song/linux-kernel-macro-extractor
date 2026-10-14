use std::path::PathBuf;
use std::time::Instant;

use kmx::index::Index;
use kmx::index::build::build;
use kmx::index::sources::collect;
use kmx::tree::Layout;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let srctree = PathBuf::from(&args[1]).canonicalize().unwrap();
    let objtree = args
        .get(2)
        .map(|p| PathBuf::from(p).canonicalize().unwrap())
        .unwrap_or(srctree.clone());
    let out = PathBuf::from(args.get(3).cloned().unwrap_or("/dev/null".into()));
    let layout = Layout {
        srctree,
        objtree,
        srcarch: "x86".into(),
    };
    let t = Instant::now();
    let list = collect(&layout).unwrap();
    let t_collect = t.elapsed();
    let built = build(&layout, &list.files, &list.files, 0, &[]);
    let t_build = t.elapsed();
    std::fs::write(&out, &built.bytes).unwrap();
    println!(
        "files={} collect={:?} build={:?} bytes={} stats={:?} failed={}",
        list.files.len(),
        t_collect,
        t_build - t_collect,
        built.bytes.len(),
        built.stats,
        built.failed.len()
    );
    if out.to_str() != Some("/dev/null") {
        let dir = out.parent().unwrap();
        let name = out.file_name().unwrap().to_str().unwrap();
        let t = Instant::now();
        let idx = Index::open(dir, name, None).unwrap();
        let open = t.elapsed();
        for q in [
            "PAGE_SIZE",
            "pr_fmt",
            "FIX_APIC_BASE",
            "enum fixed_addresses",
            "CONFIG_PAGE_SHIFT",
        ] {
            let t = Instant::now();
            let p = idx.lookup(q);
            let dt = t.elapsed();
            let sample: Vec<String> = p
                .iter()
                .take(3)
                .map(|p| {
                    format!(
                        "{}:{} {:?} d{}",
                        idx.file(p.file).unwrap().path,
                        p.line,
                        p.kind,
                        p.depth
                    )
                })
                .collect();
            println!(
                "{q}: {} postings in {:?} (open {:?}) e.g. {:?}",
                p.len(),
                dt,
                open,
                sample
            );
        }
    }
}
