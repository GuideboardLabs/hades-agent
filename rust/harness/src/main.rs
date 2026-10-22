//! The binary. Everything it does lives in [`harness::cli`], so a test can drive the CLI without a
//! terminal, and this file is the only place that touches the process.

fn main() {
    use std::io::IsTerminal;
    // Before anything reads a key or takes a model call: a program this process runs must not be able to
    // read the environment back out of /proc. A failure here is fatal for every invocation that could run
    // something, because "the protection did not take" and "the keys are not exposed" cannot both be true.
    // `HADES_ALLOW_PTRACE=1` is the documented way to accept the risk; help and version output run nothing
    // and stay usable on a host whose seccomp profile refuses the call.
    if let Err(error) = harness::cage::seal_environment() {
        let informational = std::env::args().skip(1).any(|argument| {
            matches!(
                argument.as_str(),
                "--help" | "-h" | "--version" | "-V" | "version" | "help"
            )
        });
        if informational {
            eprintln!("hades: {error} (continuing: this invocation runs no program)");
        } else {
            eprintln!(
                "hades: {error}\nthis process cannot keep its environment away from the programs it \
                 runs; set HADES_ALLOW_PTRACE=1 to accept that, or fix the restriction"
            );
            std::process::exit(1);
        }
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
