use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kmx::analysis::context::{compiler_headers, prepare, sim_for};
use kmx::pp::kbuild::{CompileModel, normalize, read_cmd_line};
use kmx::pp::macros::{Params, Variadic};
use kmx::pp::sim::{Main, Sim};
use kmx::syntax::lex::{self, Token, TokenKind};
use kmx::workspace::Workspace;

const POSITIONAL: [&str; 9] = [
    "__COUNTER__",
    "__LINE__",
    "__FILE__",
    "__DATE__",
    "__TIME__",
    "__TIMESTAMP__",
    "__BASE_FILE__",
    "__INCLUDE_LEVEL__",
    "_Pragma",
];

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

pub struct MacroCheck {
    pub sim_time: Duration,
    pub files: usize,
    pub ours: BTreeMap<String, String>,
    pub gcc: BTreeMap<String, String>,
    pub diags: Vec<String>,
    pub gcc_status: String,
}

impl MacroCheck {
    pub fn only_gcc(&self) -> Vec<&String> {
        self.gcc
            .keys()
            .filter(|k| !self.ours.contains_key(*k))
            .collect()
    }

    pub fn only_ours(&self) -> Vec<&String> {
        self.ours
            .keys()
            .filter(|k| !self.gcc.contains_key(*k))
            .collect()
    }

    pub fn differ(&self) -> Vec<&String> {
        self.gcc
            .iter()
            .filter(|(k, v)| self.ours.get(*k).is_some_and(|o| o != *v))
            .map(|(k, _)| k)
            .collect()
    }
}

pub struct ExpansionCheck {
    pub compared: usize,
    pub mismatches: Vec<(String, Option<String>, String)>,
    pub gcc_status: String,
}

pub struct Oracle<'w> {
    ws: &'w Workspace,
    rel: String,
    model: CompileModel,
    compiler: String,
    args: Vec<String>,
}

