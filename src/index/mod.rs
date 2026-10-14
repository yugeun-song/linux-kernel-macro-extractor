pub mod build;
mod extract;
pub mod format;
pub mod sources;
pub mod verify;

use std::fs::File;
use std::path::{Path, PathBuf};

use fst::{IntoStreamer, Streamer};
use memmap2::Mmap;
use rayon::prelude::*;
use rustc_hash::FxHashSet;

use crate::tree::Root;
use format::{
    CHUNK_REC, DIR_REC, DirRec, FILE_REC, Sections, Stat, Tag, get_varint, rd_u16, rd_u32, rd_u64,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum FileClass {
    Other = 0,
    CSource = 1,
    Header = 2,
    Asm = 3,
    DeviceTree = 4,
    Kconfig = 5,
    Makefile = 6,
    DotConfig = 7,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Scope {
    Kernel = 0,
    Tools = 1,
    Scripts = 2,
    Docs = 3,
    Usr = 4,
    Generated = 5,
}

pub mod flag {
    pub const OBJ_ROOT: u32 = 1;
    pub const CLASS_SHIFT: u32 = 1;
    pub const CLASS_MASK: u32 = 0xf << CLASS_SHIFT;
    pub const SCOPE_SHIFT: u32 = 5;
    pub const SCOPE_MASK: u32 = 0xf << SCOPE_SHIFT;
    pub const INCLUDE_DIR: u32 = 1 << 9;
    pub const GUARDED: u32 = 1 << 10;
}

pub const NO_ARCH: u16 = u16::MAX;

impl FileClass {
    pub fn of_name(name: &str) -> FileClass {
        if name == ".config" {
            return FileClass::DotConfig;
        }
        if name.starts_with("Kconfig") {
            return FileClass::Kconfig;
        }
        if name == "Makefile"
            || name == "Kbuild"
            || name.starts_with("Makefile.")
            || name.starts_with("Kbuild.")
        {
            return FileClass::Makefile;
        }
        match name.rsplit_once('.').map(|(_, ext)| ext) {
            Some("c") => FileClass::CSource,
            Some("h") => FileClass::Header,
            Some("S" | "s") => FileClass::Asm,
            Some("dts" | "dtsi") => FileClass::DeviceTree,
            _ => FileClass::Other,
        }
    }

    pub fn is_preprocessed(self) -> bool {
        matches!(
            self,
            FileClass::CSource | FileClass::Header | FileClass::Asm | FileClass::DeviceTree
        )
    }

    fn from_bits(v: u32) -> FileClass {
        match v {
            1 => FileClass::CSource,
            2 => FileClass::Header,
            3 => FileClass::Asm,
            4 => FileClass::DeviceTree,
            5 => FileClass::Kconfig,
            6 => FileClass::Makefile,
            7 => FileClass::DotConfig,
            _ => FileClass::Other,
        }
    }
}

impl Scope {
    fn from_bits(v: u32) -> Scope {
        match v {
            1 => Scope::Tools,
            2 => Scope::Scripts,
            3 => Scope::Docs,
            4 => Scope::Usr,
            5 => Scope::Generated,
            _ => Scope::Kernel,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Scope::Kernel => "kernel",
            Scope::Tools => "tools",
            Scope::Scripts => "scripts",
            Scope::Docs => "docs",
            Scope::Usr => "usr",
            Scope::Generated => "generated",
        }
    }
}

pub fn classify(path: &str, root: Root, arch_names: &[String]) -> (u32, u16) {
    let name = path.rsplit('/').next().unwrap_or(path);
    let class = FileClass::of_name(name);
    let first = path.split('/').next().unwrap_or("");
    let generated = path.contains("/generated/") || path.starts_with("include/generated/");
    let scope = if generated {
        Scope::Generated
    } else {
        match first {
            "tools" => Scope::Tools,
            "scripts" => Scope::Scripts,
            "Documentation" => Scope::Docs,
            "usr" => Scope::Usr,
            _ => Scope::Kernel,
        }
    };
    let mut arch = NO_ARCH;
    let mut include_dir = first == "include";
    if let Some(rest) = path.strip_prefix("arch/")
        && let Some((a, tail)) = rest.split_once('/')
    {
        if let Some(i) = arch_names.iter().position(|n| n == a) {
            arch = i as u16;
        }
        include_dir = tail.starts_with("include/");
    }
    let mut flags = (class as u32) << flag::CLASS_SHIFT | (scope as u32) << flag::SCOPE_SHIFT;
    if root == Root::Obj {
        flags |= flag::OBJ_ROOT;
    }
    if include_dir {
        flags |= flag::INCLUDE_DIR;
    }
    (flags, arch)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DefKind {
    Object = 0,
    Function = 1,
    Undef = 2,
    Enumerator = 3,
    EnumTag = 4,
}

impl DefKind {
    pub fn from_bits(v: u8) -> DefKind {
        match v & 7 {
            0 => DefKind::Object,
            1 => DefKind::Function,
            2 => DefKind::Undef,
            3 => DefKind::Enumerator,
            _ => DefKind::EnumTag,
        }
    }

    pub fn is_macro(self) -> bool {
        matches!(self, DefKind::Object | DefKind::Function | DefKind::Undef)
    }
}

pub const ENUM_TAG_PREFIX: &str = "enum ";
pub const USE_PREFIX: &str = "\u{1}";
const AFTER_USES: &[u8] = b"\x02";

pub type Matcher = Box<dyn Fn(&str) -> bool>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Posting {
    pub file: u32,
    pub line: u32,
    pub offset: u32,
    pub kind: DefKind,
    pub depth: u8,
}

pub type FileId = u32;

#[derive(Clone, Copy, Debug)]
pub struct FileView<'a> {
    pub id: FileId,
    pub path: &'a str,
    pub flags: u32,
    pub arch: Option<&'a str>,
    pub stat: Stat,
    pub hash: u128,
}

impl FileView<'_> {
    pub fn root(&self) -> Root {
        if self.flags & flag::OBJ_ROOT != 0 {
            Root::Obj
        } else {
            Root::Src
        }
    }

    pub fn class(&self) -> FileClass {
        FileClass::from_bits((self.flags & flag::CLASS_MASK) >> flag::CLASS_SHIFT)
    }

    pub fn scope(&self) -> Scope {
        Scope::from_bits((self.flags & flag::SCOPE_MASK) >> flag::SCOPE_SHIFT)
    }

    pub fn in_include_dir(&self) -> bool {
        self.flags & flag::INCLUDE_DIR != 0
    }

    pub fn guarded(&self) -> bool {
        self.flags & flag::GUARDED != 0
    }
}

struct Chunk {
    first: &'static [u8],
    names: fst::Map<&'static [u8]>,
    post_base: u64,
}

struct Layer {
    _map: Mmap,
    data: &'static [u8],
    strs: &'static [u8],
    files: &'static [u8],
    dirs: &'static [u8],
    posts: &'static [u8],
    chunks: Vec<Chunk>,
    paths: fst::Map<&'static [u8]>,
    arch_names: Vec<String>,
    base_id: u32,
}

impl Layer {
    fn open(path: &Path, base_id: u32) -> Option<Layer> {
        let file = File::open(path).ok()?;
        let map = unsafe { Mmap::map(&file) }.ok()?;
        let data: &'static [u8] = unsafe { std::slice::from_raw_parts(map.as_ptr(), map.len()) };
        let sec = Sections::parse(data)?;
        let strs = sec.get(Tag::STRS);
        let nfst = sec.get(Tag::NFST);
        let mut chunks = Vec::new();
        for rec in sec.get(Tag::CHNK).as_chunks::<CHUNK_REC>().0 {
            let (ko, kl) = (rd_u32(rec, 0) as usize, rd_u32(rec, 4) as usize);
            let (fo, fl) = (rd_u64(rec, 8) as usize, rd_u64(rec, 16) as usize);
            chunks.push(Chunk {
                first: strs.get(ko..ko + kl)?,
                names: fst::Map::new(nfst.get(fo..fo + fl)?).ok()?,
                post_base: rd_u64(rec, 24),
            });
        }
        let paths = fst::Map::new(sec.get(Tag::PFST)).ok()?;
        let arch_names = std::str::from_utf8(sec.get(Tag::ARCH))
            .ok()?
            .split('\n')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        Some(Layer {
            data,
            strs,
            files: sec.get(Tag::FILE),
            dirs: sec.get(Tag::DIRS),
            posts: sec.get(Tag::POST),
            chunks,
            paths,
            arch_names,
            base_id,
            _map: map,
        })
    }

    fn file_count(&self) -> u32 {
        (self.files.len() / FILE_REC) as u32
    }

    fn file(&self, local: u32) -> Option<FileView<'_>> {
        let at = local as usize * FILE_REC;
        let rec = self.files.get(at..at + FILE_REC)?;
        let off = rd_u32(rec, 0) as usize;
        let len = rd_u32(rec, 4) as usize;
        let path = std::str::from_utf8(self.strs.get(off..off + len)?).ok()?;
        let arch_idx = rd_u16(rec, 12);
        Some(FileView {
            id: self.base_id + local,
            path,
            flags: rd_u32(rec, 8),
            arch: (arch_idx != NO_ARCH)
                .then(|| self.arch_names.get(arch_idx as usize).map(String::as_str))
                .flatten(),
            stat: Stat {
                size: rd_u64(rec, 16),
                mtime_ns: rd_u64(rec, 24) as i64,
                ctime_ns: rd_u64(rec, 32) as i64,
                ino: rd_u64(rec, 40),
            },
            hash: u128::from(rd_u64(rec, 48)) | (u128::from(rd_u64(rec, 56)) << 64),
        })
    }

    fn dir(&self, i: usize) -> Option<DirRec> {
        let at = i * DIR_REC;
        let rec = self.dirs.get(at..at + DIR_REC)?;
        let off = rd_u32(rec, 0) as usize;
        let len = rd_u32(rec, 4) as usize;
        Some(DirRec {
            path: String::from_utf8_lossy(self.strs.get(off..off + len)?).into_owned(),
            flags: rd_u32(rec, 8),
            mtime_ns: rd_u64(rec, 16) as i64,
            ctime_ns: rd_u64(rec, 24) as i64,
        })
    }

    fn chunk_for(&self, key: &[u8]) -> Option<&Chunk> {
        let idx = self.chunks.partition_point(|c| c.first <= key);
        self.chunks.get(idx.checked_sub(1)?)
    }

    fn get_name(&self, key: &str) -> Option<u64> {
        let c = self.chunk_for(key.as_bytes())?;
        c.names.get(key).map(|v| v + c.post_base)
    }

    fn contains_name(&self, key: &[u8]) -> bool {
        self.chunk_for(key)
            .is_some_and(|c| c.names.contains_key(key))
    }

    fn postings_at(&self, offset: u64, out: &mut Vec<Posting>) {
        let mut pos = offset as usize;
        let Some(count) = get_varint(self.posts, &mut pos) else {
            return;
        };
        let mut file = 0u64;
        for _ in 0..count {
            let (Some(df), Some(line), Some(off)) = (
                get_varint(self.posts, &mut pos),
                get_varint(self.posts, &mut pos),
                get_varint(self.posts, &mut pos),
            ) else {
                return;
            };
            let Some(&kd) = self.posts.get(pos) else {
                return;
            };
            pos += 1;
            file += df;
            out.push(Posting {
                file: self.base_id + file as u32,
                line: line as u32,
                offset: off as u32,
                kind: DefKind::from_bits(kd),
                depth: kd >> 3,
            });
        }
    }
}

