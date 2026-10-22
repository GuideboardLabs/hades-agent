//! Fetch one URL N times through the tool and report the per-fetch time.
//!
//! The comparison in `docs/fetch-benchmark.md` needs a number for the path the tool actually takes, not
//! for a shell loop around curl. This runs the real policy, resolve, pin and read.
//!
//! `cargo run --release --example bench_fetch -- http://127.0.0.1:8000/ 50`

fn main() {
    let mut arguments = std::env::args().skip(1);
    let url = arguments
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:8000/".to_string());
    let count: usize = arguments
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(50);
    let allow_private = std::env::var("HADES_FETCH_ALLOW_PRIVATE").as_deref() == Ok("1");
    // One warm fetch, so the first connection's cost does not land in the measurement.
    let _ = harness::fetch::run(&url, 2_000_000, allow_private);
    let start = std::time::Instant::now();
    for _ in 0..count {
        let body = harness::fetch::run(&url, 2_000_000, allow_private);
        if body.starts_with("refused") || body.starts_with("curl failed") {
            eprintln!("failed: {body}");
            std::process::exit(1);
        }
    }
    let elapsed = start.elapsed();
    println!(
        "{count} fetches in {:.3} s, {:.2} ms each",
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / count as f64
    );
}
