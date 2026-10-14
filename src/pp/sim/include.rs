use std::path::{Path, PathBuf};
use std::rc::Rc;

use rustc_hash::FxHashSet;

use super::{CondId, DiagKind, IncludeEvent, IncludeKey, IncludeResult, MAX_INCLUDE_DEPTH, Sim};
use crate::intern::{Sym, sym};
use crate::pp::expand::Expander;
use crate::pp::kbuild::normalize;
use crate::pp::macros::SrcFile;
use crate::syntax::lex::{self, F_SPACE, TokenKind};
use crate::syntax::scan::{DirKind, DirLine};

const QUOTED: u32 = u32::MAX;

pub(super) struct Wrappers {
    kernel: FxHashSet<String>,
    uapi: FxHashSet<String>,
}

impl Sim<'_> {
    fn header_spec(&mut self, content: &[u8], d: &DirLine, asm: bool) -> Option<(String, bool)> {
        let raw = lex::splice(&content[d.off as usize..d.end as usize]).into_owned();
        let mut i = 1;
        while i < raw.len() && raw[i].is_ascii_whitespace() {
            i += 1;
        }
        while i < raw.len() && (raw[i].is_ascii_alphanumeric() || raw[i] == b'_') {
            i += 1;
        }
        let mut lx = lex::Lexer::new(&raw[i..], asm);
        lx.skip_ws();
        let rest = lx.rest();
        match rest.first() {
            Some(b'"') => {
                let end = memchr::memchr(b'"', &rest[1..])?;
                Some((
                    String::from_utf8_lossy(&rest[1..1 + end]).into_owned(),
                    false,
                ))
            }
            Some(b'<') => {
                let end = memchr::memchr(b'>', &rest[1..])?;
                Some((
                    String::from_utf8_lossy(&rest[1..1 + end]).into_owned(),
                    true,
                ))
            }
            _ => {
                let toks = lex::lex_all(rest, asm, &mut self.it);
                let mut ex = Expander::new(self, false);
                let out = ex.expand(toks);
                drop(ex);
                match out.first() {
                    Some(t) if t.kind == TokenKind::Str => {
                        Some((self.it.get(t.sym).trim_matches('"').to_string(), false))
                    }
                    Some(t) if t.is_punct(sym::LT) => {
                        let mut s = String::new();
                        for (k, x) in out[1..].iter().enumerate() {
                            if x.is_punct(sym::GT) {
                                break;
                            }
                            if k > 0 && x.flags & F_SPACE != 0 {
                                s.push(' ');
                            }
                            s.push_str(lex::spell(x, &self.it));
                        }
                        Some((s, true))
                    }
                    _ => None,
                }
            }
        }
    }

    pub fn replaced_by_what_if(&self, abs: &Path) -> bool {
        self.autoconf_override.is_some() && normalize(abs) == self.autoconf
    }

    fn generated_override(&mut self, abs: &Path) -> Option<SrcFile> {
        if abs != self.autoconf {
            return None;
        }
        let text = self.autoconf_override.clone()?;
        Some(self.files.add_virtual(
            "include/generated/autoconf.h (what-if)",
            Some(self.autoconf.clone()),
            text.into_bytes(),
        ))
    }

    fn synthesized(&mut self, abs: &Path) -> Option<SrcFile> {
        if abs == self.autoconf {
            let text = self.config.autoconf_h();
            return Some(self.files.add_virtual(
                "include/generated/autoconf.h (from .config)",
                Some(self.autoconf.clone()),
                text.into_bytes(),
            ));
        }
        let parent = abs.parent()?;
        let uapi = if parent == self.gen_asm {
            false
        } else if parent == self.gen_uapi {
            true
        } else {
            return None;
        };
        let name = abs.file_name()?.to_str()?;
        let lists = self.wrapper_lists();
        let listed = if uapi { &lists.uapi } else { &lists.kernel };
        if !listed.contains(name) {
            return None;
        }
        let display = self.locator().display_for(abs);
        Some(self.files.add_virtual(
            &format!("{display} (asm-generic wrapper)"),
            Some(abs.to_path_buf()),
            format!("#include <asm-generic/{name}>\n").into_bytes(),
        ))
    }

    fn wrapper_lists(&mut self) -> Rc<Wrappers> {
        if let Some(w) = &self.wrappers {
            return w.clone();
        }
        let s = &self.layout.srctree;
        let arch = &self.layout.srcarch;
        let read = |rel: &str, keys: &[&str]| -> FxHashSet<String> {
            let text = std::fs::read_to_string(s.join(rel)).unwrap_or_default();
            text.lines()
                .filter_map(|l| {
                    let (k, v) = l.split_once("+=")?;
                    keys.contains(&k.trim())
                        .then(|| v.split_whitespace().map(str::to_string).collect::<Vec<_>>())
                })
                .flatten()
                .collect()
        };
        let mut kernel = read("include/asm-generic/Kbuild", &["mandatory-y"]);
        kernel.extend(read(
            &format!("arch/{arch}/include/asm/Kbuild"),
            &["generic-y", "mandatory-y"],
        ));
        let mut uapi = read("include/uapi/asm-generic/Kbuild", &["mandatory-y"]);
        uapi.extend(read(
            &format!("arch/{arch}/include/uapi/asm/Kbuild"),
            &["generic-y", "mandatory-y"],
        ));
        let own = |dir: &str, set: &mut FxHashSet<String>| {
            if let Ok(rd) = std::fs::read_dir(s.join(dir)) {
                for e in rd.flatten() {
                    if let Some(n) = e.file_name().to_str() {
                        set.remove(n);
                    }
                }
            }
        };
        own(&format!("arch/{arch}/include/asm"), &mut kernel);
        own(&format!("arch/{arch}/include/uapi/asm"), &mut uapi);
        let w = Rc::new(Wrappers { kernel, uapi });
        self.wrappers = Some(w.clone());
        w
    }

    pub fn resolve(
        &mut self,
        name: &str,
        angled: bool,
        next: bool,
    ) -> Option<(SrcFile, Option<usize>)> {
        if Path::new(name).is_absolute() {
            let loc = self.locator();
            return loc
                .find(&mut self.files, Path::new(name), true)
                .map(|f| (f, None));
        }
        let cur = self.stack.last().map(|fr| (fr.file, fr.found));
        let name_sym = self.it.intern(name);
        if !angled
            && !next
            && let Some(f) = cur.map(|(f, _)| f)
            && let Some(dir) = self.dir_sym(f)
        {
            let key = (QUOTED, dir, name_sym);
            let found = match self.cached_include(key) {
                Some(r) => r,
                None => {
                    let dir = PathBuf::from(self.it.get(dir));
                    let r = self.resolve_quoted(&dir, name);
                    self.cache_include(key, r);
                    r
                }
            };
            if found.is_some() {
                return found;
            }
        } else if !angled
            && !next
            && let Some(dir) =
                cur.and_then(|(f, _)| self.files.entry(f).dir().map(Path::to_path_buf))
            && let Some(r) = self.resolve_quoted(&dir, name)
        {
            return Some(r);
        }
        let start = if next {
            match cur.and_then(|(_, found)| found) {
                Some(i) => i + 1,
                None => self.angled_start,
            }
        } else if angled {
            self.angled_start
        } else {
            0
        };
        let key = (start as u32, sym::EMPTY, name_sym);
        if let Some(r) = self.cached_include(key) {
            return r;
        }
        let r = self.resolve_chain(name, start);
        self.cache_include(key, r);
        r
    }

    fn dir_sym(&mut self, f: SrcFile) -> Option<Sym> {
        let i = f.0 as usize;
        if let Some(&Some(known)) = self.dir_syms.get(i) {
            return known;
        }
        let dir = self
            .files
            .entry(f)
            .dir()
            .and_then(Path::to_str)
            .map(str::to_string);
        let s = dir.map(|d| self.it.intern(&d));
        if self.dir_syms.len() <= i {
            self.dir_syms.resize(i + 1, None);
        }
        self.dir_syms[i] = Some(s);
        s
    }

    fn cached_include(&mut self, key: IncludeKey) -> Option<Option<(SrcFile, Option<usize>)>> {
        if self.files.volatile() != self.resolved_at {
            self.resolved.clear();
            self.resolved_at = self.files.volatile();
        }
        self.resolved.get(&key).copied()
    }

    fn cache_include(&mut self, key: IncludeKey, r: Option<(SrcFile, Option<usize>)>) {
        if self.files.volatile() != self.resolved_at {
            self.resolved.clear();
            self.resolved_at = self.files.volatile();
        }
        self.resolved.insert(key, r);
    }

    fn resolve_quoted(&mut self, dir: &Path, name: &str) -> Option<(SrcFile, Option<usize>)> {
        let cand = normalize(&dir.join(name));
        if let Some(v) = self.generated_override(&cand) {
            return Some((v, None));
        }
        let loc = self.locator();
        loc.find_normal(&mut self.files, &cand, true)
            .map(|f| (f, None))
    }

    fn resolve_chain(&mut self, name: &str, start: usize) -> Option<(SrcFile, Option<usize>)> {
        for i in start..self.chain.len() {
            let cand = normalize(&self.chain[i].join(name));
            if let Some(v) = self.generated_override(&cand) {
                return Some((v, Some(i)));
            }
            let loc = self.locator();
            if let Some(f) = loc.find_normal(&mut self.files, &cand, false) {
                return Some((f, Some(i)));
            }
            if let Some(v) = self.synthesized(&cand) {
                return Some((v, Some(i)));
            }
        }
        None
    }

    pub fn is_guard_condition(&mut self, id: CondId) -> bool {
        let rec = &self.conds[id as usize];
        if rec.kind != DirKind::Ifndef {
            return false;
        }
        let (file, line) = (rec.file, rec.line);
        let name = rec.consulted.first().map(|(s, _)| *s);
        let first_line = self.files.directives(file).first().map(|d| d.line);
        first_line == Some(line) && name.is_some() && self.guard_of(file) == name
    }

    pub fn guard_of(&mut self, f: SrcFile) -> Option<Sym> {
        if let Some(g) = self.guards.get(&f) {
            return *g;
        }
        let g = self.detect_guard(f);
        self.guards.insert(f, g);
        g
    }

    fn detect_guard(&mut self, f: SrcFile) -> Option<Sym> {
        let dirs = self.files.directives(f);
        if dirs.len() < 3 {
            return None;
        }
        let h0 = dirs.head(0);
        let h1 = dirs.head(1);
        if h0.kind != DirKind::Ifndef
            || h1.kind != DirKind::Define
            || h0.name.is_none()
            || h0.name != h1.name
        {
            return None;
        }
        let mut depth = 0i32;
        for i in 0..dirs.len() {
            match dirs.head(i).kind {
                DirKind::If | DirKind::Ifdef | DirKind::Ifndef => depth += 1,
                DirKind::Endif => {
                    depth -= 1;
                    if depth == 0 {
                        return (i == dirs.len() - 1)
                            .then(|| self.it.intern_bytes(h0.name.as_deref().unwrap_or(b"")));
                    }
                }
                _ => {}
            }
        }
        None
    }

    pub(super) fn do_include(
        &mut self,
        f: SrcFile,
        d: &DirLine,
        content: &[u8],
        next: bool,
        asm: bool,
        depth: usize,
    ) {
        let Some((name, angled)) = self.header_spec(content, d, asm) else {
            self.diag(
                f,
                d.line,
                DiagKind::Error,
                "#include expects \"FILENAME\" or <FILENAME>".to_string(),
            );
            return;
        };
        let result = match self.resolve(&name, angled, next) {
            None => {
                self.diag(
                    f,
                    d.line,
                    DiagKind::MissingInclude,
                    format!("{name}: not found"),
                );
                IncludeResult::Missing
            }
            Some(_) if depth + 1 > MAX_INCLUDE_DEPTH => IncludeResult::TooDeep,
            Some((target, _)) if self.once.contains(&target) => IncludeResult::Once(target),
            Some((target, found)) => {
                let guard = if self.visited.contains(&target) {
                    self.guard_of(target)
                } else {
                    None
                };
                match guard {
                    Some(g) if self.macros.contains_key(&g) => IncludeResult::Guarded(target, g),
                    _ => {
                        self.includes.push(IncludeEvent {
                            from: f,
                            line: d.line,
                            spec: name,
                            angled,
                            result: IncludeResult::Entered(target),
                        });
                        self.process_file(target, found, depth + 1);
                        return;
                    }
                }
            }
        };
        self.includes.push(IncludeEvent {
            from: f,
            line: d.line,
            spec: name,
            angled,
            result,
        });
    }
}
