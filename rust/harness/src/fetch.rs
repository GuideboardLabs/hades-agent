//! The fetch tool and its policy, in the order a request is actually judged.
//!
//! The old fetch was one shell out to `curl -sSL`. That shape let the tool do four things no tool of
//! this harness should do, and each is answered before curl is allowed to connect once:
//!
//! - **any scheme curl speaks.** `file:///etc/passwd` and `gopher://` are URLs a person would never
//!   type and a model sometimes will. Only `http` and `https` pass; everything else is refused by
//!   name, before any process is started.
//! - **credentials in the URL.** `user:pass@host` is a way to send a header the caller did not mean
//!   to write down. A URL with userinfo is refused, full stop.
//! - **the address curl resolves to.** The point of a fetch policy is that a hostname whose answer
//!   is `10.0.0.9` is judged on the answer, not on the name. So the host is resolved here, with the
//!   std's `ToSocketAddrs`, and *every* address it answers with is classified: loopback,
//!   unspecified, link-local, the private ranges, CGNAT and multicast are refused unless the caller
//!   explicitly allows private targets. A host that answers nothing is refused with the name in
//!   the message.
//! - **the redirect chain curl followed.** `-L` handed the decision at every hop to curl, and
//!   curl's decision is "scheme I was built with is fine", which is how a 302 to
//!   `file:///etc/passwd` came back as a page. Redirects are now driven from here: curl is run
//!   *without* `-L`, its headers are read for a `Location`, and every hop goes through the same
//!   parse, classify and pin the first URL went through. At most five hops.
//!
//! Three guards keep the drive honest rather than best effort. `--proto =http,https` and
//! `--proto-redir =http,https` stand on every invocation, so curl itself also refuses anything even
//! if this module's own judgment were wrong. `--resolve host:port:addr` pins the address that was
//! validated, so the resolve-then-connect race (lookup answers one address, connect opens another)
//! cannot happen within one call: curl never does a fresh lookup of a name whose answer this module
//! did not classify. And `-o` writes the body to a file the parent reads back *capped*, so an
//! oversized body never exists in this process as an oversized string.
//!
//! Two edges are named because they are edges, not oversights. An IPv6 literal is classified like
//! any address but is not pinned: curl's `--resolve` cannot parse a bracketed host, and a literal
//! has no lookup to race - the address is the text, so resolving and classifying are the same act.
//! And the proxy environment is cleared per call, because a drive that pins addresses would be
//! silently re-routed by a proxy variable this module never read.
//!
//! The body cap is two walls, not one. `--max-filesize` makes curl stop an oversized transfer;
//! measured: with a known size curl exits 63 without writing, and with a close-delimited body it
//! exits 56 after some partial write. Exit 63 is therefore read as "the cap fired" and answered with
//! the cap's own wording; a 56 is answered as a failed transfer, because nothing in its exit code
//! says the cap is why. The second wall is the parent's read: `take(READ_CAP)` bounds the bytes that
//! ever become a string here, whatever a server claimed or streamed, so the cap holds on both
//! shapes even where the message differs. Both are before the text is returned; nothing buffers a
//! whole oversized body.
//!
//! Three things this module owns that curl cannot be asked to own.
//!
//! **The clock.** One `Instant` deadline is taken per fetch, not per hop, and each hop is handed
//! the whole seconds left in it (at least one); a hop that would start after the deadline is
//! refused instead. Six hops therefore spend at most the one deadline, not six of them.
//! `getaddrinfo` itself carries no deadline: the resolver this module calls before the hop is a
//! blocking call that this module cannot interrupt, so a resolver that never answers holds the
//! thread past the deadline. The deadline bounds curl's transfer, not the lookup.
//!
//! **The scratch it writes.** One directory per call, created `0700`, holds both the header file
//! and the body file, and a `Drop` guard removes the whole directory on every exit path - a page,
//! an oversize refusal, a non-zero exit, a panic. A failure that leaves a page's headers in a shared
//! temp directory is a failure that wrote on its way out.
//!
//! **The TLS gap, closed.** HTTPS is proven end to end by a live fixture: the test mints a
//! self-signed certificate for `127.0.0.1` with the `openssl` CLI, serves a file with
//! `openssl s_server -WWW` on a loopback port, and fetches it through the tool with `--cacert`
//! supplied by a test-only seam. No Rust dependency is added, and production never sets a CA:
//! `run` passes `None`, so the tool trusts the system's store and nothing else. The fixture makes
//! the `openssl` binary a test-time prerequisite; a machine without it fails that test loudly
//! rather than skipping it.
//!
//! Only `301`, `302`, `303`, `307` and `308` are followed. Any other 3xx carrying a `Location` is a
//! response, not an invitation, and its body is what the call answers.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The tool's own name, so `agent.rs` dispatches to this module without spelling it twice.
pub const NAME: &str = "fetch";

use crate::agent::{clip, strip_html};

/// The most bytes one curl call may leave on disk before this module reads no further. Sits far
/// above the 12000 characters `clip` keeps - headroom for a page whose value survives stripping -
/// and far below a page that exists to be big.
const BODY_CAP: u64 = 4 * 1024 * 1024;

/// The most bytes the parent reads back from one call's body file, bound to the same number: a
/// transfer that survived `--max-filesize` never becomes a bigger string here.
const READ_CAP: u64 = BODY_CAP;

/// How many redirect hops may be followed.
const MAX_REDIRECTS: usize = 5;

/// The whole fetch's clock: every hop of one chain is spent out of this one deadline, not given a
/// fresh one.
const FETCH_DEADLINE: Duration = Duration::from_secs(30);

/// The refusal a hop gets when the fetch's clock has run out before it can start.
const DEADLINE_PASSED: &str =
    "refused: the fetch's 30-second deadline passed before this hop could start";

/// One per-call scratch directory, `0700`, holding that call's header file and body file. It is
/// made here and removed by `Drop` here, so every exit path - a page, an oversize refusal, a
/// non-zero exit, a panic - leaves the shared temp directory as it found it. The mode is set at
/// creation, so there is no window in which another user could open what curl is about to write.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Result<Scratch, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("hades-fetch-{}-{stamp}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(|error| {
            format!("could not make a scratch directory for the fetch: {error}")
        })?;
        #[cfg(test)]
        record_scratch(&path);
        Ok(Scratch { path })
    }

    /// One file inside this call's directory. Both files share the directory's lifetime, so no
    /// path outlives the call that made it.
    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// The scratch directories this thread made, oldest first. Only tests read this: the failure paths
// a test can reach (oversize, a non-zero exit) must be shown to leave nothing behind, and the
// directory's name is otherwise private to `Scratch`.
#[cfg(test)]
thread_local! {
    static SCRATCH_MADE: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn record_scratch(path: &Path) {
    SCRATCH_MADE.with(|made| made.borrow_mut().push(path.to_path_buf()));
}

#[cfg(test)]
fn scratch_made() -> Vec<PathBuf> {
    SCRATCH_MADE.with(|made| made.borrow().clone())
}

#[cfg(test)]
fn forget_scratch() {
    SCRATCH_MADE.with(|made| made.borrow_mut().clear());
}

/// What one URL is, parsed off its text. No URL crate is wanted here: the parts this policy judges -
/// scheme, userinfo, host, port - are exactly the parts the `://` split and the `@` cannot hide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Lowercased scheme: `http` or `https`. Everything else is refused before an address is asked
    /// for.
    pub scheme: String,
    /// The host, brackets already off an IPv6 literal. Percent-escapes are not decoded: a host that
    /// spelled an address escaped is a refusal, because a policy that judged the unescaped spelling
    /// would judge an address the URL never named.
    pub host: String,
    /// The port the URL names, or the scheme's default (80, 443). Port 0 is invalid and refused.
    pub port: u16,
}

