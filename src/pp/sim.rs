mod include;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};

use super::expand::{Env, Expander};
use super::expr;
use super::features::{Answer, Features, Probe};
use super::files::{FileTable, Locator, Sources};
use super::hide_set::HideSets;
use super::kbuild::{CompileModel, DefineOp, normalize};
use super::macros::{Builtin, MacroDef, Origin, Params, Site, SrcFile, parse_after_keyword};
use crate::index::Index;
use crate::intern::{Interner, Sym, sym};
use crate::syntax::lex::{self, Token, TokenKind};
use crate::syntax::scan::{self, DirKind, DirLine};
use crate::tree::Layout;
use crate::tree::dot_config::DotConfig;
use include::Wrappers;

pub type CondId = u32;

#[derive(Clone, Debug)]
pub struct CondRecord {
    pub file: SrcFile,
    pub line: u32,
    pub kind: DirKind,
    pub off: u32,
    pub end: u32,
    pub value: Option<bool>,
    pub parent: CondId,
    pub consulted: Vec<(Sym, Option<Rc<MacroDef>>)>,
    pub assumptions: Vec<String>,
    pub error: Option<String>,
    pub prev: Vec<CondId>,
    pub answers: Vec<(String, bool)>,
}

#[derive(Clone, Copy, Debug)]
pub struct SkipRange {
    pub from: u32,
    pub to: u32,
    pub cond: CondId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Define,
    Redefine,
    Undef,
}

#[derive(Clone, Debug)]
pub struct MacroEvent {
    pub sym: Sym,
    pub kind: EventKind,
    pub site: Option<Site>,
    pub origin: Origin,
    pub cond: CondId,
}

#[derive(Clone, Debug)]
pub enum IncludeResult {
    Entered(SrcFile),
    Guarded(SrcFile, Sym),
    Once(SrcFile),
    Missing,
    TooDeep,
}

#[derive(Clone, Debug)]
pub struct IncludeEvent {
    pub from: SrcFile,
    pub line: u32,
    pub spec: String,
    pub angled: bool,
    pub result: IncludeResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagKind {
    Error,
    Warning,
    MissingInclude,
    Internal,
}

#[derive(Clone, Debug)]
pub struct Diag {
    pub file: SrcFile,
    pub line: u32,
    pub kind: DiagKind,
    pub message: String,
}

#[derive(Clone)]
struct Slot {
    site: Option<Site>,
    origin: Origin,
    def: Option<Rc<MacroDef>>,
    asm: bool,
}

#[derive(Default)]
struct MacroTable {
    slots: Vec<Option<Slot>>,
}

impl MacroTable {
    fn get(&self, s: &Sym) -> Option<&Slot> {
        self.slots.get(s.0 as usize)?.as_ref()
    }

    fn get_mut(&mut self, s: &Sym) -> Option<&mut Slot> {
        self.slots.get_mut(s.0 as usize)?.as_mut()
    }

    fn contains_key(&self, s: &Sym) -> bool {
        self.get(s).is_some()
    }

    fn insert(&mut self, s: Sym, slot: Slot) -> Option<Slot> {
        let i = s.0 as usize;
        if i >= self.slots.len() {
            self.slots.resize_with(i + 1, || None);
        }
        self.slots[i].replace(slot)
    }

    fn remove(&mut self, s: &Sym) -> Option<Slot> {
        self.slots.get_mut(s.0 as usize)?.take()
    }

    fn keys(&self) -> impl Iterator<Item = Sym> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_some())
            .map(|(i, _)| Sym(i as u32))
    }
}

struct Frame {
    file: SrcFile,
    found: Option<usize>,
}

struct IfState {
    active: bool,
    taken: bool,
    parent_active: bool,
    skip_from: Option<(u32, CondId)>,
    pushed: bool,
    branches: Vec<CondId>,
}

#[derive(Clone, Debug)]
pub enum Main {
    File(PathBuf),
    Virtual { name: String, content: String },
}

pub struct SimSetup<'w> {
    pub index: &'w Index,
    pub sources: &'w Sources,
    pub layout: &'w Layout,
    pub config: &'w DotConfig,
    pub model: CompileModel,
    pub predefined: String,
    pub features: Features,
    pub autoconf_override: Option<String>,
}

