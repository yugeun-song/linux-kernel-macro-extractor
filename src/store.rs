use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::{xxh3_64_with_seed, xxh3_128};

use crate::tree::{HeadInfo, KernelVersion, StatusEntry};

pub const FORMAT_VERSION: u32 = 3;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectId(pub String);

const IDENTITY: &str = "identity";
const ID_PROBES: u64 = 64;

impl ProjectId {
    fn probe(key: &[u8], seed: u64) -> ProjectId {
        ProjectId(format!("{:016x}", xxh3_64_with_seed(key, seed)))
    }

    pub fn valid(text: &str) -> bool {
        text.len() == 16 && text.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
    }

    pub fn label<'a, 'b>(&'a self, all: impl IntoIterator<Item = &'b ProjectId>) -> &'a str {
        let mut n = 12;
        for other in all.into_iter().filter(|o| o.0 != self.0) {
            let common = self
                .0
                .bytes()
                .zip(other.0.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            n = n.max(common + 1);
        }
        &self.0[..n.min(self.0.len())]
    }
}

fn identity(srctree: &Path, objtree: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut key = srctree.as_os_str().as_bytes().to_vec();
    key.push(0);
    key.extend_from_slice(objtree.as_os_str().as_bytes());
    key
}

pub fn abbrev(hex: &str) -> &str {
    &hex[..12.min(hex.len())]
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigInfo {
    pub path: PathBuf,
    pub hash: String,
    pub arch: String,
    pub compiler: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IndexStats {
    pub files: u64,
    pub source_files: u64,
    pub definitions: u64,
    pub names: u64,
    pub source_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub kmx_version: String,
    pub id: ProjectId,
    pub srctree: PathBuf,
    pub objtree: PathBuf,
    pub kernel: KernelVersion,
    pub head: HeadInfo,
    #[serde(default)]
    pub content: Option<String>,
    pub dirty: Vec<StatusEntry>,
    pub config: ConfigInfo,
    pub stats: IndexStats,
    pub created: u64,
    pub refreshed: Option<u64>,
    pub generation: u32,
    pub base: String,
    pub overlay: Option<String>,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn hash_hex(data: &[u8]) -> String {
    format!("{:032x}", xxh3_128(data))
}

pub fn file_identity(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    match fs::metadata(path) {
        Ok(m) => format!(
            "{:x}-{:x}",
            m.len(),
            m.mtime() * 1_000_000_000 + m.mtime_nsec()
        ),
        Err(_) => "absent".to_string(),
    }
}

pub fn build_tag() -> &'static str {
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TAG.get_or_init(|| hash_hex(build_id().as_bytes())[..12].to_string())
}

pub fn tagged_dir(parent: &Path) -> PathBuf {
    const KEEP_OTHERS: usize = 1;
    let dir = parent.join(build_tag());
    if fs::create_dir_all(&dir).is_ok()
        && let Ok(f) = File::open(&dir)
    {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
    let Ok(rd) = fs::read_dir(parent) else {
        return dir;
    };
    let mut others = Vec::new();
    for e in rd.flatten().filter(|e| e.file_name() != build_tag()) {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            let used = e.metadata().and_then(|m| m.modified()).ok();
            others.push((used, e.path()));
        } else {
            let _ = fs::remove_file(e.path());
        }
    }
    others.sort_by_key(|o| std::cmp::Reverse(o.0));
    for (_, p) in others.into_iter().skip(KEEP_OTHERS) {
        let _ = fs::remove_dir_all(p);
    }
    dir
}

pub fn slot_lock(dir: &Path, name: &str) -> Option<File> {
    fs::create_dir_all(dir).ok()?;
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(name))
        .ok()?;
    f.lock().ok()?;
    Some(f)
}

pub fn write_slot(dir: &Path, prefix: &str, name: &str, data: &[u8]) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(name);
    write_atomic(&path, data)?;
    for e in fs::read_dir(dir)?.flatten() {
        let other = e.file_name();
        let other = other.to_string_lossy();
        if other.starts_with(prefix) && other != name && !other.contains(".tmp-") {
            let _ = fs::remove_file(e.path());
        }
    }
    sweep_temps(dir);
    Ok(path)
}

