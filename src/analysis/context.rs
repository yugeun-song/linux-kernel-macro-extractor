use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::candidates::{self, Cand, Class};
use super::report::{ContextInfo, HeaderDir};

use crate::index::DefKind;
use crate::pp::features::{self, Features, Probed};
use crate::pp::kbuild::{CompileModel, ModelSource};
use crate::pp::predef;
use crate::pp::sim::{Main, Sim, SimSetup};
use crate::store::{abbrev, build_id, file_identity, hash_hex, write_atomic, write_slot};
use crate::tree::arch::Compiler;
use crate::workspace::Workspace;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Toolchain {
    pub bin: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredefSource {
    Compiler,
    Table,
}

#[derive(Clone)]
pub struct Prepared {
    pub predefined: String,
    pub predef_source: PredefSource,
    pub features: Features,
    pub compiler_bin: Option<String>,
    pub compiler: Compiler,
}

fn detected_compiler(ws: &Workspace, bin: &str) -> Compiler {
    let version = std::process::Command::new(bin)
        .arg("-dumpfullversion")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .and_then(|v| Compiler::parse_spec(&format!("gcc-{v}")));
    match version {
        Some(mut c) => {
            c.kind = ws.compiler.kind;
            c.text = Some(format!("{bin} (detected from the build command)"));
            c
        }
        None => ws.compiler.clone(),
    }
}

fn toolchain_key(ws: &Workspace, model: &CompileModel) -> String {
    let mut text = format!(
        "{:?}\u{1f}{:?}\u{1f}{}\u{1f}{}",
        ws.compiler,
        model.compiler,
        ws.target.arch,
        build_id()
    );
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            text.push_str(&format!("\u{1f}{}={}", dir.display(), file_identity(&dir)));
        }
    }
    hash_hex(text.as_bytes())
}

fn wanted_compiler(ws: &Workspace, model: &CompileModel) -> Compiler {
    let mut wanted = ws.compiler.clone();
    if wanted.assumed
        && let Some(hint) = &model.compiler
        && let Some(major) = std::process::Command::new(hint)
            .arg("-dumpversion")
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .split('.')
                    .next()?
                    .parse()
                    .ok()
            })
    {
        wanted.major = major;
    }
    wanted
}

pub fn toolchain(ws: &Workspace, model: &CompileModel) -> Toolchain {
    let name = format!("toolchain-{}.json", toolchain_key(ws, model));
    cached(ws, &name, Some("toolchain-"), || {
        let wanted = wanted_compiler(ws, model);
        Some(Toolchain {
            bin: features::find_compiler(&wanted, ws.target.arch, model.compiler.as_deref()),
        })
    })
    .unwrap_or(Toolchain { bin: None })
}

fn compiler_args(ws: &Workspace, model: &CompileModel) -> Vec<String> {
    let mut args: Vec<String> = model
        .flags
        .iter()
        .filter(|f| {
            !(f.starts_with("-fplugin")
                || f.starts_with("-fdump")
                || f.starts_with("-M")
                || f.starts_with("-save-temps")
                || f.starts_with("-W"))
        })
        .cloned()
        .collect();
    if let Some(t) = features::target_flag(&ws.compiler, ws.target.arch) {
        args.push(t.to_string());
    }
    args
}

pub(crate) fn cached<T: Serialize + for<'de> Deserialize<'de>>(
    ws: &Workspace,
    name: &str,
    slot: Option<&str>,
    compute: impl FnOnce() -> Option<T>,
) -> Option<T> {
    let dir = ws.cache_dir();
    let path = dir.join(name);
    if let Some(v) = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        return Some(v);
    }
    let v = compute()?;
    if let Ok(json) = serde_json::to_vec(&v) {
        let _ = match slot {
            Some(prefix) => write_slot(dir, prefix, name, &json).map(drop),
            None => fs::create_dir_all(dir).and_then(|()| write_atomic(&path, &json)),
        };
    }
    Some(v)
}

