use std::path::{Path, PathBuf};

use clap::{Arg, CommandFactory};

use crate::cli::{Cli, MANAGEMENT, ShellArg};
use crate::index::{ENUM_TAG_PREFIX, FileClass, Scope};
use crate::pp::kbuild::CompileModel;
use crate::store::{Project, Store};
use crate::workspace::Workspace;

const LIMIT: usize = 2000;
const DIRS: &str = ":dirs";

const ZSH: &str = include_str!("completion/kmx.zsh");
const BASH: &str = include_str!("completion/kmx.bash");
const FISH: &str = include_str!("completion/kmx.fish");

pub fn script(shell: ShellArg) -> &'static str {
    match shell {
        ShellArg::Zsh => ZSH,
        ShellArg::Bash => BASH,
        ShellArg::Fish => FISH,
    }
}

#[derive(Default)]
struct Line {
    tree: Option<String>,
    build_dir: Option<String>,
    command: Option<String>,
    args_after_command: usize,
    pending: Option<String>,
    scope_all: bool,
}

fn expand_home(p: &str) -> PathBuf {
    let home = std::env::home_dir().unwrap_or_default();
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if p == "~" => home,
        None => PathBuf::from(p),
    }
}

fn find_arg<'a>(args: &[&'a Arg], word: &str) -> Option<&'a Arg> {
    if let Some(long) = word.strip_prefix("--") {
        let long = long.split_once('=').map_or(long, |(l, _)| l);
        return args.iter().copied().find(|a| {
            a.get_long() == Some(long) || a.get_all_aliases().is_some_and(|al| al.contains(&long))
        });
    }
    let mut chars = word.strip_prefix('-')?.chars();
    let c = chars.next()?;
    args.iter().copied().find(|a| a.get_short() == Some(c))
}

fn scan(before: &[String], cmd: &clap::Command) -> Line {
    let mut line = Line::default();
    let mut args: Vec<&Arg> = cmd.get_arguments().collect();
    let mut many: Option<String> = None;
    let mut i = 0;
    while i < before.len() {
        let w = &before[i];
        if w.starts_with('-') && w.len() > 1 {
            many = None;
            let arg = find_arg(&args, w);
            let long = arg.and_then(Arg::get_long).unwrap_or_default().to_string();
            let inline = w.split_once('=').map(|(_, v)| v.to_string());
            let takes = arg.is_some_and(|a| a.get_action().takes_values());
            if takes && inline.is_none() {
                let Some(value) = before.get(i + 1) else {
                    line.pending = Some(long);
                    return line;
                };
                line.record(&long, value);
                if arg
                    .and_then(|a| a.get_num_args())
                    .is_some_and(|n| n.max_values() > 1)
                {
                    many = Some(long);
                }
                i += 2;
                continue;
            }
            if let Some(v) = inline {
                line.record(&long, &v);
            }
            i += 1;
            continue;
        }
        if many.is_some() {
            i += 1;
            continue;
        }
        if line.tree.is_none() && line.command.is_none() && MANAGEMENT.contains(&w.as_str()) {
            line.command = Some(w.clone());
            if let Some(sub) = cmd.find_subcommand(w) {
                args.extend(sub.get_arguments());
            }
        } else if line.tree.is_none() && line.command.is_none() {
            line.tree = Some(w.clone());
        } else if line.command.is_none() {
            match cmd.find_subcommand(w) {
                Some(sub) => {
                    line.command = Some(w.clone());
                    args.extend(sub.get_arguments());
                }
                None => {
                    line.command = Some("show".into());
                    line.args_after_command = 1;
                }
            }
        } else {
            line.args_after_command += 1;
        }
        i += 1;
    }
    line.pending = many;
    line
}

impl Line {
    fn record(&mut self, long: &str, value: &str) {
        match long {
            "build-dir" => self.build_dir = Some(value.to_string()),
            "scope" => self.scope_all = value == "all",
            _ => {}
        }
    }

    fn target(&self) -> Option<(PathBuf, Option<PathBuf>)> {
        let tree = expand_home(self.tree.as_deref()?);
        let build = self.build_dir.as_deref().map(expand_home);
        match crate::projects::tree_of_id(&tree).ok()? {
            Some((src, obj)) => {
                let build = build.or_else(|| (obj != src).then_some(obj));
                Some((src, build))
            }
            None => Some((tree, build)),
        }
    }

    fn workspace(&self) -> Option<Workspace> {
        let (tree, build) = self.target()?;
        Workspace::open_unverified(&tree, build.as_deref())
    }
}

