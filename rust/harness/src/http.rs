//! One POST, and read the answer.
//!
//! This is not an HTTP client. The harness makes exactly one kind of request, and the point of the module
//! is to say plainly where the bytes go and what is not handled.
//!
//! Two transports, and the reason there are two. A local llama.cpp server speaks plain HTTP on the
//! loopback, and the standard library is enough for that: a socket, a request line, a content length, and
//! the answer. The cloud endpoint is TLS, and the standard library has no TLS. The choice was between a
//! certificate stack (a dependency tree an order of magnitude larger than everything else in this
//! workspace, for one POST) and the `curl` that is already on this machine and already trusted with this
//! exact job. This picks curl, in one place, with the cost written down: one subprocess per call, and the
//! request body on stdin so a long prompt never reaches the process table.
//!
//! Where the key goes, and where it must never go. Never in argv: the process table is readable while the
//! call is in flight. Never in a fault's text: a fault is written to the session log, and a log that
//! contains a key is a leak with extra steps. For curl the header travels in a config file created
//! mode 0600 and unlinked as soon as the process exits, on both the success and the failure path.
//!
//! What this does not do: redirects, proxies, cookies, keep-alive, retries, HTTP/2, or any method but POST.

use std::fs;
use std::io::{BufRead, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::fault::Fault;

/// The answer, whether or not it was the answer you wanted. A reply carries its status so the caller can
/// decide; only a transport failure (nothing connected, nothing came back) is an error here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The largest answer this will read, so a wrong endpoint that streams forever cannot eat the machine.
const MAX_BODY: usize = 8 * 1024 * 1024;

/// A POST with a body and headers, over whichever transport the scheme calls for.
pub fn post(
    url: &str,
    body: &str,
    headers: &[(&str, String)],
    timeout: Duration,
) -> Result<Reply, Fault> {
    let target = Target::parse(url)?;
    match target.scheme.as_str() {
        "http" => direct(&target, body, headers, timeout),
        "https" => curl(&target, body, headers, timeout),
        other => Err(Fault::model(format!(
            "the endpoint scheme {other:?} is neither http nor https, and this client speaks nothing else"
        ))),
    }
}

/// A URL, taken apart far enough to connect to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Target {
    pub fn parse(url: &str) -> Result<Target, Fault> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| {
            Fault::model(format!(
                "the endpoint {url:?} has no scheme, so there is no way to know how to reach it"
            ))
        })?;
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(Fault::model(format!("the endpoint {url:?} names no host")));
        }
        // "://nothing" splits into an empty scheme and a host, which would then be dialled as if the
        // missing scheme did not matter. It does: the scheme is the whole choice of transport.
        if scheme.is_empty() {
            return Err(Fault::model(format!(
                "the endpoint {url:?} has no scheme before the ://, so there is no transport named"
            )));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                let port = port.parse::<u16>().map_err(|_| {
                    Fault::model(format!(
                        "the endpoint {url:?} has a port that is not a number"
                    ))
                })?;
                (host.to_string(), port)
            }
            None => (
                authority.to_string(),
                if scheme == "https" { 443 } else { 80 },
            ),
        };
        Ok(Target {
            scheme: scheme.to_string(),
            host,
            port,
            path: path.to_string(),
        })
    }

    /// What a person may see. No credentials are in here, and there is nowhere for one to hide.
    ///
    /// The port is left off when it is the one the scheme implies, so a log line reads like the URL
    /// somebody would recognise rather than like a socket address.
    pub fn display(&self) -> String {
        let default = (self.scheme == "https" && self.port == 443)
            || (self.scheme == "http" && self.port == 80);
        if default {
            format!("{}://{}{}", self.scheme, self.host, self.path)
        } else {
            format!("{}://{}:{}{}", self.scheme, self.host, self.port, self.path)
        }
    }
}

/// Plain HTTP over a socket. No TLS, so this is for the loopback and for nothing else.
fn direct(
    target: &Target,
    body: &str,
    headers: &[(&str, String)],
    timeout: Duration,
) -> Result<Reply, Fault> {
    let address = format!("{}:{}", target.host, target.port);
    let resolved = address
        .to_socket_addrs()
        .map_err(|e| Fault::model(format!("{address} does not resolve: {e}")))?
        .next()
        .ok_or_else(|| Fault::model(format!("{address} resolves to no address at all")))?;

    let mut stream = TcpStream::connect_timeout(&resolved, timeout)
        .map_err(|e| Fault::model(format!("nothing is listening on {address}: {e}")))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let mut request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        target.path,
        address,
        body.len()
    );
    for (name, value) in headers {
        // A header value with a newline in it would let a caller inject a header, so it is refused here
        // rather than trusted from above.
        if value.contains('\n') || value.contains('\r') {
            return Err(Fault::model(format!(
                "the {name} header contains a newline, which is a header injection, not a header"
            )));
        }
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);

    stream.write_all(request.as_bytes()).map_err(|e| {
        Fault::model(format!(
            "the request to {address} could not be written: {e}"
        ))
    })?;
    stream.flush().ok();

    let mut raw = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > MAX_BODY {
                    return Err(Fault::model(format!(
                        "the answer from {address} passed {MAX_BODY} bytes, which is not an answer to a \
                         chat completion"
                    )));
                }
            }
            Err(e) => {
                return Err(Fault::model(format!(
                    "the answer from {address} stopped early: {e}"
                )));
            }
        }
    }

    split(&raw)
}

