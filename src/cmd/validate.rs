use std::path::Path;

use crate::analysis::context;
use crate::cli::{Command, Global};
use crate::error::{Error, Result};
use crate::pp::kbuild::CompileModel;
use crate::workspace::Workspace;

fn command_label(command: &Command) -> &'static str {
    match command {
        Command::Show { .. } => "show",
        Command::Expand { .. } => "expand",
        Command::Line { .. } => "line",
        Command::List { resolve: true, .. } => "list --resolve",
        Command::List { .. } => "list without --resolve",
        Command::Grep { .. } => "grep",
        Command::Uses { .. } => "uses",
        Command::Dump { .. } => "dump",
        Command::Index => "index",
        Command::Ls => "ls",
        Command::Rm { .. } => "rm",
        Command::Prune => "prune",
        Command::Inspect { .. } => "inspect",
    }
}

pub fn applicable(command: &Command, g: &Global) -> Result<()> {
    let given = [
        ("--set", !g.set.is_empty()),
        ("--in", g.context_file.is_some()),
        ("--include", !g.include.is_empty()),
    ];
    let allowed: &[&str] = match command {
        Command::Show { .. }
        | Command::Expand { .. }
        | Command::Dump { .. }
        | Command::Grep { .. }
        | Command::Uses { .. }
        | Command::List { resolve: true, .. } => &["--set", "--in", "--include"],
        Command::Line { .. } => &["--set"],
        Command::List { .. }
        | Command::Index
        | Command::Ls
        | Command::Rm { .. }
        | Command::Prune
        | Command::Inspect { .. } => &[],
    };
    if let Some((flag, _)) = given
        .iter()
        .find(|(flag, on)| *on && !allowed.contains(flag))
    {
        return Err(Error::Usage(format!(
            "{flag} has no effect on {}",
            command_label(command)
        )));
    }
    let lines = matches!(
        command,
        Command::Grep { .. } | Command::Uses { .. } | Command::List { .. }
    );
    if g.vimgrep && !lines {
        return Err(Error::Usage(format!(
            "--vimgrep has no effect on {}",
            command_label(command)
        )));
    }
    if g.context_file.is_some() && !g.include.is_empty() {
        return Err(Error::Usage(
            "--in and --include select different contexts; pass only one of them".into(),
        ));
    }
    Ok(())
}

fn line_count(content: &[u8]) -> usize {
    let newlines = memchr::memchr_iter(b'\n', content).count();
    newlines + usize::from(content.last().is_some_and(|&b| b != b'\n'))
}

pub fn location(ws: &Workspace, spec: &str, flag: &str) -> Result<(String, Option<u32>)> {
    let (file, line) = match spec.rsplit_once(':') {
        Some((f, l)) if !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()) => {
            let n = l
                .parse::<u32>()
                .map_err(|_| Error::Usage(format!("{flag} {spec}: line {l} is out of range")))?;
            (f, Some(n))
        }
        _ => (spec, None),
    };
    let path = Path::new(file);
    let rel = if path.is_absolute() {
        path.strip_prefix(&ws.layout.srctree)
            .map_err(|_| {
                Error::Usage(format!(
                    "{flag} {spec}: {file} is outside {}",
                    ws.layout.srctree.display()
                ))
            })?
            .display()
            .to_string()
    } else {
        file.strip_prefix("./").unwrap_or(file).to_string()
    };
    let abs = ws.layout.srctree.join(&rel);
    let content = std::fs::read(&abs)
        .ok()
        .filter(|_| abs.is_file())
        .ok_or_else(|| {
            Error::Usage(format!(
                "{flag} {spec}: {rel} is not a file in {}",
                ws.layout.srctree.display()
            ))
        })?;
    if let Some(n) = line {
        let lines = line_count(&content);
        if n == 0 || n as usize > lines {
            return Err(Error::Usage(format!(
                "{flag} {spec}: {rel} has {lines} lines, so line {n} does not exist"
            )));
        }
    }
    Ok((rel, line))
}

pub fn header(ws: &Workspace, spec: &str) -> Result<String> {
    let (name, angled) = if let Some(n) = spec.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        (n, false)
    } else if let Some(n) = spec.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
        (n, true)
    } else {
        (spec, true)
    };
    let base = CompileModel::default_for(&ws.layout);
    let (mut sim, _) = context::sim_for(ws, base, None);
    if sim.resolve(name, angled, false).is_none() {
        return Err(Error::Usage(format!(
            "--include {spec}: no such header on the kernel include path"
        )));
    }
    Ok(if angled {
        format!("<{name}>")
    } else {
        format!("\"{name}\"")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_lines_with_and_without_final_newline() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"a\n"), 1);
        assert_eq!(line_count(b"a\nb"), 2);
        assert_eq!(line_count(b"a\nb\n"), 2);
    }
}
