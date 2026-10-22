//! The binary. Everything it does lives in [`harness::cli`], so a test can drive the CLI without a
//! terminal, and this file is the only place that touches the process.

fn main() {
    use std::io::IsTerminal;
    // Before anything reads a key or takes a model call: a program this process runs must not be able to
    // read the environment back out of /proc. Failing to seal is not fatal (an old kernel may not offer
    // it), but it is reported once rather than assumed.
    if let Err(error) = harness::cage::seal_environment() {
        eprintln!("hades: {error}");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if (args.is_empty() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal())
        || args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "tui" | "chat"))
    {
        let rest = if args.is_empty() {
            &args[..]
        } else {
            &args[1..]
        };
        if let Err(error) = harness::tui::run(rest) {
            eprintln!("hades: {error}");
            std::process::exit(2);
        }
        return;
    }
    let mut out = std::io::stdout();
    std::process::exit(harness::cli::run(&args, &mut out));
}