pub fn prepare(ws: &Workspace, model: &CompileModel) -> Prepared {
    let tc = toolchain(ws, model);
    let args = compiler_args(ws, model);
    let key = hash_hex(
        format!(
            "{:?}\u{1f}{:?}\u{1f}{}\u{1f}{}\u{1f}{}",
            tc.bin,
            tc.bin
                .as_deref()
                .and_then(features::resolve_in_path)
                .map(|p| file_identity(&p)),
            args.join("\u{1e}"),
            model.asm,
            build_id()
        )
        .as_bytes(),
    );
    let from_compiler: Option<String> = tc.bin.as_ref().and_then(|bin| {
        cached(ws, &format!("predef-{key}.json"), None, || {
            let mut a = args.clone();
            a.push("-dM".to_string());
            let lang = if model.asm { "assembler-with-cpp" } else { "c" };
            features::run_preprocessor(bin, &a, lang, "", &ws.layout.objtree)
        })
    });
    let probed: Option<Probed> = tc.bin.as_ref().and_then(|bin| {
        cached(ws, &format!("probe-{key}.json"), None, || {
            features::probe(bin, &args, &ws.layout.objtree)
        })
    });
    let compiler = match (&tc.bin, ws.compiler.assumed) {
        (Some(bin), true) => detected_compiler(ws, bin),
        _ => ws.compiler.clone(),
    };
    let features = Features::new(&compiler, ws.target.arch, &model.flags, probed.as_ref());
    let (predefined, predef_source) = match from_compiler {
        Some(text) => (
            predef::with_compiler_version(&text, &compiler),
            PredefSource::Compiler,
        ),
        None => (
            predef::predefined_source(ws.target.arch, &compiler, model),
            PredefSource::Table,
        ),
    };
    Prepared {
        predefined,
        predef_source,
        features,
        compiler_bin: tc.bin,
        compiler,
    }
}

pub struct SimInfo {
    pub compiler: Compiler,
    pub predef_source: PredefSource,
    pub compiler_bin: Option<String>,
    pub header_dirs: Vec<(PathBuf, PathBuf)>,
}

fn compiler_include_dir(ws: &Workspace, bin: &str) -> Option<PathBuf> {
    let key = hash_hex(
        format!(
            "{bin}\u{1f}{:?}\u{1f}{}",
            features::resolve_in_path(bin).map(|p| file_identity(&p)),
            build_id()
        )
        .as_bytes(),
    );
    let dir: String = cached(ws, &format!("incdir-{key}.json"), None, || {
        let out = std::process::Command::new(bin)
            .arg("-print-file-name=include")
            .output()
            .ok()?;
        let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && Path::new(&dir).is_absolute() && Path::new(&dir).is_dir())
            .then_some(dir)
    })?;
    Some(PathBuf::from(dir))
}

pub fn compiler_headers(
    ws: &Workspace,
    model: &CompileModel,
    bin: Option<&str>,
) -> Vec<(PathBuf, PathBuf)> {
    let missing: Vec<&PathBuf> = model
        .system_dirs
        .iter()
        .filter(|d| d.ends_with("include") && !d.is_dir())
        .collect();
    if missing.is_empty() {
        return Vec::new();
    }
    let Some(found) = bin.and_then(|b| compiler_include_dir(ws, b)) else {
        return Vec::new();
    };
    missing
        .into_iter()
        .map(|d| (d.clone(), found.clone()))
        .collect()
}

pub struct Setup {
    prepared: Prepared,
    header_dirs: Vec<(PathBuf, PathBuf)>,
}

fn setup_for(ws: &Workspace, model: &CompileModel) -> Arc<Setup> {
    let key = hash_hex(
        format!(
            "{:?}\u{1f}{:?}\u{1f}{:?}\u{1f}{}",
            model.compiler, model.flags, model.system_dirs, model.asm
        )
        .as_bytes(),
    );
    if let Some(s) = ws.setups.lock().ok().and_then(|m| m.get(&key).cloned()) {
        return s;
    }
    let prepared = prepare(ws, model);
    let header_dirs = compiler_headers(ws, model, prepared.compiler_bin.as_deref());
    let made = Arc::new(Setup {
        prepared,
        header_dirs,
    });
    if let Ok(mut m) = ws.setups.lock() {
        m.entry(key).or_insert_with(|| made.clone());
    }
    made
}

pub fn sim_for<'w>(
    ws: &'w Workspace,
    model: CompileModel,
    autoconf_override: Option<String>,
) -> (Sim<'w>, SimInfo) {
    let setup = setup_for(ws, &model);
    let prepared = setup.prepared.clone();
    let header_dirs = setup.header_dirs.clone();
    let mut model = model;
    for (recorded, used) in &header_dirs {
        for dir in model.system_dirs.iter_mut().filter(|d| *d == recorded) {
            dir.clone_from(used);
        }
    }
    let info = SimInfo {
        compiler: prepared.compiler,
        predef_source: prepared.predef_source,
        compiler_bin: prepared.compiler_bin,
        header_dirs,
    };
    let sim = Sim::new(SimSetup {
        index: &ws.index,
        sources: &ws.sources,
        layout: &ws.layout,
        config: &ws.config,
        model,
        predefined: prepared.predefined,
        features: prepared.features,
        autoconf_override,
    });
    (sim, info)
}

#[derive(Clone, Debug)]
pub enum ContextChoice {
    Auto,
    File { rel: String, line: Option<u32> },
    Includes(Vec<String>),
}

#[derive(Clone)]
pub struct Ctx {
    pub label: String,
    pub model: CompileModel,
    pub main: Main,
    pub stop: Option<(PathBuf, u32)>,
    pub header: Option<PathBuf>,
}

