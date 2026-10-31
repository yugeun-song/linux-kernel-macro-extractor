pub mod dwarf;
pub mod encode;

use std::path::{Path, PathBuf};
use std::time::Instant;

pub use encode::{
    DefKind, DefView, DerivedKind, EntityView, EnumeratorView, FuncView, MemberView, NameKind,
    TRef, Table,
};

use crate::index::{FileClass, FileView, Scope};
use crate::store;
use crate::workspace::Workspace;

pub enum Status {
    Ready { modules_skipped: usize },
    NoBuild { objtree: PathBuf },
    Stale { product: PathBuf, input: String },
    Unreadable(String),
}

pub struct Types {
    pub table: Option<Table>,
    pub status: Status,
}

impl Types {
    pub fn note(&self) -> Option<String> {
        let why = match &self.status {
            Status::Ready { .. } => return None,
            Status::NoBuild { objtree } => format!("{} has no vmlinux", where_built(objtree)),
            Status::Stale { product, input } => format!(
                "{} is older than {input}; rebuild the kernel to resolve them",
                product.file_name().map_or_else(
                    || product.display().to_string(),
                    |n| n.to_string_lossy().into_owned()
                )
            ),
            Status::Unreadable(why) => why.clone(),
        };
        Some(format!(
            "sizeof and offsetof of kernel types stay unresolved: {why}"
        ))
    }
}

fn where_built(objtree: &Path) -> &'static str {
    if objtree.join("Kconfig").is_file() {
        "the tree"
    } else {
        "the build directory"
    }
}

fn is_input(f: &FileView<'_>) -> bool {
    match f.class() {
        FileClass::CSource | FileClass::Header | FileClass::Asm => f.scope() == Scope::Kernel,
        FileClass::Makefile => matches!(f.scope(), Scope::Kernel | Scope::Scripts),
        FileClass::DotConfig => true,
        _ => false,
    }
}

pub fn module_paths(objtree: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(objtree.join("modules.order")) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let rel = match l.strip_suffix(".o") {
                Some(stem) => format!("{stem}.ko"),
                None => l.strip_prefix("kernel/").unwrap_or(l).to_string(),
            };
            rel.ends_with(".ko").then(|| objtree.join(rel))
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

struct Product {
    path: PathBuf,
    identity: String,
    mtime_ns: i64,
}

fn product(path: PathBuf) -> Option<Product> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(&path).ok()?;
    m.is_file().then(|| Product {
        identity: format!(
            "{}\u{1f}{:x}\u{1f}{:x}\u{1f}{:x}",
            path.display(),
            m.len(),
            m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            m.ino()
        ),
        mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
        path,
    })
}

impl Workspace {
    pub fn types(&self) -> &Types {
        self.types.get_or_init(|| self.load_types())
    }

    pub fn types_if_loaded(&self) -> Option<&Types> {
        self.types.get()
    }

    fn load_types(&self) -> Types {
        let objtree = &self.layout.objtree;
        let none = |status| Types {
            table: None,
            status,
        };
        let Some(vmlinux) = product(objtree.join("vmlinux")) else {
            return none(Status::NoBuild {
                objtree: objtree.clone(),
            });
        };
        let newest = self
            .index
            .files()
            .filter(is_input)
            .max_by_key(|f| f.stat.mtime_ns)
            .map(|f| (f.stat.mtime_ns, f.path.to_string()));
        if let Some((t, path)) = &newest
            && *t > vmlinux.mtime_ns
        {
            return none(Status::Stale {
                product: vmlinux.path,
                input: path.clone(),
            });
        }
        let floor = newest.map_or(i64::MIN, |(t, _)| t);
        let mut usable = vec![vmlinux];
        let mut modules_skipped = 0;
        for p in module_paths(objtree).into_iter().filter_map(product) {
            if p.mtime_ns >= floor {
                usable.push(p);
            } else {
                modules_skipped += 1;
            }
        }
        let identity: Vec<&str> = usable.iter().map(|p| p.identity.as_str()).collect();
        let name = format!(
            "types-{}.bin",
            store::hash_hex(
                format!("{}\u{1e}{}", store::build_tag(), identity.join("\u{1e}")).as_bytes()
            )
        );
        let dir = self.cache_dir();
        if let Some(table) = Table::open(&dir.join(&name)) {
            return Types {
                table: Some(table),
                status: Status::Ready { modules_skipped },
            };
        }
        let _guard = store::slot_lock(dir, ".types.lock");
        if let Some(table) = Table::open(&dir.join(&name)) {
            return Types {
                table: Some(table),
                status: Status::Ready { modules_skipped },
            };
        }
        let paths: Vec<PathBuf> = usable.into_iter().map(|p| p.path).collect();
        let modules = paths.len() - 1;
        if !self.quiet {
            eprintln!(
                "kmx: reading C types from the DWARF of vmlinux{}",
                if modules > 0 {
                    format!(" and {modules} modules")
                } else {
                    String::new()
                }
            );
        }
        let t = Instant::now();
        let extracted = crate::analysis::build_graph::outside_rayon(|| dwarf::extract(&paths));
        let mut meta = encode::Meta {
            address_size: extracted.raw.address_size,
            units: extracted.raw.units,
            ..encode::Meta::default()
        };
        for p in &extracted.products {
            meta.failed_units += u64::from(p.failed_units);
            meta.ignored_relocations += u64::from(p.ignored_relocations);
            let shown = p
                .path
                .strip_prefix(objtree)
                .unwrap_or(&p.path)
                .display()
                .to_string();
            match &p.error {
                Some(e) => meta.failed_products.push((shown, e.clone())),
                None => meta.products.push(shown),
            }
        }
        if extracted
            .products
            .first()
            .is_some_and(|p| p.error.is_some())
        {
            let why = meta
                .failed_products
                .first()
                .map(|(p, e)| format!("{p}: {e}"))
                .unwrap_or_default();
            return none(Status::Unreadable(why));
        }
        meta.conflicts = extracted
            .raw
            .defs
            .iter()
            .filter(|(_, d)| d.conflicts != 0)
            .count() as u64;
        meta.elapsed_ms = t.elapsed().as_millis() as u64;
        let bytes = encode::encode(&extracted.raw, &meta);
        if !self.quiet {
            eprintln!(
                "kmx: read {} type definitions from {} compile units in {:.1}s",
                extracted.raw.defs.len(),
                meta.units,
                t.elapsed().as_secs_f64()
            );
        }
        let _ = store::write_slot(dir, "types-", &name, &bytes);
        match Table::from_vec(bytes) {
            Some(table) => Types {
                table: Some(table),
                status: Status::Ready { modules_skipped },
            },
            None => none(Status::Unreadable(
                "the type table did not decode".to_string(),
            )),
        }
    }
}
