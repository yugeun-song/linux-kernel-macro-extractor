pub mod c_format;
pub mod human;
pub mod paths;
pub mod sections;
pub mod steps;
pub mod style;
pub mod table;
pub mod tree;

use std::io::{IsTerminal, Write};
use std::process::{Child, Command, Stdio};

use crate::analysis::report::Report;
use crate::cli::{ColorArg, Global, Shown, Style};

pub struct Output {
    sink: Box<dyn Write>,
    child: Option<Child>,
    color: bool,
    terminal: bool,
}

impl Output {
    pub fn open(g: &Global) -> Output {
        let stdout = std::io::stdout();
        let tty = stdout.is_terminal();
        let detected = !matches!(
            anstream::AutoStream::choice(&stdout),
            anstream::ColorChoice::Never
        );
        let color = match g.color {
            ColorArg::Always => true,
            ColorArg::Never => false,
            ColorArg::Auto => detected && !g.json,
        };
        if g.pager && tty && !g.json {
            let spec = std::env::var("KMX_PAGER")
                .or_else(|_| std::env::var("PAGER"))
                .unwrap_or_else(|_| "less -FRX".to_string());
            let mut parts = spec.split_whitespace();
            if let Some(bin) = parts.next() {
                let args: Vec<&str> = parts.collect();
                if let Ok(mut child) = Command::new(bin).args(&args).stdin(Stdio::piped()).spawn()
                    && let Some(stdin) = child.stdin.take()
                {
                    return Output {
                        sink: Box::new(stdin),
                        child: Some(child),
                        color,
                        terminal: tty,
                    };
                }
            }
        }
        Output {
            sink: Box::new(stdout),
            child: None,
            color,
            terminal: tty,
        }
    }

    pub fn color(&self) -> bool {
        self.color
    }

    pub fn terminal(&self) -> bool {
        self.terminal
    }

    pub fn tree_heading(&mut self) {
        if !self.terminal {
            return;
        }
        if let Some((src, obj)) = paths::roots() {
            let mut text = format!(
                "{}  {}\n",
                style::paint("tree", style::LABEL),
                src.display()
            );
            if let Some(o) = obj {
                text.push_str(&format!(
                    "{} {}\n",
                    style::paint("build", style::LABEL),
                    o.display()
                ));
            }
            self.emit(&text);
        }
    }

    pub fn emit(&mut self, text: &str) {
        let r = if self.color {
            self.sink.write_all(text.as_bytes())
        } else {
            write!(self.sink, "{}", anstream::adapter::strip_str(text))
        };
        if let Err(e) = r {
            closed_on(&e);
            self.child = None;
        }
    }
}

fn closed_on(e: &std::io::Error) {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        std::process::exit(crate::error::exit::OK);
    }
}

pub fn print(text: &str) {
    let mut out = std::io::stdout().lock();
    if let Err(e) = out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        closed_on(&e);
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.sink.flush();
        if let Some(mut child) = self.child.take() {
            self.sink = Box::new(std::io::sink());
            let _ = child.wait();
        }
    }
}

pub fn per_group(g: &Global) -> Option<usize> {
    match g.shown(Shown::Active).unwrap_or(Shown::Active) {
        Shown::All => None,
        Shown::Active => Some(0),
        Shown::PerGroup(n) => Some(n),
    }
}

pub fn onion(g: &Global) -> bool {
    g.shown(Shown::Active).unwrap_or(Shown::Active) == Shown::Active
}

pub fn report_text(r: &Report, g: &Global) -> String {
    paths::in_text(&layout(r, g)).into_owned()
}

pub fn glyphs(g: &Global) -> style::Glyphs {
    if g.ascii {
        style::ASCII
    } else {
        style::UNICODE
    }
}

pub fn columns() -> Option<usize> {
    let stdout = std::io::stdout();
    if stdout.is_terminal() {
        let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) };
        if ok == 0 {
            let cols = unsafe { size.assume_init() }.ws_col;
            if cols > 0 {
                return Some(usize::from(cols));
            }
        }
    }
    std::env::var("COLUMNS")
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|&c| c > 0)
}

fn tree_opts(r: &Report, g: &Global) -> tree::Opts {
    tree::Opts {
        glyphs: glyphs(g),
        width: columns().map_or(80, |c| c.clamp(40, 80)),
        per_group: per_group(g),
        column: 44,
        config_section: r.kconfig.is_some(),
        onion: onion(g),
    }
}

fn layout(r: &Report, g: &Global) -> String {
    if g.brief {
        let mut text = tree::headline(r, &tree_opts(r, g)).join("\n");
        text.push('\n');
        return text;
    }
    let glyphs = glyphs(g);
    match g.style {
        Style::Tree => tree::render(r, &tree_opts(r, g)),
        Style::Steps => steps::render(r, &glyphs, per_group(g), onion(g)),
        Style::Report => sections::render(r, &glyphs, per_group(g), onion(g)),
    }
}
