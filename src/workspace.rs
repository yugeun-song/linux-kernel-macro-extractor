use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use rustc_hash::FxHashMap;

use crate::analysis::build_graph::BuildGraph;
use crate::analysis::kconf::InfoTable;
use crate::error::{Error, IoContext, Result};
use crate::index::verify::{self, Changes};
use crate::index::{FileId, Index, build, sources};
use crate::pp::files::Sources;
use crate::store::{self, ConfigInfo, FORMAT_VERSION, Manifest, Memo, Project, Store, abbrev};
use crate::timing::Timer;
use crate::tree::KernelTree;
use crate::tree::Layout;
use crate::tree::arch::{Compiler, Target};
use crate::tree::dot_config::DotConfig;
use crate::tree::git;

#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    pub build_dir: Option<PathBuf>,
    pub refresh: bool,
    pub compiler: Option<Compiler>,
    pub rebuild: bool,
}

pub struct Workspace {
    pub tree: KernelTree,
    pub layout: Layout,
    pub config: DotConfig,
    pub config_bytes: Vec<u8>,
    pub target: Target,
    pub compiler: Compiler,
    pub store: Store,
    pub project: Project,
    pub manifest: Manifest,
    pub index: Index,
    pub generation: PathBuf,
    pub(crate) build_graph: OnceLock<Option<BuildGraph>>,
    build_identity: OnceLock<String>,
    pub(crate) kconfig_table: OnceLock<Option<InfoTable>>,
    pub(crate) definitions: Mutex<FxHashMap<(FileId, u32), String>>,
    pub(crate) sources: Sources,
    pub(crate) setups: Mutex<FxHashMap<String, std::sync::Arc<crate::analysis::context::Setup>>>,
    cache_dir: OnceLock<PathBuf>,
}

pub const CACHE_DIR: &str = "cache";
const BASE: &str = "base.idx";
const OVERLAY: &str = "overlay.idx";
const CONFIG_SNAPSHOT: &str = "config.snapshot";
const BUILD_ARTIFACTS: [&str; 6] = [
    "vmlinux.a",
    "vmlinux",
    "modules.order",
    "include/config/auto.conf",
    ".config",
    "include/generated/compile.h",
];

pub fn backfill(project: &Project, m: &mut Manifest) {
    if m.content.is_some() {
        return;
    }
    let Some(tree) = git::tree_of(&m.srctree, &m.head.commit) else {
        return;
    };
    m.content = Some(tree);
    if let (Some(dir), Ok(bytes)) = (project.current(), serde_json::to_vec_pretty(&*m)) {
        let _ = store::write_atomic(&dir.join("manifest.json"), &bytes);
    }
}

fn describe(m: &Manifest) -> String {
    format!(
        "{} @ {}{}",
        m.kernel.release(),
        abbrev(&m.head.commit),
        m.head
            .reference
            .as_deref()
            .map(|r| format!(" ({})", r.trim_start_matches("refs/heads/")))
            .unwrap_or_default()
    )
}

fn config_diff(old: &DotConfig, new: &DotConfig, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    for (name, e) in new.iter() {
        match old.get(name) {
            Some(o) if o.value == e.value => {}
            Some(o) => out.push(format!("CONFIG_{name}: {} -> {}", o.value, e.value)),
            None => out.push(format!("CONFIG_{name}: (absent) -> {}", e.value)),
        }
    }
    for (name, e) in old.iter() {
        if new.get(name).is_none() {
            out.push(format!("CONFIG_{name}: {} -> (absent)", e.value));
        }
    }
    let total = out.len();
    out.truncate(limit);
    if total > limit {
        out.push(format!("... {} more", total - limit));
    }
    out
}

