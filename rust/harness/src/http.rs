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
use std::path::{Path, PathBuf};
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
        "http" if !target.loopback() => Err(Fault::model(format!(
            "{} is plain http and not this machine, so the key would travel in cleartext; use https",
            target.display()
        ))),
        "http" => direct(&target, body, headers, timeout),
        "https" => curl(url, &target, body, headers, timeout),
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

    /// Whether this endpoint is this machine.
    ///
    /// The only place plain HTTP is allowed, because plain HTTP sends every header, including a bearer key,
    /// in cleartext: anything on the path can read it and a redirect can move it. A name is not loopback
    /// unless it says so, so `localhost` counts and nothing else does; a hostname that resolves to a
    /// loopback address does not, because the resolution is the thing an attacker would control.
    pub fn loopback(&self) -> bool {
        if self.host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        let bare = self.host.trim_start_matches('[').trim_end_matches(']');
        bare.parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
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
    // A local model server is plain HTTP on the loopback and that is the case this transport exists for.
    // Anywhere else the key would be readable on the way, so the request is refused before it is made.
    if target.scheme == "http" && !target.loopback() {
        return Err(Fault::model(format!(
            "{} is plain http and not this machine, so the key would travel in cleartext; use https",
            target.display()
        )));
    }
    let secret = Private::for_request(url, headers)?;

    let mut command = Command::new("curl");
    command.args(stream_args(timeout));
    command.args(["-X", "POST", "--data-binary", "@-"]);
    command.arg("-K");
    command.arg(secret.path());
    command.args(["--header", "Content-Type: application/json"]);
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
        return Err(Fault::model(redact(&format!(
            "the model stream ended badly: {said}"
        ))));
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
        // First, because curl honours it only in position zero: a `.curlrc` that named a proxy would
        // otherwise decide where a provider key goes.
        "-q".to_string(),
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
    url: &str,
    target: &Target,
    body: &str,
    headers: &[(&str, String)],
    timeout: Duration,
) -> Result<Reply, Fault> {
    // The URL and the header lines go through one config file so neither becomes an argument. A URL can
    // carry a credential by the provider's own design (Telegram's `/bot<token>/method`), and the process
    // table is readable while the call is in flight, so "the endpoint is not secret" is not a distinction
    // this client is willing to make. The file is created private and removed by the guard, including when
    // curl fails or panics out.
    let secret = Private::for_request(url, headers)?;

    let mut command = Command::new("curl");
    command.args(curl_args(timeout, secret.path()));
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
        // Redacted at the boundary: curl's own sentence and this module's target both describe a request
        // whose path can be a credential, and a fault is written to the session log.
        return Err(Fault::model(redact(&format!(
            "curl could not reach {}: {stderr}",
            target.display()
        ))));
    }

    let (body, status) = match stdout.rsplit_once('\n') {
        Some((body, code)) if code.trim().parse::<u16>().is_ok() => {
            (body.to_string(), code.trim().parse::<u16>().unwrap_or(0))
        }
        _ => (stdout, 0),
    };
    // A zero status means curl never got an HTTP answer: the only thing on stdout is the write-out line. That
    // is the transport failure this module's doc speaks of, and calling it a reply lost curl's own sentence
    // and handed the caller an empty body to misread as a parse error.
    if status == 0 {
        return Err(Fault::model(redact(&format!(
            "curl could not reach {}: {stderr}",
            target.display()
        ))));
    }
    Ok(Reply { status, body })
}

/// The arguments curl is given, apart from the pipes.
///
/// A function rather than four inline arg calls, because the one rule that matters here is a rule about
/// this list: nothing in it may be a credential, and that includes a URL whose path carries one. The URL and
/// every header travel in the config file named by `-K`, so this list can be printed, tested, and read in a
/// process table without leaking anything. `-q` comes first because curl honours it only in position zero,
/// and a `.curlrc` that named a proxy would otherwise decide where a provider key goes.
fn curl_args(timeout: Duration, config: &std::path::Path) -> Vec<String> {
    vec![
        "-q".to_string(),
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
        "-K".to_string(),
        config.display().to_string(),
        "--header".to_string(),
        "Content-Type: application/json".to_string(),
    ]
}