pub struct Sim<'w> {
    pub it: Interner,
    pub hs: HideSets,
    pub files: FileTable<'w>,
    pub model: CompileModel,
    pub events: Vec<MacroEvent>,
    pub conds: Vec<CondRecord>,
    pub includes: Vec<IncludeEvent>,
    pub diags: Vec<Diag>,
    pub stopped: bool,
    pub main: Option<SrcFile>,
    pub symbolic_builtins: bool,
    index: &'w Index,
    sources: &'w Sources,
    layout: &'w Layout,
    config: &'w DotConfig,
    features: Features,
    predefined: String,
    autoconf_override: Option<String>,
    chain: Vec<PathBuf>,
    angled_start: usize,
    macros: MacroTable,
    pushed: FxHashMap<Sym, Vec<Option<Slot>>>,
    events_by_sym: Vec<(u32, u32)>,
    event_next: Vec<u32>,
    skips: FxHashMap<SrcFile, Vec<SkipRange>>,
    once: FxHashSet<SrcFile>,
    guards: FxHashMap<SrcFile, Option<Sym>>,
    visited: FxHashSet<SrcFile>,
    stack: Vec<Frame>,
    active_conds: Vec<CondId>,
    counter: u32,
    recording: Option<Vec<(Sym, Option<Rc<MacroDef>>)>>,
    assumptions: Vec<String>,
    answers: Vec<(String, bool)>,
    stop: Option<(PathBuf, u32)>,
    cur_line: u32,
    origin_now: Origin,
    asm_now: bool,
    wrappers: Option<Rc<Wrappers>>,
    resolved: FxHashMap<IncludeKey, Option<(SrcFile, Option<usize>)>>,
    resolved_at: u32,
    dir_syms: Vec<Option<Option<Sym>>>,
    autoconf: PathBuf,
    gen_asm: PathBuf,
    gen_uapi: PathBuf,
    expand_depth: u32,
    unsigned_char: bool,
}

const MAX_INCLUDE_DEPTH: usize = 200;
const NO_EVENT: u32 = u32::MAX;

type IncludeKey = (u32, Sym, Sym);
const SYMBOLS: usize = 1 << 15;

fn has_operator(s: Sym) -> Option<Probe> {
    match s {
        x if x == sym::HAS_ATTRIBUTE => Some(Probe::Attribute),
        x if x == sym::HAS_BUILTIN => Some(Probe::Builtin),
        x if x == sym::HAS_FEATURE || x == sym::HAS_EXTENSION => Some(Probe::Feature),
        x if x == sym::HAS_C_ATTRIBUTE => Some(Probe::CAttribute),
        _ => None,
    }
}

impl<'w> Sim<'w> {
    pub fn new(setup: SimSetup<'w>) -> Sim<'w> {
        let flags = &setup.model.flags;
        let unsigned_char = if flags.iter().any(|f| f == "-funsigned-char") {
            true
        } else if flags.iter().any(|f| f == "-fsigned-char") {
            false
        } else {
            setup.layout.srcarch != "x86"
        };
        let objtree = &setup.layout.objtree;
        let arch = &setup.layout.srcarch;
        let autoconf = normalize(&objtree.join("include/generated/autoconf.h"));
        let gen_asm = normalize(&objtree.join(format!("arch/{arch}/include/generated/asm")));
        let gen_uapi = normalize(&objtree.join(format!("arch/{arch}/include/generated/uapi/asm")));
        let mut chain = setup.model.quote_dirs.clone();
        let angled_start = chain.len();
        chain.extend(setup.model.dirs.iter().cloned());
        chain.extend(setup.model.system_dirs.iter().cloned());
        let mut sim = Sim {
            it: Interner::with_capacity(SYMBOLS),
            hs: HideSets::new(),
            files: FileTable::new(),
            model: setup.model,
            events: Vec::new(),
            conds: vec![CondRecord {
                file: SrcFile(0),
                line: 0,
                kind: DirKind::Null,
                off: 0,
                end: 0,
                value: Some(true),
                parent: 0,
                consulted: Vec::new(),
                assumptions: Vec::new(),
                error: None,
                prev: Vec::new(),
                answers: Vec::new(),
            }],
            includes: Vec::new(),
            diags: Vec::new(),
            stopped: false,
            main: None,
            symbolic_builtins: false,
            index: setup.index,
            sources: setup.sources,
            layout: setup.layout,
            config: setup.config,
            features: setup.features,
            predefined: setup.predefined,
            autoconf_override: setup.autoconf_override,
            chain,
            angled_start,
            macros: MacroTable::default(),
            pushed: FxHashMap::default(),
            events_by_sym: Vec::new(),
            event_next: Vec::new(),
            skips: FxHashMap::default(),
            once: FxHashSet::default(),
            guards: FxHashMap::default(),
            visited: FxHashSet::default(),
            stack: Vec::new(),
            active_conds: Vec::new(),
            counter: 0,
            recording: None,
            assumptions: Vec::new(),
            answers: Vec::new(),
            stop: None,
            cur_line: 0,
            origin_now: Origin::Source,
            asm_now: false,
            wrappers: None,
            resolved: FxHashMap::default(),
            resolved_at: 0,
            dir_syms: Vec::new(),
            autoconf,
            gen_asm,
            gen_uapi,
            expand_depth: 0,
            unsigned_char,
        };
        for (s, b) in Builtin::ALL {
            sim.macros.insert(
                s,
                Slot {
                    site: None,
                    origin: Origin::Builtin,
                    def: Some(Rc::new(MacroDef::builtin(s, b))),
                    asm: false,
                },
            );
        }
        sim
    }

    pub fn set_stop(&mut self, file: PathBuf, line: u32) {
        self.stop = Some((normalize(&file), line));
    }

    pub fn locator(&self) -> Locator<'w> {
        Locator {
            index: self.index,
            layout: self.layout,
            sources: self.sources,
        }
    }