/// A URL judged and found speakable. Build one only through [`parse`], which is the whole gate; the
/// fields are public so a test can lay one out by hand, and the constructor is the policy.
impl Target {
    /// Parse a URL enough to judge it, and refuse everything this fetch does not speak: another
    /// scheme, a URL with userinfo, no host, a malformed IPv6 literal, a port that is not one, a
    /// host with whitespace, control bytes or percent-escapes in it.
    ///
    /// This is the gate every hop goes through, not only the first: a redirect's `Location` is
    /// parsed right here, so a chain cannot carry the first URL's clearance into a scheme the first
    /// never asked about.
    pub fn parse(text: &str) -> Result<Target, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("refused: no URL was given".to_string());
        }
        // Control bytes anywhere are a refusal. A newline in a URL is a header the caller meant to
        // write and a command line the caller does not get to rearrange.
        if let Some(ch) = text
            .chars()
            .find(|ch| matches!(ch, '\0' | '\n' | '\r' | '\t'))
        {
            return Err(format!(
                "refused: a control character ({ch:?}) in a URL is not allowed"
            ));
        }
        // Scheme and hier-part, cut apart the only way RFC 3986 makes `://` meaningful: `data:`
        // and `mailto:` carry everything after the colon as opaque data and have no `//` at all,
        // which is exactly why the split is the gate - an opaque URL cannot be judged, so it is
        // refused with its scheme named, not parsed as a hostless one.
        let Some((scheme, hier_part)) = text.split_once(':') else {
            return Err(
                "refused: this URL has no scheme; fetch speaks http and https only".to_string(),
            );
        };
        let scheme = scheme.to_ascii_lowercase();
        match scheme.as_str() {
            "http" | "https" => {}
            "file" | "data" | "ftp" | "gopher" | "ssh" | "mailto" => {
                return Err(format!(
                    "refused: the `{scheme}` scheme is not allowed; fetch speaks http and https only"
                ));
            }
            _ => {
                return Err(format!(
                    "refused: the `{scheme}` scheme is not recognised; fetch speaks http and https only"
                ));
            }
        }
        let Some((scheme_marker, rest)) = hier_part.split_once("//") else {
            return Err(format!(
                "refused: the `{scheme}` scheme carries no host, and fetch judges hosts"
            ));
        };
        if !scheme_marker.is_empty() {
            return Err(format!(
                "refused: the `{scheme}` scheme is not a hierarchical URL fetch judges"
            ));
        }
        // The authority is everything up to the path, the query or the fragment. Escapes inside it
        // are left escaped and refused by the host checks below: a `%2F` in a host is not a slash,
        // it is a spelling this fetch declines to guess at.
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        // The first `@` makes whatever follows credentials, however it is spelled.
        if authority.contains('@') {
            return Err("refused: a URL with userinfo (user:pass@host) is not allowed".to_string());
        }
        if authority.is_empty() {
            return Err("refused: this URL has no host".to_string());
        }
        let (host, port) = split_host_port(authority, &scheme)?;
        // A parsing shortcut would see `http://example.com .evil.com` as `example.com`. There is no
        // shortcut here: whitespace, control bytes and percent-escapes in a host are refusals, each
        // for the same reason - a host may not carry a hidden second spelling.
        if host.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            return Err("refused: the host carries whitespace or control characters".to_string());
        }
        // curl normalises an internationalised name to punycode before it looks anything up, so
        // `--resolve <the name as written>` would never match, curl would resolve the punycode
        // itself, and the address this module classified would not be the address connected to.
        // The name is refused instead of being judged twice.
        if !host.is_ascii() {
            return Err("refused: a non-ASCII host is not a host this fetch can judge".to_string());
        }
        if host.contains('%') {
            return Err(
                "refused: a percent-escaped host is not a host this fetch can judge".to_string(),
            );
        }
        Ok(Target { scheme, host, port })
    }
}

/// `:` + the port, or nothing when the URL named the scheme's default and curl would fill it in.
#[cfg(test)]
fn port_suffix(target: &Target) -> String {
    let default = default_port(&target.scheme);
    if target.port == default {
        String::new()
    } else {
        format!(":{}", target.port)
    }
}

/// The host with its brackets back on, the spelling a URL needs. An IPv6 literal is the one host
/// form that must be re-bracketed to name itself in a URL again.
#[cfg(test)]
fn host_in_url(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Split the authority into host and port. An IPv6 literal arrives bracketed and leaves with its
/// brackets off, because classification and messages want the address, not the spelling.
fn split_host_port(authority: &str, scheme: &str) -> Result<(String, u16), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let close = rest.find(']').ok_or_else(|| {
            "refused: the IPv6 literal in this URL has no closing bracket".to_string()
        })?;
        let host = rest[..close].to_string();
        if host.is_empty() {
            return Err("refused: the IPv6 literal in this URL is empty".to_string());
        }
        let after = &rest[close + 1..];
        let port =
            match after.strip_prefix(':') {
                Some(text) => non_zero_port(text)?,
                _ if after.is_empty() => default_port(scheme),
                _ => return Err(
                    "refused: text after an IPv6 literal's bracket is not a URL this fetch judges"
                        .to_string(),
                ),
            };
        return Ok((host, port));
    }
    if authority.contains(':') {
        // One colon is a port separator. Two or more without brackets is an IPv6 host this URL does
        // not spell correctly, and an address cannot be validated from this text.
        let (host, text) = authority
            .split_once(':')
            .expect("a colon in the authority splits once");
        if host.contains(':') || text.contains(':') {
            return Err("refused: an unbracketed IPv6 address is not a URL host".to_string());
        }
        if host.is_empty() {
            return Err("refused: this URL has no host".to_string());
        }
        Ok((host.to_string(), non_zero_port(text)?))
    } else {
        Ok((authority.to_string(), default_port(scheme)))
    }
}

fn non_zero_port(text: &str) -> Result<u16, String> {
    let port = text
        .parse::<u16>()
        .map_err(|_| format!("refused: `{text}` is not a port this fetch can use"))?;
    if port == 0 {
        return Err("refused: port 0 is not a URL this fetch judges".to_string());
    }
    Ok(port)
}

fn default_port(scheme: &str) -> u16 {
    if scheme == "https" { 443 } else { 80 }
}

/// Is this address one fetch refuses? The list is closed on purpose: everything on it is a place a
/// stranger's packet does not come from, or a place no packet comes from at all. `allow_private` is
/// the caller's deliberate hole, never the default.
///
/// Classification is by octets and segments rather than methods, so the policy's list is what this
/// answers from: 127/8, ::1, the unspecified address with its block, 169.254/16, fe80::/10, 10/8,
/// 172.16/12, 192.168/16, 100.64/10, fc00::/7, multicast, the reserved upper blocks, and the two
/// IPv6 shapes that tunnelling uses to carry a v4 target inside.
pub fn is_private(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => is_private_v4(ip),
        IpAddr::V6(ip) => is_private_v6(ip),
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 127                                 // loopback
        || o[0] == 0                            // unspecified and the 0/8 this-network block
        || o[0] == 10                           // 10/8
        || (o[0] == 172 && (o[1] & 0xf0) == 16) // 172.16/12
        || (o[0] == 192 && o[1] == 168)         // 192.168/16
        || (o[0] == 169 && o[1] == 254)         // link-local 169.254/16
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT 100.64/10
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking 198.18/15
        || (o[0] & 0xf0) == 224                 // multicast 224/4
        || (o[0] & 0xf0) == 240 // reserved and broadcast 240/4
}

fn is_private_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // The shapes an IPv6 address uses to carry an IPv4 one, each judged as the v4 it holds.
    // ::ffff:a.b.c.d is the mapped shape; ::a.b.c.d is the deprecated compatible shape, anything
    // in it but the unspecified address itself; 64:ff9b::a.b.c.d is the NAT64 well-known prefix,
    // whose embedded address is the one a translator would dial.
    let embedded: Option<Ipv4Addr> = if s[0..5] == [0, 0, 0, 0, 0] && s[5] == 0xffff
        || (s[0..6] == [0, 0, 0, 0, 0, 0] && !(s[6] == 0 && s[7] == 0))
        || s[0..6] == [0x0064, 0xff9b, 0, 0, 0, 0]
    {
        Some(v4_from_segments(s[6], s[7]))
    } else {
        None
    };
    if let Some(address) = embedded {
        return is_private_v4(address);
    }
    ip.is_loopback()                                    // ::1
        || s == [0, 0, 0, 0, 0, 0, 0, 0]                // the unspecified address
        || (s[0] & 0xffc0) == 0xfe80                    // link-local fe80::/10
        || (s[0] & 0xffc0) == 0xfec0                    // deprecated site-local fec0::/10
        || (s[0] & 0xfe00) == 0xfc00                    // unique-local fc00::/7
        || (s[0] & 0xff00) == 0xff00                    // multicast
        || (s[0] == 0x2001 && s[1] == 0x0000)           // Teredo 2001::/32
        || s[0] == 0x2002 // 6to4 2002::/16
}