impl Workspace {
    fn prepare(tree_arg: &Path, build_dir: Option<&Path>, claim: bool) -> Result<(Pending, Store)> {
        let tree = KernelTree::open(tree_arg)?;
        let objtree = match build_dir {
            Some(d) => d
                .canonicalize()
                .map_err(|_| Error::Usage(format!("{}: no such build directory", d.display())))?,
            None => tree.root.clone(),
        };
        let config_path = objtree.join(".config");
        let config_bytes = fs::read(&config_path).map_err(|_| {
            Error::Config(format!(
                "{}: no .config here; configure the kernel first (e.g. make defconfig) or pass -O <build dir>",
                objtree.display()
            ))
        })?;
        let mut config = DotConfig::parse(&String::from_utf8_lossy(&config_bytes));
        config.path = config_path;
        let target = Target::detect(&config)?;
        let mut compiler = Compiler::from_config(&config);
        if compiler.assumed
            && let Some(recorded) = fs::read_to_string(objtree.join("include/generated/compile.h"))
                .ok()
                .and_then(|t| Compiler::from_compile_h(&t))
        {
            compiler = recorded;
        }
        let layout = Layout {
            srctree: tree.root.clone(),
            objtree: objtree.clone(),
            srcarch: target.arch.srcarch().to_string(),
        };
        let store = Store::open().at("kmx data directory")?;
        let project = store.locate(&tree.root, &objtree, claim).at(store.root())?;
        let config_hash = store::hash_hex(&config_bytes);
        let pending = Pending {
            tree,
            layout,
            config,
            config_bytes,
            config_hash,
            target,
            compiler,
            project,
        };
        Ok((pending, store))
    }

    pub fn open_unverified(tree_arg: &Path, build_dir: Option<&Path>) -> Option<Workspace> {
        let (ws, store) = Workspace::prepare(tree_arg, build_dir, false).ok()?;
        let (gen_dir, manifest) = ws.project.current().zip(ws.project.manifest())?;
        if manifest.format != FORMAT_VERSION {
            return None;
        }
        let index = Index::open(&gen_dir, &manifest.base, manifest.overlay.as_deref())?;
        Some(ws.finish(store, manifest, index, gen_dir))
    }

    pub fn open(
        tree_arg: &Path,
        opts: &OpenOptions,
        notify: &mut dyn FnMut(&str),
    ) -> Result<Workspace> {
        let (mut ws, store) = Workspace::prepare(tree_arg, opts.build_dir.as_deref(), true)?;
        if let Some(c) = &opts.compiler {
            ws.compiler = c.clone();
        }
        let mut rebuild = opts.rebuild;
        for _ in 0..4 {
            let current = ws.project.current().zip(ws.project.manifest());
            let Some((gen_dir, mut manifest)) =
                current.filter(|(_, m)| m.format == FORMAT_VERSION && !rebuild)
            else {
                let _lock = ws.project.lock().at(ws.project.dir())?;
                let fresh = ws.project.manifest();
                let someone_else_built =
                    !rebuild && fresh.as_ref().is_some_and(|m| m.format == FORMAT_VERSION);
                if !someone_else_built {
                    ws.full_build(None, notify)?;
                }
                rebuild = false;
                continue;
            };
            backfill(&ws.project, &mut manifest);
            let drift = ws.drift(&manifest, &gen_dir);
            ws.refuse_drift(opts, &manifest, drift.as_ref())?;
            let Some(index) = Index::open(&gen_dir, &manifest.base, manifest.overlay.as_deref())
            else {
                notify("kmx: index files are unreadable; rebuilding");
                rebuild = true;
                continue;
            };
            let mut timer = Timer::new();
            let changes = verify::check(&index, &ws.layout);
            timer.mark("verify (stat all files)");
            if changes.is_clean() && drift.is_none() {
                return Ok(ws.finish(store, manifest, index, gen_dir));
            }
            ws.refuse_if_frozen(opts, &manifest, &changes)?;
            let _lock = ws.project.lock().at(ws.project.dir())?;
            let (gen_dir, manifest, index, changes, drift) = if ws.project.current().as_deref()
                == Some(gen_dir.as_path())
            {
                (gen_dir, manifest, index, changes, drift)
            } else {
                let Some((fresh_dir, m)) = ws.project.current().zip(ws.project.manifest()) else {
                    continue;
                };
                let d = ws.drift(&m, &fresh_dir);
                ws.refuse_drift(opts, &m, d.as_ref())?;
                let Some(i) = Index::open(&fresh_dir, &m.base, m.overlay.as_deref()) else {
                    rebuild = true;
                    continue;
                };
                let c = verify::check(&i, &ws.layout);
                if c.is_clean() && d.is_none() {
                    return Ok(ws.finish(store, m, i, fresh_dir));
                }
                ws.refuse_if_frozen(opts, &m, &c)?;
                (fresh_dir, m, i, c, d)
            };
            ws.refresh(
                &manifest,
                &gen_dir,
                &index,
                &changes,
                drift.as_ref(),
                notify,
            )?;
            if let Some((fresh_dir, m)) = ws.project.current().zip(ws.project.manifest())
                && let Some(index) = Index::open(&fresh_dir, &m.base, m.overlay.as_deref())
            {
                return Ok(ws.finish(store, m, index, fresh_dir));
            }
        }
        Err(Error::Internal(
            "the index kept changing underneath; another kmx process may be rebuilding it"
                .to_string(),
        ))
    }

