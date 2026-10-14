use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "kmx",
    version,
    about = "Config-aware Linux kernel macro explorer",
    long_about = "Shows which definition of a kernel macro (or enum constant) the current .config selects, \
why every other candidate lost, and the full layer-by-layer expansion with intermediate values.",
    override_usage = "kmx <TREE> [OPTIONS] [COMMAND] [ARGS]...\n       kmx <TREE> <NAME>...\n       kmx ls | rm <ID|TREE>... | prune | inspect <ID|TREE>...\n       kmx --completions <SHELL>",
    after_help = "TREE is the top level of a Linux kernel git work tree. Every analysis needs it.\n\
TREE can also be an index ID from kmx ls, or a unique prefix of it (4+ hex digits).\n\
Examples:\n  kmx ~/src/linux PAGE_SIZE\n  kmx fc789d5303c8 PAGE_SIZE\n  kmx ~/src/linux expand 'container_of(p, struct page, lru)'\n  kmx ~/src/linux line mm/slub.c:1234\n  kmx ~/src/linux HZ --set HZ_250=y"
)]
pub struct Cli {
    #[arg(skip)]
    pub tree: Option<PathBuf>,

    #[command(flatten)]
    pub global: Global,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Args, Debug, Clone)]
pub struct Global {
    #[arg(
        short = 'O',
        long = "build-dir",
        value_name = "DIR",
        global = true,
        help = "Kbuild output directory (O=) holding .config and generated headers"
    )]
    pub build_dir: Option<PathBuf>,

    #[arg(
        long = "norefresh",
        visible_alias = "no-refresh",
        global = true,
        help = "Abort instead of updating the index when the tree was edited"
    )]
    pub no_refresh: bool,

    #[arg(
        long = "cc",
        value_name = "SPEC",
        global = true,
        help = "Override the compiler identity, e.g. gcc-13.2 or clang-19"
    )]
    pub cc: Option<String>,

    #[arg(
        long = "set",
        value_name = "SYM=VAL",
        global = true,
        help = "What-if: override a Kconfig symbol (re-evaluated with Kconfig rules)"
    )]
    pub set: Vec<String>,

    #[arg(
        long = "in",
        value_name = "FILE[:LINE]",
        global = true,
        help = "Context: preprocess as part of this source file, optionally up to LINE"
    )]
    pub context_file: Option<String>,

    #[arg(
        long = "include",
        value_name = "HEADER",
        global = true,
        help = "Context: also include this header (repeatable)"
    )]
    pub include: Vec<String>,

    #[arg(long, value_enum, default_value_t = Style::Tree, global = true, help = "Output layout")]
    pub style: Style,

    #[arg(
        long,
        value_name = "N",
        global = true,
        help = "Limit the expansion tree depth"
    )]
    pub depth: Option<usize>,

    #[arg(
        long,
        value_name = "N|all|none",
        global = true,
        help = "Also show losing candidates, N per reason group (default: show and expand print only the active chain, grep and uses 3)"
    )]
    pub candidates: Option<String>,

    #[arg(
        long,
        global = true,
        help = "Show every candidate (same as --candidates all)"
    )]
    pub all: bool,

    #[arg(long, value_enum, default_value_t = ScopeArg::Kernel, global = true, help = "Which definitions count as candidates")]
    pub scope: ScopeArg,

    #[arg(
        short = 'b',
        long,
        global = true,
        help = "Print only the conclusion, the first line of the tree output"
    )]
    pub brief: bool,

    #[arg(long, global = true, help = "Machine-readable JSON Lines output")]
    pub json: bool,

    #[arg(long, value_enum, default_value_t = ColorArg::Auto, global = true, help = "Colorize output")]
    pub color: ColorArg,

    #[arg(
        long,
        global = true,
        help = "Page output through $KMX_PAGER, $PAGER or less -FRX when stdout is a terminal"
    )]
    pub pager: bool,

    #[arg(long, global = true, help = "Use plain ASCII for tree drawing")]
    pub ascii: bool,

    #[arg(
        long = "no-census",
        global = true,
        help = "Do not enumerate per-TU variants of TU-dependent macros"
    )]
    pub no_census: bool,

    #[arg(
        long = "no-memo",
        global = true,
        help = "Ignore and do not store memoized results"
    )]
    pub no_memo: bool,

    #[arg(
        short = 'j',
        long,
        value_name = "N",
        global = true,
        help = "Worker threads (default: all cores)"
    )]
    pub threads: Option<usize>,

    #[arg(
        short = 'q',
        long,
        global = true,
        help = "Suppress progress notices on stderr"
    )]
    pub quiet: bool,

    #[arg(
        long,
        global = true,
        help = "Print grep, uses and list results as path:line:column:text for editors"
    )]
    pub vimgrep: bool,

    #[arg(
        long,
        value_enum,
        value_name = "SHELL",
        help = "Print a Tab-completion script for this shell"
    )]
    pub completions: Option<ShellArg>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    #[command(about = "Resolve macros or enum constants: active definition, candidates, expansion")]
    Show {
        #[arg(required = true, value_name = "NAME")]
        names: Vec<String>,
    },
    #[command(about = "Expand arbitrary text, e.g. a macro invocation with arguments")]
    Expand {
        #[arg(required = true, value_name = "TEXT")]
        text: Vec<String>,
    },
    #[command(about = "Expand every macro on one source line in that file's context")]
    Line {
        #[arg(value_name = "FILE:LINE")]
        location: String,
    },
    #[command(about = "List macro and enum names matching a glob")]
    List {
        #[arg(value_name = "PATTERN", default_value = "*")]
        pattern: String,
        #[arg(long, help = "Treat PATTERN as a regular expression")]
        regex: bool,
        #[arg(long, help = "Also resolve each name's active value")]
        resolve: bool,
    },
    #[command(
        about = "Search definitions by name: one line per definition with its status in this configuration"
    )]
    Grep {
        #[arg(
            value_name = "PATTERN",
            help = "Regular expression matched anywhere in a name, as in rg"
        )]
        pattern: String,
        #[arg(
            short = 'w',
            long = "word",
            conflicts_with = "glob",
            help = "Match whole names only"
        )]
        word: bool,
        #[arg(
            short = 'g',
            long = "glob",
            help = "Treat PATTERN as a glob over whole names"
        )]
        glob: bool,
    },
    #[command(
        about = "List the definitions whose body uses NAME, with their status in this configuration"
    )]
    Uses {
        #[arg(value_name = "NAME")]
        name: String,
    },
    #[command(about = "Dump every macro active in a context")]
    Dump {
        #[arg(long, value_enum, default_value_t = DumpFormat::Tsv)]
        format: DumpFormat,
    },
    #[command(about = "Rebuild this tree's index from scratch")]
    Index,
    #[command(about = "List every indexed kernel tree (no TREE needed)")]
    Ls,
    #[command(about = "Delete the index of these kernel trees (no TREE needed)")]
    Rm {
        #[arg(required = true, value_name = "ID|TREE")]
        targets: Vec<String>,
    },
    #[command(
        about = "Delete indexes whose tree or build directory is gone or is no longer a kernel tree"
    )]
    Prune,
    #[command(about = "Show everything recorded about these indexes, with their current state")]
    Inspect {
        #[arg(required = true, value_name = "ID|TREE")]
        targets: Vec<String>,
    },
}