pub struct Index {
    pub dir: PathBuf,
    base: Layer,
    overlay: Option<Layer>,
    tombstones: FxHashSet<u32>,
}

impl Index {
    pub fn open(generation_dir: &Path, base: &str, overlay: Option<&str>) -> Option<Index> {
        let base_layer = Layer::open(&generation_dir.join(base), 0)?;
        let (overlay_layer, tombstones) = match overlay {
            Some(name) => {
                let layer = Layer::open(&generation_dir.join(name), base_layer.file_count())?;
                let sec = Sections::parse(layer.data)?;
                let tomb = sec
                    .get(Tag::TOMB)
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| rd_u32(c, 0))
                    .collect();
                (Some(layer), tomb)
            }
            None => (None, FxHashSet::default()),
        };
        Some(Index {
            dir: generation_dir.to_path_buf(),
            base: base_layer,
            overlay: overlay_layer,
            tombstones,
        })
    }

    fn layer_of(&self, id: FileId) -> Option<(&Layer, u32)> {
        if id < self.base.file_count() {
            Some((&self.base, id))
        } else {
            let o = self.overlay.as_ref()?;
            let local = id - o.base_id;
            (local < o.file_count()).then_some((o, local))
        }
    }

    pub fn file(&self, id: FileId) -> Option<FileView<'_>> {
        let (layer, local) = self.layer_of(id)?;
        layer.file(local)
    }

    pub fn is_live(&self, id: FileId) -> bool {
        !self.tombstones.contains(&id)
    }

    pub fn files(&self) -> impl Iterator<Item = FileView<'_>> + '_ {
        let base = (0..self.base.file_count()).filter_map(move |i| self.base.file(i));
        let over = self
            .overlay
            .iter()
            .flat_map(|o| (0..o.file_count()).filter_map(move |i| o.file(i)));
        base.chain(over).filter(move |f| self.is_live(f.id))
    }

    pub fn par_files(&self) -> impl ParallelIterator<Item = FileView<'_>> + '_ {
        (0..self.id_end())
            .into_par_iter()
            .filter_map(move |id| self.file(id))
            .filter(move |f| self.is_live(f.id))
    }

    pub fn id_end(&self) -> FileId {
        self.base.file_count() + self.overlay.as_ref().map_or(0, |o| o.file_count())
    }

    pub fn base_file_count(&self) -> u32 {
        self.base.file_count()
    }

    pub fn tombstones(&self) -> impl Iterator<Item = FileId> + '_ {
        self.tombstones.iter().copied()
    }

    pub fn file_count(&self) -> usize {
        self.base.file_count() as usize
            + self.overlay.as_ref().map_or(0, |o| o.file_count() as usize)
            - self.tombstones.len()
    }

    pub fn par_dirs(&self) -> impl ParallelIterator<Item = DirRec> + '_ {
        let layer = self.overlay.as_ref().unwrap_or(&self.base);
        (0..layer.dirs.len() / DIR_REC)
            .into_par_iter()
            .filter_map(move |i| layer.dir(i))
    }

    pub fn uses(&self, name: &str) -> Vec<Posting> {
        self.lookup(&format!("{USE_PREFIX}{name}"))
    }

    pub fn lookup(&self, name: &str) -> Vec<Posting> {
        let mut out = Vec::new();
        if let Some(off) = self.base.get_name(name) {
            self.base.postings_at(off, &mut out);
            if !self.tombstones.is_empty() {
                out.retain(|p| !self.tombstones.contains(&p.file));
            }
        }
        if let Some(o) = &self.overlay
            && let Some(off) = o.get_name(name)
        {
            o.postings_at(off, &mut out);
        }
        out
    }

    pub fn file_id(&self, root: Root, path: &str) -> Option<FileId> {
        let key = path_key(root, path);
        if let Some(o) = &self.overlay
            && let Some(id) = o.paths.get(&key)
        {
            return Some(id as u32);
        }
        self.base
            .paths
            .get(&key)
            .map(|id| id as u32)
            .filter(|id| self.is_live(*id))
    }

    pub fn names_with_prefix(&self, prefix: &str, mut f: impl FnMut(&str)) {
        fn each(layer: &Layer, prefix: &str, f: &mut dyn FnMut(&[u8])) {
            let p = prefix.as_bytes();
            let start = layer.chunk_for(p).map_or(0, |c| {
                layer
                    .chunks
                    .iter()
                    .position(|x| std::ptr::eq(x, c))
                    .unwrap_or(0)
            });
            for c in &layer.chunks[start..] {
                if !p.is_empty() && c.first > p && !c.first.starts_with(p) {
                    break;
                }
                let mut s = c.names.range().ge(p).into_stream();
                while let Some((k, _)) = s.next() {
                    if !k.starts_with(p) {
                        break;
                    }
                    f(k);
                }
            }
        }
        let hidden = |k: &[u8]| k.starts_with(USE_PREFIX.as_bytes());
        each(&self.base, prefix, &mut |k| {
            if !hidden(k)
                && let Ok(s) = std::str::from_utf8(k)
            {
                f(s);
            }
        });
        if let Some(o) = &self.overlay {
            each(o, prefix, &mut |k| {
                if !hidden(k)
                    && !self.base.contains_name(k)
                    && let Ok(s) = std::str::from_utf8(k)
                {
                    f(s);
                }
            });
        }
    }

    pub fn names_matching(
        &self,
        prefix: &str,
        matcher: &(dyn Fn() -> Matcher + Sync),
    ) -> Vec<String> {
        let p = prefix.as_bytes();
        let scan = |layer: &Layer, skip_base: bool| -> Vec<String> {
            layer
                .chunks
                .par_iter()
                .filter(|c| {
                    p.is_empty()
                        || c.first <= p
                        || c.first.starts_with(p)
                        || layer.chunks.len() == 1
                })
                .flat_map_iter(|c| {
                    let pred = matcher();
                    let mut found = Vec::new();
                    let start: &[u8] = if p.is_empty() { AFTER_USES } else { p };
                    let mut s = c.names.range().ge(start).into_stream();
                    while let Some((k, _)) = s.next() {
                        if !k.starts_with(p) {
                            break;
                        }
                        if k.starts_with(USE_PREFIX.as_bytes())
                            || (skip_base && self.base.contains_name(k))
                        {
                            continue;
                        }
                        if let Ok(name) = std::str::from_utf8(k)
                            && pred(name)
                        {
                            found.push(name.to_string());
                        }
                    }
                    found
                })
                .collect()
        };
        let mut out = scan(&self.base, false);
        if let Some(o) = &self.overlay {
            out.extend(scan(o, true));
        }
        out.sort_unstable();
        out
    }
}

