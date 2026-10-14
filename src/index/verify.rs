use std::ffi::CStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use rayon::prelude::*;
use rustc_hash::FxHashSet;
use xxhash_rust::xxh3::xxh3_128;

use super::format::Stat;
use super::sources::{SourceFile, generated_dirs};
use super::{FileClass, FileId, Index, flag};
use crate::tree::{Layout, Root, git};

#[derive(Debug, Default)]
pub struct Changes {
    pub modified: Vec<SourceFile>,
    pub added: Vec<SourceFile>,
    pub deleted: Vec<SourceFile>,
    pub touched: Vec<SourceFile>,
    pub dirs_changed: usize,
}

impl Changes {
    pub fn is_clean(&self) -> bool {
        self.modified.is_empty()
            && self.added.is_empty()
            && self.deleted.is_empty()
            && self.touched.is_empty()
            && self.dirs_changed == 0
    }

    pub fn content_changed(&self) -> bool {
        !(self.modified.is_empty() && self.added.is_empty() && self.deleted.is_empty())
    }

    pub fn summary(&self, limit: usize) -> String {
        let mut parts = Vec::new();
        let mut list = |label: &str, v: &[SourceFile]| {
            if v.is_empty() {
                return;
            }
            let names: Vec<&str> = v.iter().take(limit).map(|f| f.path.as_str()).collect();
            let more = if v.len() > limit {
                format!(" (+{} more)", v.len() - limit)
            } else {
                String::new()
            };
            parts.push(format!("{label} {}: {}{more}", v.len(), names.join(", ")));
        };
        list("modified", &self.modified);
        list("added", &self.added);
        list("deleted", &self.deleted);
        if parts.is_empty() && !self.touched.is_empty() {
            parts.push(format!(
                "{} files touched without content change",
                self.touched.len()
            ));
        }
        parts.join("; ")
    }
}

enum FileState {
    Same,
    Touched,
    Modified,
    Deleted,
}

fn file_state(path: &std::path::Path, stat: Stat, hash: u128) -> FileState {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return FileState::Deleted;
    };
    if Stat::of(&meta) == stat {
        return FileState::Same;
    }
    let content = if meta.file_type().is_symlink() {
        std::fs::read_link(path).map(|t| t.as_os_str().as_encoded_bytes().to_vec())
    } else {
        std::fs::read(path)
    };
    match content {
        Ok(data) if xxh3_128(&data) == hash => FileState::Touched,
        Ok(_) => FileState::Modified,
        Err(_) => FileState::Deleted,
    }
}

struct Roots {
    src: Option<OwnedFd>,
    obj: Option<OwnedFd>,
}

impl Roots {
    fn open(layout: &Layout) -> Roots {
        let open = |p: &Path| {
            let mut buf = Vec::new();
            open_dir(libc::AT_FDCWD, p.as_os_str().as_encoded_bytes(), &mut buf)
        };
        Roots {
            src: open(&layout.srctree),
            obj: open(&layout.objtree),
        }
    }

    fn fd(&self, root: Root) -> Option<RawFd> {
        match root {
            Root::Src => self.src.as_ref(),
            Root::Obj => self.obj.as_ref(),
        }
        .map(AsRawFd::as_raw_fd)
    }
}

fn c_path<'b>(name: &[u8], buf: &'b mut Vec<u8>) -> Option<&'b CStr> {
    buf.clear();
    buf.extend_from_slice(if name.is_empty() { b"." } else { name });
    buf.push(0);
    CStr::from_bytes_with_nul(buf).ok()
}

