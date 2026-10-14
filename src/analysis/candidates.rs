use std::path::{Path, PathBuf};

use super::explain;
use super::report::{Candidate, Status};
use rayon::prelude::*;
use rustc_hash::FxHashMap;

use crate::index::{DefKind, FileClass, FileId, FileView, Posting, Scope};
use crate::intern::Sym;
use crate::pp::kbuild::{CompileModel, normalize};
use crate::pp::sim::{EventKind, Sim};
use crate::syntax::lex;
use crate::tree::Root;
use crate::workspace::Workspace;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Class {
    Header,
    LocalHeader,
    TuLocal,
    OtherArch(String),
    Host(&'static str),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reach {
    #[default]
    Kernel,
    All,
}

impl Reach {
    pub fn admits(self, class: &Class) -> bool {
        !matches!(class, Class::Host(_)) || self == Reach::All
    }
}

pub fn fixed_class(view: &FileView<'_>, srcarch: &str) -> Option<Class> {
    Some(match (view.scope(), view.arch, view.class()) {
        (Scope::Tools, ..) => Class::Host("tools"),
        (Scope::Scripts, ..) => Class::Host("scripts"),
        (Scope::Docs, ..) => Class::Host("Documentation"),
        (Scope::Usr, ..) => Class::Host("usr"),
        (_, Some(a), _) if a != srcarch => Class::OtherArch(a.to_string()),
        (_, _, FileClass::CSource | FileClass::Asm | FileClass::DeviceTree) => Class::TuLocal,
        _ => return None,
    })
}

#[derive(Clone, Debug)]
pub struct Cand {
    pub posting: Posting,
    pub path: String,
    pub abs: PathBuf,
    pub class: Class,
    pub spec: Option<String>,
}

pub fn include_spec(model: &CompileModel, abs: &Path) -> Option<String> {
    model
        .dirs
        .iter()
        .chain(model.system_dirs.iter())
        .find_map(|d| abs.strip_prefix(d).ok())
        .map(|rel| format!("<{}>", rel.display()))
}

pub fn gather(ws: &Workspace, name: &str, model: &CompileModel, reach: Reach) -> Vec<Cand> {
    let srcarch = ws.target.arch.srcarch();
    let mut out: Vec<Cand> = ws
        .index
        .lookup(name)
        .into_iter()
        .filter(|p| p.kind.is_macro())
        .filter_map(|p| {
            let view = ws.index.file(p.file)?;
            let abs = normalize(&ws.abs(&view));
            let class =
                fixed_class(&view, srcarch).unwrap_or_else(|| match include_spec(model, &abs) {
                    Some(_) => Class::Header,
                    None => Class::LocalHeader,
                });
            if !reach.admits(&class) {
                return None;
            }
            let spec = match class {
                Class::Header => include_spec(model, &abs),
                Class::LocalHeader => Some(format!("\"{}\"", abs.display())),
                _ => None,
            };
            Some(Cand {
                posting: p,
                path: view.path.to_string(),
                abs,
                class,
                spec,
            })
        })
        .collect();
    out.sort_by_key(|c| {
        (
            class_rank(&c.class, &c.path, srcarch),
            c.path.matches('/').count(),
            c.path.clone(),
            c.posting.line,
        )
    });
    out
}

pub fn class_rank(class: &Class, path: &str, srcarch: &str) -> u8 {
    match class {
        Class::Header if path.starts_with(&format!("arch/{srcarch}/")) => 0,
        Class::Header if path.starts_with("include/generated") => 1,
        Class::Header if path.starts_with("include/linux") => 2,
        Class::Header if path.starts_with("include/asm-generic") => 4,
        Class::Header => 3,
        Class::LocalHeader => 5,
        Class::TuLocal => 6,
        Class::OtherArch(_) => 7,
        Class::Host(_) => 8,
    }
}

pub fn headers_defining_undefined(
    ws: &Workspace,
    sim: &mut Sim<'_>,
    out: &[lex::Token],
    base: &CompileModel,
    have: &[String],
) -> Vec<String> {
    let mut wanted = Vec::new();
    let mut seen = rustc_hash::FxHashSet::default();
    for t in out {
        if t.kind != lex::TokenKind::Ident
            || t.flags & lex::F_NOEXP != 0
            || !seen.insert(t.sym)
            || sim.is_defined(t.sym)
        {
            continue;
        }
        let name = sim.it.get(t.sym).to_string();
        if name.starts_with("__") && name.ends_with("__") {
            continue;
        }
        let header = gather(ws, &name, base, Reach::Kernel)
            .iter()
            .filter(|c| c.class == Class::Header && c.posting.kind != DefKind::Undef)
            .find_map(|c| c.spec.clone());
        if let Some(h) = header
            && !have.contains(&h)
            && !wanted.contains(&h)
        {
            wanted.push(h);
        }
    }
    wanted
}

pub fn header_contexts(cands: &[Cand], limit: usize) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = Vec::new();
    for c in cands {
        if !matches!(c.class, Class::Header | Class::LocalHeader)
            || c.posting.kind == DefKind::Undef
        {
            continue;
        }
        if let Some(spec) = &c.spec
            && !v.iter().any(|(_, a)| *a == c.abs)
        {
            v.push((spec.clone(), c.abs.clone()));
        }
        if v.len() >= limit {
            break;
        }
    }
    v
}