pub fn path_key(root: Root, path: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(path.len() + 1);
    k.push(match root {
        Root::Src => b'S',
        Root::Obj => b'O',
    });
    k.extend_from_slice(path.as_bytes());
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        let arches = vec!["arm64".to_string(), "x86".to_string()];
        let (f, a) = classify("arch/x86/include/asm/page_types.h", Root::Src, &arches);
        assert_eq!(a, 1);
        assert_ne!(f & flag::INCLUDE_DIR, 0);
        assert_eq!(
            (f & flag::CLASS_MASK) >> flag::CLASS_SHIFT,
            FileClass::Header as u32
        );
        let (f, a) = classify("tools/include/linux/kernel.h", Root::Src, &arches);
        assert_eq!(a, NO_ARCH);
        assert_eq!(
            (f & flag::SCOPE_MASK) >> flag::SCOPE_SHIFT,
            Scope::Tools as u32
        );
        let (f, _) = classify("include/generated/autoconf.h", Root::Obj, &arches);
        assert_eq!(
            (f & flag::SCOPE_MASK) >> flag::SCOPE_SHIFT,
            Scope::Generated as u32
        );
        assert_ne!(f & flag::OBJ_ROOT, 0);
        assert_eq!(FileClass::of_name("Kconfig.debug"), FileClass::Kconfig);
        assert_eq!(FileClass::of_name("Makefile.lib"), FileClass::Makefile);
        assert_eq!(FileClass::of_name("entry_64.S"), FileClass::Asm);
    }
}