fn with_prefix<'a>(items: impl IntoIterator<Item = &'a str>, current: &str) -> Vec<String> {
    items
        .into_iter()
        .filter(|s| s.starts_with(current))
        .map(str::to_string)
        .collect()
}

fn options(cmd: &clap::Command, command: Option<&str>, current: &str) -> Vec<String> {
    let sub = command.and_then(|c| cmd.find_subcommand(c));
    let mut out: Vec<String> = cmd
        .get_arguments()
        .chain(sub.into_iter().flat_map(|s| s.get_arguments()))
        .filter(|a| !a.is_hide_set())
        .filter_map(|a| a.get_long().map(|l| format!("--{l}")))
        .filter(|l| l.starts_with(current))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn names(line: &Line, current: &str) -> Vec<String> {
    if current.is_empty() {
        return Vec::new();
    }
    let Some(ws) = line.workspace() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    ws.index.names_with_prefix(current, |n| {
        if found.len() < LIMIT && !n.starts_with(ENUM_TAG_PREFIX) {
            found.push(n.to_string());
        }
    });
    let host = |s: Scope| matches!(s, Scope::Tools | Scope::Scripts | Scope::Docs | Scope::Usr);
    let mut out: Vec<String> = found
        .into_iter()
        .filter(|n| {
            ws.index
                .lookup(n)
                .iter()
                .any(|p| line.scope_all || ws.index.file(p.file).is_some_and(|v| !host(v.scope())))
        })
        .collect();
    if let Some(rest) = current.strip_prefix("CONFIG_")
        && let Some(table) = ws.kconfig_table()
    {
        out.extend(
            table
                .names_with_prefix(rest, LIMIT)
                .into_iter()
                .map(|n| format!("CONFIG_{n}")),
        );
    }
    out.sort();
    out.dedup();
    out.truncate(LIMIT);
    out
}

fn entries(dir: &Path, prefix: &str, keep: &dyn Fn(&str) -> bool) -> Vec<(String, bool)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let hidden = name.starts_with('.') && !prefix.starts_with('.');
            if hidden || !name.starts_with(prefix) {
                return None;
            }
            let is_dir = e.path().is_dir();
            (is_dir || keep(&name)).then_some((name, is_dir))
        })
        .collect()
}

fn paths_under(roots: &[PathBuf], current: &str, keep: &dyn Fn(&str) -> bool) -> Vec<String> {
    let (dir, prefix) = match current.rfind('/') {
        Some(i) => (&current[..=i], &current[i + 1..]),
        None => ("", current),
    };
    let mut out: Vec<String> = roots
        .iter()
        .flat_map(|r| entries(&r.join(dir), prefix, keep))
        .map(|(name, is_dir)| {
            if is_dir {
                format!("{dir}{name}/")
            } else {
                format!("{dir}{name}")
            }
        })
        .collect();
    out.sort();
    out.dedup();
    out.truncate(LIMIT);
    out
}

fn source_file(name: &str) -> bool {
    FileClass::of_name(name) != FileClass::Other
}

fn tree_files(line: &Line, current: &str) -> Vec<String> {
    if current.contains(':') {
        return Vec::new();
    }
    let Some((root, _)) = line.target() else {
        return Vec::new();
    };
    paths_under(&[root], current, &source_file)
}

fn headers(line: &Line, current: &str) -> Vec<String> {
    let Some(ws) = line.workspace() else {
        return Vec::new();
    };
    let model = CompileModel::default_for(&ws.layout);
    let roots: Vec<PathBuf> = model
        .dirs
        .iter()
        .chain(&model.system_dirs)
        .cloned()
        .collect();
    paths_under(&roots, current, &|n| n.ends_with(".h"))
}

fn kconfig_values(line: &Line, current: &str) -> Vec<String> {
    let Some(ws) = line.workspace() else {
        return Vec::new();
    };
    let Some(table) = ws.kconfig_table() else {
        return Vec::new();
    };
    let (shown, bare) = match current.strip_prefix("CONFIG_") {
        Some(rest) => ("CONFIG_", rest),
        None => ("", current),
    };
    match bare.split_once('=') {
        Some((name, value)) => {
            let kind = table.get(name).map(|i| i.kind).unwrap_or_default();
            let values: &[&str] = if kind.starts_with("tristate") {
                &["y", "m", "n"]
            } else if kind.starts_with("bool") {
                &["y", "n"]
            } else {
                &[]
            };
            values
                .iter()
                .filter(|v| v.starts_with(value))
                .map(|v| format!("{shown}{name}={v}"))
                .collect()
        }
        None => table
            .names_with_prefix(bare, LIMIT)
            .into_iter()
            .map(|n| format!("{shown}{n}="))
            .collect(),
    }
}

