use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use rayon::prelude::*;
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};

use regex::Regex;

use super::BuildGraph;
use crate::pp::kbuild::normalize;
use crate::tree::Layout;
use crate::tree::dot_config::DotConfig;

impl BuildGraph {
    pub(super) fn scan(layout: &Layout, config: &DotConfig) -> Option<BuildGraph> {
        let (builtin, modules, origin) = objects(layout, config)?;
        let stale = config_newer_than_build(layout);
        let raw = RawDeps::new();
        let mut seen = FxHashSet::default();
        let mut read = Vec::new();
        expand(layout, &raw, builtin, false, &mut seen, &mut read);
        expand(layout, &raw, modules, true, &mut seen, &mut read);
        let resolved: Vec<PathBuf> = raw
            .into_strings()
            .par_iter()
            .map(|d| resolve(layout, d))
            .collect();
        let mut paths = resolved.clone();
        paths.sort_unstable();
        paths.dedup();
        let remap: Vec<u32> = resolved
            .iter()
            .map(|p| paths.binary_search(p).map_or(0, |i| i as u32))
            .collect();
        let members = read
            .into_par_iter()
            .map(|(obj, module, source, deps)| {
                let mut list: Vec<u32> = deps.into_iter().map(|d| remap[d as usize]).collect();
                list.sort_unstable();
                list.dedup();
                (obj, source, module, list)
            })
            .collect();
        Some(BuildGraph::assemble(members, paths, origin, stale))
    }
}

fn config_newer_than_build(layout: &Layout) -> bool {
    let o = &layout.objtree;
    match (
        std::fs::metadata(o.join(".config")).and_then(|m| m.modified()),
        std::fs::metadata(o.join("include/config/auto.conf")).and_then(|m| m.modified()),
    ) {
        (Ok(c), Ok(a)) => c > a,
        _ => false,
    }
}

fn ar_members(path: &Path) -> Option<Vec<String>> {
    let data = std::fs::read(path).ok()?;
    let thin = data.starts_with(b"!<thin>\n");
    if !thin && !data.starts_with(b"!<arch>\n") {
        return None;
    }
    let mut pos = 8;
    let mut long_names: &[u8] = &[];
    let mut out = Vec::new();
    while pos + 60 <= data.len() {
        let hdr = &data[pos..pos + 60];
        let name = std::str::from_utf8(&hdr[..16]).ok()?.trim_end();
        let size: usize = std::str::from_utf8(&hdr[48..58])
            .ok()?
            .trim()
            .parse()
            .ok()?;
        pos += 60;
        let has_data = !thin || name == "/" || name == "//" || name == "/SYM64/";
        if name == "//" {
            long_names = data.get(pos..pos + size)?;
        } else if name != "/" && name != "/SYM64/" {
            let member = if let Some(off) = name.strip_prefix('/') {
                let digits: String = off.chars().take_while(char::is_ascii_digit).collect();
                let off: usize = digits.parse().ok()?;
                let rest = long_names.get(off..)?;
                let end = memchr::memchr(b'\n', rest).unwrap_or(rest.len());
                String::from_utf8_lossy(&rest[..end])
                    .trim_end_matches('/')
                    .to_string()
            } else {
                name.trim_end_matches('/').to_string()
            };
            out.push(member);
        }
        if has_data {
            pos += size + (size & 1);
        }
    }
    Some(out)
}

fn is_archive(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && (&magic == b"!<thin>\n" || &magic == b"!<arch>\n")
}

fn objtree_relative(layout: &Layout, p: &Path) -> String {
    let p = normalize(p);
    p.strip_prefix(&layout.objtree)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| p.display().to_string())
}

fn archive_parts(layout: &Layout, archive: &str) -> Option<Vec<String>> {
    let dir = layout
        .objtree
        .join(Path::new(archive).parent().unwrap_or(Path::new("")));
    Some(
        ar_members(&layout.objtree.join(archive))?
            .into_iter()
            .map(|m| objtree_relative(layout, &dir.join(m)))
            .collect(),
    )
}

fn cmd_file(layout: &Layout, obj: &str) -> PathBuf {
    let (dir, file) = obj.rsplit_once('/').unwrap_or(("", obj));
    let mut p = layout.objtree.clone();
    if !dir.is_empty() {
        p.push(dir);
    }
    p.push(format!(".{file}.cmd"));
    p
}

fn resolve(layout: &Layout, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        return normalize(path);
    }
    let o = normalize(&layout.objtree.join(p));
    if o.exists() {
        o
    } else {
        normalize(&layout.srctree.join(p))
    }
}