pub fn sweep_temps(dir: &Path) {
    const STALE: std::time::Duration = std::time::Duration::from_secs(3600);
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if !e.file_name().to_string_lossy().contains(".tmp-") {
            continue;
        }
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > STALE);
        if stale {
            let _ = fs::remove_file(e.path());
        }
    }
}

pub fn build_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| format!("{}+{}", env!("CARGO_PKG_VERSION"), env!("KMX_SOURCE_HASH")))
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open() -> io::Result<Store> {
        let root = match std::env::var_os("KMX_HOME").filter(|v| !v.is_empty()) {
            Some(p) => PathBuf::from(p),
            None => match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
                Some(x) => PathBuf::from(x).join("kmx"),
                None => std::env::home_dir()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "cannot locate the home directory")
                    })?
                    .join(".local/share/kmx"),
            },
        };
        fs::create_dir_all(root.join("projects"))?;
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn project(&self, id: &ProjectId) -> Project {
        Project {
            dir: self.root.join("projects").join(&id.0),
            id: id.clone(),
        }
    }

    pub fn locate(&self, srctree: &Path, objtree: &Path, claim: bool) -> io::Result<Project> {
        let key = identity(srctree, objtree);
        for seed in 0..ID_PROBES {
            let project = self.project(&ProjectId::probe(&key, seed));
            match project.holder()? {
                Some(holder) if holder == key => {
                    if claim && !project.dir.join(IDENTITY).exists() {
                        write_atomic(&project.dir.join(IDENTITY), &key)?;
                    }
                    return Ok(project);
                }
                Some(_) => continue,
                None if !claim => return Ok(project),
                None => {
                    if project.claim(&key)? {
                        return Ok(project);
                    }
                }
            }
        }
        Err(io::Error::other(format!(
            "all {ID_PROBES} candidate project IDs for {} are taken",
            srctree.display()
        )))
    }

    pub fn projects(&self) -> io::Result<Vec<Project>> {
        let mut out = Vec::new();
        for e in fs::read_dir(self.root.join("projects"))? {
            let e = e?;
            let name = e.file_name();
            let Some(name) = name.to_str().filter(|n| ProjectId::valid(n)) else {
                continue;
            };
            if e.file_type()?.is_dir() {
                out.push(self.project(&ProjectId(name.to_string())));
            }
        }
        out.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        Ok(out)
    }
}

pub struct Project {
    dir: PathBuf,
    id: ProjectId,
}

pub struct Generation {
    pub number: u32,
    pub dir: PathBuf,
    staging: PathBuf,
}

impl Generation {
    pub fn path(&self, name: &str) -> PathBuf {
        self.staging.join(name)
    }
}

impl Project {
    pub fn id(&self) -> &ProjectId {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn exists(&self) -> bool {
        self.dir.join("current").exists()
    }

    fn holder(&self) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.dir.join(IDENTITY)) {
            Ok(key) => Ok(Some(key)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(self.dir.exists().then(|| {
                self.manifest()
                    .map(|m| identity(&m.srctree, &m.objtree))
                    .unwrap_or_default()
            })),
            Err(e) => Err(e),
        }
    }

    fn claim(&self, key: &[u8]) -> io::Result<bool> {
        let parent = self.dir.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let staging = parent.join(format!(".claim-{}-{}", self.id.0, std::process::id()));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir(&staging)?;
        let placed =
            fs::write(staging.join(IDENTITY), key).and_then(|()| fs::rename(&staging, &self.dir));
        let _ = fs::remove_dir_all(&staging);
        match placed {
            Ok(()) => Ok(true),
            Err(_) if self.dir.exists() => Ok(self.holder()?.is_some_and(|h| h == key)),
            Err(e) => Err(e),
        }
    }

