//! curl's own configuration file, in a process of its own.
//!
//! `curl` reads `.curlrc` unless `-q` is its first argument, and that file can name a proxy, extra URLs or
//! a relaxed TLS check. The fetch path clears the environment for a reason, so a config file that overrides
//! the destination has to be closed as well. This test sets `CURL_HOME` and `HOME`, which is process-wide,
//! so it lives in its own integration-test binary rather than beside the unit tests.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;

#[test]
fn a_curlrc_proxy_does_not_take_a_subprocess_fetch() {
    let fixture = TcpListener::bind("127.0.0.1:0").expect("a fixture port");
    let fixture_address = fixture.local_addr().expect("an address");
    let destination = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = fixture.accept() {
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            let body = "page";
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });

    let proxy = TcpListener::bind("127.0.0.1:0").expect("a proxy port");
    proxy.set_nonblocking(true).expect("a nonblocking proxy");
    let proxy_url = format!("http://{}", proxy.local_addr().expect("an address"));

    let home = std::env::temp_dir().join(format!("hades-curlrc-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("a home directory");
    let config: PathBuf = home.join(".curlrc");
    std::fs::write(&config, format!("proxy = \"{proxy_url}\"\n")).expect("a config file");

    // SAFETY: one test in its own process; nothing else reads these.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("CURL_HOME", &home);
        // The subprocess path, which is the one that runs `curl`.
        std::env::set_var("HADES_FETCH_POOL", "off");
    }

    let url = format!("http://{fixture_address}/page");
    let said = harness::fetch::run(&url, 5_000, true);

    let mut proxied = false;
    for _ in 0..20 {
        if proxy.accept().is_ok() {
            proxied = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    assert_eq!(
        said, "page\n",
        "the fetch did not read the destination: {said}"
    );
    assert!(!proxied, "curl read .curlrc and went to the proxy");
    let _ = destination.join();
    let _ = std::fs::remove_dir_all(&home);
}
