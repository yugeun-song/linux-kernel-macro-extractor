use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--complete") => std::process::exit(kmx::cmd::complete::run_words(&args[2..])),
        Some("--complete-line") => std::process::exit(kmx::cmd::complete::run_line(&args[2..])),
        _ => {}
    }
    let cli = kmx::cli::parse();
    let code = match kmx::cmd::run(cli) {
        Ok(code) => code,
        Err(e) => {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "kmx: error: {e}");
            for d in e.details() {
                let _ = writeln!(err, "  {d}");
            }
            e.exit_code()
        }
    };
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