    pub fn lock(&self) -> io::Result<File> {
        fs::create_dir_all(&self.dir)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("lock"))?;
        f.lock()?;
        Ok(f)
    }

    pub fn current(&self) -> Option<PathBuf> {
        let target = fs::read_link(self.dir.join("current")).ok()?;
        let full = self.dir.join(target);
        full.is_dir().then_some(full)
    }

    pub fn manifest(&self) -> Option<Manifest> {
        let gen_dir = self.current()?;
        let text = fs::read(gen_dir.join("manifest.json")).ok()?;
        serde_json::from_slice(&text).ok()
    }

    fn generation_numbers(&self) -> Vec<u32> {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        rd.flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_str()?;
                name.strip_prefix("gen-")?.split('.').next()?.parse().ok()
            })
            .collect()
    }

    pub fn begin_generation(&self) -> io::Result<Generation> {
        fs::create_dir_all(&self.dir)?;
        let number = self.generation_numbers().into_iter().max().unwrap_or(0) + 1;
        let dir = self.dir.join(format!("gen-{number:06}"));
        let staging = self
            .dir
            .join(format!("gen-{number:06}.staging-{}", std::process::id()));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(staging.join("memo"))?;
        Ok(Generation {
            number,
            dir,
            staging,
        })
    }

    pub fn publish(&self, generation: Generation) -> io::Result<PathBuf> {
        fs::rename(&generation.staging, &generation.dir)?;
        let link_tmp = self.dir.join(format!("current.tmp-{}", std::process::id()));
        let _ = fs::remove_file(&link_tmp);
        let rel = generation
            .dir
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| generation.dir.clone());
        std::os::unix::fs::symlink(&rel, &link_tmp)?;
        fs::rename(&link_tmp, self.dir.join("current"))?;
        self.remove_stale_generations(&generation.dir);
        Ok(generation.dir)
    }

    fn remove_stale_generations(&self, keep: &Path) {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let is_gen = e
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("gen-"));
            if is_gen && p != keep {
                let _ = fs::remove_dir_all(&p);
            }
        }
    }

    pub fn remove(&self) -> io::Result<()> {
        let _guard = self.lock()?;
        let gens: Vec<PathBuf> = fs::read_dir(&self.dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()) != Some("lock"))
            .collect();
        for p in gens {
            if p.is_dir() && !p.is_symlink() {
                fs::remove_dir_all(&p)?;
            } else {
                fs::remove_file(&p)?;
            }
        }
        drop(_guard);
        fs::remove_file(self.dir.join("lock"))?;
        fs::remove_dir(&self.dir)
    }

    pub fn size_bytes(&self) -> u64 {
        fn walk(p: &Path) -> u64 {
            let Ok(rd) = fs::read_dir(p) else { return 0 };
            rd.flatten()
                .map(|e| match e.file_type() {
                    Ok(t) if t.is_dir() => walk(&e.path()),
                    Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
                    _ => 0,
                })
                .sum()
        }
        walk(&self.dir)
    }
}

pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}-{seq}", std::process::id()));
    let tmp = path.with_file_name(name);
    let written = File::create(&tmp)
        .and_then(|mut f| f.write_all(data))
        .and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

pub struct Memo {
    dir: PathBuf,
    salt: String,
}

impl Memo {
    pub fn new(generation_dir: &Path) -> Memo {
        Memo {
            dir: generation_dir.join("memo").join(build_tag()),
            salt: String::new(),
        }
    }

    pub fn salted(mut self, salt: &str) -> Memo {
        self.salt = salt.to_string();
        self
    }

