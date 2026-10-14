use std::path::{Path, PathBuf};
use std::time::Instant;

use rustc_hash::{FxHashMap, FxHashSet};

use super::build_graph::BuildGraph;
use super::candidates::{Cand, Class, Reach};
use super::context::{self, file_context};
use super::expansion::{Builder, TreeOpts};
use super::report::{Census, Report, Site, Value, Variant};
use crate::index::DefKind;
use crate::pp::kbuild::CompileModel;
use crate::workspace::Workspace;

const MAX_SIMULATED: usize = 16;

#[derive(Default)]
struct Acc {
    site: Option<Site>,
    text: Option<String>,
    count: usize,
    sample: Vec<String>,
}

#[derive(PartialEq, Eq, Hash)]
enum AccKey {
    Header(String, u32),
    LocalText(String),
    Other(Option<String>),
}

#[derive(Default)]
struct Accs {
    list: Vec<Acc>,
    index: FxHashMap<AccKey, usize>,
}

impl Accs {
    fn slot(&mut self, site: Option<Site>) -> usize {
        let local = site
            .as_ref()
            .is_some_and(|s| s.path.ends_with(".c") || s.path.ends_with(".S"));
        let key = match &site {
            Some(s) if local => AccKey::LocalText(s.text.clone()),
            Some(s) => AccKey::Header(s.path.clone(), s.line),
            None => AccKey::Other(None),
        };
        let list = &mut self.list;
        *self.index.entry(key).or_insert_with(|| {
            list.push(Acc {
                site,
                ..Acc::default()
            });
            list.len() - 1
        })
    }

    fn bump(&mut self, i: usize, tu: impl FnOnce() -> String) {
        let acc = &mut self.list[i];
        acc.count += 1;
        if acc.sample.len() < 4 {
            acc.sample.push(tu());
        }
    }

    fn add(&mut self, site: Option<Site>, tu: impl FnOnce() -> String) {
        let i = self.slot(site);
        self.bump(i, tu);
    }

    fn push(&mut self, text: String, count: usize, sample: Vec<String>) {
        self.list.push(Acc {
            site: None,
            text: Some(text),
            count,
            sample,
        });
    }
}

pub struct HeaderOutcome {
    pub header: PathBuf,
    pub site: Option<Site>,
    pub value: Option<Value>,
    pub covers: Vec<PathBuf>,
}

fn same_site(a: &Option<Site>, b: &Option<Site>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.path == b.path && a.line == b.line,
        (None, None) => true,
        _ => false,
    }
}

pub fn resolve(headers: &[HeaderOutcome], hits: &[usize]) -> Option<usize> {
    let dominant = hits.iter().copied().find(|&k| {
        hits.iter()
            .all(|&j| j == k || headers[k].covers.contains(&headers[j].header))
    });
    dominant.or_else(|| {
        let first = *hits.first()?;
        hits.iter()
            .all(|&j| same_site(&headers[j].site, &headers[first].site))
            .then_some(first)
    })
}

pub fn decided_by(graph: &BuildGraph, headers: &[HeaderOutcome]) -> Vec<u32> {
    let ids: Vec<Option<u32>> = headers.iter().map(|h| graph.id_of(&h.header)).collect();
    let mut counts = vec![0u32; headers.len()];
    let mut hits = Vec::new();
    for m in 0..graph.members.len() {
        hits.clear();
        hits.extend(
            ids.iter()
                .enumerate()
                .filter(|(_, id)| id.is_some_and(|id| graph.includes(m, id)))
                .map(|(k, _)| k),
        );
        if let Some(k) = resolve(headers, &hits) {
            counts[k] += 1;
        }
    }
    counts
}

pub struct ModuleVariant {
    pub builtin: Option<Site>,
    pub site: Option<Site>,
    pub value: Option<Value>,
}