/// Where a short-lived file goes: Hades' own directory, never the machine's temp directory.
/// `TMPDIR` belongs to whatever launched the process, and under the live install that is the install's own
/// cache. A harness that keeps its secrets in another system's cache has crossed the boundary it draws.
fn private_path(name: &str) -> PathBuf {
    // Before the first file is written, once per process: a run that was killed could not run its `Drop`,
    // and what it left behind is a 0600 file holding a bearer key with nobody left to remove it.
    sweep_once();
    private_base().join(name)
}

/// The directory the short-lived files live in, under this process's home.
fn private_base() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => private_base_in(Path::new(&home)),
        None => std::env::temp_dir(),
    }
}

/// The directory under one home, created `0700` the way every other state directory is.
///
/// A bare `create_dir_all` gives the directory the process umask, which on a desktop is `0775`: every user
/// on the machine could list the files while a call was in flight, and the header file is a provider key.
/// `scratch::state_dir` is the one creator for that reason, and it also brings an existing directory back to
/// `0700` rather than trusting one an older build made.
fn private_base_in(home: &Path) -> PathBuf {
    let base = home.join(".hades/tmp");
    let _ = crate::scratch::state_dir(&base);
    base
}

/// Remove the header files whose process is gone.
///
/// The naming scheme already says whose a file is (`headers-<pid>-<nanos>.conf`), so the rule is the pid and
/// nothing else: if that process no longer exists, the file is residue from a killed run and goes. A pid the
/// kernel has recycled keeps an orphan alive until its new owner exits, which is the price of a rule that
/// does not have to trust a clock or a boot time. Files this module did not write are left alone.
///
/// It runs on every private write rather than once at startup: a gateway that has been up for a week is the
/// process most likely to be sharing the directory with a run that got killed, and the readdir costs less
/// than the file it is about to create.
fn sweep_once() {
    sweep_in(&private_base());
}

fn sweep_in(base: &std::path::Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(owner) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(header_owner)
        else {
            continue;
        };
        if owner == std::process::id() || alive(owner) {
            continue;
        }
        let _ = fs::remove_file(&path);
    }
}

