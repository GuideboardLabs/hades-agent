//! The binary. Everything it does lives in [`harness::cli`], so a test can drive the CLI without a
//! terminal, and this file is the only place that touches the process.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout();
    std::process::exit(harness::cli::run(&args, &mut out));
}