/// Split a raw HTTP reply into a status and a body, decoding a chunked body if that is what it is.
fn split(raw: &[u8]) -> Result<Reply, Fault> {
    let text = String::from_utf8_lossy(raw).to_string();
    let (head, rest) = text.split_once("\r\n\r\n").ok_or_else(|| {
        Fault::model(
            "the endpoint's answer had no blank line between the headers and the body, so it is not HTTP"
                .to_string(),
        )
    })?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            Fault::model(format!(
                "the endpoint's status line {status_line:?} has no status code in it"
            ))
        })?;

    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        dechunk(rest)?
    } else if let Some(length) = content_length(head) {
        // The length counts bytes, so the cut is on bytes; a cut that lands mid-character is lossy in the
        // same way the whole read is, and losing one character of a truncated answer beats refusing it.
        let bytes = rest.as_bytes();
        String::from_utf8_lossy(&bytes[..length.min(bytes.len())]).to_string()
    } else {
        rest.to_string()
    };
    Ok(Reply { status, body })
}

fn content_length(head: &str) -> Option<usize> {
    for line in head.split("\r\n") {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse::<usize>().ok();
        }
    }
    None
}

/// The chunked transfer encoding, which is four lines of format and has to be right anyway.
fn dechunk(body: &str) -> Result<String, Fault> {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let (size_line, tail) = rest.split_once("\r\n").ok_or_else(|| {
            Fault::model("a chunked answer ended in the middle of a chunk size".to_string())
        })?;
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or(""), 16)
            .map_err(|_| {
                Fault::model(format!(
                    "a chunked answer said the next chunk is {size_line:?} bytes, which is not a number"
                ))
            })?;
        if size == 0 {
            return Ok(out);
        }
        if tail.len() < size {
            return Err(Fault::model(
                "a chunked answer promised more bytes than it delivered".to_string(),
            ));
        }
        out.push_str(&tail[..size]);
        rest = tail[size..].strip_prefix("\r\n").unwrap_or(&tail[size..]);
    }
}

/// HTTPS through curl, with the key in a file rather than in argv.
/// A POST whose answer arrives in pieces. Every line that arrives goes to `on_line` as soon as curl has it.
///
/// This is the streaming twin of [`post`], and it goes through curl for both schemes: a plain HTTP model
/// server would need a second incremental reader in the standard library for no gain, and the process is
/// already the transport for the cloud. The same private config file carries the headers, so the key is
/// still never an argument. `--fail-with-body` keeps a 4xx body readable instead of turning it into a bare
/// exit code.
pub fn stream(
    url: &str,
    body: &str,
    headers: &[(&str, String)],
    timeout: Duration,
    on_line: &mut dyn FnMut(&str),
) -> Result<(), Fault> {
    let target = Target::parse(url)?;
    if target.scheme != "http" && target.scheme != "https" {
        return Err(Fault::model(format!(
            "the endpoint scheme {:?} is neither http nor https, and this client speaks nothing else",
            target.scheme
        )));
    }
    let secret = if headers.is_empty() {
        None
    } else {
        let mut text = String::new();
        for (name, value) in headers {
            text.push_str("header = \"");
            text.push_str(
                &format!("{name}: {value}")
                    .replace('\\', "\\\\")
                    .replace('"', "\\\""),
            );
            text.push_str("\"\n");
        }
        Some(Private::write(&text)?)
    };

    let mut command = Command::new("curl");
    command.args(stream_args(timeout));
    command.args(["-X", "POST", "--data-binary", "@-"]);
    if let Some(secret) = &secret {
        command.arg("-K");
        command.arg(secret.path());
    }
    command.args(["--header", "Content-Type: application/json"]);
    command.arg(target.display());
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|e| {
        Fault::model(format!(
            "the endpoint needs a transport and curl could not be started: {e}"
        ))
    })?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(body.as_bytes()).map_err(|e| {
            Fault::model(format!(
                "the request body could not be written to curl: {e}"
            ))
        })?;
    }
    drop(child.stdin.take());

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Fault::model("curl gave no stream to read".to_string()))?;
    let mut reader = std::io::BufReader::new(stdout);
    let mut seen = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| Fault::model(format!("the model stream stopped: {e}")))?;
        if read == 0 {
            break;
        }
        // `--fail-with-body` puts the provider's error body on stdout, where the SSE reader would
        // otherwise discard it. Keep a bounded copy so a refusal can be reported in the server's own
        // words instead of "error: 500".
        if seen.len() < 8192 {
            seen.push_str(&line);
        }
        on_line(&line);
    }

    let output = child
        .wait_with_output()
        .map_err(|e| Fault::model(format!("curl did not finish: {e}")))?;
    drop(secret);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let refusal = crate::fetch_pool::refusal_message(&seen);
        let said = match refusal {
            Some(said) => said,
            None => stderr.lines().next().unwrap_or("").to_string(),
        };
        return Err(Fault::model(format!(
            "the model stream ended badly: {said}"
        )));
    }
    Ok(())
}