    pub fn run(&mut self, main: Main) {
        let predefined = std::mem::take(&mut self.predefined);
        let pre = self
            .files
            .add_virtual("<predefined>", None, predefined.into_bytes());
        self.origin_now = Origin::Predefined;
        self.guarded_process(pre, None, 0);
        let mut cmdline = String::new();
        for op in &self.model.ops {
            match op {
                DefineOp::Define(n, v) => cmdline.push_str(&format!("#define {n} {v}\n")),
                DefineOp::Undef(n) => cmdline.push_str(&format!("#undef {n}\n")),
            }
        }
        let cl = self
            .files
            .add_virtual("<command line>", None, cmdline.into_bytes());
        self.origin_now = Origin::CommandLine;
        self.guarded_process(cl, None, 0);
        self.origin_now = Origin::Source;
        let main_file = match main {
            Main::File(p) => {
                let loc = self.locator();
                loc.find(&mut self.files, &p, true)
            }
            Main::Virtual { name, content } => {
                Some(self.files.add_virtual(&name, None, content.into_bytes()))
            }
        };
        self.main = main_file;
        for fi in self.model.force_includes.clone() {
            if self.stopped {
                break;
            }
            let loc = self.locator();
            match loc.find(&mut self.files, &fi, true) {
                Some(f) => {
                    self.includes.push(IncludeEvent {
                        from: main_file.unwrap_or(cl),
                        line: 0,
                        spec: format!("-include {}", loc.display_for(&fi)),
                        angled: false,
                        result: IncludeResult::Entered(f),
                    });
                    self.guarded_process(f, None, 1);
                }
                None => self.diags.push(Diag {
                    file: cl,
                    line: 0,
                    kind: DiagKind::MissingInclude,
                    message: format!("-include {}: no such file", fi.display()),
                }),
            }
        }
        if let Some(m) = main_file
            && !self.stopped
        {
            self.guarded_process(m, None, 0);
        }
    }

    fn guarded_process(&mut self, f: SrcFile, found: Option<usize>, depth: usize) {
        let r = catch_unwind(AssertUnwindSafe(|| self.process_file(f, found, depth)));
        if r.is_err() {
            self.stack.truncate(depth);
            self.diags.push(Diag {
                file: f,
                line: 0,
                kind: DiagKind::Internal,
                message: format!(
                    "internal error while preprocessing {}",
                    self.files.display(f)
                ),
            });
        }
    }

    fn current_cond(&self) -> CondId {
        self.active_conds.last().copied().unwrap_or(0)
    }

    fn push_event(&mut self, s: Sym, kind: EventKind, site: Option<Site>, origin: Origin) {
        let idx = self.events.len() as u32;
        self.events.push(MacroEvent {
            sym: s,
            kind,
            site,
            origin,
            cond: self.current_cond(),
        });
        self.event_next.push(NO_EVENT);
        let k = s.0 as usize;
        if k >= self.events_by_sym.len() {
            self.events_by_sym.resize(k + 1, (NO_EVENT, NO_EVENT));
        }
        let (first, last) = self.events_by_sym[k];
        if first == NO_EVENT {
            self.events_by_sym[k] = (idx, idx);
        } else {
            self.event_next[last as usize] = idx;
            self.events_by_sym[k].1 = idx;
        }
    }