    fn file(&self, key: &str) -> PathBuf {
        let name = if self.salt.is_empty() {
            key.to_string()
        } else {
            hash_hex(format!("{}\u{1f}{key}", self.salt).as_bytes())
        };
        self.dir.join(format!("{name}.json"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn key(parts: &[&str]) -> String {
        hash_hex(parts.join("\u{1f}").as_bytes())
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        fs::read(self.file(key)).ok()
    }

    pub fn put(&self, key: &str, data: &[u8]) {
        static PRUNED: std::sync::Once = std::sync::Once::new();
        if let Some(parent) = self.dir.parent() {
            PRUNED.call_once(|| {
                tagged_dir(parent);
                sweep_temps(&self.dir);
            });
        }
        if fs::create_dir_all(&self.dir).is_ok() {
            let _ = write_atomic(&self.file(key), data);
        }
    }

    pub fn count(&self) -> usize {
        fs::read_dir(&self.dir).map(|rd| rd.count()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("kmx-store-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("projects")).unwrap();
        (Store { root: dir.clone() }, dir)
    }

    #[test]
    fn project_ids_are_stable_and_distinct() {
        let (store, dir) = temp_store("ids");
        let (ka, kb) = (Path::new("/k/a/kernel"), Path::new("/k/b/kernel"));
        let a = store.locate(ka, ka, false).unwrap();
        let b = store.locate(kb, kb, false).unwrap();
        assert_ne!(a.id(), b.id());
        assert_eq!(a.id(), store.locate(ka, ka, true).unwrap().id());
        assert_eq!(a.id().0.len(), 16);
        assert_eq!(
            a.id().0,
            format!(
                "{:016x}",
                xxhash_rust::xxh3::xxh3_64(b"/k/a/kernel\0/k/a/kernel")
            )
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_taken_id_moves_the_next_tree_to_another_slot() {
        let (store, dir) = temp_store("collide");
        let k = Path::new("/k/a/kernel");
        let first = store.locate(k, k, false).unwrap();
        fs::create_dir_all(first.dir()).unwrap();
        fs::write(first.dir().join(IDENTITY), b"/k/other\0/k/other").unwrap();
        let moved = store.locate(k, k, true).unwrap();
        assert_ne!(moved.id(), first.id());
        assert_eq!(
            fs::read(moved.dir().join(IDENTITY)).unwrap(),
            identity(k, k)
        );
        assert_eq!(store.locate(k, k, false).unwrap().id(), moved.id());
        assert_eq!(store.locate(k, k, true).unwrap().id(), moved.id());
        let listed: Vec<ProjectId> = store
            .projects()
            .unwrap()
            .iter()
            .map(|p| p.id().clone())
            .collect();
        assert_eq!(listed.len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn labels_grow_past_twelve_digits_only_when_needed() {
        let ids = [
            ProjectId("0123456789abcdef".into()),
            ProjectId("0123456789abcd00".into()),
            ProjectId("fedcba9876543210".into()),
        ];
        assert_eq!(ids[0].label(&ids), "0123456789abcde");
        assert_eq!(ids[1].label(&ids), "0123456789abcd0");
        assert_eq!(ids[2].label(&ids), "fedcba987654");
        assert_eq!(ids[2].label([]), "fedcba987654");
    }

    #[test]
    fn generations_swap_atomically() {
        let (store, dir) = temp_store("gen");
        let p = store.project(&ProjectId("abc".into()));
        let _lock = p.lock().unwrap();
        let g1 = p.begin_generation().unwrap();
        fs::write(g1.path("manifest.json"), b"{}").unwrap();
        let d1 = p.publish(g1).unwrap();
        assert_eq!(p.current().unwrap(), d1);
        let g2 = p.begin_generation().unwrap();
        assert_eq!(g2.number, 2);
        fs::write(g2.path("manifest.json"), b"{}").unwrap();
        let d2 = p.publish(g2).unwrap();
        assert_eq!(p.current().unwrap(), d2);
        assert!(!d1.exists());
        drop(_lock);
        p.remove().unwrap();
        assert!(!p.dir().exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn memo_roundtrip() {
        let (_store, dir) = temp_store("memo");
        let m = Memo::new(&dir);
        let k = Memo::key(&["show", "PAGE_SIZE"]);
        assert!(m.get(&k).is_none());
        m.put(&k, b"{\"v\":1}");
        assert_eq!(m.get(&k).unwrap(), b"{\"v\":1}");
        assert_eq!(m.count(), 1);
        let before = Memo::new(&dir).salted("vmlinux-1");
        before.put(&k, b"{\"v\":2}");
        assert_eq!(before.get(&k).unwrap(), b"{\"v\":2}");
        assert!(Memo::new(&dir).salted("vmlinux-2").get(&k).is_none());
        assert_eq!(m.get(&k).unwrap(), b"{\"v\":1}");
        let _ = fs::remove_dir_all(dir);
    }
}
