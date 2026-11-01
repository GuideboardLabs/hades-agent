//! One bounded wait, for every stub server the integration tests stand in front of.
//!
//! Included per test binary with `#[path = "fixtures/accept.rs"] mod accept;`. The tests that use it spawn a
//! fixture thread and, in most of them, join it after the call under test. A fixture that blocks in
//! `accept()` cannot report a change that stops the code from connecting: cargo has no per-test timeout, so
//! the run parks until something outside kills it and the log says only that the job died. The wait is
//! therefore bounded here, once, so a missing connection is an assertion naming what was expected.

#![allow(dead_code)] // each binary includes both and uses one or the other.

use std::time::{Duration, Instant};

/// How long a fixture waits for the connection it was built for.
///
/// Generous on purpose: this is a bound on a hang, not a performance assertion, and a loaded machine must not
/// fail one. A real regression never connects at all, so the difference between 10 s and 60 s is nothing.
const WAIT: Duration = Duration::from_secs(10);

/// The same, on disk.
pub fn unix_within(
    listener: &std::os::unix::net::UnixListener,
    what: &str,
) -> std::os::unix::net::UnixStream {
    listener
        .set_nonblocking(true)
        .expect("a pollable socket in the fixture");
    let deadline = Instant::now() + WAIT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("a blocking connection");
                // A peer that connects and then says nothing must also fail rather than park.
                stream
                    .set_read_timeout(Some(WAIT))
                    .expect("a read deadline");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    panic!(
                        "no {what} arrived within 10 s: the code under test never connected, so the fixture \
                         has nothing to answer"
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("waiting for {what}: {error}"),
        }
    }
}

/// The same, over TCP.
pub fn tcp_within(listener: &std::net::TcpListener, what: &str) -> std::net::TcpStream {
    listener
        .set_nonblocking(true)
        .expect("a pollable socket in the fixture");
    let deadline = Instant::now() + WAIT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("a blocking connection");
                stream
                    .set_read_timeout(Some(WAIT))
                    .expect("a read deadline");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    panic!(
                        "no {what} arrived within 10 s: the code under test never connected, so the fixture \
                         has nothing to answer"
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("waiting for {what}: {error}"),
        }
    }
}