fn open_dir(at: RawFd, rel: &[u8], buf: &mut Vec<u8>) -> Option<OwnedFd> {
    let path = c_path(rel, buf)?;
    let fd = unsafe {
        libc::openat(
            at,
            path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

fn stat_at(dir: RawFd, name: &[u8], follow: bool, buf: &mut Vec<u8>) -> Option<Stat> {
    let path = c_path(name, buf)?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    if unsafe { libc::fstatat(dir, path.as_ptr(), st.as_mut_ptr(), flags) } != 0 {
        return None;
    }
    let st = unsafe { st.assume_init() };
    Some(Stat {
        size: st.st_size as u64,
        mtime_ns: st.st_mtime * 1_000_000_000 + st.st_mtime_nsec,
        ctime_ns: st.st_ctime * 1_000_000_000 + st.st_ctime_nsec,
        ino: st.st_ino,
    })
}

const RUN: FileId = 512;

fn changed_files(index: &Index, layout: &Layout, roots: &Roots) -> Vec<(SourceFile, FileState)> {
    let end = index.id_end();
    (0..end.div_ceil(RUN))
        .into_par_iter()
        .flat_map_iter(|run| {
            let mut out = Vec::new();
            let mut buf = Vec::new();
            let mut open: Option<(Root, &str, Option<OwnedFd>)> = None;
            for id in run * RUN..((run + 1) * RUN).min(end) {
                let Some(f) = index.file(id).filter(|f| index.is_live(f.id)) else {
                    continue;
                };
                let root = f.root();
                let (dir, name) = f.path.rsplit_once('/').unwrap_or(("", f.path));
                if open
                    .as_ref()
                    .is_none_or(|(r, d, _)| *r != root || *d != dir)
                {
                    let fd = roots
                        .fd(root)
                        .and_then(|at| open_dir(at, dir.as_bytes(), &mut buf));
                    open = Some((root, dir, fd));
                }
                let fd = open.as_ref().and_then(|(_, _, fd)| fd.as_ref());
                let quick =
                    fd.and_then(|fd| stat_at(fd.as_raw_fd(), name.as_bytes(), false, &mut buf));
                if quick == Some(f.stat) {
                    continue;
                }
                match file_state(&layout.abs(root, f.path), f.stat, f.hash) {
                    FileState::Same => {}
                    st => out.push((
                        SourceFile {
                            root,
                            path: f.path.to_string(),
                        },
                        st,
                    )),
                }
            }
            out
        })
        .collect()
}

pub fn check(index: &Index, layout: &Layout) -> Changes {
    let roots = Roots::open(layout);
    let states = changed_files(index, layout, &roots);
    let mut changes = Changes::default();
    for (sf, st) in states {
        match st {
            FileState::Same => {}
            FileState::Touched => changes.touched.push(sf),
            FileState::Modified => changes.modified.push(sf),
            FileState::Deleted => changes.deleted.push(sf),
        }
    }
    let changed_dirs: Vec<(Root, String)> = index
        .par_dirs()
        .filter_map(|d| {
            let root = if d.flags & flag::OBJ_ROOT != 0 {
                Root::Obj
            } else {
                Root::Src
            };
            let mut buf = Vec::new();
            let st = match roots.fd(root) {
                Some(at) => stat_at(at, d.path.as_bytes(), true, &mut buf),
                None => std::fs::metadata(layout.abs(root, &d.path))
                    .ok()
                    .map(|m| Stat::of(&m)),
            }?;
            (st.mtime_ns != d.mtime_ns || st.ctime_ns != d.ctime_ns).then_some((root, d.path))
        })
        .collect();
    changes.dirs_changed = changed_dirs.len();
    let generated: Vec<String> = generated_dirs(layout).to_vec();
    let obj_root = layout.obj_root();
    let mut candidates: Vec<SourceFile> = changed_dirs
        .par_iter()
        .flat_map_iter(|(root, rel)| {
            let base = layout.abs(*root, rel);
            let entries: Vec<SourceFile> = std::fs::read_dir(&base)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_file() || t.is_symlink()))
                        .filter_map(|e| {
                            let name = e.file_name().to_str()?.to_string();
                            let path = if rel.is_empty() {
                                name.clone()
                            } else {
                                format!("{rel}/{name}")
                            };
                            let in_generated = *root == obj_root
                                && generated.iter().any(|g| path.starts_with(g.as_str()));
                            let relevant = in_generated
                                || FileClass::of_name(&name) != FileClass::Other
                                || (*root == obj_root && path == ".config");
                            (relevant && index.file_id(*root, &path).is_none())
                                .then_some(SourceFile { root: *root, path })
                        })
                        .collect()
                })
                .unwrap_or_default();
            entries
        })
        .collect();
    candidates.sort();
    candidates.dedup();
    let (always, ask): (Vec<SourceFile>, Vec<SourceFile>) = candidates.into_iter().partition(|c| {
        (c.root == obj_root && generated.iter().any(|g| c.path.starts_with(g.as_str())))
            || (c.root == obj_root && c.path == ".config")
            || c.root == Root::Obj
    });
    changes.added.extend(always);
    if !ask.is_empty() {
        let paths: Vec<String> = ask.iter().map(|c| c.path.clone()).collect();
        let ignored: FxHashSet<String> = git::check_ignore(&layout.srctree, &paths)
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();
        changes
            .added
            .extend(ask.into_iter().filter(|c| !ignored.contains(&c.path)));
    }
    changes
}