/// One IPv4 address out of the two segments that hold it, high half first.
fn v4_from_segments(high: u16, low: u16) -> Ipv4Addr {
    Ipv4Addr::new((high >> 8) as u8, high as u8, (low >> 8) as u8, low as u8)
}

/// The address a target names. A literal is classified directly; a name is resolved and *every*
/// answer judged, because a resolver that answers half private is doing exactly the thing this check
/// exists to see. The first answer is returned, to pin.
fn resolve(target: &Target, allow_private: bool) -> Result<IpAddr, String> {
    let answer = match target.host.parse::<IpAddr>() {
        Ok(address) => address,
        Err(_) => {
            let answers: Vec<IpAddr> = (target.host.as_str(), target.port)
                .to_socket_addrs()
                .map_err(|error| {
                    format!(
                        "refused: the host `{}` could not be resolved ({error})",
                        target.host
                    )
                })?
                .map(|socket| socket.ip())
                .collect();
            if answers.is_empty() {
                return Err(format!(
                    "refused: the host `{}` resolved to no address",
                    target.host
                ));
            }
            for address in &answers {
                if is_private(*address) && !allow_private {
                    return Err(format!(
                        "refused: the host `{}` resolves to {}, a private address, and private \
                         targets are not allowed",
                        target.host, address
                    ));
                }
            }
            answers[0]
        }
    };
    if is_private(answer) && !allow_private {
        return Err(format!(
            "refused: {answer} is a private address, and private targets are not allowed"
        ));
    }
    Ok(answer)
}

/// The tool's entry. `allow_private` comes from the surface: agent.rs reads
/// `HADES_FETCH_ALLOW_PRIVATE=1` and passes it down; tests pass it directly. `limit` is the
/// character clip the tool's caller keeps (12000). Nothing here reads the environment, so a test
/// never has to.
pub fn run(url: &str, limit: usize, allow_private: bool) -> String {
    match chase(url, allow_private, None) {
        Ok(text) => clip(&strip_html(&text), limit),
        Err(reason) => reason,
    }
}

/// The same validated fetch without stripping markup. Social link discovery needs the `href`
/// attributes that `run` intentionally removes; the caller keeps this body for scanning only and
/// never stores it as evidence.
pub fn run_raw(url: &str, limit: usize, allow_private: bool) -> Result<String, String> {
    chase(url, allow_private, None).map(|text| clip(&text, limit))
}

/// The tool's entry with a CA file for the suite's TLS fixture. Production never reaches this:
/// `run` passes `None`, so the tool trusts the system's store and nothing else. The parameter
/// exists because a self-signed loopback certificate is the only way to prove HTTPS end to end
/// without a Rust TLS dependency, and curl must be told to trust exactly that certificate.
#[cfg(test)]
fn run_with_ca(url: &str, limit: usize, allow_private: bool, ca: &Path) -> String {
    match chase(url, allow_private, Some(ca)) {
        Ok(text) => clip(&strip_html(&text), limit),
        Err(reason) => reason,
    }
}

/// Validate, connect, and follow. One curl call per hop, never `-L`: the `Location` header is read
/// from the header file, resolved against the URL that produced it, and pushed through the same
/// parse, classify and pin the first URL went through.
fn chase(url: &str, allow_private: bool, ca: Option<&Path>) -> Result<String, String> {
    let deadline = Instant::now() + FETCH_DEADLINE;
    let mut current = url.to_string();
    let mut followed = 0usize;
    loop {
        // The clock is read before the hop, not inside curl: a hop that has no time left is
        // refused here rather than started with a zero or negative `--max-time`.
        let max_time = remaining_seconds(deadline)?;
        let target = Target::parse(&current)?;
        let address = resolve(&target, allow_private)?;
        match hop(&target, &current, address, max_time, ca)? {
            Outcome::Page(bytes) => return Ok(String::from_utf8_lossy(&bytes).to_string()),
            Outcome::Oversize => {
                return Err(
                    "refused: the body passed the cap, and a body this tool would not be able to \
                     use is not fetched"
                        .to_string(),
                );
            }
            Outcome::Redirect(next) => {
                followed += 1;
                if followed > MAX_REDIRECTS {
                    return Err(format!(
                        "refused: the redirect chain from `{}` passed {MAX_REDIRECTS} hops",
                        clip_ref(url, 80)
                    ));
                }
                current = resolve_location(&current, &next);
            }
        }
    }
}

/// The whole seconds one hop may spend out of the fetch's single deadline. A partial second left
/// is one second, the smallest `--max-time` curl takes; a deadline that has passed - or has no
/// time left at all - is a refusal, so no hop is ever started with a zero clock.
fn remaining_seconds(deadline: Instant) -> Result<u64, String> {
    match deadline.checked_duration_since(Instant::now()) {
        None => Err(DEADLINE_PASSED.to_string()),
        Some(left) if left.is_zero() => Err(DEADLINE_PASSED.to_string()),
        Some(left) => Ok(left.as_secs().max(1)),
    }
}

/// The three ways one curl call can end, decided from its exit code and its header file.
enum Outcome {
    /// A terminal response (2xx, 4xx, 5xx): the body file is what matters.
    Page(Vec<u8>),
    /// `--max-filesize` fired: the body was never completed, so the call is over.
    Oversize,
    /// A 3xx with a `Location`: the chain continues with the value as it arrived.
    Redirect(String),
}

/// One curl call: no `-L`, protocols restricted on both the primary and the redirects, redirect
/// following off (`--max-redirs 0`, curl's own counter agreeing with the driver), the body capped
/// before transfer, both files inside one `0700` directory that `Drop` removes however the call
/// ends. The URL travels as it was given or resolved: no reassembly, because a second parser is a
/// second chance to be wrong.
fn call(
    target: &Target,
    url: &str,
    address: IpAddr,
    max_time: u64,
    ca: Option<&Path>,
) -> Result<Outcome, String> {
    let scratch = Scratch::new()?;
    let headers = scratch.file("hdr");
    let body = scratch.file("body");
    let args = curl_args(target, url, address, max_time, &headers, &body, ca);
    let mut command = std::process::Command::new("curl");
    command
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // A proxy variable would re-route a drive that pins addresses, so every spelling of one goes.
    for variable in [
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        command.env_remove(variable);
    }
    let output = command
        .output()
        .map_err(|error| format!("could not run curl: {error}"))?;
    let code = output.status.code().unwrap_or(-1);
    // `-sS` means stderr carries curl's own sentence for a failure. It is kept and quoted rather
    // than discarded: "curl failed with exit 7" alone does not say what went wrong, and the
    // sentence is the one thing the model can act on.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    // 63 is `--max-filesize` exceeded with a known size; 56 and the like is the same trip with a
    // close-delimited body. Both mean the cap fired, and the message says so rather than guessing.
    if code == 63 {
        return Ok(Outcome::Oversize);
    }
    if code != 0 {
        return Err(if stderr.is_empty() {
            format!("curl failed with exit {code}")
        } else {
            format!("curl failed with exit {code}: {}", clip_ref(stderr, 300))
        });
    }
    let location = read_location(&headers)?;
    Ok(match location {
        Some(value) => Outcome::Redirect(value),
        None => Outcome::Page(read_capped(&body)?),
    })
}