impl<'w> Oracle<'w> {
    pub fn new(ws: &'w Workspace, rel: &str) -> Option<Oracle<'w>> {
        let fallback = CompileModel::default_for(&ws.layout);
        let model = CompileModel::for_file(&ws.layout, rel, &fallback);
        let line = read_cmd_line(&CompileModel::cmd_path(&ws.layout, rel))?;
        let argv = shlex::split(&line)?;
        let mut args = Vec::new();
        let mut i = 1;
        while i < argv.len() {
            let x = &argv[i];
            if x.starts_with("-Wp,") || x == "-c" || x.starts_with("-MD") || x.starts_with("-MMD") {
                i += 1;
                continue;
            }
            if x == "-o" || x == "-MF" {
                i += 2;
                continue;
            }
            args.push(x.clone());
            i += 1;
        }
        let bin = prepare(ws, &model).compiler_bin;
        let subs = compiler_headers(ws, &model, bin.as_deref());
        let substitute = |dir: &str| {
            let abs = normalize(&ws.layout.objtree.join(dir));
            subs.iter()
                .find(|(recorded, _)| *recorded == abs)
                .map(|(_, used)| used.display().to_string())
        };
        let mut i = 0;
        while i < args.len() {
            if args[i] == "-isystem" {
                if let Some(used) = args.get(i + 1).and_then(|d| substitute(d)) {
                    args[i + 1] = used;
                }
                i += 2;
                continue;
            }
            if let Some(used) = args[i].strip_prefix("-isystem").and_then(substitute) {
                args[i] = format!("-isystem{used}");
            }
            i += 1;
        }
        Some(Oracle {
            ws,
            rel: rel.to_string(),
            model,
            compiler: argv.first()?.clone(),
            args,
        })
    }

    pub fn macros(&self) -> (MacroCheck, Sim<'w>) {
        let t = Instant::now();
        let (mut sim, _) = sim_for(self.ws, self.model.clone(), None);
        sim.run(Main::File(self.ws.layout.srctree.join(&self.rel)));
        let sim_time = t.elapsed();
        let mut ours = BTreeMap::new();
        let names: Vec<_> = sim.macro_names().collect();
        for s in names {
            let Some(def) = sim.def_of(s) else { continue };
            if def.builtin.is_some() {
                continue;
            }
            let mut head = sim.it.get(s).to_string();
            if let Params::Function { names, variadic } = &def.params {
                let mut ps: Vec<String> =
                    names.iter().map(|n| sim.it.get(*n).to_string()).collect();
                match variadic {
                    Variadic::Anonymous => ps.push("...".into()),
                    Variadic::Named => {
                        if let Some(l) = ps.last_mut() {
                            l.push_str("...");
                        }
                    }
                    Variadic::No => {}
                }
                head = format!("{head}({})", ps.join(","));
            }
            ours.insert(
                sim.it.get(s).to_string(),
                squash(&format!("{head} {}", lex::render(&def.body, &sim.it))),
            );
        }
        let out = Command::new(&self.compiler)
            .args(&self.args)
            .args(["-E", "-dM"])
            .current_dir(&self.ws.layout.objtree)
            .output()
            .expect("run the kernel compiler");
        let mut gcc = BTreeMap::new();
        for l in String::from_utf8_lossy(&out.stdout).lines() {
            let Some(rest) = l.strip_prefix("#define ") else {
                continue;
            };
            let name_end = rest.find([' ', '(']).unwrap_or(rest.len());
            gcc.insert(rest[..name_end].to_string(), squash(rest));
        }
        let diags = sim
            .diags
            .iter()
            .map(|d| {
                format!(
                    "{:?} {}:{} {}",
                    d.kind,
                    sim.files.display(d.file),
                    d.line,
                    d.message
                )
            })
            .collect();
        let check = MacroCheck {
            sim_time,
            files: sim.files.len(),
            ours,
            gcc,
            diags,
            gcc_status: out.status.to_string(),
        };
        (check, sim)
    }

    pub fn expansions(&self, sim: &mut Sim<'w>) -> ExpansionCheck {
        let mut probes: Vec<(String, String)> = Vec::new();
        sim.symbolic_builtins = true;
        let names: Vec<_> = sim.macro_names().collect();
        for s in names {
            let Some(def) = sim.def_of(s) else { continue };
            if def.builtin.is_some() || def.is_function() {
                continue;
            }
            let tok = Token::new(TokenKind::Ident, s, 0);
            let (out, _, truncated) = sim.expand_text(vec![tok], false);
            let (again, _, _) = sim.expand_text(vec![tok], false);
            let r = lex::render(&out, &sim.it);
            if truncated
                || lex::render(&again, &sim.it) != r
                || POSITIONAL.iter().any(|k| r.contains(k))
            {
                continue;
            }
            probes.push((sim.it.get(s).to_string(), squash(&r)));
        }
        probes.sort();
        let mut input = format!(
            "#include \"{}\"\n__kmx_begin__\n",
            self.ws.layout.srctree.join(&self.rel).display()
        );
        for (i, (n, _)) in probes.iter().enumerate() {
            input.push_str(&format!("__kmx_p{i}__ {n} __kmx_q__\n"));
        }
        let lang = if self.model.asm {
            "assembler-with-cpp"
        } else {
            "c"
        };
        let mut child = Command::new(&self.compiler)
            .args(
                self.args
                    .iter()
                    .filter(|x| !x.ends_with(".c") && !x.ends_with(".S")),
            )
            .args(["-E", "-P", "-x", lang, "-"])
            .current_dir(&self.ws.layout.objtree)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("run the kernel compiler");
        child
            .stdin
            .take()
            .expect("compiler stdin")
            .write_all(input.as_bytes())
            .expect("write the probe file");
        let out = child.wait_with_output().expect("compiler output");
        let text = String::from_utf8_lossy(&out.stdout);
        let squashed = squash(text.split("__kmx_begin__").nth(1).unwrap_or(""));
        let mut got: BTreeMap<usize, String> = BTreeMap::new();
        let mut rest = squashed.as_str();
        while let Some(start) = rest.find("__kmx_p") {
            let after = &rest[start + 7..];
            let Some(num_end) = after.find("__") else {
                break;
            };
            let idx: usize = after[..num_end].parse().unwrap_or(usize::MAX);
            let body = &after[num_end + 2..];
            let Some(end) = body.find("__kmx_q__") else {
                break;
            };
            got.insert(idx, body[..end].to_string());
            rest = &body[end + 9..];
        }
        let mismatches = probes
            .iter()
            .enumerate()
            .filter(|(i, (_, ours))| got.get(i) != Some(ours))
            .map(|(i, (n, ours))| (n.clone(), got.get(&i).cloned(), ours.clone()))
            .collect();
        ExpansionCheck {
            compared: probes.len(),
            mismatches,
            gcc_status: out.status.to_string(),
        }
    }
}
