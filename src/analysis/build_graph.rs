mod scan;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rustc_hash::FxHashMap;

use crate::store::{hash_hex, slot_lock, write_slot};
use crate::workspace::Workspace;

const MAGIC: &[u8; 8] = b"KMXBLD\0\x02";

#[derive(Clone, Debug)]
pub struct Member {
    pub obj: String,
    pub source: PathBuf,
    pub module: bool,
    start: u32,
    len: u32,
}

#[derive(Debug)]
enum Deps {
    Owned(Vec<u32>),
    Mapped {
        map: memmap2::Mmap,
        at: usize,
        len: usize,
    },
}

impl Default for Deps {
    fn default() -> Self {
        Deps::Owned(Vec::new())
    }
}

impl Deps {
    fn mapped(map: memmap2::Mmap, at: usize, len: usize) -> Deps {
        let bytes = &map[at..at + len * 4];
        let aligned = bytes.as_ptr().align_offset(std::mem::align_of::<u32>()) == 0;
        if cfg!(target_endian = "little") && aligned {
            return Deps::Mapped { map, at, len };
        }
        Deps::Owned(
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|x| u32::from_le_bytes(*x))
                .collect(),
        )
    }

    fn all(&self) -> &[u32] {
        match self {
            Deps::Owned(v) => v,
            Deps::Mapped { map, at, len } => {
                let bytes = &map[*at..*at + *len * 4];
                unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u32>(), *len) }
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct BuildGraph {
    pub members: Vec<Member>,
    pub origin: String,
    pub stale: bool,
    paths: Vec<PathBuf>,
    ids: FxHashMap<PathBuf, u32>,
    deps: Deps,
    usage: Vec<u32>,
    reverse: OnceLock<Vec<Vec<u32>>>,
    by_source: OnceLock<FxHashMap<PathBuf, Vec<u32>>>,
}

impl BuildGraph {
    pub fn id_of(&self, file: &Path) -> Option<u32> {
        self.ids.get(file).copied()
    }

    pub fn includes(&self, member: usize, id: u32) -> bool {
        self.deps_of(member).binary_search(&id).is_ok()
    }

    pub fn users(&self, files: &[&Path]) -> Vec<Vec<usize>> {
        const REVERSE_FROM: usize = 32;
        let by_source = self.by_source.get_or_init(|| {
            let mut map: FxHashMap<PathBuf, Vec<u32>> = FxHashMap::default();
            for (i, m) in self.members.iter().enumerate() {
                map.entry(m.source.clone()).or_default().push(i as u32);
            }
            map
        });
        let reverse = (files.len() >= REVERSE_FROM).then(|| {
            self.reverse.get_or_init(|| {
                let mut r = vec![Vec::new(); self.paths.len()];
                for m in 0..self.members.len() {
                    for &d in self.deps_of(m) {
                        if let Some(users) = r.get_mut(d as usize) {
                            users.push(m as u32);
                        }
                    }
                }
                r
            })
        });
        files
            .iter()
            .map(|file| {
                let mut out: Vec<usize> = by_source
                    .get(*file)
                    .map(|v| v.iter().map(|&i| i as usize).collect())
                    .unwrap_or_default();
                if let Some(id) = self.id_of(file) {
                    match reverse {
                        Some(r) => out.extend(r[id as usize].iter().map(|&i| i as usize)),
                        None => {
                            out.extend((0..self.members.len()).filter(|&m| self.includes(m, id)))
                        }
                    }
                }
                out.sort_unstable();
                out.dedup();
                out
            })
            .collect()
    }

    pub fn usage(&self, file: &Path) -> u32 {
        self.ids.get(file).map_or(0, |&id| self.usage[id as usize])
    }

    fn deps_of(&self, member: usize) -> &[u32] {
        let m = &self.members[member];
        self.deps
            .all()
            .get(m.start as usize..(m.start + m.len) as usize)
            .unwrap_or_default()
    }

    fn assemble(
        members: Vec<(String, PathBuf, bool, Vec<u32>)>,
        paths: Vec<PathBuf>,
        origin: String,
        stale: bool,
    ) -> BuildGraph {
        let mut deps = Vec::new();
        let mut usage = vec![0u32; paths.len()];
        let members = members
            .into_iter()
            .map(|(obj, source, module, ids)| {
                let start = deps.len() as u32;
                for &id in &ids {
                    usage[id as usize] += 1;
                }
                deps.extend_from_slice(&ids);
                Member {
                    obj,
                    source,
                    module,
                    start,
                    len: ids.len() as u32,
                }
            })
            .collect();
        BuildGraph::from_parts(members, paths, Deps::Owned(deps), usage, origin, stale)
    }

    fn from_parts(
        members: Vec<Member>,
        paths: Vec<PathBuf>,
        deps: Deps,
        usage: Vec<u32>,
        origin: String,
        stale: bool,
    ) -> BuildGraph {
        let mut ids = FxHashMap::with_capacity_and_hasher(paths.len(), Default::default());
        for (i, p) in paths.iter().enumerate() {
            ids.insert(p.clone(), i as u32);
        }
        BuildGraph {
            members,
            origin,
            stale,
            paths,
            ids,
            deps,
            usage,
            reverse: OnceLock::new(),
            by_source: OnceLock::new(),
        }
    }

    fn encode(&self) -> Vec<u8> {
        fn put_str(out: &mut Vec<u8>, s: &str) {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        let deps = self.deps.all();
        let mut out = Vec::with_capacity(deps.len() * 4 + self.paths.len() * 72);
        out.extend_from_slice(MAGIC);
        out.push(u8::from(self.stale));
        put_str(&mut out, &self.origin);
        out.extend_from_slice(&(self.paths.len() as u32).to_le_bytes());
        for (p, n) in self.paths.iter().zip(&self.usage) {
            put_str(&mut out, &p.to_string_lossy());
            out.extend_from_slice(&n.to_le_bytes());
        }
        out.extend_from_slice(&(self.members.len() as u32).to_le_bytes());
        for m in &self.members {
            put_str(&mut out, &m.obj);
            put_str(&mut out, &m.source.to_string_lossy());
            out.push(u8::from(m.module));
            out.extend_from_slice(&m.len.to_le_bytes());
        }
        out.extend_from_slice(&(deps.len() as u32).to_le_bytes());
        out.resize(out.len().next_multiple_of(4), 0);
        for d in deps {
            out.extend_from_slice(&d.to_le_bytes());
        }
        out
    }

    fn decode(map: memmap2::Mmap) -> Option<BuildGraph> {
        let mut c = Cursor { b: &map, at: 0 };
        if c.take(8)? != MAGIC {
            return None;
        }
        let stale = c.u8()? != 0;
        let origin = c.str()?.to_string();
        let n_paths = c.u32()? as usize;
        let mut paths = Vec::with_capacity(n_paths);
        let mut usage = Vec::with_capacity(n_paths);
        for _ in 0..n_paths {
            paths.push(PathBuf::from(c.str()?));
            usage.push(c.u32()?);
        }
        let n_members = c.u32()? as usize;
        let mut members = Vec::with_capacity(n_members);
        let mut start = 0u32;
        for _ in 0..n_members {
            let obj = c.str()?.to_string();
            let source = PathBuf::from(c.str()?);
            let module = c.u8()? != 0;
            let len = c.u32()?;
            members.push(Member {
                obj,
                source,
                module,
                start,
                len,
            });
            start = start.checked_add(len)?;
        }
        let n_deps = c.u32()?;
        if n_deps != start {
            return None;
        }
        c.at = c.at.next_multiple_of(4);
        let at = c.at;
        c.take((n_deps as usize).checked_mul(4)?)?;
        if c.at != map.len() {
            return None;
        }
        let deps = Deps::mapped(map, at, n_deps as usize);
        Some(BuildGraph::from_parts(
            members, paths, deps, usage, origin, stale,
        ))
    }
}

struct Cursor<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let s = self.b.get(self.at..end)?;
        self.at = end;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        let s = self.take(4)?;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn str(&mut self) -> Option<&'a str> {
        let n = self.u32()? as usize;
        std::str::from_utf8(self.take(n)?).ok()
    }
}

