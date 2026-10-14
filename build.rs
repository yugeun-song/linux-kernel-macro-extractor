use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash ^= u64::from(b);
        *hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
}

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let mut inputs = Vec::new();
    for dir in ["src", "data"] {
        files(&root.join(dir), &mut inputs);
        println!("cargo:rerun-if-changed={dir}");
    }
    for file in ["Cargo.toml", "Cargo.lock", "build.rs"] {
        inputs.push(root.join(file));
        println!("cargo:rerun-if-changed={file}");
    }
    inputs.sort();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for p in &inputs {
        let rel = p.strip_prefix(&root).unwrap_or(p);
        fnv(&mut hash, rel.to_string_lossy().as_bytes());
        fnv(&mut hash, &[0]);
        fnv(&mut hash, &std::fs::read(p).unwrap_or_default());
        fnv(&mut hash, &[0]);
    }
    println!("cargo:rustc-env=KMX_SOURCE_HASH={hash:016x}");
}