pub struct Judged {
    pub status: Status,
    pub detail: String,
    pub cond: Option<u32>,
}

pub fn judge(sim: &mut Sim<'_>, sym: Sym, c: &Cand) -> Option<Judged> {
    if sim.replaced_by_what_if(&c.abs) {
        return Some(Judged {
            status: Status::Replaced,
            detail: "the value from .config; --set swapped in a what-if autoconf.h".into(),
            cond: None,
        });
    }
    let f = sim.files.lookup(&c.abs).filter(|f| sim.visited(*f))?;
    let events: Vec<_> = sim.events_for(sym).cloned().collect();
    let pos = events.iter().position(|e| {
        e.site
            .is_some_and(|s| s.file == f && s.line == c.posting.line)
    });
    if let Some(i) = pos {
        if c.posting.kind == DefKind::Undef {
            return Some(Judged {
                status: Status::Undefined,
                detail: "#undef executed here".into(),
                cond: Some(events[i].cond),
            });
        }
        return Some(match events.get(i + 1) {
            None => Judged {
                status: Status::Active,
                detail: String::new(),
                cond: Some(events[i].cond),
            },
            Some(e) => {
                let at = e
                    .site
                    .map(|s| format!("{}:{}", sim.files.display(s.file), s.line))
                    .unwrap_or_else(|| "the command line".into());
                let (status, what) = match e.kind {
                    EventKind::Undef => (Status::Undefined, "#undef"),
                    _ => (Status::Overridden, "redefined"),
                };
                Judged {
                    status,
                    detail: format!("{what} at {at}"),
                    cond: Some(events[i].cond),
                }
            }
        });
    }
    if let Some(r) = sim.skip_at(f, c.posting.line) {
        return Some(Judged {
            status: Status::ConditionFalse,
            detail: String::new(),
            cond: Some(r.cond),
        });
    }
    Some(Judged {
        status: Status::NotReached,
        detail: "the file was included, but this line was never processed".into(),
        cond: None,
    })
}

pub fn static_status(c: &Cand, ws: &Workspace) -> Judged {
    let srcarch = ws.target.arch.srcarch();
    let (status, detail) = match &c.class {
        Class::OtherArch(a) => (
            Status::OtherArch,
            format!("arch/{a}; the current arch is {srcarch}"),
        ),
        Class::Host(what) => (
            Status::Host,
            format!("{what}/ is built for the host or userspace, not the kernel"),
        ),
        Class::TuLocal => (
            Status::TuLocal,
            format!("visible only while compiling {}", c.path),
        ),
        Class::Header | Class::LocalHeader => match c.path.strip_prefix("include/asm-generic/") {
            Some(name)
                if ws
                    .index
                    .file_id(Root::Src, &format!("arch/{srcarch}/include/asm/{name}"))
                    .is_some() =>
            {
                (
                    Status::Shadowed,
                    format!("arch/{srcarch}/include/asm/{name} provides <asm/{name}> instead"),
                )
            }
            _ => (Status::NotIncluded, "not included in this context".into()),
        },
    };
    Judged {
        status,
        detail,
        cond: None,
    }
}

