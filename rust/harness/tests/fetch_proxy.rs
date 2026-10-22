//! The proxy rule, in its own process.
//!
//! This test sets `http_proxy` and `HTTP_PROXY`, which are process-wide. A unit test doing that can re-route
//! a fetch another test is making at the same moment, and a suite failure that depends on thread order is
//! worse than no test. Cargo gives each integration-test binary its own process, so the variables are
//! private here.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;

#[test]
fn a_proxy_in_the_environment_does_not_take_a_pooled_fetch() {
    // The destination: a plain loopback server that answers one request.
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

    // The proxy: anything that arrives here is a fetch that went to the wrong place.
    let proxy = TcpListener::bind("127.0.0.1:0").expect("a proxy port");
    proxy.set_nonblocking(true).expect("a nonblocking proxy");
    let proxy_url = format!("http://{}", proxy.local_addr().expect("an address"));

    // SAFETY: this binary is one process that runs this one test, and nothing else reads these variables.
    unsafe {
        std::env::set_var("http_proxy", &proxy_url);
        std::env::set_var("HTTP_PROXY", &proxy_url);
        std::env::set_var("all_proxy", &proxy_url);
        std::env::set_var("ALL_PROXY", &proxy_url);
    }

    // The public path, so this covers the pool selection as well as the option it sets.
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
    assert!(
        !proxied,
        "the fetch went to the proxy instead of the checked address"
    );
    let _ = destination.join();
}