    pub fn memo(&self) -> Memo {
        Memo::new(&self.generation).salted(self.build_identity())
    }

    pub fn build_identity(&self) -> &str {
        self.build_identity.get_or_init(|| {
            BUILD_ARTIFACTS
                .map(|f| store::file_identity(&self.layout.objtree.join(f)))
                .join("\u{1f}")
        })
    }

    pub fn cache_dir(&self) -> &Path {
        self.cache_dir
            .get_or_init(|| store::tagged_dir(&self.generation.join(CACHE_DIR)))
    }

    pub fn locator(&self) -> crate::pp::files::Locator<'_> {
        crate::pp::files::Locator {
            index: &self.index,
            layout: &self.layout,
            sources: &self.sources,
        }
    }

    pub fn file_bytes(&self, abs: &Path) -> Option<&[u8]> {
        self.sources.bytes(&self.locator(), abs)
    }

    pub fn abs(&self, view: &crate::index::FileView<'_>) -> PathBuf {
        self.layout.abs(view.root(), view.path)
    }
}

struct Pending {
    tree: KernelTree,
    layout: Layout,
    config: DotConfig,
    config_bytes: Vec<u8>,
    config_hash: String,
    target: Target,
    compiler: Compiler,
    project: Project,
}

impl Pending {
    fn finish(
        self,
        store: Store,
        manifest: Manifest,
        index: Index,
        generation: PathBuf,
    ) -> Workspace {
        Workspace {
            tree: self.tree,
            layout: self.layout,
            config: self.config,
            config_bytes: self.config_bytes,
            target: self.target,
            compiler: self.compiler,
            store,
            project: self.project,
            manifest,
            index,
            generation,
            build_graph: OnceLock::new(),
            build_identity: OnceLock::new(),
            kconfig_table: OnceLock::new(),
            definitions: Mutex::new(FxHashMap::default()),
            sources: Sources::default(),
            setups: Mutex::new(FxHashMap::default()),
            cache_dir: OnceLock::new(),
        }
    }

    fn refuse_if_frozen(&self, opts: &OpenOptions, m: &Manifest, changes: &Changes) -> Result<()> {
        if opts.refresh || !changes.content_changed() {
            return Ok(());
        }
        Err(Error::Mismatch {
            summary: format!(
                "{}: the tree was edited since it was indexed ({})",
                self.tree.root.display(),
                changes.summary(5)
            ),
            details: vec![
                format!("indexed: {}", describe(m)),
                "rerun without --norefresh to update the index automatically".to_string(),
            ],
        })
    }

    fn drift(&self, m: &Manifest, gen_dir: &Path) -> Option<Drift> {
        let head = (self.tree.head.commit != m.head.commit)
            .then(|| (m.head.commit.clone(), self.tree.head.commit.clone()));
        let version = (self.tree.version != m.kernel)
            .then(|| (m.kernel.release(), self.tree.version.release()));
        let config = (self.config_hash != m.config.hash).then(|| {
            let old = fs::read(gen_dir.join(CONFIG_SNAPSHOT))
                .map(|b| DotConfig::parse(&String::from_utf8_lossy(&b)))
                .unwrap_or_default();
            config_diff(&old, &self.config, 20)
        });
        let arch = m.config.arch != self.target.arch.name();
        (head.is_some() || version.is_some() || config.is_some() || arch).then_some(Drift {
            head,
            version,
            config,
            arch,
        })
    }

