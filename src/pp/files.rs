use std::borrow::Cow;
use std::cell::OnceCell;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{OnceLock, PoisonError, RwLock};

use rustc_hash::FxHashMap;

use super::kbuild::normalize;
use super::macros::SrcFile;
use crate::index::{FileId, Index};
use crate::syntax::scan::{self, DirKind, DirLine, Head};
use crate::tree::{Layout, Root};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Indexed(FileId),
    Disk,
    Virtual,
}

#[derive(Clone, Copy)]
struct HeadAt {
    kind: DirKind,
    function_like: bool,
    spliced: bool,
    name: (u32, u32),
}

pub struct Parsed {
    bytes: Vec<u8>,
    index: OnceLock<DirIndex>,
}

struct DirIndex {
    dirs: Vec<DirLine>,
    heads: Vec<HeadAt>,
    spliced: Vec<u8>,
}

impl DirIndex {
    fn new(bytes: &[u8]) -> DirIndex {
        let mut dirs = Vec::new();
        scan::scan(bytes, &mut dirs);
        let mut spliced = Vec::new();
        let base = bytes.as_ptr() as usize;
        let heads = dirs
            .iter()
            .map(|d| {
                let h = scan::parse_head(bytes, *d);
                let (spliced_name, name) = match &h.name {
                    None => (false, (0, 0)),
                    Some(Cow::Borrowed(b)) => {
                        (false, ((b.as_ptr() as usize - base) as u32, b.len() as u32))
                    }
                    Some(Cow::Owned(v)) => {
                        let at = spliced.len() as u32;
                        spliced.extend_from_slice(v);
                        (true, (at, v.len() as u32))
                    }
                };
                HeadAt {
                    kind: h.kind,
                    function_like: h.function_like,
                    spliced: spliced_name,
                    name,
                }
            })
            .collect();
        DirIndex {
            dirs,
            heads,
            spliced,
        }
    }
}

impl Parsed {
    fn new(bytes: Vec<u8>) -> Parsed {
        Parsed {
            bytes,
            index: OnceLock::new(),
        }
    }

    fn index(&self) -> &DirIndex {
        self.index.get_or_init(|| DirIndex::new(&self.bytes))
    }

    pub fn dirs(&self) -> &[DirLine] {
        &self.index().dirs
    }

    pub fn head(&self, i: usize) -> Head<'_> {
        let ix = self.index();
        let h = ix.heads[i];
        let name = (h.name.1 > 0).then(|| {
            let src = if h.spliced { &ix.spliced } else { &self.bytes };
            Cow::Borrowed(&src[h.name.0 as usize..(h.name.0 + h.name.1) as usize])
        });
        Head {
            kind: h.kind,
            name,
            function_like: h.function_like,
        }
    }
}

#[derive(Clone)]
pub enum Text<'w> {
    Shared(&'w Parsed),
    Local(Rc<Parsed>),
}

impl Text<'_> {
    pub fn parsed(&self) -> &Parsed {
        match self {
            Text::Shared(p) => p,
            Text::Local(p) => p,
        }
    }

    pub fn dirs(&self) -> &[DirLine] {
        self.parsed().dirs()
    }

    pub fn head(&self, i: usize) -> Head<'_> {
        self.parsed().head(i)
    }
}

impl Deref for Text<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.parsed().bytes
    }
}

#[derive(Clone)]
pub struct Dirs<'w>(Text<'w>);

impl Dirs<'_> {
    pub fn head(&self, i: usize) -> Head<'_> {
        self.0.head(i)
    }
}

impl Deref for Dirs<'_> {
    type Target = [DirLine];

    fn deref(&self) -> &[DirLine] {
        self.0.dirs()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Indexed(FileId),
    SrcDisk,
    Disk,
}

pub struct Source {
    abs: PathBuf,
    display: String,
    place: Place,
    parsed: OnceLock<Parsed>,
}

impl Source {
    fn parsed(&self) -> &Parsed {
        self.parsed
            .get_or_init(|| Parsed::new(std::fs::read(&self.abs).unwrap_or_default()))
    }
}

#[derive(Default)]
pub struct Sources {
    files: RwLock<FxHashMap<PathBuf, Option<Box<Source>>>>,
}

impl Sources {
    fn get<'s>(&'s self, loc: &Locator<'_>, abs: &Path) -> Option<&'s Source> {
        let known = self
            .files
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(abs)
            .map(|s| s.as_deref().map(std::ptr::from_ref));
        let ptr = match known {
            Some(p) => p,
            None => {
                let made = loc.classify(abs).map(Box::new);
                let mut files = self.files.write().unwrap_or_else(PoisonError::into_inner);
                files
                    .entry(abs.to_path_buf())
                    .or_insert(made)
                    .as_deref()
                    .map(std::ptr::from_ref)
            }
        };
        ptr.map(|p| unsafe { &*p })
    }

    pub fn bytes<'s>(&'s self, loc: &Locator<'_>, abs: &Path) -> Option<&'s [u8]> {
        self.get(loc, &normalize(abs))
            .map(|s| s.parsed().bytes.as_slice())
    }
}

pub struct FileEntry<'w> {
    pub display: Cow<'w, str>,
    pub abs: Option<Cow<'w, Path>>,
    pub kind: FileKind,
    pub asm: bool,
    shared: Option<&'w Source>,
    local: OnceCell<Rc<Parsed>>,
}