/// One hop, through the shared libcurl handle when there is one and through a child process when there
/// is not. The pool is an optimisation: anything it cannot do, or does without a sentence, is handed to
/// the path that has been answering all along, so a missing `libcurl.so.4` changes the bill and not the
/// behaviour.
fn hop(
    target: &Target,
    url: &str,
    address: IpAddr,
    max_time: u64,
    ca: Option<&Path>,
) -> Result<Outcome, String> {
    if let Some(result) = crate::fetch_pool::get(
        url,
        &target.host,
        target.port,
        address,
        max_time,
        BODY_CAP as usize,
        ca,
    ) {
        return match result {
            Ok((status, body, headers)) => {
                let text = String::from_utf8_lossy(&headers);
                match location_in(&text) {
                    Some(value) => Ok(Outcome::Redirect(value)),
                    // A `204`, a `304` or a `3xx` this tool does not follow: the body is the answer,
                    // and a body is what the caller gets.
                    None => {
                        let _ = status;
                        Ok(Outcome::Page(body))
                    }
                }
            }
            // `--max-filesize` in the process path reports 63; the pool reports the same trip as a
            // short write. Both are the cap, and the wording belongs to this module, not to curl.
            Err(reason) if reason == "cap" => Ok(Outcome::Oversize),
            // No sentence means the failure has nothing the model can act on, so the process path
            // answers instead and quotes curl itself.
            Err(reason) if reason.is_empty() => call(target, url, address, max_time, ca),
            Err(reason) => Err(clip_ref(&reason, 300)),
        };
    }
    call(target, url, address, max_time, ca)
}

/// The argument vector one hop runs with, split out so a test can read exactly what curl is handed
/// without a server. The protocol walls, the redirects-off switch, the clock, the two scratch files,
/// the pin and - only for the TLS fixture - a CA file are all here; `call` adds only the process's
/// environment.
fn curl_args(
    target: &Target,
    url: &str,
    address: IpAddr,
    max_time: u64,
    headers: &Path,
    body: &Path,
    ca: Option<&Path>,
) -> Vec<String> {
    // The pin is `--resolve host:port:addr`, host spelled as the URL spelled it (bracketed for a
    // v6 literal, which curl also cannot parse - but a literal needs no pin, because there is no
    // lookup for a literal to race).
    let mut args: Vec<String> = vec![
        "-sS".to_string(),
        "--max-time".to_string(),
        max_time.to_string(),
        "--proto".to_string(),
        "=http,https".to_string(),
        "--proto-redir".to_string(),
        "=http,https".to_string(),
        "--max-redirs".to_string(),
        "0".to_string(),
        "--max-filesize".to_string(),
        BODY_CAP.to_string(),
        "-A".to_string(),
        "Mozilla/5.0 (compatible; Hades/0.21)".to_string(),
        "-D".to_string(),
        headers.display().to_string(),
        "-o".to_string(),
        body.display().to_string(),
        url.to_string(),
    ];
    if target.host.parse::<IpAddr>().is_err() {
        args.push("--resolve".to_string());
        args.push(format!("{}:{}:{}", target.host, target.port, address));
    }
    // Trust exactly the fixture's certificate, and nothing else. Production passes `None`, so this
    // branch is dead in the tool and live only in the end-to-end HTTPS test: there is no `-k`, and
    // no way for a page to talk the tool out of verification.
    if let Some(ca) = ca {
        args.push("--cacert".to_string());
        args.push(ca.display().to_string());
    }
    args
}

/// What the header file says about continuing. `Some` is one of the five followed 3xx codes with a
/// `Location` read clean; `None` is every other shape, including any other 3xx - with or without a
/// `Location` - which is terminal because the method and body have nothing left to say.
fn read_location(headers: &Path) -> Result<Option<String>, String> {
    let text = std::fs::read_to_string(headers)
        .map_err(|error| format!("could not read curl's headers: {error}"))?;
    Ok(location_in(&text))
}

/// The same reading, over header bytes already in memory. Two callers, one parser: a second copy is a
/// second chance for the redirect rule to disagree with itself.
fn location_in(text: &str) -> Option<String> {
    let mut redirected = false;
    let mut location: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("HTTP/") {
            // curl with no `-L` and `--max-redirs 0` writes exactly one status block; a second
            // prefix would mean a prior response, and only the last line is acted on, so the
            // variables are overwritten rather than accumulated.
            // `HTTP/1.1 302 Found`: the version is the first word and the status is the second, so
            // the code is read past the version rather than off the front of it.
            let code = rest
                .split_ascii_whitespace()
                .nth(1)
                .and_then(|code| code.parse::<u16>().ok())
                .unwrap_or(0);
            // Only the codes a client actually follows. A 300, a 304 or any other 3xx carrying a
            // `Location` is a response, not an invitation; it keeps its body and ends the chain.
            redirected = matches!(code, 301 | 302 | 303 | 307 | 308);
            // A new block wipes a `Location` read before it: that value belonged to a response the
            // status line has now replaced.
            location = None;
        } else if redirected
            && location.is_none()
            && let Some(value) = line
                .strip_prefix("Location:")
                .or_else(|| line.strip_prefix("location:"))
        {
            location = Some(value.trim().to_string());
        }
    }
    location.filter(|_| redirected)
}

/// Resolve a `Location` against the URL it came from. The shapes that matter, in the order they are
/// asked: an empty value is the current URL back; a fragment-only value replaces the current
/// fragment and a query-only value replaces the current query, neither of them the path; an
/// absolute URL stays; `//host` gets the original scheme; `/path` gets the original authority; a
/// relative name gets the original directory. Each result is judged by the caller as its own hop.
fn resolve_location(current: &str, location: &str) -> String {
    let location = location.trim();
    if location.is_empty() {
        return current.to_string();
    }
    if location.contains("://") {
        return location.to_string();
    }
    let Some((scheme, rest)) = current.split_once("://") else {
        return location.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let path = &rest[authority_end..];
    // `#x` against `/page?q=1#old` is `/page?q=1#x`, and `?a=1` against `/search?q=old` is
    // `/search?a=1`: a reference that starts with `?` or `#` names the part it replaces. Treating
    // either as a relative path would append it to the current path instead.
    if let Some(fragment) = location.strip_prefix('#') {
        let base = current.split('#').next().unwrap_or(current);
        return format!("{base}#{fragment}");
    }
    if let Some(query) = location.strip_prefix('?') {
        let base = current.split('#').next().unwrap_or(current);
        let base = base.split('?').next().unwrap_or(base);
        return format!("{base}?{query}");
    }
    if let Some(without) = location.strip_prefix("//") {
        return format!("{scheme}://{without}");
    }
    if location.starts_with('/') {
        return format!("{scheme}://{authority}{location}");
    }
    // The base is the current path's directory, read from the path alone. A path of `/` has no
    // directory but the root, and the root is the empty string, not the slash: keeping the slash
    // gives `//rel`, which a server reads as another path from the first one.
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let base = path
        .rsplit_once('/')
        .map(|(directory, _)| directory)
        .unwrap_or("")
        .trim_end_matches('/');
    format!("{scheme}://{authority}{base}/{location}")
}

/// Read one call's body file at most `READ_CAP` bytes in. The `take` is what bounds the buffer: a
/// server that lied about its size never becomes an oversized string here, because the read stops
/// and the remainder stays on disk, deleted on the caller's path out.
fn read_capped(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|mut file| file.by_ref().take(READ_CAP).read_to_end(&mut bytes))
        .map_err(|error| format!("could not read the fetched body: {error}"))?;
    Ok(bytes)
}