impl Workspace {
    pub fn build_graph(&self) -> Option<&BuildGraph> {
        self.build_graph
            .get_or_init(|| {
                let name = format!("build-{}.bin", hash_hex(self.build_identity().as_bytes()));
                let dir = self.cache_dir();
                let load = || {
                    let file = std::fs::File::open(dir.join(&name)).ok()?;
                    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
                    BuildGraph::decode(map)
                };
                if let Some(g) = load() {
                    return Some(g);
                }
                let _guard = slot_lock(dir, ".build.lock");
                if let Some(g) = load() {
                    return Some(g);
                }
                let graph = outside_rayon(|| BuildGraph::scan(&self.layout, &self.config))?;
                let _ = write_slot(dir, "build-", &name, &graph.encode());
                Some(graph)
            })
            .as_ref()
    }
}

fn outside_rayon<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    if rayon::current_thread_index().is_none() {
        return f();
    }
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    let pool = POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(rayon::current_num_threads())
            .thread_name(|i| format!("kmx-scan-{i}"))
            .build()
            .expect("a thread pool for the build graph scan")
    });
    std::thread::scope(|s| match s.spawn(|| pool.install(f)).join() {
        Ok(v) => v,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Layout;
    use crate::tree::dot_config::DotConfig;

    #[test]
    fn graph_of_a_kernel_without_vmlinux_a() {
        let dir = std::env::temp_dir().join(format!("kmx-oldgraph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let put = |rel: &str, text: &[u8]| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        let unit = |obj: &str| {
            let (d, f) = obj.rsplit_once('/').unwrap();
            let src = obj.replace(".o", ".c");
            let cmd = format!(
                "cmd_{obj} := gcc -c -o {obj} {src}\nsource_{obj} := {src}\n\ndeps_{obj} := \\\n  include/linux/a.h \\\n\n{obj}: $(deps_{obj})\n"
            );
            put(&format!("{d}/.{f}.cmd"), cmd.as_bytes());
            put(obj, b"\x7fELF");
        };
        let link = |obj: &str, parts: &str| {
            let (d, f) = obj.rsplit_once('/').unwrap();
            put(
                &format!("{d}/.{f}.cmd"),
                format!("cmd_{obj} := ld -r -o {obj} {parts}\n").as_bytes(),
            );
            put(obj, b"\x7fELF");
        };
        put(
            "Makefile",
            b"init-y := init/\ncore-y := kernel/\nlibs-y := lib/\n",
        );
        put(
            "arch/x86/Makefile",
            b"head-y := arch/x86/kernel/head_$(BITS).o\ncore-$(CONFIG_KVM) += arch/x86/kvm/\ncore-$(CONFIG_XEN) += arch/x86/xen/\n",
        );
        link("init/built-in.o", "init/main.o init/multi.o");
        unit("init/main.o");
        link("init/multi.o", "init/a.o init/b.o");
        unit("init/a.o");
        unit("init/b.o");
        link(
            "kernel/built-in.o",
            "kernel/fork.o kernel/sched/built-in.o kernel/stub.stub.o",
        );
        unit("kernel/fork.o");
        link("kernel/sched/built-in.o", "kernel/sched/core.o");
        unit("kernel/sched/core.o");
        put(
            "kernel/.stub.stub.o.cmd",
            b"cmd_kernel/stub.stub.o := objcopy --prefix-symbols=__efistub_ kernel/stub.o kernel/stub.stub.o\n",
        );
        put("kernel/stub.stub.o", b"\x7fELF");
        unit("kernel/stub.o");
        unit("arch/x86/kernel/head_64.o");
        link("arch/x86/kvm/built-in.o", "arch/x86/kvm/x86.o");
        unit("arch/x86/kvm/x86.o");
        link("arch/x86/xen/built-in.o", "arch/x86/xen/xen.o");
        unit("arch/x86/xen/xen.o");
        link("lib/built-in.o", "lib/sort.o");
        unit("lib/sort.o");
        unit("lib/string.o");
        let mut ar = b"!<thin>\n".to_vec();
        ar.extend_from_slice(
            format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                "string.o/", "0", "0", "0", "644", 4
            )
            .as_bytes(),
        );
        put("lib/lib.a", &ar);
        put("modules.order", b"kernel/drivers/foo/foo.ko\n");
        link("drivers/foo/foo.o", "drivers/foo/p1.o drivers/foo/p2.o");
        unit("drivers/foo/p1.o");
        unit("drivers/foo/p2.o");
        let layout = Layout {
            srctree: dir.clone(),
            objtree: dir.clone(),
            srcarch: "x86".into(),
        };
        let g = BuildGraph::scan(&layout, &DotConfig::parse("CONFIG_KVM=y\n")).unwrap();
        let mut builtin: Vec<&str> = g
            .members
            .iter()
            .filter(|m| !m.module)
            .map(|m| m.obj.as_str())
            .collect();
        builtin.sort_unstable();
        assert_eq!(
            builtin,
            [
                "arch/x86/kernel/head_64.o",
                "arch/x86/kvm/x86.o",
                "init/a.o",
                "init/b.o",
                "init/main.o",
                "kernel/fork.o",
                "kernel/sched/core.o",
                "kernel/stub.o",
                "lib/sort.o",
                "lib/string.o",
            ]
        );
        let modules: Vec<&str> = g
            .members
            .iter()
            .filter(|m| m.module)
            .map(|m| m.obj.as_str())
            .collect();
        assert_eq!(modules, ["drivers/foo/p1.o", "drivers/foo/p2.o"]);
        assert_eq!(g.origin, "built-in.o + modules.order");
        assert_eq!(g.members[0].source, dir.join("arch/x86/kernel/head_64.c"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn graph_roundtrip() {
        let paths = vec![
            PathBuf::from("/k/include/linux/a.h"),
            PathBuf::from("/k/b.h"),
        ];
        let g = BuildGraph::assemble(
            vec![
                ("x.o".into(), "/k/x.c".into(), false, vec![0, 1]),
                ("y.o".into(), "/k/y.c".into(), true, vec![1]),
            ],
            paths,
            "vmlinux.a".into(),
            false,
        );
        let dir = std::env::temp_dir().join(format!("kmx-graph-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mapped = |bytes: &[u8]| {
            let path = dir.join("g.bin");
            std::fs::write(&path, bytes).unwrap();
            let file = std::fs::File::open(&path).unwrap();
            unsafe { memmap2::Mmap::map(&file) }.unwrap()
        };
        let back = BuildGraph::decode(mapped(&g.encode())).unwrap();
        assert_eq!(back.members.len(), 2);
        let a = back.id_of(Path::new("/k/include/linux/a.h")).unwrap();
        assert!(back.includes(0, a));
        assert!(!back.includes(1, a));
        assert_eq!(
            back.users(&[Path::new("/k/b.h"), Path::new("/k/y.c")]),
            vec![vec![0, 1], vec![1]]
        );
        assert_eq!(back.usage(Path::new("/k/b.h")), 2);
        assert_eq!(back.usage(Path::new("/k/none.h")), 0);
        assert!(back.members[1].module);
        assert!(BuildGraph::decode(mapped(&g.encode()[..20])).is_none());
        let mut longer = g.encode();
        longer.push(0);
        assert!(BuildGraph::decode(mapped(&longer)).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
