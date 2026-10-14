use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if p.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            walk(&p, out);
        } else if ft.is_file()
            && let Some(x) = p.extension().and_then(|x| x.to_str())
            && matches!(x, "c" | "h" | "S")
        {
            out.push(p);
        }
    }
}

fn scan_one(p: &Path) -> (u64, u64) {
    let Ok(buf) = std::fs::read(p) else {
        return (0, 0);
    };
    let mut v = Vec::new();
    kmx::syntax::scan::scan(&buf, &mut v);
    let mut n = 0u64;
    for d in &v {
        let h = kmx::syntax::scan::parse_head(&buf, *d);
        if h.name.is_some() {
            n += 1;
        }
    }
    (buf.len() as u64, n)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut files = Vec::new();
    let mut threads = 1usize;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-j" => {
                threads = args[i + 1].parse().unwrap();
                i += 2;
            }
            "--list" => {
                let s = std::fs::read_to_string(&args[i + 1]).unwrap();
                files.extend(s.lines().filter(|l| !l.is_empty()).map(PathBuf::from));
                i += 2;
            }
            d => {
                walk(Path::new(d), &mut files);
                i += 1;
            }
        }
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();
    for round in 0..3 {
        let t = Instant::now();
        let (bytes, names) = pool.install(|| {
            files
                .par_iter()
                .map(|p| scan_one(p))
                .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
        });
        let dt = t.elapsed().as_secs_f64();
        println!(
            "round {round}: files={} bytes={} named_directives={} time={:.3}s throughput={:.0} MB/s threads={threads}",
            files.len(),
            bytes,
            names,
            dt,
            bytes as f64 / dt / 1e6
        );
    }
}