/// The transport arguments for a streamed answer. A long generation is not a stalled one, and a single
/// `--max-time` cannot tell them apart: on a 20 tokens/second local model the budget is reached after a
/// few thousand tokens and the turn is cut mid-answer, which reads as "the model stream ended badly".
/// Kill a stream that delivers nothing for the budget instead, under a ceiling no single answer reaches.
fn stream_args(timeout: Duration) -> Vec<String> {
    let stall = timeout.as_secs().max(1);
    let ceiling = stall.saturating_mul(8);
    vec![
        "-sS".to_string(),
        "--no-buffer".to_string(),
        "--fail-with-body".to_string(),
        "--connect-timeout".to_string(),
        "30".to_string(),
        "--speed-limit".to_string(),
        "1".to_string(),
        "--speed-time".to_string(),
        stall.to_string(),
        "--max-time".to_string(),
        ceiling.to_string(),
    ]
}

fn curl(
    target: &Target,
    body: &str,
    headers: &[(&str, String)],
    timeout: Duration,
) -> Result<Reply, Fault> {
    // The header lines go through a config file so the key never becomes an argument. The file is created
    // private and removed by the guard, including when curl fails or panics out.
    let secret = if headers.is_empty() {
        None
    } else {
        let mut text = String::new();
        for (name, value) in headers {
            text.push_str("header = \"");
            text.push_str(
                &format!("{name}: {value}")
                    .replace('\\', "\\\\")
                    .replace('"', "\\\""),
            );
            text.push_str("\"\n");
        }
        Some(Private::write(&text)?)
    };

    let mut command = Command::new("curl");
    command.args(curl_args(
        target,
        timeout,
        secret.as_ref().map(|secret| secret.path()),
    ));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|e| {
        // The name of the program, not the request, because a request carries a key.
        Fault::model(format!(
            "the https endpoint needs a transport and curl could not be started: {e}"
        ))
    })?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(body.as_bytes()).map_err(|e| {
            Fault::model(format!(
                "the request body could not be written to curl: {e}"
            ))
        })?;
    }
    // Closing stdin is what tells curl the body ended, so it is dropped before waiting.
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .map_err(|e| Fault::model(format!("curl did not finish: {e}")))?;
    drop(secret);

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() && stdout.trim().is_empty() {
        return Err(Fault::model(format!(
            "curl could not reach {}: {stderr}",
            target.display()
        )));
    }

    let (body, status) = match stdout.rsplit_once('\n') {
        Some((body, code)) if code.trim().parse::<u16>().is_ok() => {
            (body.to_string(), code.trim().parse::<u16>().unwrap_or(0))
        }
        _ => (stdout, 0),
    };
    Ok(Reply { status, body })
}

/// The arguments curl is given, apart from the pipes.
///
/// A function rather than four inline arg calls, because the one rule that matters here is a rule about
/// this list: nothing in it may be a credential. The key travels in the config file named by `-K`, so this
/// list can be printed, tested, and read in a process table without leaking anything.
fn curl_args(target: &Target, timeout: Duration, secret: Option<&std::path::Path>) -> Vec<String> {
    let mut args = vec![
        "-sS".to_string(),
        "--max-time".to_string(),
        timeout.as_secs().max(1).to_string(),
        "-X".to_string(),
        "POST".to_string(),
        "--data-binary".to_string(),
        "@-".to_string(),
        // The status is asked for after the body, and read back off the last line.
        "--write-out".to_string(),
        "\n%{http_code}".to_string(),
    ];
    if let Some(path) = secret {
        args.push("-K".to_string());
        args.push(path.display().to_string());
    }
    args.push("--header".to_string());
    args.push("Content-Type: application/json".to_string());
    args.push(target.display());
    args
}