/// The pid in a `headers-<pid>-<nanos>.conf` name, when the name has the shape this module writes.
fn header_owner(name: &str) -> Option<u32> {
    name.strip_prefix("headers-")?
        .strip_suffix(".conf")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// Whether a pid names a process that exists. A process owned by somebody else counts as alive, because it
/// is: `EPERM` is an answer, not an absence.
fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 is the existence probe; it is not delivered.
    let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if status == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// A file with the secret in it, removed when it goes out of scope.
pub(crate) struct Private {
    path: PathBuf,
}

impl Private {
    /// The config file for one curl call: the URL first, then the header lines.
    ///
    /// Both halves are secret for the same reason. A key in argv is readable in the process table, and a URL
    /// whose path carries a token (Telegram's `/bot<token>/method`) is a key in argv one level up. Putting
    /// the URL here too means no caller has to remember which of the two its endpoint is: the argument list
    /// carries a file name and the request, and nothing else.
    fn for_request(url: &str, headers: &[(&str, String)]) -> Result<Private, Fault> {
        let mut text = format!("url = \"{}\"\n", quoted(url));
        for (name, value) in headers {
            text.push_str(&format!(
                "header = \"{}\"\n",
                quoted(&format!("{name}: {value}"))
            ));
        }
        Private::write(&text)
    }

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

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// A private config file that names a URL and nothing else, for a caller that must keep it out of argv.
pub(crate) fn secret_url(url: &str) -> Result<Private, Fault> {
    Private::for_request(url, &[])
}

/// One value inside a curl config file, quoted and escaped: curl reads the file with its own quoting rules,
/// and a value with a quote in it would otherwise end the string early.
fn quoted(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Remove a bot token from any text that is about to be logged, chatted or faulted.
///
/// Telegram puts the credential in the URL's path (`/bot<token>/method`, `/file/bot<token>/path`), so this is
/// keyed to that shape and not to a guess at what a token looks like: everything between `/bot` and the next
/// `/` goes. It is applied where the fault is built, because the message that leaks is always the one path
/// nobody enumerated.
pub fn redact(text: &str) -> String {
    const MARK: &str = "/bot";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(MARK) {
        out.push_str(&rest[..at]);
        out.push_str("/bot<redacted>");
        let after = &rest[at + MARK.len()..];
        // The token runs to the next separator; keeping the separator leaves `/bot<redacted>/method`
        // readable as a URL rather than as a word.
        match after.find('/') {
            Some(slash) => rest = &after[slash..],
            None => rest = "",
        }
    }
    out.push_str(rest);
    out
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
        // so nothing secret may be in it, and the URL is in that file too because a provider can put a
        // credential in a URL's path.
        let sentinel = "Bearer sentinel-token-that-must-not-appear";
        let file =
            Private::write(&format!("header = \"Authorization: {sentinel}\"\n")).expect("a file");
        let args = curl_args(Duration::from_secs(5), file.path());
        let joined = args.join(" ");
        assert!(!joined.contains("sentinel"), "{joined}");
        assert!(!joined.contains("Bearer"), "{joined}");
        assert!(
            joined.contains("-K "),
            "{joined}: the key has to be passed by file or not at all"
        );
        assert!(
            !joined.contains("://"),
            "a URL in the argument list is a credential for any provider that puts one in the path: \
             {joined}"
        );
        assert_eq!(
            args.first().map(String::as_str),
            Some("-q"),
            "curl reads .curlrc unless -q is the first argument: {joined}"
        );
        // And the file it names is private and goes away when the value does.
        let path = file.path().to_path_buf();
        assert!(path.exists());
        drop(file);
        assert!(!path.exists(), "the header file outlived the call");
    }

    #[test]
    fn a_bot_url_never_reaches_a_fault_or_an_argument() {
        // Telegram's token is in the URL by design, so `format!("{}/bot{}/{method}")` makes every message
        // about the request a message about the token. It reached the process table as an argument and the
        // session log as "curl could not reach https://api.telegram.org/bot<token>/sendMessage".
        let token = "123456:SECRET-sentinel";
        let url = format!("https://127.0.0.1:1/bot{token}/sendMessage");
        let config = Private::for_request(&url, &[]).expect("a config");
        let args = curl_args(Duration::from_secs(5), config.path());
        let joined = args.join(" ");
        assert!(
            !joined.contains(token),
            "the token is an argument: {joined}"
        );
        assert!(
            !joined.contains("/bot"),
            "the bot path is an argument: {joined}"
        );
        // The file carries it instead, which is the one place the process table cannot read.
        let text = std::fs::read_to_string(config.path()).expect("the config");
        assert!(text.contains(&url), "{text}");

        // Nothing listens on port 1, so this fails at the transport, which is the path that prints it.
        let fault = post(&url, "{}", &[], Duration::from_secs(5)).expect_err("nothing listens");
        let detail = fault.detail();
        // The URL keeps its shape and loses the token, which is what a reader needs to act on the failure.
        assert!(!detail.contains(token), "{detail}");
        assert!(detail.contains("/bot<redacted>/sendMessage"), "{detail}");
        assert!(
            detail.contains("could not reach"),
            "the failure is still reported: {detail}"
        );
        assert!(detail.contains("<redacted>"), "{detail}");

        // And the helper on its own, because a fault or a log line is built from text this module did not
        // construct: the token is whatever sits between `/bot` and the next `/`.
        assert_eq!(
            redact("GET https://api.telegram.org/bot123456:SECRET/getMe failed"),
            "GET https://api.telegram.org/bot<redacted>/getMe failed"
        );
        assert_eq!(
            redact("https://api.telegram.org/file/bot123456:SECRET/photos/x.jpg"),
            "https://api.telegram.org/file/bot<redacted>/photos/x.jpg"
        );
        assert_eq!(redact("nothing to hide"), "nothing to hide");
    }

    #[test]
    fn plain_http_is_loopback_only() {
        // The comment said "loopback only" and nothing enforced it. A POST to any http host would have sent
        // the bearer key in cleartext, so the scheme is refused for every host that is not this machine and
        // kept for the one that is.
        let refused = post(
            "http://example.com/v1/chat/completions",
            "{}",
            &[("Authorization", "Bearer secret".to_string())],
            Duration::from_secs(2),
        )
        .expect_err("cleartext to a remote host");
        assert!(refused.detail().contains("cleartext"), "{refused:?}");

        // The twin: the loopback is allowed through to the transport, which then fails because nothing is
        // listening. The failure names the connection, not the scheme.
        let local = post(
            "http://127.0.0.1:1/v1/chat/completions",
            "{}",
            &[],
            Duration::from_secs(2),
        )
        .expect_err("nothing listens");
        assert!(
            !local.detail().contains("cleartext"),
            "the loopback was refused by the scheme rule: {local:?}"
        );
        assert!(
            local.detail().contains("nothing is listening")
                || local.detail().contains("does not resolve"),
            "{local:?}"
        );

        // And the streaming path, which is where a local model actually attaches.
        let streaming = stream(
            "http://example.com/v1/chat/completions",
            "{}",
            &[],
            Duration::from_secs(2),
            &mut |_| {},
        )
        .expect_err("cleartext to a remote host");
        assert!(streaming.detail().contains("cleartext"), "{streaming:?}");
    }

    /// The directory that holds a provider key is not world-readable, whether it existed or not.
    ///
    /// `create_dir_all` gives the directory the process umask, which on a desktop is `0775`. Measured on the
    /// host this was written for: the directory was `0775` with 63 header files in it, one of which held a
    /// real bearer key.
    #[test]
    fn the_private_directory_is_0700_even_when_it_was_not() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = scratch("private-mode");
        let base = home.join(".hades/tmp");
        std::fs::create_dir_all(&base).expect("a directory");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o775))
            .expect("permissive");
        let returned = private_base_in(&home);
        assert_eq!(returned, base);
        let mode = std::fs::metadata(&base)
            .expect("the directory")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "an existing directory kept mode {mode:o}");
        assert!(!private_base_in(&home).join("never").exists());

        // And a directory this creates is 0700 from the start, not only after an earlier build made it wrong.
        let fresh = scratch("private-fresh");
        let created = private_base_in(&fresh);
        let mode = std::fs::metadata(&created)
            .expect("the directory")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "a new directory was made mode {mode:o}");
    }

    /// A file left by a killed run goes; a file whose process is alive stays.
    ///
    /// The `Drop` that removes a header file cannot run when the process is SIGKILLed, so the sweep is the
    /// only thing that can remove what is left. The rule is the pid in the name, because a killed run leaves
    /// no record anywhere else.
    #[test]
    fn the_sweep_removes_an_orphan_and_keeps_a_live_one() {
        let base = scratch("private-sweep");
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("a short-lived process");
        let dead = child.id();
        child.wait().expect("reaped");

        let orphan = base.join(format!("headers-{dead}-1.conf"));
        let live = base.join(format!("headers-{}-2.conf", std::process::id()));
        let foreign = base.join("something-else.conf");
        for path in [&orphan, &live, &foreign] {
            std::fs::write(path, "header = \"Authorization: Bearer key\"\n").expect("a file");
        }
        sweep_in(&base);
        assert!(!orphan.exists(), "the orphan survived the sweep");
        assert!(live.exists(), "a live process's file was removed");
        assert!(
            foreign.exists(),
            "a file this module did not write was removed"
        );

        // And the wiring: the sweep is what a private write runs, so a gateway that has been up for a week
        // cleans up after a sibling that was killed rather than waiting for its own restart.
        let real = private_base();
        let orphan = real.join(format!("headers-{dead}-3.conf"));
        std::fs::write(&orphan, "url = \"https://example.invalid/\"\n").expect("a file");
        let _ = private_path("probe.conf");
        assert!(
            !orphan.exists(),
            "a private write did not sweep the directory it writes in"
        );
    }

    /// A directory of this test's own, under the crate's scratch root.
    fn scratch(name: &str) -> PathBuf {
        crate::scratch::unique(name)
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