/// Trim for a message: one line, at most this width, no character cut in half.
fn clip_ref(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut at = max;
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    format!("{}…", &text[..at])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// What the parser refuses, before curl is even a thought. Each row is a URL and the one
    /// wording every refusal of it shares; the whole point of judging first is that the model is
    /// told why, in prose it can hear.
    #[test]
    fn a_url_this_fetch_does_not_speak_is_refused_before_curl() {
        for (url, why) in [
            ("file:///etc/passwd", "file"),
            ("data:text/plain,hello", "data"),
            ("ftp://example.com/pub", "ftp"),
            ("gopher://example.com/1", "gopher"),
            ("ssh://user@example.com", "ssh"),
            ("mailto:someone@example.com", "mailto"),
            ("svn://example.com/trunk", "not recognised"),
            ("//example.com/no-scheme", "no scheme"),
            ("http://user:pass@example.com/page", "userinfo"),
            ("https://@example.com/", "userinfo"),
            ("http:///", "no host"),
            ("http://[::1/typo", "closing bracket"),
            ("http://example.com:0/", "port"),
            ("http://example.com:letters/", "port"),
            ("http://[fe80::1%25eth0]/", "percent"),
            // curl would IDNA-normalise this to punycode before looking it up, so `--resolve` on
            // the spelling above would not match and the pinned address would not be the one
            // connected to. The name is refused rather than judged twice.
            ("http://exämple.test/", "non-ASCII host"),
        ] {
            let said = Target::parse(url).expect_err(url);
            assert!(said.starts_with("refused:"), "{url}: {said}");
            assert!(said.contains(why), "{url}: {said}");
        }
    }

    /// The two schemes that speak, parsed clean. The host of a bracketed v6 literal loses the
    /// brackets, so classification sees the address.
    #[test]
    fn a_url_this_fetch_speaks_is_parsed() {
        let plain = Target::parse("http://example.com/a?b=c").expect("parses");
        assert_eq!(plain.scheme, "http");
        assert_eq!(plain.host, "example.com");
        assert_eq!(plain.port, 80);
        let https = Target::parse("https://example.com:8443/x").expect("parses");
        assert_eq!(https.port, 8443);
        let v6 = Target::parse("http://[2001:db8::1]:8080/").expect("parses");
        assert_eq!(v6.host, "2001:db8::1");
        assert_eq!(v6.port, 8080);
        // Round trip: the re-bracketed spelling is the spelling the URL came in as.
        assert_eq!(host_in_url(&v6.host), "[2001:db8::1]");
        // Upper case schemes fold down.
        let upper = Target::parse("HTTPS://EXAMPLE.COM/").expect("parses");
        assert_eq!(upper.scheme, "https");
        assert_eq!(port_suffix(&upper), "");
    }

    /// The closed list, address by address, in the exact terms the policy names them. Every
    /// address here is refused unless the caller says otherwise, and every refusal is the same
    /// wording.
    #[test]
    fn every_private_shape_is_refused_and_the_public_one_passes() {
        let refused = [
            "127.0.0.1",
            "127.8.8.8",
            "::1",
            "169.254.1.1",
            "10.0.0.1",
            "192.168.1.1",
            "0.0.0.0",
            "172.16.0.1",
            "172.31.255.255",
            "100.64.0.1",
            "100.127.255.255",
            "198.18.0.1",
            "198.19.255.255",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "224.0.0.1",
            "239.255.255.255",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "64:ff9b::127.0.0.1",
            "64:ff9b::10.0.0.1",
            "2001:0:db8::1",
            "2002:102:304::1",
        ];
        for text in refused {
            let address: IpAddr = text.parse().expect(text);
            assert!(is_private(address), "{text} must be refused");
        }
        // The classifier judges the v4 an IPv6 shape carries, not the shape: `::102:304` and
        // `::ffff:1.2.3.4` both carry 1.2.3.4, which is public, so both pass. The mapping is a
        // spelling; the address is what the policy has an opinion about.
        let public = [
            "8.8.8.8",
            "1.2.3.4",
            "198.20.0.1",
            "2606:4700::1111",
            "::ffff:8.8.4.4",
            "::102:304",
            "::ffff:1.2.3.4",
            "64:ff9b::8.8.8.8",
        ];
        for text in public {
            let address: IpAddr = text.parse().expect(text);
            assert!(!is_private(address), "{text} must pass");
        }
    }

    /// A redirect that leaves the allowed schemes, cut at the hop rather than fetched. The first
    /// URL is judged with the hole open so the chain is what is on trial: this is the hop a URL
    /// already answered yes to, and the answer the hop gives is no.
    #[test]
    fn a_302_to_a_file_url_is_refused_at_the_hop() {
        let fixture = site(&[
            (
                b"/hop",
                b"HTTP/1.1 302 Found\r\nLocation: file:///etc/passwd\r\nContent-Length: 0\r\n\r\n",
            ),
            (b"/gone", b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"),
        ]);
        let said = run(&fixture.url("/hop"), 12_000, true);
        assert!(said.starts_with("refused:"), "{said}");
        assert!(said.contains("`file`"), "{said}");
        assert!(!said.contains("passwd"), "no hop past the refusal: {said}");
    }

    /// A loopback target, refused unless the caller's environment allows private targets. The
    /// fixture binds `127.0.0.1:0` exactly so the two sides of the gate can be tested against the
    /// same live server.
    #[test]
    fn a_loopback_target_answers_only_when_private_is_allowed() {
        let fixture = site(&[(
            b"/",
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 16\r\n\r\n<p>loop page</p>",
        )]);
        let url = fixture.url("/");
        let said = run(&url, 12_000, false);
        assert!(said.starts_with("refused:"), "{said}");
        assert!(said.contains("private address"), "{said}");
        let seen = run(&url, 12_000, true);
        assert_eq!(seen, "loop page\n");
    }

    /// A redirect *into* loopback is judged at the hop by the same pair every first URL meets:
    /// parsed, then resolved against the caller's hole. End to end this chain cannot refuse - the
    /// first URL is on loopback too and only answered because the hole was open - so the closed
    /// side is asserted on the hop itself, and the open side is asserted end to end: the body that
    /// comes back is the tunnelled page, so the hop is judged and followed like any other.
    #[test]
    fn a_302_to_loopback_is_judged_like_any_first_url() {
        let fixture = site(&[
            (
                b"/out",
                b"HTTP/1.1 302 Found\r\nLocation: /inner\r\nContent-Length: 0\r\n\r\n",
            ),
            (
                b"/inner",
                b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\ntunnelled",
            ),
        ]);
        let url = fixture.url("/out");
        let hop = Target::parse(&url).expect("the fixture URL parses");
        let closed = resolve(&hop, false);
        assert!(
            matches!(&closed, Err(said) if said.contains("private address")),
            "{closed:?}"
        );
        assert_eq!(resolve(&hop, true), Ok(IpAddr::from([127, 0, 0, 1])));
        assert_eq!(run(&url, 12_000, true), "tunnelled\n");
    }

    /// A chain longer than the bound is cut, and the message names the bound. The fixture hands
    /// out `/loop-N` hops; the run from `/loop-6` needs six and the sixth is the refusal. (From
    /// `/loop-5` the same chain answers, so the bound is the only difference.)
    #[test]
    fn a_redirect_chain_longer_than_five_is_refused() {
        let fixture = site(&[]);
        let url = fixture.url("/loop-6");
        let said = run(&url, 12_000, true);
        assert_eq!(
            said,
            format!("refused: the redirect chain from `{url}` passed 5 hops")
        );
        // Inside the bound, the same chain answers: the bound is the only difference.
        let within = run(&fixture.url("/loop-5"), 12_000, true);
        assert_eq!(within, "chain\n");
    }

    /// A redirect with a relative `Location` is resolved against the URL that carried it, then
    /// judged like any hop: the fixture answers `/rel` with a page, so the fetch's answer is the
    /// page and not the redirect's prose.
    #[test]
    fn a_relative_location_is_resolved_and_validated() {
        let fixture = site(&[
            (
                b"/",
                b"HTTP/1.1 302 Found\r\nLocation: rel\r\nContent-Length: 0\r\n\r\n",
            ),
            (
                b"/rel",
                b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nlanded ok",
            ),
        ]);
        let seen = run(&fixture.url("/"), 12_000, true);
        assert_eq!(seen, "landed ok\n");
        // The unit itself, against the shapes a chain actually meets. A base of `/` is the empty
        // directory, not a slash: `http://h/` + `rel` is `http://h/rel`, never `http://h//rel`.
        assert_eq!(
            resolve_location("http://example.com/", "rel"),
            "http://example.com/rel"
        );
        assert_eq!(
            resolve_location("http://example.com/dir/page", "other"),
            "http://example.com/dir/other"
        );
        assert_eq!(
            resolve_location("http://example.com/dir/page", "/root"),
            "http://example.com/root"
        );
        assert_eq!(
            resolve_location("http://example.com/dir", "//else.example.com/x"),
            "http://else.example.com/x"
        );
        assert_eq!(
            resolve_location("http://example.com/dir", "https://example.com/x"),
            "https://example.com/x"
        );
        // A query-only or fragment-only reference names the part it replaces, not a path.
        assert_eq!(
            resolve_location("http://example.com/search?q=old", "?a=1"),
            "http://example.com/search?a=1"
        );
        assert_eq!(
            resolve_location("http://example.com/page#old", "#x"),
            "http://example.com/page#x"
        );
        assert_eq!(
            resolve_location("http://example.com/page?q=1#old", "#x"),
            "http://example.com/page?q=1#x"
        );
    }

    /// The raw channel keeps the markup the canonical body throws away, which is the only way a
    /// social post's artifact link can be seen at all.
    #[test]
    fn the_raw_fetch_keeps_markup_the_body_does_not() {
        let fixture = site(&[(
            b"/post",
            b"HTTP/1.1 200 OK\r\nContent-Length: 26\r\n\r\n<a href=\"/paper\">paper</a>",
        )]);
        let url = fixture.url("/post");
        let raw = run_raw(&url, 12_000, true).expect("a raw body");
        assert!(raw.contains("href=\"/paper\""), "{raw}");
        let body = run(&url, 12_000, true);
        assert!(!body.contains("href"), "{body}");
        assert!(body.contains("paper"), "{body}");
    }

    /// A query-only `Location` replaces the query of the URL that carried it, and the result is
    /// fetched as its own hop: the fixture answers `/find?page=2` with a page, so a `?page=2` sent
    /// after `/find?page=1` lands there rather than being appended to the path.
    #[test]
    fn a_query_only_location_replaces_the_query() {
        let fixture = site(&[
            (
                b"/find?page=1",
                b"HTTP/1.1 302 Found\r\nLocation: ?page=2\r\nContent-Length: 0\r\n\r\n",
            ),
            (
                b"/find?page=2",
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nsecond page",
            ),
        ]);
        assert_eq!(
            run(&fixture.url("/find?page=1"), 12_000, true),
            "second page\n"
        );
    }

    /// One deadline for the whole fetch: the helper hands a hop the seconds it has left, at least
    /// one, and refuses a hop whose deadline has already passed rather than running curl with a
    /// zero clock. `Instant::now()` is the zero case: the check's own clock read is never earlier
    /// than the argument.
    #[test]
    fn a_hop_that_starts_after_the_deadline_is_refused() {
        let said = remaining_seconds(Instant::now()).expect_err("no time left refuses");
        assert_eq!(said, DEADLINE_PASSED);
        let past = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("a monotonic instant one second ago");
        assert_eq!(
            remaining_seconds(past).expect_err("a passed deadline refuses"),
            DEADLINE_PASSED
        );
        let ahead = Instant::now() + Duration::from_secs(31);
        assert_eq!(remaining_seconds(ahead).expect("time left"), 30);
        // A partial second is one second, never zero.
        let almost = Instant::now()
            .checked_add(Duration::from_millis(250))
            .expect("a quarter second ahead");
        assert_eq!(remaining_seconds(almost).expect("a partial second"), 1);
    }

    /// The scratch directory is `0700`, and `Drop` removes it: the same guard every exit path in
    /// `call` gets, exercised on its own first.
    /// The pin has to be per transfer, which is exactly what the process path's `--resolve` cannot be.
    ///
    /// Two sites, both named `localhost`, both on loopback and both private: the only thing that tells
    /// them apart is the port the pin names. If the pin were global (or applied once and left in place),
    /// the second fetch would reach the first site and answer with its body.
    /// Four fetches that each take 250 ms have to finish in about 250 ms, not a second.
    ///
    /// This is the regression the pool of handles exists to prevent: one easy handle behind a mutex is
    /// held for the whole blocking transfer, so research workers that should run together queue behind
    /// each other. The fallback path also passes, which is the point of asserting on the tool.
    #[test]
    fn fetches_in_different_threads_do_not_queue() {
        let site = slow_site(std::time::Duration::from_millis(250), "slow");
        let url = site.url("/");
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let url = url.clone();
                scope.spawn(move || {
                    let said = run(&url, 12_000, true);
                    assert_eq!(said, "slow\n");
                });
            }
        });
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(700),
            "four 250 ms fetches took {elapsed:?}, so they were serialized"
        );
    }

    #[test]
    fn the_second_host_of_a_run_is_pinned_to_its_own_address() {
        let first = site(&[(b"/", b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nfirst")]);
        let second = site(&[(b"/", b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nsecond")]);
        let one = run(&first.named("/"), 12_000, true);
        let two = run(&second.named("/"), 12_000, true);
        assert_eq!(one, "first\n");
        assert_eq!(two, "second\n");
        // And again in the other order, so a pin that only survives one direction does not pass.
        assert_eq!(run(&second.named("/"), 12_000, true), "second\n");
        assert_eq!(run(&first.named("/"), 12_000, true), "first\n");
    }

    /// The pool is an optimisation, so switching it off has to leave the tool working rather than degrade
    /// it: this is the path a machine without `libcurl.so.4` takes.
    #[test]
    fn the_process_path_still_answers_when_the_pool_is_off() {
        let fixture = site(&[(b"/", b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nback")]);
        // SAFETY: no other test in this binary reads this variable concurrently; the fetches below are
        // the only readers and they run on this thread.
        unsafe {
            std::env::set_var("HADES_FETCH_POOL", "off");
        }
        let pooled_off = run(&fixture.url("/"), 12_000, true);
        unsafe {
            std::env::remove_var("HADES_FETCH_POOL");
        }
        assert_eq!(pooled_off, "back\n");
    }

    #[test]
    fn a_scratched_call_leaves_no_directory_behind() {
        let scratch = Scratch::new().expect("a scratch directory");
        let path = scratch.path.clone();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("the directory exists")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "{mode:o}");
        }
        drop(scratch);
        assert!(!path.exists(), "the guard left {} behind", path.display());
    }

    /// The two failure paths that used to leak: an oversized body and a non-zero curl exit. Each
    /// names its own reason, and each leaves no scratch directory - the `Drop` guard runs on the
    /// return that carries the error, not only on the one that carries a page.
    #[test]
    fn a_failed_call_leaves_no_scratch_directory_behind() {
        // Oversize: curl exits 63 with the header and body files already in place.
        let big = (BODY_CAP * 4) as usize;
        let fixture = site_owned(
            vec![owned(
                "/",
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {big}\r\n\r\n{}",
                    "x".repeat(big)
                ),
            )],
            &[],
        );
        forget_scratch();
        let said = run(&fixture.url("/"), 12_000, true);
        assert!(said.contains("cap"), "{said}");
        let made = scratch_made();
        // The pool path holds the body in memory and makes no directory at all; the child path makes one
        // and removes it. Both are covered: `a_scratched_call_leaves_no_directory_behind` drives the
        // child directly, so this test only has to prove that whatever was made is gone.
        for path in made {
            assert!(
                !path.exists(),
                "the oversize path left {} behind",
                path.display()
            );
        }

        // A non-zero exit: a connection that closes before answering is curl exit 52, and its
        // files still exist when the call returns.
        let empty = site_owned(vec![owned("/", String::new())], &[]);
        forget_scratch();
        let said = run(&empty.url("/"), 12_000, true);
        assert!(said.contains("curl failed with exit"), "{said}");
        // `-sS`'s own sentence is in the message, not discarded (M4).
        assert!(said.contains("curl: ("), "curl's stderr is missing: {said}");
        let made = scratch_made();
        assert!(
            made.is_empty() || made.iter().all(|path| !path.exists()),
            "the failing call left a scratch directory"
        );
        for path in made {
            assert!(
                !path.exists(),
                "the failure path left {} behind",
                path.display()
            );
        }
    }

    /// `https` through the gate and through curl's own walls, without a server: the scheme's port,
    /// both protocol lists, the clock, and the pin rule for a name versus a literal. The live
    /// request is the next test's job, against an `openssl s_server` fixture.
    #[test]
    fn https_is_admitted_by_the_gate_and_by_curls_protocol_walls() {
        let target = Target::parse("https://example.test/x").expect("https parses");
        assert_eq!(target.scheme, "https");
        assert_eq!(target.port, 443);
        let args = curl_args(
            &target,
            "https://example.test/x",
            IpAddr::from([93, 184, 216, 34]),
            30,
            Path::new("/scratch/hdr"),
            Path::new("/scratch/body"),
            None,
        );
        for wall in ["--proto", "--proto-redir"] {
            let at = args
                .iter()
                .position(|arg| arg == wall)
                .unwrap_or_else(|| panic!("{wall} is not in {args:?}"));
            assert_eq!(args[at + 1], "=http,https", "{wall}: {args:?}");
            assert!(args[at + 1].contains("https"), "{wall}: {args:?}");
        }
        let clock = args
            .iter()
            .position(|arg| arg == "--max-time")
            .expect("a clock is present");
        assert_eq!(args[clock + 1], "30");
        // A name is pinned; a literal is not.
        assert!(args.contains(&"--resolve".to_string()), "{args:?}");
        let literal = Target::parse("https://127.0.0.1/x").expect("a literal parses");
        let args = curl_args(
            &literal,
            "https://127.0.0.1/x",
            IpAddr::from([127, 0, 0, 1]),
            30,
            Path::new("/scratch/hdr"),
            Path::new("/scratch/body"),
            None,
        );
        assert!(!args.contains(&"--resolve".to_string()), "{args:?}");
    }

    /// HTTPS end to end: a self-signed certificate for `127.0.0.1`, an `openssl s_server -WWW`
    /// fixture serving one file, and the tool's own parse → resolve → curl path. The CA is supplied
    /// only through the test seam. The same URL without it is asserted to fail verification, which
    /// is what proves the fetch is doing TLS rather than being talked out of it: there is no `-k`
    /// anywhere on the command line.
    #[test]
    fn a_https_page_is_fetched_end_to_end_through_a_tls_fixture() {
        let fixture = TlsSite::new(&[("page", "tls page\n")]);
        let url = fixture.url("/page");
        let target = Target::parse(&url).expect("the fixture URL parses");
        assert_eq!(target.scheme, "https");
        assert_eq!(target.port, fixture.port);

        let seen = run_with_ca(&url, 12_000, true, &fixture.ca());
        assert_eq!(seen, "tls page\n");

        // The same request with the system's store alone: the certificate is unknown there, so
        // verification refuses it. That is the difference the `--cacert` made, and it is the reason
        // this test can claim a real handshake rather than a bypass.
        let refused = run(&url, 12_000, true);
        assert!(refused.contains("curl failed with exit 60"), "{refused}");
        assert!(refused.contains("certificate"), "{refused}");
    }

    /// The absolute-`Location` hop, judged on the header the fixture actually sent rather than on
    /// a string this test retyped. The fixture answers 302 with `Location: http://127.0.0.1:<its own
    /// port>/inner`, which is loopback: `allow_private=true` reaches the fixture, and the hop the
    /// header names is then judged closed as refused and open as 127.0.0.1. One flag cannot both
    /// reach a loopback fixture and refuse a loopback hop, so the header is what is put on trial.
    #[test]
    fn a_302_to_an_absolute_private_location_is_judged_at_the_hop() {
        let fixture = site_owned(
            vec![
                owned(
                    "/out",
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{{PORT}}/inner\r\n\
                     Content-Length: 0\r\n\r\n"
                        .to_string(),
                ),
                owned(
                    "/inner",
                    "HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\ntunnelled".to_string(),
                ),
            ],
            &[],
        );
        let location = location_of(&raw_get(fixture.port, "/out"));
        assert_eq!(location, format!("http://127.0.0.1:{}/inner", fixture.port));
        let hop = Target::parse(&location).expect("the absolute Location parses");
        assert_eq!(hop.scheme, "http");
        assert_eq!(hop.port, fixture.port);
        assert_eq!(resolve(&hop, true), Ok(IpAddr::from([127, 0, 0, 1])));
        let closed = resolve(&hop, false);
        assert!(
            matches!(&closed, Err(said) if said.contains("private address")),
            "{closed:?}"
        );
        // It is already absolute: resolving it against the URL that carried it changes nothing.
        assert_eq!(resolve_location(&fixture.url("/out"), &location), location);
        // And followed end to end when the hole is open: the body is the tunnel's page.
        assert_eq!(run(&fixture.url("/out"), 12_000, true), "tunnelled\n");
    }

    /// Only 301, 302, 303, 307 and 308 are followed. Any other 3xx carrying a `Location` is a
    /// response whose body ends the call - the fixture's `/t300` and `/t399` say `plain` and that
    /// is what comes back, while the five followed codes reach `/inner`.
    #[test]
    fn only_the_five_followed_3xx_codes_redirect() {
        let mut answers = Vec::new();
        for code in [301u16, 302, 303, 307, 308] {
            answers.push(owned(
                &format!("/r{code}"),
                format!(
                    "HTTP/1.1 {code} Redirect\r\nLocation: /inner\r\nContent-Length: 0\r\n\r\n"
                ),
            ));
        }
        for code in [300u16, 399] {
            answers.push(owned(
                &format!("/t{code}"),
                format!(
                    "HTTP/1.1 {code} Not Followed\r\nLocation: /inner\r\nContent-Length: 5\r\n\r\nplain"
                ),
            ));
        }
        answers.push(owned(
            "/inner",
            "HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\ntunnelled".to_string(),
        ));
        let fixture = site_owned(answers, &[]);
        for code in [301u16, 302, 303, 307, 308] {
            assert_eq!(
                run(&fixture.url(&format!("/r{code}")), 12_000, true),
                "tunnelled\n",
                "{code} must be followed"
            );
        }
        for code in [300u16, 399] {
            assert_eq!(
                run(&fixture.url(&format!("/t{code}")), 12_000, true),
                "plain\n",
                "{code} must be terminal"
            );
        }
    }

    /// The bound is real in both walls: an oversized body is refused, and the refusal is about the
    /// cap rather than about curl's own error text. The fixture answers with a body four times the
    /// cap, announced in `Content-Length` so curl's trip is exit 63 and quick.
    #[test]
    fn a_body_past_the_cap_is_refused_and_not_read_into_the_process() {
        let big = (BODY_CAP * 4) as usize;
        let fixture = site_owned(
            vec![owned(
                "/",
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {big}\r\n\r\n{}",
                    "x".repeat(big)
                ),
            )],
            &[],
        );
        let said = run(&fixture.url("/"), 12_000, true);
        assert!(said.starts_with("refused:"), "{said}");
        assert!(said.contains("cap"), "{said}");
    }

    /// The other wall, on the parent's side: a bounded read of a body file. A file past the cap is
    /// read to exactly the cap; a file under it is read whole. No curl, no server - the shape the
    /// post-transfer check sits on.
    #[test]
    fn the_parent_read_is_bounded_by_the_cap() {
        let dir = std::env::temp_dir().join(format!("hades-fetch-read-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("body");
        std::fs::write(&path, vec![b'a'; BODY_CAP as usize + 4096]).expect("an oversized body");
        let bytes = read_capped(&path).expect("a bounded read");
        assert_eq!(bytes.len() as u64, READ_CAP);
        std::fs::write(&path, b"small").expect("a small body");
        let bytes = read_capped(&path).expect("a whole read");
        assert_eq!(bytes, b"small");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A loopback HTTPS fixture: `openssl` mints a self-signed certificate for `127.0.0.1` and
    /// `openssl s_server -WWW` serves the named files out of the fixture directory. The certificate
    /// is its own CA, so `curl --cacert cert.pem` trusts it and nothing else. `openssl` is a
    /// test-time prerequisite, named in the failure when it is missing.
    struct TlsSite {
        port: u16,
        dir: PathBuf,
        child: Option<std::process::Child>,
    }

    impl TlsSite {
        fn new(files: &[(&str, &str)]) -> TlsSite {
            let dir = std::env::temp_dir().join(format!(
                "hades-tls-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|since| since.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("a TLS fixture directory");
            for (name, body) in files {
                std::fs::write(dir.join(name), body).expect("a fixture file");
            }
            let key = dir.join("key.pem");
            let cert = dir.join("cert.pem");
            let minted = std::process::Command::new("openssl")
                .arg("req")
                .arg("-x509")
                .arg("-newkey")
                .arg("rsa:2048")
                .arg("-nodes")
                .arg("-keyout")
                .arg(&key)
                .arg("-out")
                .arg(&cert)
                .arg("-days")
                .arg("1")
                .arg("-subj")
                .arg("/CN=127.0.0.1")
                .arg("-addext")
                .arg("subjectAltName=IP:127.0.0.1")
                .arg("-addext")
                .arg("basicConstraints=critical,CA:TRUE")
                .output()
                .expect(
                    "openssl is a test prerequisite: the TLS fixture mints its certificate with it",
                );
            assert!(
                minted.status.success(),
                "openssl req failed: {}",
                String::from_utf8_lossy(&minted.stderr)
            );
            let port = free_port();
            let child = std::process::Command::new("openssl")
                .arg("s_server")
                .arg("-accept")
                .arg(format!("127.0.0.1:{port}"))
                .arg("-cert")
                .arg(&cert)
                .arg("-key")
                .arg(&key)
                .arg("-WWW")
                .arg("-quiet")
                .current_dir(&dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("openssl s_server is a test prerequisite");
            // Ready when the loopback port accepts. A fixture that never listens fails here, with
            // this message, rather than later inside curl's error text.
            let mut open = false;
            for _ in 0..100 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    open = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(open, "the TLS fixture never accepted on 127.0.0.1:{port}");
            TlsSite {
                port,
                dir,
                child: Some(child),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("https://127.0.0.1:{}{path}", self.port)
        }

        fn ca(&self) -> PathBuf {
            self.dir.join("cert.pem")
        }
    }

    impl Drop for TlsSite {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A loopback port no listener holds. Bind-then-drop is the smallest way to ask the kernel for
    /// one, and the tiny race with another process is the one every port-using fixture takes.
    fn free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        listener.local_addr().expect("an address").port()
    }

    /// The tools' fixture: a tiny HTTP server on `127.0.0.1:0`, one thread, answers by path. Kept
    /// deliberately at the std's surface - listener, read, write, drop - because the point of every
    /// test here is that fetch's policy is what answers, not the server's cleverness.
    ///
    /// A site owns its own thread. The listener is nonblocking and the loop polls a flag this site
    /// holds, so `Drop` clears the flag, joins *that* thread and no other, and the port is released
    /// before the test returns. Parallel tests never wait on each other's servers, and a test that
    /// ends leaves nothing running behind it.
    struct Site {
        port: u16,
        running: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Site {
        /// The URL for one path on this site: loopback as the host, the port that was bound.
        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        /// The same site named by host, which is the shape that needs a pin.
        fn named(&self, path: &str) -> String {
            format!("http://localhost:{}{path}", self.port)
        }
    }

    impl Drop for Site {
        fn drop(&mut self) {
            self.running.store(false, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn site(answers: &[(&[u8], &[u8])]) -> Site {
        let mut owned_answers: Vec<(String, Vec<u8>)> = Vec::new();
        for (path, reply) in answers {
            owned_answers.push((String::from_utf8_lossy(path).to_string(), reply.to_vec()));
        }
        // A `/loop-<n>` path answers with a redirect to `/loop-<n-1>`, down to 0, which answers a
        // page: a chain of any length, without hand-writing every hop.
        for depth in (1..12).rev() {
            owned_answers.push(owned(
                &format!("/loop-{depth}"),
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: /loop-{}\r\nContent-Length: 0\r\n\r\n",
                    depth - 1
                ),
            ));
        }
        owned_answers.push(owned(
            "/loop-0",
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nchain".to_string(),
        ));
        site_owned(owned_answers, &[])
    }

    /// `format!` an owned answer into the fixture's shape: dynamic paths and bodies go through
    /// here, spelled-out ones stay byte literals.
    fn owned(path: &str, reply: String) -> (String, Vec<u8>) {
        (path.to_string(), reply.into_bytes())
    }

    /// The `site` overload for owned answers, so a test writes its server as one list of pairs
    /// whatever shape its answers take.
    fn site_owned(
        owned_answers: Vec<(String, Vec<u8>)>,
        static_answers: &[(&[u8], &[u8])],
    ) -> Site {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        listener
            .set_nonblocking(true)
            .expect("a nonblocking fixture");
        let port_text = port.to_string();
        let mut addresses: Vec<(String, Arc<Vec<u8>>)> = owned_answers
            .into_iter()
            .map(|(path, reply)| (path, Arc::new(expand_port(&reply, &port_text))))
            .collect();
        for (path, reply) in static_answers {
            addresses.push((
                String::from_utf8_lossy(path).to_string(),
                Arc::new(expand_port(reply, &port_text)),
            ));
        }
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let handle = serve_until(flag, listener, addresses);
        Site {
            port,
            running,
            handle: Some(handle),
        }
    }

    /// A fixture that handles each connection on its own thread and waits before answering, so a test
    /// can tell whether two fetches overlapped. The ordinary `Site` answers one connection at a time,
    /// which would make a timing test measure the server rather than the pool.
    struct SlowSite {
        port: u16,
        running: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl SlowSite {
        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }
    }

    impl Drop for SlowSite {
        fn drop(&mut self) {
            self.running.store(false, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn slow_site(delay: std::time::Duration, body: &'static str) -> SlowSite {
        use std::io::{Read as _, Write as _};
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        listener
            .set_nonblocking(true)
            .expect("a nonblocking fixture");
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let handle = std::thread::spawn(move || {
            while flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        std::thread::spawn(move || {
                            let _ = stream.set_nonblocking(false);
                            let _ =
                                stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                            let mut buffer = [0u8; 2048];
                            let _ = stream.read(&mut buffer);
                            std::thread::sleep(delay);
                            let reply = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(reply.as_bytes());
                            let _ = stream.flush();
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        SlowSite {
            port,
            running,
            handle: Some(handle),
        }
    }

    /// Replace the fixture's `{{PORT}}` token with the port the listener was bound to, so an answer
    /// can name this site's own address - the shape an absolute `Location` needs, where the port is
    /// unknown until the listener exists.
    fn expand_port(reply: &[u8], port: &str) -> Vec<u8> {
        let token = b"{{PORT}}";
        let mut out = Vec::with_capacity(reply.len());
        let mut rest = reply;
        while let Some(at) = rest.windows(token.len()).position(|window| window == token) {
            out.extend_from_slice(&rest[..at]);
            out.extend_from_slice(port.as_bytes());
            rest = &rest[at + token.len()..];
        }
        out.extend_from_slice(rest);
        out
    }

    /// Talk to a fixture over a socket and hand back its whole answer, so a test can assert on the
    /// header the fixture actually sent rather than on the string the test meant to send.
    fn raw_get(port: u16, path: &str) -> String {
        use std::io::{Read as _, Write as _};
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", port)).expect("the fixture answers");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("a read timeout");
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("a write timeout");
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )
        .expect("a request");
        stream.flush().expect("a flushed request");
        let mut text = String::new();
        stream
            .read_to_string(&mut text)
            .expect("the fixture's answer");
        text
    }

    /// The one `Location` header out of a raw response.
    fn location_of(response: &str) -> String {
        response
            .lines()
            .find_map(|line| line.strip_prefix("Location:"))
            .expect("a Location header")
            .trim()
            .to_string()
    }

    /// The accept loop, split out so both `site` builders hand the same body their address list.
    /// The poll is 20ms, the same shape `browser.rs`'s proxy fixture uses: short enough that a
    /// dropped site is released before the test returns, long enough not to spin.
    fn serve_until(
        flag: Arc<AtomicBool>,
        listener: TcpListener,
        addresses: Vec<(String, Arc<Vec<u8>>)>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while flag.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    }
                    Err(_) => break,
                };
                let _ = stream.set_nonblocking(false);
                // A client that goes away mid-request or mid-response must not hold this thread
                // past a timeout: the site's `Drop` joins it.
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                // Read to the end of the headers, not just to the first newline: closing a socket
                // with bytes still queued on its receive side answers with an RST, and an RST can
                // throw away a response the client has not read yet.
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            request.extend_from_slice(&buffer[..read]);
                            if request.windows(4).any(|window| window == b"\r\n\r\n")
                                || request.len() > 8192
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let line = String::from_utf8_lossy(&request);
                // The request line is all the routing the fixture needs: `GET /path HTTP/1.1`,
                // and the path is the second word.
                let mut words = line.lines().next().unwrap_or("").split_ascii_whitespace();
                let path = match (words.next(), words.next()) {
                    (Some("GET"), Some(path)) => path.to_string(),
                    _ => String::new(),
                };
                let reply = addresses
                    .iter()
                    .find(|(known, _)| known == &path)
                    .map(|(_, reply)| reply.as_slice())
                    .unwrap_or(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                let _ = stream.write_all(reply);
                let _ = stream.flush();
            }
        })
    }
}