impl Command {
    pub fn needs_tree(&self) -> bool {
        !matches!(
            self,
            Command::Ls | Command::Rm { .. } | Command::Prune | Command::Inspect { .. }
        )
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    #[value(help = "Layered tree: each macro with its definition, conditions, rivals and value")]
    Tree,
    #[value(help = "Numbered rewrite steps, then the alternatives")]
    Steps,
    #[value(help = "Sections: summary, dependencies, candidates")]
    Report,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeArg {
    #[value(help = "The kernel proper, for every architecture")]
    Kernel,
    #[value(help = "Also tools/, scripts/, Documentation/ and usr/")]
    All,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorArg {
    #[value(help = "Color when stdout is a terminal; NO_COLOR and CLICOLOR_FORCE are honored")]
    Auto,
    #[value(help = "Always emit ANSI colors")]
    Always,
    #[value(help = "Never emit ANSI colors")]
    Never,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellArg {
    Zsh,
    Bash,
    Fish,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DumpFormat {
    Tsv,
    Json,
}

pub const MANAGEMENT: &[&str] = &["ls", "rm", "prune", "inspect"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shown {
    Active,
    PerGroup(usize),
    All,
}

impl Global {
    pub fn shown(&self, default: Shown) -> Result<Shown, String> {
        if self.all {
            return Ok(Shown::All);
        }
        match self.candidates.as_deref() {
            None => Ok(default),
            Some("all") => Ok(Shown::All),
            Some("none") => Ok(Shown::Active),
            Some(n) => n
                .parse()
                .map(Shown::PerGroup)
                .map_err(|_| format!("--candidates {n}: expected a number, all or none")),
        }
    }
}

const SUBCOMMANDS: &[&str] = &[
    "show", "expand", "line", "list", "grep", "uses", "dump", "index", "help",
];

fn value_options() -> (Vec<String>, Vec<char>) {
    let cmd = Cli::command();
    let mut longs = Vec::new();
    let mut shorts = Vec::new();
    for a in cmd.get_arguments() {
        if !a.get_action().takes_values() {
            continue;
        }
        if let Some(l) = a.get_long() {
            longs.push(format!("--{l}"));
        }
        if let Some(aliases) = a.get_all_aliases() {
            longs.extend(aliases.iter().map(|al| format!("--{al}")));
        }
        if let Some(s) = a.get_short() {
            shorts.push(s);
        }
    }
    (longs, shorts)
}

pub fn normalize(args: Vec<OsString>) -> (Option<PathBuf>, Vec<OsString>) {
    let (longs, shorts) = value_options();
    let takes_value = |tok: &str| -> bool {
        if tok.starts_with("--") {
            !tok.contains('=') && longs.iter().any(|l| l == tok)
        } else if let Some(rest) = tok.strip_prefix('-') {
            let mut chars = rest.chars();
            matches!((chars.next(), chars.next()), (Some(c), None) if shorts.contains(&c))
        } else {
            false
        }
    };
    let mut out = Vec::with_capacity(args.len() + 1);
    let mut tree = None;
    let mut it = args.into_iter();
    if let Some(argv0) = it.next() {
        out.push(argv0);
    }
    let rest: Vec<OsString> = it.collect();
    let mut i = 0;
    let mut positional_seen = 0;
    let mut after_dashdash = false;
    let mut management = false;
    while i < rest.len() {
        let tok = rest[i].to_string_lossy().into_owned();
        if !after_dashdash && tok == "--" {
            after_dashdash = true;
            out.push(rest[i].clone());
            i += 1;
            continue;
        }
        let is_option = !after_dashdash && tok.starts_with('-') && tok.len() > 1;
        if is_option {
            out.push(rest[i].clone());
            if takes_value(&tok) && i + 1 < rest.len() {
                out.push(rest[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if management {
            out.push(rest[i].clone());
            i += 1;
            continue;
        }
        match positional_seen {
            0 if MANAGEMENT.contains(&tok.as_str()) || tok == "help" => {
                management = true;
                out.push(rest[i].clone());
            }
            0 => tree = Some(PathBuf::from(&rest[i])),
            1 => {
                if !SUBCOMMANDS.contains(&tok.as_str()) {
                    out.push(OsString::from("show"));
                }
                out.push(rest[i].clone());
            }
            _ => out.push(rest[i].clone()),
        }
        positional_seen += 1;
        i += 1;
    }
    (tree, out)
}

pub fn parse() -> Cli {
    let (tree, argv) = normalize(std::env::args_os().collect());
    let mut cli = Cli::parse_from(argv);
    cli.tree = tree;
    cli
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> (Option<String>, Vec<String>) {
        let args: Vec<OsString> = s.split_whitespace().map(OsString::from).collect();
        let (t, v) = normalize(args);
        (
            t.map(|p| p.to_string_lossy().into_owned()),
            v.into_iter()
                .map(|o| o.to_string_lossy().into_owned())
                .collect(),
        )
    }

    #[test]
    fn shorthand_inserts_show() {
        let (t, v) = norm("kmx /k PAGE_SIZE THREAD_SIZE");
        assert_eq!(t.as_deref(), Some("/k"));
        assert_eq!(v, ["kmx", "show", "PAGE_SIZE", "THREAD_SIZE"]);
    }

    #[test]
    fn explicit_subcommand_kept() {
        let (t, v) = norm("kmx --json /k line mm/slub.c:10");
        assert_eq!(t.as_deref(), Some("/k"));
        assert_eq!(v, ["kmx", "--json", "line", "mm/slub.c:10"]);
    }

    #[test]
    fn option_values_are_not_positionals() {
        let (t, v) = norm("kmx -O /build --set X=y /k FOO --depth 2");
        assert_eq!(t.as_deref(), Some("/k"));
        assert_eq!(
            v,
            [
                "kmx", "-O", "/build", "--set", "X=y", "show", "FOO", "--depth", "2"
            ]
        );
    }

    #[test]
    fn management_needs_no_tree() {
        let (t, v) = norm("kmx rm abc /some/tree");
        assert_eq!(t, None);
        assert_eq!(v, ["kmx", "rm", "abc", "/some/tree"]);
        let cli = Cli::parse_from(v);
        assert!(
            matches!(cli.command, Some(Command::Rm { ref targets }) if targets == &["abc", "/some/tree"])
        );
        let (t, v) = norm("kmx --json ls");
        assert_eq!(t, None);
        assert!(matches!(Cli::parse_from(v).command, Some(Command::Ls)));
        let (t, v) = norm("kmx ./ls PAGE_SIZE");
        assert_eq!(t.as_deref(), Some("./ls"));
        assert_eq!(v, ["kmx", "show", "PAGE_SIZE"]);
    }

    #[test]
    fn parses_full_command() {
        let (_, v) = norm("kmx /k --all FIX_APIC_BASE");
        let cli = Cli::parse_from(v);
        assert!(cli.global.all);
        assert!(
            matches!(cli.command, Some(Command::Show { ref names }) if names == &["FIX_APIC_BASE"])
        );
    }
}