    fn close_skip(&mut self, f: SrcFile, st: &mut IfState, to: u32) {
        if let Some((from, cond)) = st.skip_from.take() {
            self.skips
                .entry(f)
                .or_default()
                .push(SkipRange { from, to, cond });
        }
    }

    fn record_cond(
        &mut self,
        f: SrcFile,
        d: &DirLine,
        kind: DirKind,
        value: Option<bool>,
    ) -> CondId {
        let consulted = self.recording.take().unwrap_or_default();
        let assumptions = std::mem::take(&mut self.assumptions);
        self.conds.push(CondRecord {
            file: f,
            line: d.line,
            kind,
            off: d.off,
            end: d.end,
            value,
            parent: self.current_cond(),
            consulted,
            assumptions,
            error: None,
            prev: Vec::new(),
            answers: std::mem::take(&mut self.answers),
        });
        (self.conds.len() - 1) as CondId
    }

    fn process_file(&mut self, f: SrcFile, found: Option<usize>, depth: usize) {
        if depth > MAX_INCLUDE_DEPTH {
            return;
        }
        self.visited.insert(f);
        let content = self.files.content(f);
        let dirs = self.files.directives(f);
        let asm = self.files.entry(f).asm || self.model.asm;
        let stop_line = match &self.stop {
            Some((p, l)) if self.files.entry(f).abs.as_deref() == Some(p.as_path()) => Some(*l),
            _ => None,
        };
        self.stack.push(Frame { file: f, found });
        let mut ifs: Vec<IfState> = Vec::new();
        for (i, d) in dirs.iter().enumerate() {
            if self.stopped {
                break;
            }
            if stop_line.is_some_and(|l| d.line >= l) {
                self.stopped = true;
                break;
            }
            self.cur_line = d.line;
            self.asm_now = asm;
            let head = dirs.head(i);
            let active = ifs.last().is_none_or(|s| s.active);
            match head.kind {
                DirKind::If | DirKind::Ifdef | DirKind::Ifndef => {
                    if !active {
                        ifs.push(IfState {
                            active: false,
                            taken: true,
                            parent_active: false,
                            skip_from: None,
                            pushed: false,
                            branches: Vec::new(),
                        });
                        continue;
                    }
                    let (v, cond) = self.eval_condition(f, d, &content, head.kind, asm);
                    if v {
                        self.active_conds.push(cond);
                    }
                    ifs.push(IfState {
                        active: v,
                        taken: v,
                        parent_active: true,
                        skip_from: (!v).then_some((d.line, cond)),
                        pushed: v,
                        branches: vec![cond],
                    });
                }
                DirKind::Elif | DirKind::Elifdef | DirKind::Elifndef | DirKind::Else => {
                    let Some(mut st) = ifs.pop() else {
                        self.diag(
                            f,
                            d.line,
                            DiagKind::Error,
                            format!("#{} without #if", head.kind.as_str()),
                        );
                        continue;
                    };
                    if st.parent_active {
                        self.close_skip(f, &mut st, d.line);
                        if st.pushed {
                            self.active_conds.pop();
                            st.pushed = false;
                        }
                        let cond = if st.taken {
                            st.active = false;
                            let cond = self.record_cond(f, d, head.kind, None);
                            st.skip_from = Some((d.line, cond));
                            cond
                        } else if head.kind == DirKind::Else {
                            let cond = self.record_cond(f, d, head.kind, Some(true));
                            st.active = true;
                            st.taken = true;
                            st.pushed = true;
                            self.active_conds.push(cond);
                            cond
                        } else {
                            let (v, cond) = self.eval_condition(f, d, &content, head.kind, asm);
                            st.active = v;
                            st.taken = v;
                            if v {
                                st.pushed = true;
                                self.active_conds.push(cond);
                            } else {
                                st.skip_from = Some((d.line, cond));
                            }
                            cond
                        };
                        self.conds[cond as usize].prev = st.branches.clone();
                        st.branches.push(cond);
                    }
                    ifs.push(st);
                }
                DirKind::Endif => {
                    let Some(mut st) = ifs.pop() else {
                        self.diag(f, d.line, DiagKind::Error, "#endif without #if".to_string());
                        continue;
                    };
                    self.close_skip(f, &mut st, d.line);
                    if st.pushed {
                        self.active_conds.pop();
                    }
                }
                _ if !active => {}
                DirKind::Define => self.do_define(f, d, &head, asm),
                DirKind::Undef => self.do_undef(f, d, &head),
                DirKind::Include | DirKind::IncludeNext => self.do_include(
                    f,
                    d,
                    &content,
                    head.kind == DirKind::IncludeNext,
                    asm,
                    depth,
                ),
                DirKind::Error | DirKind::Warning => {
                    let msg = String::from_utf8_lossy(&content[d.off as usize..d.end as usize])
                        .into_owned();
                    let kind = if head.kind == DirKind::Error {
                        DiagKind::Error
                    } else {
                        DiagKind::Warning
                    };
                    self.diag(f, d.line, kind, msg.trim().to_string());
                }
                DirKind::Pragma => self.do_pragma(f, d, &content, asm),
                _ => {}
            }
        }
        let eof_line = if ifs.is_empty() {
            0
        } else {
            memchr::memchr_iter(b'\n', &content).count() as u32 + 1
        };
        while let Some(mut st) = ifs.pop() {
            self.close_skip(f, &mut st, eof_line);
            if st.pushed {
                self.active_conds.pop();
            }
            if !self.stopped {
                self.diag(
                    f,
                    eof_line,
                    DiagKind::Warning,
                    "unterminated conditional directive".to_string(),
                );
            }
        }
        self.stack.pop();
    }