fn render_directive(content: &[u8], off: u32, it: &mut crate::intern::Interner) -> String {
    let Some(range) = crate::syntax::scan::directive_at(content, off as usize) else {
        return String::new();
    };
    let toks = lex::lex_all(&content[range], false, it);
    lex::render(&toks, it)
}

impl Workspace {
    pub fn definition(&self, c: &Cand) -> String {
        let key = (c.posting.file, c.posting.offset);
        if let Some(t) = self
            .definitions
            .lock()
            .ok()
            .and_then(|m| m.get(&key).cloned())
        {
            return t;
        }
        let text = self
            .file_bytes(&c.abs)
            .map(|content| {
                render_directive(
                    content,
                    c.posting.offset,
                    &mut crate::intern::Interner::new(),
                )
            })
            .unwrap_or_default();
        if let Ok(mut m) = self.definitions.lock() {
            m.insert(key, text.clone());
        }
        text
    }

    pub fn prefetch_definitions(&self, cands: &[Cand]) {
        let mut by_file: FxHashMap<FileId, (&Path, Vec<u32>)> = FxHashMap::default();
        if let Ok(m) = self.definitions.lock() {
            for c in cands {
                if !m.contains_key(&(c.posting.file, c.posting.offset)) {
                    by_file
                        .entry(c.posting.file)
                        .or_insert_with(|| (c.abs.as_path(), Vec::new()))
                        .1
                        .push(c.posting.offset);
                }
            }
        }
        if by_file.len() < 2 {
            return;
        }
        let texts: Vec<((FileId, u32), String)> = by_file
            .into_par_iter()
            .flat_map_iter(|(file, (abs, offs))| {
                let content = self.file_bytes(abs).unwrap_or_default();
                let mut it = crate::intern::Interner::new();
                offs.into_iter()
                    .map(|off| ((file, off), render_directive(content, off, &mut it)))
                    .collect::<Vec<_>>()
            })
            .collect();
        if let Ok(mut m) = self.definitions.lock() {
            m.extend(texts);
        }
    }
}

pub fn definition_text(sim: &mut Sim<'_>, ws: &Workspace, c: &Cand) -> String {
    let loaded = sim
        .files
        .lookup(&c.abs)
        .filter(|f| sim.visited(*f) && !sim.replaced_by_what_if(&c.abs));
    match loaded {
        Some(f) => {
            let content = sim.files.content(f);
            render_directive(&content, c.posting.offset, &mut sim.it)
        }
        None => ws.definition(c),
    }
}

pub fn static_candidate(c: &Cand, ws: &Workspace) -> Candidate {
    let j = static_status(c, ws);
    Candidate {
        path: c.path.clone(),
        line: c.posting.line,
        text: ws.definition(c),
        status: j.status,
        detail: j.detail,
        conditions: Vec::new(),
    }
}

pub fn to_candidate(sim: &mut Sim<'_>, ws: &Workspace, c: &Cand, j: &Judged) -> Candidate {
    let cfg = &ws.config;
    let text = definition_text(sim, ws, c);
    let conditions = match (j.status, j.cond) {
        (Status::ConditionFalse, Some(id)) => vec![explain::condition(sim, id, cfg)],
        (Status::Active | Status::Overridden | Status::Undefined, Some(id)) => {
            explain::chain(sim, id, cfg)
        }
        _ => Vec::new(),
    };
    Candidate {
        path: c.path.clone(),
        line: c.posting.line,
        text,
        status: j.status,
        detail: j.detail.clone(),
        conditions,
    }
}
