use std::path::Path;

use anstyle::Style;
use rayon::prelude::*;
use serde::Serialize;

use super::Ctx;
use crate::cli::Global;
use crate::error::{Error, IoContext, Result, exit};
use crate::projects::{State, Status, match_projects, status_of};
use crate::render::style::{self, paint};
use crate::render::{Output, human, table};
use crate::store::{Manifest, Memo, Project, ProjectId, Store, abbrev, now_secs};
use crate::workspace::{OpenOptions, Workspace};

const HEADLINE: [usize; 4] = [0, 1, 4, 5];

fn state_style(state: State) -> Style {
    match state {
        State::UpToDate => style::OK,
        State::Edited | State::HeadMoved | State::ConfigChanged | State::OldFormat => style::WARN,
        _ => style::BAD,
    }
}

fn content_of(m: &Manifest) -> String {
    match &m.content {
        Some(c) => {
            let edited = m.dirty.iter().any(|d| d.code != "??");
            format!("{}{}", abbrev(c), if edited { "+" } else { "" })
        }
        None => "-".to_string(),
    }
}

#[derive(Serialize)]
struct Entry<'a> {
    id: &'a str,
    #[serde(flatten)]
    manifest: Option<&'a Manifest>,
    size_bytes: u64,
    memoized: usize,
    state: State,
    state_detail: &'a str,
}

struct Row {
    project: Project,
    manifest: Option<Manifest>,
    status: Status,
    size: u64,
}