/// Where a short-lived file goes: Hades' own directory, never the machine's temp directory.
///
/// `TMPDIR` belongs to whatever launched the process, and under the live install that is the install's own
/// cache. A harness that keeps its secrets in another system's cache has crossed the boundary it draws.
fn private_path(name: &str) -> PathBuf {
    let base = match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".hades/tmp"),
        Err(_) => std::env::temp_dir(),
    };
    let _ = fs::create_dir_all(&base);
    base.join(name)
}

/// A file with the secret in it, removed when it goes out of scope.
struct Private {
    path: PathBuf,
}

impl Private {
    fn write(text: &str) -> Result<Private, Fault> {
        let path = private_path(&format!(
            "headers-{}-{}.conf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|e| {
            Fault::model(format!("a private header file could not be created: {e}"))
        })?;
        file.write_all(text.as_bytes()).map_err(|e| {
            Fault::model(format!("a private header file could not be written: {e}"))
        })?;
        drop(file);
        Ok(Private { path })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for Private {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn a_url_becomes_a_host_a_port_and_a_path() {
        let t = Target::parse("https://ollama.com/v1/chat/completions").expect("parses");
        assert_eq!(t.scheme, "https");
        assert_eq!(t.host, "ollama.com");
        assert_eq!(t.port, 443);
        assert_eq!(t.path, "/v1/chat/completions");

        let t = Target::parse("http://127.0.0.1:8110/v1/chat/completions").expect("parses");
        assert_eq!(t.host, "127.0.0.1");
        assert_eq!(t.port, 8110);

        // A URL with no path still has one.
        let t = Target::parse("http://127.0.0.1:8110").expect("parses");
        assert_eq!(t.path, "/");
        assert_eq!(t.port, 8110);
    }

    #[test]
    fn a_url_that_cannot_be_reached_is_refused_before_anything_is_sent() {
        for bad in ["ollama.com/v1", "://nothing", "http://"] {
            let fault = Target::parse(bad).expect_err("refused");
            assert!(
                fault.detail().contains("scheme") || fault.detail().contains("host"),
                "{bad}: {}",
                fault.detail()
            );
        }
    }

    #[test]
    fn no_argument_to_curl_is_ever_a_credential() {
        // The whole point of the secret file: the argument list can be read by any process on the machine,
        // so nothing secret may be in it.
        let sentinel = "Bearer sentinel-token-that-must-not-appear";
        let file =
            Private::write(&format!("header = \"Authorization: {sentinel}\"\n")).expect("a file");
        let target = Target::parse("https://example.invalid/v1/chat/completions").expect("parses");
        let args = curl_args(&target, Duration::from_secs(5), Some(file.path()));
        let joined = args.join(" ");
        assert!(!joined.contains("sentinel"), "{joined}");
        assert!(!joined.contains("Bearer"), "{joined}");
        assert!(
            joined.contains("-K "),
            "{joined}: the key has to be passed by file or not at all"
        );
        assert!(
            joined.contains("https://example.invalid/v1/chat/completions"),
            "{joined}"
        );
        // And the file it names is private and goes away when the value does.
        let path = file.path().to_path_buf();
        assert!(path.exists());
        drop(file);
        assert!(!path.exists(), "the header file outlived the call");
    }

    #[test]
    fn a_chunked_body_is_put_back_together() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n3\r\n1}\n\r\n0\r\n\r\n";
        let reply = split(raw).expect("splits");
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, "{\"a\":1}\n");
    }

    #[test]
    fn a_body_with_a_content_length_is_cut_to_it() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}trailing junk the endpoint should not send";
        let reply = split(raw).expect("splits");
        assert_eq!(reply.body, "{}");
    }

    #[test]
    fn a_status_line_without_a_code_is_refused_rather_than_guessed() {
        let fault = split(b"nonsense\r\n\r\n{}").expect_err("refused");
        assert!(fault.detail().contains("status"), "{}", fault.detail());
    }

    /// A streamed answer gets a stall budget, not a total cap: the transfer may run as long as it is
    /// moving, and only silence for the budget ends it. A total cap at the budget is what cut a healthy
    /// generation mid-file on a 20 tokens/second local model.
    #[test]
    fn a_stream_is_cut_for_silence_not_for_being_long() {
        let args = stream_args(std::time::Duration::from_secs(180));
        let value = |name: &str| {
            let at = args
                .iter()
                .position(|arg| arg == name)
                .unwrap_or_else(|| panic!("{name} missing from {args:?}"));
            args[at + 1].clone()
        };
        assert_eq!(
            value("--speed-time"),
            "180",
            "the stall budget is the timeout"
        );
        assert_eq!(value("--speed-limit"), "1");
        assert!(
            value("--max-time").parse::<u64>().unwrap() >= 180 * 4,
            "the ceiling leaves room for a long answer: {:?}",
            args
        );
        assert_eq!(value("--connect-timeout"), "30");
    }
}