    fn diag(&mut self, file: SrcFile, line: u32, kind: DiagKind, message: String) {
        self.diags.push(Diag {
            file,
            line,
            kind,
            message,
        });
    }

    fn directive_tokens(&mut self, content: &[u8], d: &DirLine, asm: bool) -> Vec<Token> {
        let toks = lex::lex_all(&content[d.off as usize..d.end as usize], asm, &mut self.it);
        toks.into_iter().skip(2).collect()
    }

    fn eval_condition(
        &mut self,
        f: SrcFile,
        d: &DirLine,
        content: &[u8],
        kind: DirKind,
        asm: bool,
    ) -> (bool, CondId) {
        let toks = self.directive_tokens(content, d, asm);
        self.recording = Some(Vec::new());
        self.assumptions.clear();
        self.answers.clear();
        let (value, error) = match kind {
            DirKind::Ifdef | DirKind::Ifndef | DirKind::Elifdef | DirKind::Elifndef => {
                match toks.first() {
                    Some(t) if t.kind == TokenKind::Ident => {
                        let def = self.defined_query(t.sym);
                        let want = matches!(kind, DirKind::Ifdef | DirKind::Elifdef);
                        (def == want, None)
                    }
                    _ => (
                        false,
                        Some(format!("#{} with no macro name", kind.as_str())),
                    ),
                }
            }
            _ => {
                let unsigned_char = self.unsigned_char;
                let mut ex = Expander::new(self, true);
                let expanded = ex.expand(toks);
                let diags = std::mem::take(&mut ex.diags);
                drop(ex);
                match expr::eval(&expanded, &self.it, unsigned_char) {
                    Ok(n) => (n.is_true(), diags.into_iter().next()),
                    Err(e) => (false, Some(e.0)),
                }
            }
        };
        let id = self.record_cond(f, d, kind, Some(value));
        if let Some(e) = error {
            self.conds[id as usize].error = Some(e.clone());
            self.diag(f, d.line, DiagKind::Error, e);
        }
        (value, id)
    }

    fn operator_available(&self, s: Sym) -> bool {
        if self.macros.contains_key(&s) {
            return false;
        }
        match has_operator(s) {
            Some(op) => self.features.operator_defined(op, self.asm_now),
            None => s == sym::HAS_INCLUDE || s == sym::HAS_INCLUDE_NEXT,
        }
    }

    fn defined_query(&mut self, s: Sym) -> bool {
        if self.operator_available(s) {
            return true;
        }
        let def = self.def_of(s);
        if let Some(rec) = &mut self.recording
            && !rec.iter().any(|(x, _)| *x == s)
        {
            rec.push((s, def.clone()));
        }
        def.is_some()
    }

