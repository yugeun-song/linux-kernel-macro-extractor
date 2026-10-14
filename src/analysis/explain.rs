use std::rc::Rc;

use rustc_hash::FxHashMap;

use super::report::{Atom, Condition};
use super::value::{self, Evaluated, ValueCtx};
use crate::intern::{Interner, Sym, sym};
use crate::pp::expand::{Env, Expander};
use crate::pp::expr;
use crate::pp::hide_set::HideSets;
use crate::pp::macros::{Builtin, MacroDef, Origin, Params};
use crate::pp::sim::{CondId, CondRecord, Sim};
use crate::syntax::lex::{self, Token, TokenKind};
use crate::syntax::scan::DirKind;
use crate::tree::dot_config::DotConfig;

struct Frozen<'a> {
    it: &'a mut Interner,
    hs: HideSets,
    table: FxHashMap<Sym, Option<Rc<MacroDef>>>,
    answers: &'a [(String, bool)],
}

impl Env for Frozen<'_> {
    fn it(&mut self) -> &mut Interner {
        self.it
    }

    fn hs(&mut self) -> &mut HideSets {
        &mut self.hs
    }

    fn lookup(&mut self, s: Sym) -> Option<Rc<MacroDef>> {
        self.table.get(&s).cloned().flatten()
    }

    fn builtin(&mut self, _b: Builtin) -> Token {
        Token::new(TokenKind::Number, self.it.intern("0"), 0)
    }

    fn has_include(&mut self, name: &str, angled: bool, next: bool) -> bool {
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
        let key = format!("{op}({spec})");
        self.answers.iter().any(|(k, v)| *k == key && *v)
    }

    fn has_feature(&mut self, op: Sym, name: &str) -> Option<bool> {
        let key = format!("{}({name})", self.it.get(op));
        self.answers
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)
    }
}

pub fn config_site(cfg: &DotConfig, name: &str) -> Option<String> {
    let base = name.strip_prefix("CONFIG_")?;
    let base = base
        .strip_suffix("_MODULE")
        .filter(|b| cfg.get(b).is_some())
        .unwrap_or(base);
    let e = cfg.get(base)?;
    Some(format!(
        ".config:{} CONFIG_{base}={}",
        e.line,
        e.value.as_config_text()
    ))
}

fn eval_atom(sim: &mut Sim<'_>, rec: &CondRecord, tokens: Vec<Token>) -> String {
    let table: FxHashMap<Sym, Option<Rc<MacroDef>>> = rec.consulted.iter().cloned().collect();
    let mut env = Frozen {
        it: &mut sim.it,
        hs: HideSets::new(),
        table,
        answers: &rec.answers,
    };
    let out = Expander::new(&mut env, true).expand(tokens);
    match expr::eval(&out, &sim.it, true) {
        Ok(n) => n.display(),
        Err(_) => lex::render(&out, &sim.it),
    }
}

fn site_text(sim: &Sim<'_>, def: &MacroDef) -> Option<String> {
    let site = def.site?;
    let path = sim.files.display(site.file);
    Some(format!("{path}:{}", site.line))
}

pub fn macro_value(sim: &mut Sim<'_>, def: &Rc<MacroDef>, cfg: &DotConfig) -> String {
    if def.is_function() {
        return "function-like macro".to_string();
    }
    let name = Token::new(TokenKind::Ident, def.name, 0);
    let (out, _, _) = sim.expand_text(vec![name], false);
    let it = &sim.it;
    let cx = ValueCtx {
        it,
        config: cfg,
        char_unsigned: true,
    };
    match value::evaluate(&out, &cx) {
        Evaluated::Int(v) => value::display(&v, true).0,
        Evaluated::Str(s) => format!("\"{s}\""),
        Evaluated::NotConstant(_) => {
            let r = lex::render(&out, it);
            if r.is_empty() {
                "(empty)".to_string()
            } else {
                r
            }
        }
    }
}

fn directive_tokens(sim: &mut Sim<'_>, rec: &CondRecord) -> Vec<Token> {
    let content = sim.files.content(rec.file);
    let text = content[rec.off as usize..rec.end as usize].to_vec();
    lex::lex_all(&text, false, &mut sim.it)
        .into_iter()
        .skip(2)
        .collect()
}