impl FileEntry<'_> {
    pub fn dir(&self) -> Option<&Path> {
        self.abs.as_deref().and_then(Path::parent)
    }
}

pub struct FileTable<'w> {
    entries: Vec<FileEntry<'w>>,
    by_abs: FxHashMap<PathBuf, SrcFile>,
    volatile: u32,
}

impl Default for FileTable<'_> {
    fn default() -> Self {
        Self::new()
    }
}

fn is_asm_name(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "S" || e == "s")
}

impl<'w> FileTable<'w> {
    pub fn new() -> FileTable<'w> {
        FileTable {
            entries: Vec::new(),
            by_abs: FxHashMap::default(),
            volatile: 0,
        }
    }

    pub fn entry(&self, f: SrcFile) -> &FileEntry<'w> {
        &self.entries[f.0 as usize]
    }

    pub fn display(&self, f: SrcFile) -> &str {
        &self.entries[f.0 as usize].display
    }

    pub fn lookup(&self, abs: &Path) -> Option<SrcFile> {
        self.by_abs.get(abs).copied()
    }

    fn push(&mut self, e: FileEntry<'w>) -> SrcFile {
        let id = SrcFile(self.entries.len() as u32);
        if let Some(a) = &e.abs {
            self.by_abs.insert(a.to_path_buf(), id);
            let off_index = match e.shared {
                Some(s) => s.place == Place::SrcDisk,
                None => true,
            };
            self.volatile += u32::from(off_index);
        }
        self.entries.push(e);
        id
    }

    pub fn volatile(&self) -> u32 {
        self.volatile
    }

    pub fn add_virtual(
        &mut self,
        display: &str,
        abs: Option<PathBuf>,
        content: Vec<u8>,
    ) -> SrcFile {
        if let Some(a) = &abs
            && let Some(&f) = self.by_abs.get(a)
        {
            return f;
        }
        let local = OnceCell::new();
        let _ = local.set(Rc::new(Parsed::new(content)));
        self.push(FileEntry {
            display: Cow::Owned(display.to_string()),
            asm: false,
            abs: abs.map(Cow::Owned),
            kind: FileKind::Virtual,
            shared: None,
            local,
        })
    }

    fn add_shared(&mut self, s: &'w Source) -> SrcFile {
        if let Some(&f) = self.by_abs.get(&s.abs) {
            return f;
        }
        self.push(FileEntry {
            display: Cow::Borrowed(&s.display),
            asm: is_asm_name(&s.abs),
            abs: Some(Cow::Borrowed(&s.abs)),
            kind: match s.place {
                Place::Indexed(id) => FileKind::Indexed(id),
                Place::SrcDisk | Place::Disk => FileKind::Disk,
            },
            shared: Some(s),
            local: OnceCell::new(),
        })
    }

    pub fn text(&self, f: SrcFile) -> Text<'w> {
        let e = &self.entries[f.0 as usize];
        match e.shared {
            Some(s) => Text::Shared(s.parsed()),
            None => Text::Local(
                e.local
                    .get_or_init(|| Rc::new(Parsed::new(Vec::new())))
                    .clone(),
            ),
        }
    }

    pub fn content(&self, f: SrcFile) -> Text<'w> {
        self.text(f)
    }

    pub fn directives(&self, f: SrcFile) -> Dirs<'w> {
        Dirs(self.text(f))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub struct Locator<'a> {
    pub index: &'a Index,
    pub layout: &'a Layout,
    pub sources: &'a Sources,
}

impl<'a> Locator<'a> {
    pub fn rel_of(&self, abs: &Path) -> Option<(Root, String)> {
        let rel = |base: &Path| {
            abs.strip_prefix(base)
                .ok()
                .map(|r| r.to_string_lossy().into_owned())
        };
        if !self.layout.in_tree()
            && let Some(r) = rel(&self.layout.objtree)
        {
            return Some((Root::Obj, r));
        }
        rel(&self.layout.srctree).map(|r| (Root::Src, r))
    }

    fn classify(&self, abs: &Path) -> Option<Source> {
        let (place, display) = match self.rel_of(abs) {
            Some((root, rel)) => match self.index.file_id(root, &rel) {
                Some(id) => (Place::Indexed(id), rel),
                None if !abs.is_file() => return None,
                None if root == Root::Obj => (Place::Disk, rel),
                None => (Place::SrcDisk, rel),
            },
            None if abs.is_file() => (Place::Disk, abs.display().to_string()),
            None => return None,
        };
        Some(Source {
            abs: abs.to_path_buf(),
            display,
            place,
            parsed: OnceLock::new(),
        })
    }

    pub fn find<'w>(
        &self,
        files: &mut FileTable<'w>,
        abs: &Path,
        allow_disk: bool,
    ) -> Option<SrcFile>
    where
        'a: 'w,
    {
        self.find_normal(files, &normalize(abs), allow_disk)
    }

    pub fn find_normal<'w>(
        &self,
        files: &mut FileTable<'w>,
        abs: &Path,
        allow_disk: bool,
    ) -> Option<SrcFile>
    where
        'a: 'w,
    {
        if let Some(f) = files.lookup(abs) {
            return Some(f);
        }
        let s = self.sources.get(self, abs)?;
        (s.place != Place::SrcDisk || allow_disk).then(|| files.add_shared(s))
    }

    pub fn display_for(&self, abs: &Path) -> String {
        self.rel_of(abs)
            .map(|(_, r)| r)
            .unwrap_or_else(|| abs.display().to_string())
    }
}