fn projects(current: &str) -> Vec<String> {
    let Ok(store) = Store::open() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let all = store.projects().unwrap_or_default();
    for p in &all {
        out.push(p.id().label(all.iter().map(Project::id)).to_string());
        if let Some(m) = p.manifest() {
            out.push(m.srctree.display().to_string());
            out.push(crate::render::human::tilde(&m.srctree));
        }
    }
    out.retain(|c| c.starts_with(current));
    out.sort();
    out.dedup();
    out
}

fn values(cmd: &clap::Command, line: &Line, option: &str, current: &str) -> Vec<String> {
    let sub = line.command.as_deref().and_then(|c| cmd.find_subcommand(c));
    let arg = cmd
        .get_arguments()
        .chain(sub.into_iter().flat_map(|s| s.get_arguments()))
        .find(|a| a.get_long() == Some(option));
    let possible: Vec<String> = arg
        .map(|a| {
            a.get_possible_values()
                .iter()
                .map(|v| v.get_name().to_string())
                .collect()
        })
        .unwrap_or_default();
    if !possible.is_empty() {
        return with_prefix(possible.iter().map(String::as_str), current);
    }
    match option {
        "build-dir" => vec![DIRS.to_string()],
        "candidates" => with_prefix(["3", "all", "none"], current),
        "in" => tree_files(line, current),
        "include" => headers(line, current),
        "set" => kconfig_values(line, current),
        _ => Vec::new(),
    }
}

fn candidates(before: &[String], current: &str) -> Vec<String> {
    let cmd = Cli::command();
    let line = scan(before, &cmd);
    if let Some(option) = &line.pending {
        return values(&cmd, &line, option, current);
    }
    if current.starts_with('-') {
        return options(&cmd, line.command.as_deref(), current);
    }
    match line.command.as_deref() {
        Some("rm") | Some("inspect") => return projects(current),
        Some("ls") | Some("prune") => return Vec::new(),
        _ => {}
    }
    if line.tree.is_none() {
        let mut out = vec![DIRS.to_string()];
        out.extend(with_prefix(MANAGEMENT.iter().copied(), current));
        out.extend(
            projects(current)
                .into_iter()
                .filter(|c| c.bytes().all(|b| b.is_ascii_hexdigit())),
        );
        return out;
    }
    let subcommands: Vec<&str> = cmd
        .get_subcommands()
        .map(clap::Command::get_name)
        .filter(|n| !MANAGEMENT.contains(n))
        .collect();
    match line.command.as_deref() {
        None => {
            let mut out = with_prefix(subcommands, current);
            out.extend(names(&line, current));
            out
        }
        Some("show") | Some("list") => names(&line, current),
        Some("expand") | Some("grep") | Some("uses") if line.args_after_command == 0 => {
            names(&line, current)
        }
        Some("line") if line.args_after_command == 0 => tree_files(&line, current),
        Some("help") if line.args_after_command == 0 => with_prefix(subcommands, current),
        _ => Vec::new(),
    }
}

fn print(list: &[String]) -> i32 {
    let mut text = list.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    use std::io::Write;
    let _ = std::io::stdout().write_all(text.as_bytes());
    0
}

pub fn run_words(args: &[String]) -> i32 {
    let Some(cword) = args.first().and_then(|a| a.parse::<usize>().ok()) else {
        return 0;
    };
    let words: Vec<String> = args
        .iter()
        .skip_while(|a| *a != "--")
        .skip(1)
        .cloned()
        .collect();
    if cword == 0 || cword > words.len() {
        return 0;
    }
    let current = words.get(cword).cloned().unwrap_or_default();
    print(&candidates(&words[1..cword], &current))
}

pub fn run_line(args: &[String]) -> i32 {
    let Some(text) = args.first() else { return 0 };
    let mut words = shlex::split(text).unwrap_or_default();
    let current = if text.ends_with(char::is_whitespace) || words.is_empty() {
        String::new()
    } else {
        words.pop().unwrap_or_default()
    };
    let before = if words.is_empty() {
        &[][..]
    } else {
        &words[1..]
    };
    let mut out = vec![current.clone()];
    out.extend(candidates(before, &current));
    print(&out)
}