fn atoms_for_if(sim: &mut Sim<'_>, rec: &CondRecord, cfg: &DotConfig) -> Vec<Atom> {
    let toks = directive_tokens(sim, rec);
    let mut atoms: Vec<Atom> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if t.kind != TokenKind::Ident {
            i += 1;
            continue;
        }
        if t.sym == sym::DEFINED {
            let mut j = i + 1;
            let paren = toks.get(j).is_some_and(|x| x.is_punct(sym::LPAREN));
            if paren {
                j += 1;
            }
            if let Some(n) = toks.get(j).filter(|x| x.kind == TokenKind::Ident) {
                let name = sim.it.get(n.sym).to_string();
                let def = rec
                    .consulted
                    .iter()
                    .find(|(s, _)| *s == n.sym)
                    .and_then(|(_, d)| d.clone());
                let text = format!("defined({name})");
                if !seen.contains(&text) {
                    seen.push(text.clone());
                    atoms.push(Atom {
                        value: if def.is_some() {
                            "1".into()
                        } else {
                            "0".into()
                        },
                        site: def
                            .as_ref()
                            .and_then(|d| config_site(cfg, &name).or_else(|| site_text(sim, d))),
                        text,
                    });
                }
                i = j + 1 + usize::from(paren);
                continue;
            }
        }
        let mut end = i + 1;
        if toks.get(end).is_some_and(|x| x.is_punct(sym::LPAREN)) {
            let mut depth = 0;
            while end < toks.len() {
                if toks[end].is_punct(sym::LPAREN) {
                    depth += 1;
                } else if toks[end].is_punct(sym::RPAREN) {
                    depth -= 1;
                    if depth == 0 {
                        end += 1;
                        break;
                    }
                }
                end += 1;
            }
        }
        let group: Vec<Token> = toks[i..end].to_vec();
        let text = lex::render(&group, &sim.it);
        if !seen.contains(&text) {
            seen.push(text.clone());
            let value = eval_atom(sim, rec, group);
            let name = sim.it.get(t.sym).to_string();
            let def = rec
                .consulted
                .iter()
                .find(|(s, _)| *s == t.sym)
                .and_then(|(_, d)| d.clone());
            let shown = if def.is_none()
                && end == i + 1
                && !rec.answers.iter().any(|(k, _)| k.starts_with(&name))
            {
                "undefined (0)".to_string()
            } else {
                value
            };
            atoms.push(Atom {
                site: config_site(cfg, &name)
                    .or_else(|| def.as_ref().and_then(|d| site_text(sim, d))),
                text,
                value: shown,
            });
        }
        i = end;
    }
    for (s, def) in rec.consulted.clone() {
        let name = sim.it.get(s).to_string();
        if !name.starts_with("CONFIG_")
            || seen
                .iter()
                .any(|x| x == &name || x.contains(&format!("({name})")))
        {
            continue;
        }
        seen.push(name.clone());
        atoms.push(Atom {
            value: match &def {
                Some(d) => macro_value(sim, d, cfg),
                None => "undefined".to_string(),
            },
            site: config_site(cfg, &name),
            text: name,
        });
    }
    atoms
}

fn atoms_of(sim: &mut Sim<'_>, rec: &CondRecord, cfg: &DotConfig) -> Vec<Atom> {
    match rec.kind {
        DirKind::Ifdef | DirKind::Ifndef | DirKind::Elifdef | DirKind::Elifndef => {
            match rec.consulted.first() {
                Some((s, def)) => {
                    let name = sim.it.get(*s).to_string();
                    vec![Atom {
                        value: match def {
                            Some(d) if d.origin == Origin::Predefined => {
                                "defined (compiler predefined)".into()
                            }
                            Some(d) if d.origin == Origin::CommandLine => {
                                "defined (command line -D)".into()
                            }
                            Some(_) => "defined".into(),
                            None => "undefined".into(),
                        },
                        site: config_site(cfg, &name)
                            .or_else(|| def.as_ref().and_then(|d| site_text(sim, d))),
                        text: name,
                    }]
                }
                None => Vec::new(),
            }
        }
        DirKind::If | DirKind::Elif if rec.value.is_some() => atoms_for_if(sim, rec, cfg),
        _ => Vec::new(),
    }
}

pub fn condition(sim: &mut Sim<'_>, id: CondId, cfg: &DotConfig) -> Condition {
    let rec = sim.conds[id as usize].clone();
    let directive = sim.cond_text(&rec);
    let mut notes = Vec::new();
    let atoms = atoms_of(sim, &rec, cfg);
    let mut prior = Vec::new();
    if matches!(
        rec.kind,
        DirKind::Else | DirKind::Elif | DirKind::Elifdef | DirKind::Elifndef
    ) {
        for p in &rec.prev {
            let pr = sim.conds[*p as usize].clone();
            prior.extend(atoms_of(sim, &pr, cfg));
            let t = sim.cond_text(&pr);
            let verdict = match pr.value {
                Some(true) => "true, so the rest of this group is skipped",
                Some(false) => "false",
                None => "not evaluated",
            };
            let why = pr
                .consulted
                .first()
                .filter(|_| {
                    matches!(
                        pr.kind,
                        DirKind::Ifdef | DirKind::Ifndef | DirKind::Elifdef | DirKind::Elifndef
                    )
                })
                .map(|(s, d)| {
                    let state = match d {
                        Some(d) if d.origin == Origin::CommandLine => "defined on the command line",
                        Some(d) if d.origin == Origin::Predefined => "predefined by the compiler",
                        Some(_) => "defined",
                        None => "undefined",
                    };
                    format!(" ({} {state})", sim.it.get(*s))
                })
                .unwrap_or_default();
            notes.push(format!("line {}: {t} -> {verdict}{why}", pr.line));
        }
    }
    notes.extend(rec.assumptions.iter().cloned());
    if let Some(e) = &rec.error {
        notes.push(format!("error: {e}"));
    }
    Condition {
        path: sim.files.display(rec.file).to_string(),
        line: rec.line,
        directive,
        taken: rec.value == Some(true),
        evaluated: rec.value.is_some(),
        atoms,
        prior,
        notes,
    }
}

pub fn chain(sim: &mut Sim<'_>, mut id: CondId, cfg: &DotConfig) -> Vec<Condition> {
    let mut ids = Vec::new();
    while id != 0 {
        ids.push(id);
        id = sim.conds[id as usize].parent;
    }
    ids.reverse();
    ids.retain(|&i| !sim.is_guard_condition(i));
    ids.into_iter().map(|i| condition(sim, i, cfg)).collect()
}

pub fn params_of(def: &MacroDef, it: &Interner) -> Vec<String> {
    match &def.params {
        Params::Object => Vec::new(),
        Params::Function { names, variadic } => {
            let mut v: Vec<String> = names.iter().map(|n| it.get(*n).to_string()).collect();
            match variadic {
                crate::pp::macros::Variadic::Anonymous => v.push("...".into()),
                crate::pp::macros::Variadic::Named => {
                    if let Some(l) = v.last_mut() {
                        l.push_str("...");
                    }
                }
                crate::pp::macros::Variadic::No => {}
            }
            v
        }
    }
}