    fn refuse_drift(&self, opts: &OpenOptions, m: &Manifest, drift: Option<&Drift>) -> Result<()> {
        let Some(d) = drift.filter(|_| !opts.refresh) else {
            return Ok(());
        };
        let hint = "rerun without --norefresh to update the index".to_string();
        if let Some((old, new)) = &d.head {
            return Err(Error::Mismatch {
                summary: format!(
                    "{}: HEAD moved since it was indexed ({} -> {})",
                    self.tree.root.display(),
                    abbrev(old),
                    abbrev(new)
                ),
                details: vec![
                    format!("indexed: {}", describe(m)),
                    format!(
                        "now:     {} @ {}",
                        self.tree.version.release(),
                        abbrev(&self.tree.head.commit)
                    ),
                    hint,
                ],
            });
        }
        if let Some((old, new)) = &d.version {
            return Err(Error::Mismatch {
                summary: format!(
                    "{}: kernel version changed since it was indexed ({old} -> {new})",
                    self.tree.root.display()
                ),
                details: vec![hint],
            });
        }
        let mut details = d.config.clone().unwrap_or_default();
        details.push(hint);
        Err(Error::Mismatch {
            summary: format!(
                "{}: .config changed since it was indexed",
                self.config.path.display()
            ),
            details,
        })
    }

    fn manifest_for(
        &self,
        generation: u32,
        stats: store::IndexStats,
        dirty: Vec<git::StatusEntry>,
    ) -> Manifest {
        Manifest {
            format: FORMAT_VERSION,
            kmx_version: env!("CARGO_PKG_VERSION").to_string(),
            id: self.project.id().clone(),
            srctree: self.layout.srctree.clone(),
            objtree: self.layout.objtree.clone(),
            kernel: self.tree.version.clone(),
            head: self.tree.head.clone(),
            content: git::head_tree(&self.layout.srctree),
            dirty,
            config: ConfigInfo {
                path: self.config.path.clone(),
                hash: self.config_hash.clone(),
                arch: self.target.arch.name().to_string(),
                compiler: self.compiler.to_string(),
            },
            stats,
            created: store::now_secs(),
            refreshed: None,
            generation,
            base: BASE.to_string(),
            overlay: None,
        }
    }

    fn full_build(&self, previous: Option<&Manifest>, notify: &mut dyn FnMut(&str)) -> Result<()> {
        let t = Instant::now();
        notify(&format!(
            "kmx: indexing {} ({} @ {}, {})",
            self.tree.root.display(),
            self.tree.version.release(),
            abbrev(&self.tree.head.commit),
            self.target.arch
        ));
        let mut timer = Timer::new();
        let list = sources::collect(&self.layout)?;
        timer.mark("collect (git)");
        let built = build::build(&self.layout, &list.files, &list.files, 0, &[]);
        let gen_ = self.project.begin_generation().at(self.project.dir())?;
        fs::write(gen_.path(BASE), &built.bytes).at(gen_.path(BASE))?;
        fs::write(gen_.path(CONFIG_SNAPSHOT), &self.config_bytes).at(gen_.path(CONFIG_SNAPSHOT))?;
        let mut manifest = self.manifest_for(gen_.number, built.stats.clone(), list.dirty);
        if let Some(prev) = previous {
            manifest.created = prev.created;
            manifest.refreshed = Some(store::now_secs());
        }
        let json =
            serde_json::to_vec_pretty(&manifest).map_err(|e| Error::Internal(e.to_string()))?;
        fs::write(gen_.path("manifest.json"), json).at(gen_.path("manifest.json"))?;
        self.project.publish(gen_).at(self.project.dir())?;
        for f in built.failed.iter().take(5) {
            notify(&format!(
                "kmx: warning: could not parse {f}; its definitions are missing"
            ));
        }
        notify(&format!(
            "kmx: indexed {} files, {} names in {:.1}s",
            built.stats.files,
            built.stats.names,
            t.elapsed().as_secs_f64()
        ));
        Ok(())
    }