fn rows(projects: Vec<Project>) -> Vec<Row> {
    let mut rows: Vec<Row> = projects
        .into_par_iter()
        .map(|project| {
            let mut manifest = project.manifest();
            if let Some(m) = manifest.as_mut() {
                crate::workspace::backfill(&project, m);
            }
            let status = status_of(&project, manifest.as_ref());
            let size = project.size_bytes();
            Row {
                project,
                manifest,
                status,
                size,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        let key = |r: &Row| {
            (
                r.manifest.is_none(),
                r.manifest.as_ref().map(|m| m.srctree.clone()),
                r.project.id().0.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
    rows
}

fn memoized(project: &Project) -> usize {
    project
        .current()
        .map(|g| Memo::new(&g).count())
        .unwrap_or(0)
}

fn print_json(
    project: &Project,
    manifest: Option<&Manifest>,
    st: &Status,
    size: u64,
) -> Result<()> {
    let entry = Entry {
        id: &project.id().0,
        manifest,
        size_bytes: size,
        memoized: memoized(project),
        state: st.state,
        state_detail: &st.detail,
    };
    let line = serde_json::to_string(&entry).map_err(|e| Error::Internal(e.to_string()))?;
    crate::render::print(&format!("{line}\n"));
    Ok(())
}

pub fn ls(g: &Global) -> Result<i32> {
    let store = Store::open().at("kmx data directory")?;
    let rows = rows(store.projects().at(store.root())?);
    if g.json {
        for row in &rows {
            print_json(&row.project, row.manifest.as_ref(), &row.status, row.size)?;
        }
        return Ok(exit::OK);
    }
    let now = now_secs();
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let state = paint(r.status.state.label(), state_style(r.status.state));
            let id = r
                .project
                .id()
                .label(rows.iter().map(|o| o.project.id()))
                .to_string();
            match &r.manifest {
                Some(m) => {
                    let mut tree = human::tilde(&m.srctree);
                    if m.objtree != m.srctree {
                        tree.push_str(&format!("  (O={})", human::tilde(&m.objtree)));
                    }
                    vec![
                        id,
                        m.kernel.release(),
                        abbrev(&m.head.commit).to_string(),
                        content_of(m),
                        m.config.arch.clone(),
                        state,
                        human::ago(m.refreshed.unwrap_or(m.created), now),
                        human::size(r.size),
                        tree,
                    ]
                }
                None => vec![
                    id,
                    "-".into(),
                    "-".into(),
                    "-".into(),
                    "-".into(),
                    state,
                    "-".into(),
                    human::size(r.size),
                    "-".into(),
                ],
            }
        })
        .collect();
    let header = [
        "ID",
        "KERNEL",
        "COMMIT HASH",
        "CONTENT",
        "ARCH",
        "STATE",
        "INDEXED",
        "SIZE",
        "TREE",
    ];
    let wide = table::wide(&header, &body);
    let fits =
        crate::render::columns().is_none_or(|cols| wide.lines().all(|l| style::width(l) <= cols));
    let mut text = if fits {
        wide
    } else {
        table::narrow(&header, &body, &HEADLINE, &crate::render::glyphs(g))
    };
    let total: u64 = rows.iter().map(|r| r.size).sum();
    let n = rows.len();
    let prunable = rows.iter().filter(|r| r.status.state.prunable()).count();
    text.push_str(&format!(
        "{n} kernel tree{}, {} in {}",
        if n == 1 { "" } else { "s" },
        human::size(total),
        human::tilde(store.root())
    ));
    if prunable > 0 {
        text.push_str(&format!("; {prunable} can be removed with kmx prune"));
    }
    text.push('\n');
    Output::open(g).emit(&text);
    Ok(exit::OK)
}

fn resolve_targets<'a>(
    projects: &'a [Project],
    args: &[String],
    verb: &str,
) -> Result<Vec<&'a Project>> {
    let mut chosen: Vec<&Project> = Vec::new();
    for arg in args {
        let matched = match_projects(projects, arg)
            .map_err(|why| Error::Usage(format!("{verb} {arg}: {why}")))?;
        if matched.is_empty() {
            return Err(Error::Usage(format!(
                "{verb} {arg}: no indexed kernel tree has this ID or path (see kmx ls)"
            )));
        }
        for p in matched {
            if !chosen.iter().any(|c| c.id() == p.id()) {
                chosen.push(p);
            }
        }
    }
    Ok(chosen)
}

fn label_of(p: &Project) -> String {
    p.manifest()
        .map(|m| format!("{} ({})", human::tilde(&m.srctree), m.kernel.release()))
        .unwrap_or_else(|| "(incomplete)".to_string())
}

pub fn rm(targets: &[String]) -> Result<i32> {
    let store = Store::open().at("kmx data directory")?;
    let projects = store.projects().at(store.root())?;
    for p in resolve_targets(&projects, targets, "rm")? {
        let label = label_of(p);
        let id = p.id().label(projects.iter().map(Project::id)).to_string();
        p.remove().at(p.dir())?;
        crate::render::print(&format!("removed {id}  {label}\n"));
    }
    Ok(exit::OK)
}

pub fn prune(ctx: &Ctx) -> Result<i32> {
    let store = Store::open().at("kmx data directory")?;
    let mut freed = 0;
    let mut removed = 0;
    let rows = rows(store.projects().at(store.root())?);
    let ids: Vec<ProjectId> = rows.iter().map(|r| r.project.id().clone()).collect();
    for row in rows {
        if !row.status.state.prunable() {
            continue;
        }
        let label = label_of(&row.project);
        let id = row.project.id().label(&ids).to_string();
        row.project.remove().at(row.project.dir())?;
        freed += row.size;
        removed += 1;
        crate::render::print(&format!(
            "removed {id}  {label}  ({}: {})\n",
            row.status.state.label(),
            row.status.detail
        ));
    }
    if removed == 0 {
        ctx.notify("kmx: nothing to prune");
    } else {
        crate::render::print(&format!("freed {}\n", human::size(freed)));
    }
    Ok(exit::OK)
}

pub fn inspect(targets: &[String], g: &Global) -> Result<i32> {
    let store = Store::open().at("kmx data directory")?;
    let projects = store.projects().at(store.root())?;
    let chosen = resolve_targets(&projects, targets, "inspect")?;
    let mut text = String::new();
    for (i, p) in chosen.into_iter().enumerate() {
        let mut manifest = p.manifest();
        if let Some(m) = manifest.as_mut() {
            crate::workspace::backfill(p, m);
        }
        let st = status_of(p, manifest.as_ref());
        if g.json {
            print_json(p, manifest.as_ref(), &st, p.size_bytes())?;
            continue;
        }
        if i > 0 {
            text.push('\n');
        }
        match &manifest {
            Some(m) => text.push_str(&describe_block(p, m)),
            None => text.push_str(&format!("{}  (incomplete)\n", p.id().0)),
        }
        let mut line = paint(st.state.label(), state_style(st.state));
        if !st.detail.is_empty() {
            line.push_str(&format!(": {}", st.detail));
        }
        text.push_str(&format!("  state     {line}\n"));
    }
    if !g.json {
        Output::open(g).emit(&text);
    }
    Ok(exit::OK)
}

pub fn describe_block(project: &Project, m: &Manifest) -> String {
    let mut s = String::new();
    s.push_str(&format!("{}  {}\n", m.id.0, m.srctree.display()));
    if m.objtree != m.srctree {
        s.push_str(&format!("  build     {} (O=)\n", m.objtree.display()));
    }
    let reference = m
        .head
        .reference
        .as_deref()
        .map(|r| format!(" ({})", r.trim_start_matches("refs/heads/")))
        .unwrap_or_else(|| " (detached)".to_string());
    s.push_str(&format!(
        "  kernel    {} \u{f126} {}{}{}\n",
        m.kernel.release(),
        m.head.commit,
        reference,
        if m.head.shallow {
            ", shallow clone"
        } else {
            ""
        }
    ));
    if let Some(c) = &m.content {
        s.push_str(&format!("  content   {c} (git tree of HEAD)\n"));
    }
    let worktree = if m.dirty.is_empty() {
        "clean".to_string()
    } else {
        let names: Vec<&str> = m.dirty.iter().take(4).map(|d| d.path.as_str()).collect();
        let more = if m.dirty.len() > 4 {
            format!(", +{} more", m.dirty.len() - 4)
        } else {
            String::new()
        };
        format!("{} changed ({}{more})", m.dirty.len(), names.join(", "))
    };
    s.push_str(&format!("  worktree  {worktree}\n"));
    s.push_str(&format!(
        "  config    {} {} .config {}\n",
        m.config.arch,
        m.config.compiler,
        abbrev(&m.config.hash)
    ));
    s.push_str(&format!(
        "  index     {} files, {} names, {} definitions, {} on disk, {} memoized results, generation {}{}\n",
        m.stats.files,
        m.stats.names,
        m.stats.definitions,
        human::size(project.size_bytes()),
        memoized(project),
        m.generation,
        if m.overlay.is_some() { " (+overlay)" } else { "" }
    ));
    let now = now_secs();
    s.push_str(&format!(
        "  dates     indexed {} ({}){}\n",
        human::local_time(m.created),
        human::ago(m.created, now),
        m.refreshed
            .map(|r| format!(
                ", refreshed {} ({})",
                human::local_time(r),
                human::ago(r, now)
            ))
            .unwrap_or_default()
    ));
    s.push_str(&format!("  store     {}\n", project.dir().display()));
    s
}

pub fn index(tree: &Path, mut opts: OpenOptions, ctx: &Ctx) -> Result<i32> {
    opts.rebuild = true;
    let mut notify = |m: &str| ctx.notify(m);
    let ws = Workspace::open(tree, &opts, &mut notify)?;
    crate::render::print(&describe_block(&ws.project, &ws.manifest));
    Ok(exit::OK)
}