const MAX_HEADER_CONTEXTS: usize = 6;
const MAX_FILE_CONTEXTS: usize = 3;

fn flags_label(model: &CompileModel, ws: &Workspace) -> String {
    let rel = |p: &PathBuf| {
        p.strip_prefix(&ws.layout.objtree)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| p.display().to_string())
    };
    match &model.source {
        ModelSource::Cmd(p) => format!("flags from {}", rel(p)),
        ModelSource::Representative(p) => format!("kbuild flags taken from {}", rel(p)),
        ModelSource::Synthesized => {
            "synthesized kbuild flags (no .cmd files: tree not built)".to_string()
        }
    }
}

pub fn file_context(ws: &Workspace, base: &CompileModel, rel: &str, line: Option<u32>) -> Ctx {
    let model = CompileModel::for_file(&ws.layout, rel, base);
    let abs = ws.layout.srctree.join(rel);
    let label = match line {
        Some(l) => format!("TU {rel} up to line {l} ({})", flags_label(&model, ws)),
        None => format!("TU {rel} ({})", flags_label(&model, ws)),
    };
    Ctx {
        label,
        stop: line.map(|l| (abs.clone(), l)),
        main: Main::File(abs),
        model,
        header: None,
    }
}

fn header_context(
    ws: &Workspace,
    base: &CompileModel,
    specs: &[String],
    header: Option<PathBuf>,
) -> Ctx {
    let content: String = specs.iter().map(|s| format!("#include {s}\n")).collect();
    let shown: Vec<String> = specs
        .iter()
        .map(|s| {
            let inner = s.trim_matches('"');
            match Path::new(inner).strip_prefix(&ws.layout.srctree) {
                Ok(rel) if s.starts_with('"') => format!("\"{}\"", rel.display()),
                _ => s.clone(),
            }
        })
        .collect();
    Ctx {
        label: format!(
            "virtual TU: {} ({})",
            shown.join(" "),
            flags_label(base, ws)
        ),
        model: base.clone(),
        main: Main::Virtual {
            name: "<virtual TU>".into(),
            content,
        },
        stop: None,
        header,
    }
}

pub fn contexts(
    ws: &Workspace,
    choice: &ContextChoice,
    cands: &[Cand],
    base: &CompileModel,
) -> Vec<Ctx> {
    match choice {
        ContextChoice::File { rel, line } => vec![file_context(ws, base, rel, *line)],
        ContextChoice::Includes(hs) => {
            let specs: Vec<String> = hs
                .iter()
                .map(|h| {
                    if h.starts_with('<') || h.starts_with('"') {
                        h.clone()
                    } else {
                        format!("<{h}>")
                    }
                })
                .collect();
            vec![header_context(ws, base, &specs, None)]
        }
        ContextChoice::Auto => {
            let headers = candidates::header_contexts(cands, MAX_HEADER_CONTEXTS);
            if !headers.is_empty() {
                return headers
                    .into_iter()
                    .map(|(spec, abs)| header_context(ws, base, &[spec], Some(abs)))
                    .collect();
            }
            let mut seen = Vec::new();
            cands
                .iter()
                .filter(|c| c.class == Class::TuLocal && c.posting.kind != DefKind::Undef)
                .filter(|c| {
                    let fresh = !seen.contains(&c.path);
                    seen.push(c.path.clone());
                    fresh
                })
                .take(MAX_FILE_CONTEXTS)
                .map(|c| file_context(ws, base, &c.path, None))
                .collect()
        }
    }
}

pub fn context_info(
    ws: &Workspace,
    info: &SimInfo,
    label: &str,
    what_if: &[String],
) -> ContextInfo {
    ContextInfo {
        compiler: info.compiler.to_string(),
        predefines: match info.predef_source {
            PredefSource::Compiler => format!(
                "queried from {}",
                info.compiler_bin.as_deref().unwrap_or("the compiler")
            ),
            PredefSource::Table => "built-in table (compiler not found on this host)".into(),
        },
        compiler_headers: info
            .header_dirs
            .iter()
            .map(|(recorded, used)| HeaderDir {
                used: used.display().to_string(),
                recorded: recorded.display().to_string(),
            })
            .collect(),
        mode: label.to_string(),
        what_if: what_if.to_vec(),
        ..tree_info(ws)
    }
}

pub fn tree_info(ws: &Workspace) -> ContextInfo {
    ContextInfo {
        tree: ws.layout.srctree.display().to_string(),
        objtree: (!ws.layout.in_tree()).then(|| ws.layout.objtree.display().to_string()),
        release: ws.tree.version.release(),
        commit: abbrev(&ws.tree.head.commit).to_string(),
        arch: ws.target.arch.name().to_string(),
        compiler: ws.compiler.to_string(),
        predefines: String::new(),
        compiler_headers: Vec::new(),
        mode: String::new(),
        what_if: Vec::new(),
    }
}
