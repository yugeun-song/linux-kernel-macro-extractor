use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::index::{Index, verify};
use crate::store::{FORMAT_VERSION, Manifest, Project, Store, abbrev, hash_hex};
use crate::tree::arch::Arch;
use crate::tree::{KernelTree, Layout};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    UpToDate,
    Edited,
    HeadMoved,
    ConfigChanged,
    OldFormat,
    Missing,
    BuildMissing,
    NotKernel,
    Incomplete,
    Unreadable,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::UpToDate => "up to date",
            State::Edited => "edited",
            State::HeadMoved => "HEAD moved",
            State::ConfigChanged => "config changed",
            State::OldFormat => "old format",
            State::Missing => "missing",
            State::BuildMissing => "build missing",
            State::NotKernel => "not a kernel",
            State::Incomplete => "incomplete",
            State::Unreadable => "unreadable",
        }
    }

    pub fn prunable(self) -> bool {
        matches!(
            self,
            State::Missing | State::BuildMissing | State::NotKernel | State::Incomplete
        )
    }
}

pub struct Status {
    pub state: State,
    pub detail: String,
}

fn status(state: State, detail: impl Into<String>) -> Status {
    Status {
        state,
        detail: detail.into(),
    }
}

pub fn status_of(project: &Project, manifest: Option<&Manifest>) -> Status {
    let Some(m) = manifest else {
        return status(
            State::Incomplete,
            "no usable manifest (indexing was interrupted)",
        );
    };
    if m.format != FORMAT_VERSION {
        return status(
            State::OldFormat,
            format!(
                "index format {}, this kmx uses {FORMAT_VERSION}; the next analysis rebuilds it",
                m.format
            ),
        );
    }
    if !m.srctree.is_dir() {
        return status(
            State::Missing,
            format!("{} no longer exists", m.srctree.display()),
        );
    }
    if m.objtree != m.srctree && !m.objtree.is_dir() {
        return status(
            State::BuildMissing,
            format!("build directory {} no longer exists", m.objtree.display()),
        );
    }
    let kt = match KernelTree::open(&m.srctree) {
        Ok(kt) => kt,
        Err(e) => return status(State::NotKernel, e.to_string()),
    };
    let refresh = "the next analysis updates the index";
    if kt.head.commit != m.head.commit {
        return status(
            State::HeadMoved,
            format!(
                "{} -> {}; {refresh}",
                abbrev(&m.head.commit),
                abbrev(&kt.head.commit)
            ),
        );
    }
    if let Ok(bytes) = std::fs::read(m.objtree.join(".config"))
        && hash_hex(&bytes) != m.config.hash
    {
        return status(
            State::ConfigChanged,
            format!(".config was edited; {refresh}"),
        );
    }
    let Some(gen_dir) = project.current() else {
        return status(State::Incomplete, "no current generation");
    };
    let Some(index) = Index::open(&gen_dir, &m.base, m.overlay.as_deref()) else {
        return status(
            State::Unreadable,
            "index files are unreadable; the next analysis rebuilds them",
        );
    };
    let layout = Layout {
        srctree: m.srctree.clone(),
        objtree: m.objtree.clone(),
        srcarch: Arch::ALL
            .iter()
            .find(|a| a.name() == m.config.arch)
            .map(|a| a.srcarch().to_string())
            .unwrap_or_default(),
    };
    let changes = verify::check(&index, &layout);
    if changes.content_changed() {
        status(State::Edited, format!("{}; {refresh}", changes.summary(5)))
    } else if changes.is_clean() {
        status(State::UpToDate, "")
    } else {
        status(
            State::UpToDate,
            "file timestamps changed, contents identical",
        )
    }
}

pub fn looks_like_id(arg: &str) -> bool {
    arg.len() >= 4 && arg.bytes().all(|c| c.is_ascii_hexdigit())
}

fn by_id<'a>(
    projects: &'a [Project],
    arg: &str,
) -> std::result::Result<Option<&'a Project>, String> {
    let prefix = arg.to_ascii_lowercase();
    let found: Vec<&Project> = projects
        .iter()
        .filter(|p| p.id().0.starts_with(&prefix))
        .collect();
    match found.as_slice() {
        [] => Ok(None),
        [p] => Ok(Some(*p)),
        many => {
            let ids: Vec<&str> = many
                .iter()
                .map(|p| p.id().label(projects.iter().map(Project::id)))
                .collect();
            Err(format!("the ID prefix is ambiguous: {}", ids.join(", ")))
        }
    }
}

pub fn tree_of_id(arg: &Path) -> std::result::Result<Option<(PathBuf, PathBuf)>, String> {
    if arg.exists() {
        return Ok(None);
    }
    let Some(text) = arg.to_str().filter(|t| looks_like_id(t)) else {
        return Ok(None);
    };
    let Ok(store) = Store::open() else {
        return Ok(None);
    };
    let projects = store.projects().unwrap_or_default();
    let Some(p) = by_id(&projects, text)? else {
        return Ok(None);
    };
    let m = p.manifest().ok_or_else(|| {
        format!(
            "index {} has no readable manifest; rebuild it from its tree path",
            p.id().label(projects.iter().map(Project::id))
        )
    })?;
    Ok(Some((m.srctree, m.objtree)))
}

pub fn match_projects<'a>(
    projects: &'a [Project],
    arg: &str,
) -> std::result::Result<Vec<&'a Project>, String> {
    if looks_like_id(arg)
        && let Some(p) = by_id(projects, arg)?
    {
        return Ok(vec![p]);
    }
    let given = PathBuf::from(arg);
    let path = given
        .canonicalize()
        .or_else(|_| std::path::absolute(&given))
        .unwrap_or(given);
    let path = crate::pp::kbuild::normalize(&path);
    Ok(projects
        .iter()
        .filter(|p| {
            p.manifest()
                .is_some_and(|m| m.srctree == path || m.objtree == path)
        })
        .collect())
}