struct RawDeps {
    shards: Vec<Mutex<FxHashMap<Box<str>, u32>>>,
    next: AtomicU32,
}

impl RawDeps {
    const SHARDS: usize = 64;

    fn new() -> RawDeps {
        RawDeps {
            shards: (0..Self::SHARDS)
                .map(|_| Mutex::new(FxHashMap::default()))
                .collect(),
            next: AtomicU32::new(0),
        }
    }

    fn id(&self, dep: &str) -> u32 {
        let shard = (FxBuildHasher.hash_one(dep) >> 32) as usize % Self::SHARDS;
        let mut map = self.shards[shard]
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(&id) = map.get(dep) {
            return id;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        map.insert(dep.into(), id);
        id
    }

    fn into_strings(self) -> Vec<Box<str>> {
        let mut out = vec![Box::<str>::default(); self.next.into_inner() as usize];
        for shard in self.shards {
            for (dep, id) in shard.into_inner().unwrap_or_else(PoisonError::into_inner) {
                out[id as usize] = dep;
            }
        }
        out
    }
}

type Unit = (PathBuf, Vec<u32>);

fn parse_unit(layout: &Layout, raw: &RawDeps, text: &str) -> Option<Option<Unit>> {
    let mut source = None;
    let mut deps = Vec::new();
    let mut in_deps = false;
    for line in text.lines() {
        if !in_deps {
            if let Some(rest) = line.strip_prefix("source_") {
                let Some(src) = rest.split_once(":=").map(|(_, s)| s.trim()) else {
                    return Some(None);
                };
                if !(src.ends_with(".c") || src.ends_with(".S")) {
                    return Some(None);
                }
                source = Some(resolve(layout, src));
            } else if line.starts_with("deps_") {
                in_deps = true;
            }
            continue;
        }
        let continued = line.trim_end().ends_with('\\');
        let t = line.trim().trim_end_matches('\\').trim();
        if !t.is_empty() && !t.starts_with("$(wildcard") {
            deps.push(raw.id(t));
        }
        if !continued {
            break;
        }
    }
    source.map(|s| Some((s, deps)))
}

fn link_parts(text: &str, obj: &str) -> Option<Vec<String>> {
    let command = text.lines().find_map(|l| {
        let (key, rest) = l.split_once(":=")?;
        let key = key.trim();
        let target = key
            .strip_prefix("savedcmd_")
            .or_else(|| key.strip_prefix("cmd_"))?;
        (target == obj).then_some(rest)
    })?;
    let mut toks = Vec::new();
    for t in command.split_whitespace() {
        if t == ";" || t == "&&" || t == "||" {
            break;
        }
        match t.strip_suffix(';') {
            Some(x) => {
                toks.push(x);
                break;
            }
            None => toks.push(t),
        }
    }
    let objects = |ts: &[&str]| -> Vec<String> {
        ts.iter()
            .map(|t| t.strip_prefix("./").unwrap_or(t))
            .filter(|t| (t.ends_with(".o") || t.ends_with(".a")) && *t != obj)
            .map(str::to_string)
            .collect()
    };
    match toks.iter().position(|t| *t == "-o") {
        Some(at) if toks.get(at + 1) == Some(&obj) => Some(objects(&toks[at + 2..])),
        Some(_) => None,
        None => {
            let at = toks.iter().position(|t| t.ends_with("objcopy"))?;
            (toks.last() == Some(&obj)).then(|| objects(&toks[at + 1..]))
        }
    }
}

enum Node {
    Unit(Unit),
    Parts(Vec<String>),
}

fn read_node(layout: &Layout, raw: &RawDeps, obj: &str) -> Option<Node> {
    if obj.ends_with(".a") {
        return archive_parts(layout, obj).map(Node::Parts);
    }
    let text = std::fs::read_to_string(cmd_file(layout, obj)).ok();
    if let Some(unit) = text.as_deref().and_then(|t| parse_unit(layout, raw, t)) {
        return unit.map(Node::Unit);
    }
    if is_archive(&layout.objtree.join(obj)) {
        return archive_parts(layout, obj).map(Node::Parts);
    }
    link_parts(text.as_deref()?, obj).map(Node::Parts)
}

type Scanned = (String, bool, PathBuf, Vec<u32>);

fn expand(
    layout: &Layout,
    raw: &RawDeps,
    roots: Vec<String>,
    module: bool,
    seen: &mut FxHashSet<String>,
    out: &mut Vec<Scanned>,
) {
    let mut frontier: Vec<String> = roots
        .into_iter()
        .filter(|r| seen.insert(r.clone()))
        .collect();
    while !frontier.is_empty() {
        let nodes: Vec<(String, Option<Node>)> = frontier
            .into_par_iter()
            .map(|obj| {
                let node = read_node(layout, raw, &obj);
                (obj, node)
            })
            .collect();
        frontier = Vec::new();
        for (obj, node) in nodes {
            match node {
                Some(Node::Unit((source, deps))) => out.push((obj, module, source, deps)),
                Some(Node::Parts(parts)) => {
                    frontier.extend(parts.into_iter().filter(|p| seen.insert(p.clone())))
                }
                None => {}
            }
        }
    }
}

fn walk_cmds(dir: &Path, base: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                if !matches!(name, ".git" | "scripts" | "tools" | "Documentation" | "usr") {
                    walk_cmds(&p, base, out);
                }
            }
            Ok(t) if t.is_file() && name.starts_with('.') && name.ends_with(".o.cmd") => {
                if let Ok(rel) = p.parent().unwrap_or(base).strip_prefix(base) {
                    let obj = name.trim_start_matches('.').trim_end_matches(".cmd");
                    let rel = rel.display().to_string();
                    out.push(if rel.is_empty() {
                        obj.to_string()
                    } else {
                        format!("{rel}/{obj}")
                    });
                }
            }
            _ => {}
        }
    }
}