pub fn run(
    ws: &Workspace,
    report: &Report,
    cands: &[Cand],
    name: &str,
    headers: &[HeaderOutcome],
    module: Option<&ModuleVariant>,
) -> Option<Census> {
    let t0 = Instant::now();
    let graph = ws.build_graph()?;
    let rel = |p: &Path| {
        p.strip_prefix(&ws.layout.srctree)
            .or_else(|_| p.strip_prefix(&ws.layout.objtree))
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| p.display().to_string())
    };
    let tu_name = |i: usize| -> String { rel(&graph.members[i].source) };
    let mut accs = Accs::default();
    let mut local: FxHashSet<usize> = FxHashSet::default();
    let mut examined = 0usize;
    let mut not_built: Vec<String> = Vec::new();
    let base = CompileModel::default_for(&ws.layout);
    let mut files: Vec<&Path> = cands
        .iter()
        .filter(|c| c.class == Class::TuLocal)
        .map(|c| c.abs.as_path())
        .collect();
    files.sort();
    files.dedup();
    let users = graph.users(&files);
    let mut by_file: FxHashMap<&Path, Vec<&Cand>> = FxHashMap::default();
    for c in cands.iter().filter(|c| c.class == Class::TuLocal) {
        by_file.entry(c.abs.as_path()).or_default().push(c);
    }
    for (f, tus) in files.into_iter().zip(users) {
        if tus.is_empty() {
            not_built.push(rel(f));
            continue;
        }
        let postings = by_file.get(f).map(Vec::as_slice).unwrap_or_default();
        let simple = postings.iter().all(|c| c.posting.depth == 0);
        let fixed_site = if simple {
            postings.iter().max_by_key(|c| c.posting.line).map(|c| {
                (c.posting.kind != DefKind::Undef).then(|| Site {
                    path: c.path.clone(),
                    line: c.posting.line,
                    text: ws.definition(c),
                    origin: "source".into(),
                })
            })
        } else {
            None
        };
        for i in tus {
            local.insert(i);
            match &fixed_site {
                Some(site) => accs.add(site.clone(), || tu_name(i)),
                None => {
                    examined += 1;
                    let tu = tu_name(i);
                    let ctx = file_context(ws, &base, &tu, None);
                    let (mut sim, _) = context::sim_for(ws, ctx.model.clone(), None);
                    sim.run(ctx.main);
                    let sym = sim.it.intern(name);
                    let opts = TreeOpts {
                        max_depth: Some(1),
                        alternatives: super::expansion::Alternatives::None,
                        reach: Reach::Kernel,
                        full_bodies: false,
                    };
                    let site = sim
                        .def_of(sym)
                        .and_then(|d| Builder::new(&mut sim, ws, &ctx.model, &opts).site_of(&d));
                    accs.add(site, || tu);
                }
            }
        }
    }
    let mut none = 0usize;
    let mut none_sample = Vec::new();
    let mut ambiguous: Vec<usize> = Vec::new();
    let header_label = headers
        .iter()
        .map(|o| rel(&o.header))
        .collect::<Vec<_>>()
        .join(", ");
    let header_ids: Vec<Option<u32>> = headers.iter().map(|o| graph.id_of(&o.header)).collect();
    let mut slots: FxHashMap<(usize, bool), usize> = FxHashMap::default();
    let mut hits = Vec::new();
    for i in (0..graph.members.len()).filter(|i| !local.contains(i)) {
        hits.clear();
        hits.extend(
            header_ids
                .iter()
                .enumerate()
                .filter(|(_, id)| id.is_some_and(|id| graph.includes(i, id)))
                .map(|(k, _)| k),
        );
        if hits.is_empty() {
            none += 1;
            if none_sample.len() < 4 {
                none_sample.push(tu_name(i));
            }
            continue;
        }
        match resolve(headers, &hits) {
            Some(k) => {
                let variant = module
                    .filter(|m| graph.members[i].module && same_site(&headers[k].site, &m.builtin));
                let slot = *slots.entry((k, variant.is_some())).or_insert_with(|| {
                    accs.slot(match variant {
                        Some(m) => m.site.clone(),
                        None => headers[k].site.clone(),
                    })
                });
                accs.bump(slot, || tu_name(i));
            }
            None => ambiguous.push(i),
        }
    }
    let mut order_dependent = 0usize;
    for (k, &i) in ambiguous.iter().enumerate() {
        if k >= MAX_SIMULATED {
            order_dependent += 1;
            continue;
        }
        examined += 1;
        let tu = tu_name(i);
        let ctx = file_context(ws, &base, &tu, None);
        let (mut sim, _) = context::sim_for(ws, ctx.model.clone(), None);
        sim.run(ctx.main);
        let sym = sim.it.intern(name);
        let opts = TreeOpts {
            max_depth: Some(1),
            alternatives: super::expansion::Alternatives::None,
            reach: Reach::Kernel,
            full_bodies: false,
        };
        let site = sim
            .def_of(sym)
            .and_then(|d| Builder::new(&mut sim, ws, &ctx.model, &opts).site_of(&d));
        accs.add(site, || tu);
    }
    if order_dependent > 0 {
        accs.push(
            format!(
                "order-dependent: these TUs include several defining headers; {MAX_SIMULATED} were simulated, the rest are not resolved"
            ),
            order_dependent,
            Vec::new(),
        );
    }
    if none > 0 {
        accs.push(
            format!("not visible: these TUs include none of {header_label}"),
            none,
            none_sample,
        );
    }
    let mut accs = accs.list;
    accs.sort_by_key(|a| std::cmp::Reverse(a.count));
    let same = |a: &Site, b: &Site| a.path == b.path && a.line == b.line;
    let value_at = |site: &Site| -> Option<Value> {
        if report.active.as_ref().is_some_and(|s| same(s, site)) {
            return report.value.clone();
        }
        if let Some(m) = module
            && m.site.as_ref().is_some_and(|s| same(s, site))
        {
            return m.value.clone();
        }
        headers
            .iter()
            .find(|o| o.site.as_ref().is_some_and(|s| same(s, site)))
            .and_then(|o| o.value.clone())
    };
    let variants: Vec<Variant> = accs
        .into_iter()
        .map(|a| Variant {
            expansion: a.text.or_else(|| a.site.as_ref().map(|s| s.text.clone())),
            value: a.site.as_ref().and_then(value_at),
            site: a.site,
            tu_count: a.count,
            sample: a.sample,
        })
        .collect();
    let mut origin = graph.origin.clone();
    if graph.stale {
        origin.push_str("; warning: .config is newer than the last build");
    }
    Some(Census {
        tu_total: graph.members.len(),
        tu_examined: examined,
        source: origin,
        variants,
        not_built,
        seconds: t0.elapsed().as_secs_f64(),
    })
}