    fn do_define(&mut self, f: SrcFile, d: &DirLine, head: &scan::Head<'_>, asm: bool) {
        let Some(name) = &head.name else {
            self.diag(
                f,
                d.line,
                DiagKind::Error,
                "macro names must be identifiers".to_string(),
            );
            return;
        };
        let s = self.it.intern_bytes(name);
        if s == sym::DEFINED {
            self.diag(
                f,
                d.line,
                DiagKind::Error,
                "\"defined\" cannot be used as a macro name".to_string(),
            );
            return;
        }
        let site = Site {
            file: f,
            line: d.line,
            off: d.off,
            end: d.end,
        };
        let kind = if self.macros.contains_key(&s) {
            EventKind::Redefine
        } else {
            EventKind::Define
        };
        let origin = self.origin_now;
        self.macros.insert(
            s,
            Slot {
                site: Some(site),
                origin,
                def: None,
                asm,
            },
        );
        self.push_event(s, kind, Some(site), origin);
    }

    fn do_undef(&mut self, f: SrcFile, d: &DirLine, head: &scan::Head<'_>) {
        let Some(name) = &head.name else { return };
        let s = self.it.intern_bytes(name);
        if self.macros.remove(&s).is_some() || self.events_for(s).next().is_some() {
            let site = Site {
                file: f,
                line: d.line,
                off: d.off,
                end: d.end,
            };
            let origin = self.origin_now;
            self.push_event(s, EventKind::Undef, Some(site), origin);
        }
    }

    pub fn def_of(&mut self, s: Sym) -> Option<Rc<MacroDef>> {
        let slot = self.macros.get(&s)?;
        if let Some(d) = &slot.def {
            return Some(d.clone());
        }
        let site = slot.site?;
        let origin = slot.origin;
        let asm = slot.asm;
        let content = self.files.content(site.file);
        let text = &content[site.off as usize..site.end as usize];
        let toks = lex::lex_all(text, asm, &mut self.it);
        let parsed = toks.get(2..).and_then(parse_after_keyword);
        let def = Rc::new(match parsed {
            Some(p) => MacroDef {
                name: s,
                site: Some(site),
                origin,
                params: p.params,
                body: p.body,
                builtin: None,
            },
            None => MacroDef {
                name: s,
                site: Some(site),
                origin,
                params: Params::Object,
                body: Vec::new(),
                builtin: None,
            },
        });
        if let Some(slot) = self.macros.get_mut(&s) {
            slot.def = Some(def.clone());
        }
        Some(def)
    }

