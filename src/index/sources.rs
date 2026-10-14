use std::path::{Path, PathBuf};

use rustc_hash::FxHashSet;

use super::path_key;
use crate::tree::git::{self, GitError, StatusEntry};
use crate::tree::{Layout, Root};

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(
        "{tree}: a kernel tree must hold exactly one git tree, but nested repositories were found: {list}"
    )]
    Nested { tree: PathBuf, list: String },
    #[error("{tree}: not a Linux kernel tree: git does not track {list}")]
    Untracked { tree: PathBuf, list: String },
    #[error("{0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceFile {
    pub root: Root,
    pub path: String,
}

pub struct FileList {
    pub files: Vec<SourceFile>,
    pub dirty: Vec<StatusEntry>,
}

pub fn generated_dirs(layout: &Layout) -> [String; 2] {
    [
        "include/generated".to_string(),
        format!("arch/{}/include/generated", layout.srcarch),
    ]
}

fn walk_rel(base: &Path, rel: &str, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(base.join(rel)) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let child = format!("{rel}/{name}");
        match e.file_type() {
            Ok(t) if t.is_dir() => walk_rel(base, &child, out),
            Ok(t) if t.is_file() => out.push(child),
            _ => {}
        }
    }
}

pub fn collect(layout: &Layout) -> Result<FileList, BuildError> {
    let root = &layout.srctree;
    let staged = git::run(root, &["ls-files", "-s", "-z"])?;
    let mut nested = Vec::new();
    let mut set: FxHashSet<SourceFile> = FxHashSet::default();
    let mut tracked = [false; crate::tree::EVIDENCE.len()];
    for rec in staged.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let rec = String::from_utf8_lossy(rec);
        let Some((meta, path)) = rec.split_once('\t') else {
            continue;
        };
        if meta.starts_with("160000 ") {
            nested.push(path.to_string());
            continue;
        }
        if let Some(i) = crate::tree::EVIDENCE.iter().position(|e| *e == path) {
            tracked[i] = true;
        }
        set.insert(SourceFile {
            root: Root::Src,
            path: path.to_string(),
        });
    }
    let untracked: Vec<&str> = crate::tree::EVIDENCE
        .iter()
        .zip(tracked)
        .filter(|(_, t)| !t)
        .map(|(e, _)| *e)
        .collect();
    if !untracked.is_empty() {
        return Err(BuildError::Untracked {
            tree: root.clone(),
            list: untracked.join(", "),
        });
    }
    let dirty = git::status(root)?;
    for e in &dirty {
        if e.code == "??" {
            if e.path.ends_with('/') {
                nested.push(e.path.clone());
            } else {
                set.insert(SourceFile {
                    root: Root::Src,
                    path: e.path.clone(),
                });
            }
        }
    }
    if !nested.is_empty() {
        nested.sort();
        return Err(BuildError::Nested {
            tree: root.clone(),
            list: nested.join(", "),
        });
    }
    let obj_root = layout.obj_root();
    for dir in generated_dirs(layout) {
        let mut found = Vec::new();
        walk_rel(&layout.objtree, &dir, &mut found);
        set.extend(found.into_iter().map(|path| SourceFile {
            root: obj_root,
            path,
        }));
    }
    if layout.objtree.join(".config").is_file() {
        set.insert(SourceFile {
            root: obj_root,
            path: ".config".to_string(),
        });
    }
    let mut files: Vec<SourceFile> = set.into_iter().collect();
    files.sort_by_key(|a| path_key(a.root, &a.path));
    Ok(FileList { files, dirty })
}