    fn refresh(
        &self,
        old: &Manifest,
        gen_dir: &Path,
        index: &Index,
        changes: &Changes,
        drift: Option<&Drift>,
        notify: &mut dyn FnMut(&str),
    ) -> Result<()> {
        let t = Instant::now();
        let what = describe_change(changes, drift);
        if drift.is_some_and(|d| d.arch) {
            notify(&format!(
                "kmx: {what}; the architecture changed, rebuilding the index"
            ));
            return self.full_build(Some(old), notify);
        }
        let content_changed = changes.content_changed();
        let config_changed = self.config_hash != old.config.hash;
        let gen_ = if changes.is_clean() {
            let gen_ = self.project.begin_generation().at(self.project.dir())?;
            link_or_copy(&gen_dir.join(&old.base), &gen_.path(&old.base))?;
            if let Some(overlay) = &old.overlay {
                link_or_copy(&gen_dir.join(overlay), &gen_.path(overlay))?;
            }
            gen_
        } else {
            let Some(plan) = build::plan_overlay(index, changes) else {
                notify(&format!(
                    "kmx: {what}; too many changes for an incremental update, rebuilding"
                ));
                return self.full_build(Some(old), notify);
            };
            let built = build::build(
                &self.layout,
                &plan.files,
                &plan.all_files,
                index.base_file_count(),
                &plan.tombstones,
            );
            let gen_ = self.project.begin_generation().at(self.project.dir())?;
            link_or_copy(&gen_dir.join(&old.base), &gen_.path(BASE))?;
            fs::write(gen_.path(OVERLAY), &built.bytes).at(gen_.path(OVERLAY))?;
            gen_
        };
        if config_changed {
            fs::write(gen_.path(CONFIG_SNAPSHOT), &self.config_bytes)
                .at(gen_.path(CONFIG_SNAPSHOT))?;
        } else {
            link_or_copy(&gen_dir.join(CONFIG_SNAPSHOT), &gen_.path(CONFIG_SNAPSHOT))?;
        }
        let tag = store::build_tag();
        if !content_changed && !config_changed {
            link_entries(
                &gen_dir.join("memo").join(tag),
                &gen_.path("memo").join(tag),
            );
        }
        link_entries(
            &gen_dir.join(CACHE_DIR).join(tag),
            &gen_.path(CACHE_DIR).join(tag),
        );
        let mut manifest = old.clone();
        manifest.generation = gen_.number;
        manifest.refreshed = Some(store::now_secs());
        if !changes.is_clean() {
            manifest.base = BASE.to_string();
            manifest.overlay = Some(OVERLAY.to_string());
        }
        manifest.kernel = self.tree.version.clone();
        manifest.config = ConfigInfo {
            path: self.config.path.clone(),
            hash: self.config_hash.clone(),
            arch: self.target.arch.name().to_string(),
            compiler: self.compiler.to_string(),
        };
        let head_moved = self.tree.head != old.head;
        if head_moved {
            manifest.head = self.tree.head.clone();
            manifest.content = git::head_tree(&self.layout.srctree);
        }
        if content_changed || head_moved {
            manifest.dirty = git::status(&self.layout.srctree).unwrap_or(manifest.dirty);
        }
        let json =
            serde_json::to_vec_pretty(&manifest).map_err(|e| Error::Internal(e.to_string()))?;
        fs::write(gen_.path("manifest.json"), json).at(gen_.path("manifest.json"))?;
        self.project.publish(gen_).at(self.project.dir())?;
        if content_changed || drift.is_some() {
            notify(&format!(
                "kmx: {what}; index updated in {} ms",
                t.elapsed().as_millis()
            ));
        }
        Ok(())
    }
}

struct Drift {
    head: Option<(String, String)>,
    version: Option<(String, String)>,
    config: Option<Vec<String>>,
    arch: bool,
}

fn describe_change(changes: &Changes, drift: Option<&Drift>) -> String {
    let mut parts = Vec::new();
    if let Some(d) = drift {
        if let Some((old, new)) = &d.head {
            parts.push(format!("HEAD moved {} -> {}", abbrev(old), abbrev(new)));
        }
        if let Some((old, new)) = &d.version {
            parts.push(format!("kernel version {old} -> {new}"));
        }
        if let Some(diff) = &d.config {
            let shown: Vec<&str> = diff.iter().take(3).map(String::as_str).collect();
            parts.push(if shown.is_empty() {
                ".config changed".to_string()
            } else {
                format!(".config changed ({})", shown.join(", "))
            });
        }
    }
    if changes.content_changed() {
        parts.push(format!("tree edited ({})", changes.summary(3)));
    }
    if parts.is_empty() {
        parts.push("file timestamps changed".to_string());
    }
    parts.join("; ")
}

fn link_entries(from: &Path, to: &Path) {
    let Ok(rd) = fs::read_dir(from) else { return };
    if fs::create_dir_all(to).is_err() {
        return;
    }
    for e in rd.flatten() {
        let _ = fs::hard_link(e.path(), to.join(e.file_name()));
    }
}

fn link_or_copy(from: &Path, to: &Path) -> Result<()> {
    if fs::hard_link(from, to).is_err() {
        fs::copy(from, to).at(from)?;
    }
    Ok(())
}