fn link_lists(text: &str, srcarch: &str, config: &DotConfig) -> Vec<(String, String)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r"^(head|init|core|drivers|net|libs|virt)-(y|\$\(CONFIG_([A-Za-z0-9_]+)\))\s*(?::=|\+=)(.*)$",
        )
        .expect("constant pattern")
    });
    let joined = text.replace("\\\n", " ");
    let mut out = Vec::new();
    for line in joined.lines() {
        let Some(c) = re.captures(line.trim_start()) else {
            continue;
        };
        if c.get(3).is_some_and(|name| !config.is_y(name.as_str())) {
            continue;
        }
        for tok in c[4].split_whitespace() {
            if tok.starts_with('#') {
                break;
            }
            let tok = tok
                .replace("$(objtree)/", "")
                .replace("$(SRCARCH)", srcarch)
                .replace("$(BITS)", "64");
            if tok.contains('$') {
                continue;
            }
            let tok = tok.strip_prefix("./").unwrap_or(&tok).to_string();
            out.push((c[1].to_string(), tok));
        }
    }
    out
}

fn link_roots(layout: &Layout, config: &DotConfig, archive: bool) -> Vec<String> {
    let o = &layout.objtree;
    let mut lists = Vec::new();
    for makefile in [
        layout.srctree.join("Makefile"),
        layout
            .srctree
            .join("arch")
            .join(&layout.srcarch)
            .join("Makefile"),
    ] {
        if let Ok(text) = std::fs::read_to_string(makefile) {
            lists.extend(link_lists(&text, &layout.srcarch, config));
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut add = |p: String| {
        if o.join(&p).is_file() && !out.contains(&p) {
            out.push(p);
        }
    };
    for (kind, tok) in lists {
        if let Some(dir) = tok.strip_suffix('/') {
            if kind == "libs" {
                add(format!("{dir}/lib.a"));
            }
            if !archive {
                let thin = format!("{dir}/built-in.a");
                if o.join(&thin).is_file() {
                    add(thin);
                } else {
                    add(format!("{dir}/built-in.o"));
                }
            }
        } else if tok.ends_with(".a") || (!archive && tok.ends_with(".o")) {
            add(tok);
        }
    }
    out
}

type Roots = (Vec<String>, Vec<String>, String);

fn objects(layout: &Layout, config: &DotConfig) -> Option<Roots> {
    let o = &layout.objtree;
    let mut builtin: Vec<String> = Vec::new();
    let mut origin = Vec::new();
    if o.join("vmlinux.a").is_file() {
        builtin.push("vmlinux.a".to_string());
        builtin.extend(link_roots(layout, config, true));
        for lib in [
            "lib/lib.a".to_string(),
            format!("arch/{}/lib/lib.a", layout.srcarch),
        ] {
            if o.join(&lib).is_file() && !builtin.contains(&lib) {
                builtin.push(lib);
            }
        }
        origin.push("vmlinux.a");
    } else {
        builtin = link_roots(layout, config, false);
        if builtin.iter().any(|r| r.ends_with("/built-in.a")) {
            origin.push("built-in.a");
        } else if builtin.iter().any(|r| r.ends_with("/built-in.o")) {
            origin.push("built-in.o");
        } else {
            builtin.clear();
        }
    }
    let mut module_objs: Vec<String> = Vec::new();
    if let Ok(order) = std::fs::read_to_string(o.join("modules.order")) {
        origin.push("modules.order");
        for line in order.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let base = line
                .trim_start_matches("kernel/")
                .trim_end_matches(".ko")
                .trim_end_matches(".o");
            match std::fs::read_to_string(o.join(format!("{base}.mod"))) {
                Ok(list) => module_objs.extend(
                    list.split_whitespace()
                        .filter(|s| s.ends_with(".o"))
                        .map(str::to_string),
                ),
                Err(_) => module_objs.push(format!("{base}.o")),
            }
        }
    }
    if builtin.is_empty() && module_objs.is_empty() {
        let mut all = Vec::new();
        walk_cmds(o, o, &mut all);
        if all.is_empty() {
            return None;
        }
        all.sort();
        builtin = all;
        origin = vec![
            "every .o.cmd in the build directory (no vmlinux.a: stale objects may be counted)",
        ];
    }
    Some((builtin, module_objs, origin.join(" + ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thin_archive_parsing() {
        let mut a = b"!<thin>\n".to_vec();
        let longnames = b"init/main.o/\nkernel/sched/core.o/\n";
        a.extend_from_slice(
            format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                "//",
                "",
                "",
                "",
                "",
                longnames.len()
            )
            .as_bytes(),
        );
        a.extend_from_slice(longnames);
        for off in [0, 13] {
            a.extend_from_slice(
                format!(
                    "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    format!("/{off}"),
                    "0",
                    "0",
                    "0",
                    "644",
                    1234
                )
                .as_bytes(),
            );
        }
        a.extend_from_slice(
            format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                "fork.o/", "0", "0", "0", "644", 99
            )
            .as_bytes(),
        );
        let dir = std::env::temp_dir().join(format!("kmx-ar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.a");
        std::fs::write(&p, &a).unwrap();
        assert_eq!(
            ar_members(&p).unwrap(),
            vec!["init/main.o", "kernel/sched/core.o", "fork.o"]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn link_commands() {
        let ld = "cmd_init/built-in.o :=  aarch64-linux-gnu-ld -EL   -r -o init/built-in.o init/main.o init/version.o ; scripts/mod/modpost init/built-in.o\n";
        assert_eq!(
            link_parts(ld, "init/built-in.o").unwrap(),
            ["init/main.o", "init/version.o"]
        );
        let objcopy = "cmd_k/entry.stub.o := aarch64-linux-gnu-objcopy --prefix-symbols=__efistub_  k/entry.o k/entry.stub.o\n";
        assert_eq!(
            link_parts(objcopy, "k/entry.stub.o").unwrap(),
            ["k/entry.o"]
        );
        let guarded = "cmd_s/x.stub.o := if aarch64-linux-gnu-objcopy -R .debug* --prefix-symbols=__efistub_ s/x.o s/x.stub.o; then false; fi\n";
        assert_eq!(link_parts(guarded, "s/x.stub.o").unwrap(), ["s/x.o"]);
        let strip = "savedcmd_s/x.stub.o := aarch64-linux-gnu-strip --strip-debug -o s/x.stub.o s/x.o; if objdump -r s/x.stub.o; then false; fi\n";
        assert_eq!(link_parts(strip, "s/x.stub.o").unwrap(), ["s/x.o"]);
        assert!(link_parts("cmd_a.o := gcc -c -o b.o a.c\n", "a.o").is_none());
    }

    #[test]
    fn link_lists_from_makefiles() {
        let cfg = DotConfig::parse("CONFIG_KVM=y\nCONFIG_EFI_STUB=y\n# CONFIG_XEN is not set\n");
        let text = "head-y := arch/x86/kernel/head_$(BITS).o\ncore-y += arch/x86/kernel/ \\\n\tarch/x86/mm/\ncore-$(CONFIG_KVM) += arch/x86/kvm/\ncore-$(CONFIG_XEN) += arch/x86/xen/\nlibs-y := arch/$(SRCARCH)/lib/ $(libs-y) # trailing\ncore-$(CONFIG_EFI_STUB) += $(objtree)/drivers/firmware/efi/libstub/lib.a\nKBUILD_CFLAGS += -O2\n";
        let want = [
            ("head", "arch/x86/kernel/head_64.o"),
            ("core", "arch/x86/kernel/"),
            ("core", "arch/x86/mm/"),
            ("core", "arch/x86/kvm/"),
            ("libs", "arch/x86/lib/"),
            ("core", "drivers/firmware/efi/libstub/lib.a"),
        ];
        assert_eq!(
            link_lists(text, "x86", &cfg),
            want.map(|(k, t)| (k.to_string(), t.to_string()))
        );
    }
}
