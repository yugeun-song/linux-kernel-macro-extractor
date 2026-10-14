pub mod complete;
pub mod expand;
pub mod grep;
pub mod list;
pub mod manage;
pub mod show;
pub mod validate;

use std::path::PathBuf;

use crate::cli::{Cli, Command, DumpFormat};
use crate::error::{Error, Result};
use crate::tree::arch::Compiler;
use crate::workspace::{OpenOptions, Workspace};

pub struct Ctx {
    pub quiet: bool,
}

impl Ctx {
    pub fn notify(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{msg}");
        }
    }
}

pub fn run(cli: Cli) -> Result<i32> {
    let ctx = Ctx {
        quiet: cli.global.quiet,
    };
    if let Some(n) = cli.global.threads {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n.max(1))
            .build_global();
    }
    let g = &cli.global;
    if let Some(shell) = g.completions {
        crate::render::print(complete::script(shell));
        return Ok(crate::error::exit::OK);
    }
    let Some(command) = cli.command.clone() else {
        return Err(Error::Usage(match &cli.tree {
            None => "missing <TREE>: pass the top level of a Linux kernel git tree first, \
                     or a command such as kmx ls (see kmx --help)"
                .to_string(),
            Some(tree) => format!(
                "nothing to do for {}: name a macro or a command (see kmx --help)",
                tree.display()
            ),
        }));
    };
    let compiler = match &g.cc {
        Some(spec) => Some(Compiler::parse_spec(spec).ok_or_else(|| {
            Error::Usage(format!("--cc {spec}: expected gcc-<ver> or clang-<ver>"))
        })?),
        None => None,
    };
    let opts = OpenOptions {
        build_dir: g.build_dir.clone(),
        refresh: !g.no_refresh,
        compiler,
        rebuild: false,
    };
    validate::applicable(&command, g)?;
    let target = || -> Result<(PathBuf, OpenOptions)> {
        let given = cli.tree.clone().ok_or_else(|| {
            Error::Usage(
                "missing <TREE>: pass the top level of a Linux kernel git tree first (see kmx --help)"
                    .to_string(),
            )
        })?;
        let mut opts = opts.clone();
        let Some((srctree, objtree)) = crate::projects::tree_of_id(&given)
            .map_err(|why| Error::Usage(format!("{}: {why}", given.display())))?
        else {
            if !given.exists() && given.to_str().is_some_and(crate::projects::looks_like_id) {
                return Err(Error::Usage(format!(
                    "{}: no such directory, and no index has this ID (see kmx ls)",
                    given.display()
                )));
            }
            return Ok((given, opts));
        };
        match &opts.build_dir {
            Some(dir) if dir.canonicalize().ok().as_ref() != Some(&objtree) => {
                return Err(Error::Usage(format!(
                    "{}: this index belongs to {} with build directory {}; drop -O or pass the tree path instead of the ID",
                    given.display(),
                    srctree.display(),
                    objtree.display()
                )));
            }
            Some(_) => {}
            None if objtree != srctree => opts.build_dir = Some(objtree),
            None => {}
        }
        Ok((srctree, opts))
    };
    let open = || -> Result<&'static Workspace> {
        let (tree, opts) = target()?;
        let ws = Workspace::open(&tree, &opts, &mut |m: &str| ctx.notify(m))?;
        crate::render::paths::configure(
            &tree,
            &ws.layout.srctree,
            opts.build_dir.as_deref(),
            &ws.layout.objtree,
        );
        Ok(Box::leak(Box::new(ws)))
    };
    match &command {
        Command::Ls => manage::ls(g),
        Command::Rm { targets } => manage::rm(targets),
        Command::Prune => manage::prune(&ctx),
        Command::Inspect { targets } => manage::inspect(targets, g),
        Command::Index => {
            let (tree, opts) = target()?;
            manage::index(&tree, opts, &ctx)
        }
        Command::Show { names } => show::run(open()?, names, g),
        Command::Expand { text } => expand::run_expand(open()?, text, g),
        Command::Line { location } => expand::run_line(open()?, location, g),
        Command::List {
            pattern,
            regex,
            resolve,
        } => list::run(open()?, pattern, *regex, *resolve, g),
        Command::Grep {
            pattern,
            word,
            glob,
        } => grep::run_grep(open()?, pattern, *word, *glob, g),
        Command::Uses { name } => grep::run_uses(open()?, name, g),
        Command::Dump { format } => expand::run_dump(open()?, g, *format == DumpFormat::Json),
    }
}