    fn do_pragma(&mut self, f: SrcFile, d: &DirLine, content: &[u8], asm: bool) {
        let toks = self.directive_tokens(content, d, asm);
        let Some(first) = toks.first() else { return };
        let word = self.it.get(first.sym).to_string();
        match word.as_str() {
            "once" => {
                self.once.insert(f);
            }
            "push_macro" | "pop_macro" => {
                let Some(name) = toks.iter().find(|t| t.kind == TokenKind::Str) else {
                    return;
                };
                let n = self.it.get(name.sym).trim_matches('"').to_string();
                let s = self.it.intern(&n);
                if word == "push_macro" {
                    let cur = self.macros.get(&s).cloned();
                    self.pushed.entry(s).or_default().push(cur);
                } else if let Some(prev) = self.pushed.get_mut(&s).and_then(Vec::pop) {
                    match prev {
                        Some(slot) => {
                            self.macros.insert(s, slot);
                        }
                        None => {
                            self.macros.remove(&s);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub fn events_for(&self, s: Sym) -> impl Iterator<Item = &MacroEvent> + '_ {
        let mut at = self
            .events_by_sym
            .get(s.0 as usize)
            .map_or(NO_EVENT, |&(first, _)| first);
        std::iter::from_fn(move || {
            let e = self.events.get(at as usize)?;
            at = self.event_next[at as usize];
            Some(e)
        })
    }

    pub fn autoconf_override(&self) -> Option<&str> {
        self.autoconf_override.as_deref()
    }

    pub fn is_defined(&self, s: Sym) -> bool {
        self.macros.contains_key(&s)
    }

    pub fn skip_at(&self, f: SrcFile, line: u32) -> Option<SkipRange> {
        self.skips
            .get(&f)?
            .iter()
            .filter(|r| r.from < line && line < r.to)
            .min_by_key(|r| r.to - r.from)
            .copied()
    }

    pub fn visited(&self, f: SrcFile) -> bool {
        self.visited.contains(&f)
    }

    pub fn macro_names(&self) -> impl Iterator<Item = Sym> + '_ {
        self.macros.keys()
    }
}

impl Env for Sim<'_> {
    fn it(&mut self) -> &mut Interner {
        &mut self.it
    }

    fn hs(&mut self) -> &mut HideSets {
        &mut self.hs
    }

    fn lookup(&mut self, s: Sym) -> Option<Rc<MacroDef>> {
        let def = self.def_of(s);
        if let Some(rec) = &mut self.recording
            && !rec.iter().any(|(x, _)| *x == s)
        {
            rec.push((s, def.clone()));
        }
        def
    }

    fn defined(&mut self, s: Sym) -> bool {
        self.defined_query(s)
    }

    fn builtin(&mut self, b: Builtin) -> Token {
        if self.symbolic_builtins
            && !matches!(b, Builtin::Date | Builtin::Time | Builtin::Timestamp)
        {
            let name = match b {
                Builtin::Line => sym::LINE_,
                Builtin::Counter => sym::COUNTER_,
                Builtin::IncludeLevel => sym::INCLUDE_LEVEL_,
                Builtin::File => sym::FILE_,
                _ => sym::BASE_FILE_,
            };
            return Token::new(TokenKind::Ident, name, crate::syntax::lex::F_NOEXP);
        }
        let text = match b {
            Builtin::Line => self.cur_line.to_string(),
            Builtin::Counter => {
                self.counter += 1;
                (self.counter - 1).to_string()
            }
            Builtin::IncludeLevel => self.stack.len().saturating_sub(1).to_string(),
            Builtin::File => {
                let f = self.stack.last().map(|fr| fr.file);
                format!(
                    "\"{}\"",
                    f.map(|f| self.files.display(f).to_string())
                        .unwrap_or_default()
                )
            }
            Builtin::BaseFile => {
                format!(
                    "\"{}\"",
                    self.main
                        .map(|f| self.files.display(f).to_string())
                        .unwrap_or_default()
                )
            }
            Builtin::Date => "\"??? ?? ????\"".to_string(),
            Builtin::Time => "\"??:??:??\"".to_string(),
            Builtin::Timestamp => "\"??? ??? ?? ??:??:?? ????\"".to_string(),
        };
        let kind = if text.starts_with('"') {
            TokenKind::Str
        } else {
            TokenKind::Number
        };
        Token::new(kind, self.it.intern(&text), 0)
    }

    fn has_include(&mut self, name: &str, angled: bool, next: bool) -> bool {
        let v = self.resolve(name, angled, next).is_some();
        if self.recording.is_some() {
            let op = if next {
                "__has_include_next"
            } else {
                "__has_include"
            };
            let spec = if angled {
                format!("<{name}>")
            } else {
                format!("\"{name}\"")
            };
            self.answers.push((format!("{op}({spec})"), v));
        }
        v
    }

    fn has_feature(&mut self, op: Sym, name: &str) -> Option<bool> {
        let probe = has_operator(op)?;
        let (v, how) = self.features.query(probe, name);
        if self.recording.is_some() {
            self.answers
                .push((format!("{}({name})", self.it.get(op)), v));
        }
        if how != Answer::Probed {
            let source = if how == Answer::Table {
                "version table"
            } else {
                "assumed"
            };
            self.assumptions.push(format!(
                "{}({name}) = {} ({source})",
                probe.operator(),
                u8::from(v)
            ));
        }
        Some(v)
    }

    fn asm(&self) -> bool {
        self.asm_now
    }

    fn operator(&mut self, s: Sym) -> bool {
        self.operator_available(s)
    }
}

impl Sim<'_> {
    pub fn expand_text(
        &mut self,
        tokens: Vec<Token>,
        traced: bool,
    ) -> (Vec<Token>, Option<super::expand::Trace>, bool) {
        self.expand_depth += 1;
        let mut ex = Expander::new(self, false);
        if traced {
            ex = ex.traced();
        }
        let out = ex.expand(tokens);
        let truncated = ex.truncated;
        let trace = ex.into_trace();
        self.expand_depth -= 1;
        (out, trace, truncated)
    }

    pub fn cond_text(&self, c: &CondRecord) -> String {
        let content = self.files.content(c.file);
        let raw = lex::splice(&content[c.off as usize..c.end as usize]);
        String::from_utf8_lossy(&raw).trim().to_string()
    }
}
