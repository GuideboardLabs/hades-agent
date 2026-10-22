//! A browser the harness owns.
//!
//! This is not Playwright in Rust. It is a Chrome DevTools Protocol client over the pipe Chromium already
//! exposes (`--remote-debugging-pipe`): JSON messages separated by a NUL byte on fds 3 and 4. That keeps
//! the dependency list at std plus serde_json, and it lets Hades launch the same patched Chromium the old
//! Python helper used, with the same fingerprint flags.
//!
//! The surface is deliberately agentic rather than a Playwright translation:
//!
//! - `observe` returns a compact page graph, not the raw DOM.
//! - `click`, `type` and `press` return a fresh observation with the outcome, so the model sees what its
//!   action changed without a second round trip.
//! - `wait` waits for a condition with a deadline, instead of sleeping and hoping.
//! - `eval` remains an escape hatch, and `screenshot` is for when text is not enough.
//!
//! One browser process per state directory, shared for the life of the harness. A call that fails kills
//! that state's process; the next call starts a clean one. A state's session sits unused for fifteen
//! minutes and it is closed on the next call, so an abandoned tab does not hold a profile lock for the
//! lifetime of the process. The profile under `<state>/browser/profile` survives, so cookies and logins do.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// How long a state's browser may sit unused before it is closed on the next call. A session holds a child
/// process, a profile lock and a pipe, so an abandoned one is not free.
const IDLE: Duration = Duration::from_secs(15 * 60);

/// One live session per state directory, keyed by the resolved path.
///
/// A single global session meant two state directories shared tabs, cookies and downloads: the second
/// caller silently drove the first caller's page. The key is what keeps a test, a daemon and a replay from
/// colliding without anyone having to remember they must not.
struct Held {
    session: Session,
    last_used: Instant,
}

static SESSIONS: Mutex<BTreeMap<PathBuf, Held>> = Mutex::new(BTreeMap::new());

/// The key a state directory is held under: the resolved path when it exists, the path as given when it
/// does not, so two spellings of one directory never become two sessions.
fn session_key(state: &Path) -> PathBuf {
    std::fs::canonicalize(state).unwrap_or_else(|_| state.to_path_buf())
}

/// Whether a session has been unused long enough to close.
fn stale(now: Instant, last_used: Instant) -> bool {
    now.saturating_duration_since(last_used) >= IDLE
}

/// Which URL schemes this browser session may navigate to. Production denies `file://`; a fixture session
/// that loads a local page opts in. The policy lives on the session, not in a `cfg(test)` constructor,
/// because the fixture tests drive the same `act` -> `open` path the model uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserPolicy {
    pub allow_local: bool,
}

impl BrowserPolicy {
    /// What a session gets unless a caller says otherwise.
    pub fn deny() -> BrowserPolicy {
        BrowserPolicy { allow_local: false }
    }

    /// A session that may load `file://` pages. Only tests and fixtures have a reason to ask.
    pub fn permit_local() -> BrowserPolicy {
        BrowserPolicy { allow_local: true }
    }

    /// The whole rule. Called before `Page.navigate`, and again on the current tab after any navigation
    /// (a click, a tab switch, or a redirect) because those never pass through `open`.
    pub fn check(&self, url: &str) -> Result<(), String> {
        let url = url.trim();
        // An about-to-be-navigated tab has no URL yet. That is not a scheme, and refusing it would turn
        // startup into a failure.
        if url.is_empty() {
            return Ok(());
        }
        let scheme = url
            .split_once(':')
            .map(|(scheme, _)| scheme.to_ascii_lowercase());
        match scheme.as_deref() {
            Some("http") | Some("https") => Ok(()),
            Some("about") if blank_page(url) => Ok(()),
            Some("file") if self.allow_local => Ok(()),
            Some("file") => Err(
                "the browser is not allowed to read local files: `file://` is refused".to_string(),
            ),
            Some(other) => Err(format!(
                "the browser refuses the `{other}:` scheme: only http, https and about:blank are allowed"
            )),
            None => Err(format!("`{url}` is not an absolute URL with a scheme")),
        }
    }
}

/// The blank page, with or without the fragment or query a script puts on it.
///
/// A page that sets `location.hash` is still the page Chromium opened, and refusing it tore the whole
/// browser down mid-task. A prefix test alone would accept `about:blankety`, so the character after the
/// name has to end the URL or start a fragment or query.
fn blank_page(url: &str) -> bool {
    const BLANK: &str = "about:blank";
    // `get`, not a slice: a URL whose prefix splits a multi-byte character must be refused, not panic.
    let named = url
        .get(..BLANK.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(BLANK));
    named
        && matches!(
            url.as_bytes().get(BLANK.len()),
            None | Some(b'#') | Some(b'?')
        )
}

#[cfg(test)]
mod policy {
    use super::BrowserPolicy;

    #[test]
    fn http_https_and_a_blank_page_are_allowed() {
        let policy = BrowserPolicy::deny();
        for url in [
            "https://example.com",
            "http://127.0.0.1:8000/x?y=1",
            "about:blank",
            "about:blank#step2",
            "about:blank?x=1",
            "  https://example.com  ",
        ] {
            assert!(policy.check(url).is_ok(), "{url}");
        }
        // A tab that has not navigated yet has no URL. Startup must not be a refusal.
        assert!(policy.check("").is_ok());
    }

    #[test]
    fn a_local_file_is_refused_unless_the_session_opts_in() {
        let deny = BrowserPolicy::deny();
        let error = deny.check("file:///etc/passwd").expect_err("a refusal");
        assert!(error.contains("file://"), "{error}");
        assert!(
            BrowserPolicy::permit_local()
                .check("file:///tmp/page.html")
                .is_ok()
        );
        // The opt-in is for file only. It does not become a general permission.
        assert!(
            BrowserPolicy::permit_local()
                .check("data:text/html,x")
                .is_err()
        );
    }

    #[test]
    fn data_and_script_schemes_are_refused() {
        for url in [
            "data:text/html,<h1>x</h1>",
            "javascript:alert(1)",
            "vbscript:msgbox(1)",
            "chrome://settings",
            "ftp://example.com/file",
        ] {
            let error = BrowserPolicy::deny().check(url).expect_err(url);
            assert!(error.contains("scheme"), "{url}: {error}");
        }
    }

    #[test]
    fn a_relative_url_is_not_a_navigation() {
        assert!(BrowserPolicy::deny().check("/etc/passwd").is_err());
        assert!(BrowserPolicy::deny().check("example.com").is_err());
    }
}

const MAX_TEXT: usize = 6000;
const MAX_ITEMS: usize = 60;
const MAX_HEADINGS: usize = 10;
const MAX_EVAL: usize = 6000;
const MAX_FULL: usize = 100_000;

/// The tool entry point. A failure kills the process so the next call starts from a known state.
pub fn tool(args: &Value, state: &Path) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    if action.is_empty() {
        return "no action given".to_string();
    }
    let key = session_key(state);
    let mut held = match SESSIONS.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };

    // Sweep before use, not on a timer: a session that is closed here is one whose next call has not come
    // yet, and a call is the only moment this process knows it is still alive.
    let now = Instant::now();
    let idle: Vec<PathBuf> = held
        .iter()
        .filter(|(_, session)| stale(now, session.last_used))
        .map(|(key, _)| key.clone())
        .collect();
    for key in idle {
        if let Some(mut session) = held.remove(&key) {
            session.session.close();
        }
    }

    if action == "close" {
        if let Some(mut session) = held.remove(&key) {
            session.session.close();
        }
        return "browser closed".to_string();
    }

    if !held.contains_key(&key) {
        match Session::start(state) {
            Ok(session) => {
                held.insert(
                    key.clone(),
                    Held {
                        session,
                        last_used: now,
                    },
                );
            }
            Err(said) => return said,
        }
    }
    let result = match held.get_mut(&key) {
        Some(held) => held.session.act(action, args),
        None => Err("the browser did not start".to_string()),
    };
    match result {
        Ok(said) => {
            if let Some(held) = held.get_mut(&key) {
                held.last_used = Instant::now();
            }
            said
        }
        Err(said) => {
            // The pipe or the page is no longer trustworthy. Drop this state's child only; another state's
            // session has nothing to do with this failure.
            if let Some(mut held) = held.remove(&key) {
                held.session.close();
            }
            format!("the browser failed: {said}")
        }
    }
}

struct NetworkEntry {
    request_id: String,
    method: String,
    url: String,
    resource_type: String,
    status: Option<i64>,
    mime: String,
    error: String,
}

struct FrameRow {
    id: String,
    url: String,
    name: String,
    depth: usize,
}

struct Dialog {
    kind: String,
    message: String,
}

struct Tab {
    target_id: String,
    session_id: String,
    title: String,
    url: String,
    context_id: String,
}

struct Session {
    child: Child,
    to: UnixStream,
    from: BufReader<UnixStream>,
    id: u64,
    session_id: String,
    target_id: String,
    tabs: Vec<Tab>,
    current: usize,
    state: PathBuf,
    refs: HashMap<String, (String, i64)>,
    ref_by_backend: HashMap<(String, i64), String>,
    frame_id: String,
    frame_contexts: HashMap<String, i64>,
    frame_sessions: HashMap<String, String>,
    oopif_targets: std::collections::HashSet<String>,
    default_context: String,
    proxy_auth: Option<(String, String)>,
    ref_labels: HashMap<String, String>,
    next_ref: u64,
    closed_tabs: std::collections::HashSet<String>,
    dialog: Option<Dialog>,
    file_chooser: Option<String>,
    network: Vec<NetworkEntry>,
    blocked: Vec<String>,
    humanize: bool,
    dialog_accept: bool,
    downloads: PathBuf,
    events: VecDeque<Value>,
    /// The schemes this session may navigate to. Checked in `open` and after every navigation.
    policy: BrowserPolicy,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The three directories a browser run owns. All of them live under `<state>/browser`, and that is checked
/// rather than assumed: a symlink where the directory should be would otherwise put the profile, the
/// downloads and Chromium's temp files wherever the link pointed.
#[derive(Debug)]
struct StatePaths {
    profile: PathBuf,
    downloads: PathBuf,
    tmp: PathBuf,
}

fn state_paths(state: &Path) -> Result<StatePaths, String> {
    let root = state.join("browser");
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("could not make {}: {error}", root.display()))?;
    let root = std::fs::canonicalize(&root)
        .map_err(|error| format!("could not resolve {}: {error}", root.display()))?;
    let under = |name: &str| -> Result<PathBuf, String> {
        let path = root.join(name);
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("could not make {}: {error}", path.display()))?;
        let resolved = std::fs::canonicalize(&path)
            .map_err(|error| format!("could not resolve {}: {error}", path.display()))?;
        if !resolved.starts_with(&root) {
            return Err(format!(
                "{} resolves to {}, which is outside {}",
                path.display(),
                resolved.display(),
                root.display()
            ));
        }
        Ok(resolved)
    };
    Ok(StatePaths {
        profile: under("profile")?,
        downloads: under("downloads")?,
        tmp: under("tmp")?,
    })
}

impl Session {
    fn start(state: &Path) -> Result<Self, String> {
        Session::start_with(state, BrowserPolicy::deny())
    }

    fn start_with(state: &Path, policy: BrowserPolicy) -> Result<Self, String> {
        let binary = find_chrome()?;
        // Chromium's process singleton creates a socket in its TMPDIR and aborts with "Failed to create
        // socket directory" when that directory does not exist, so the paths are made here and the
        // variable is set below. The paths are resolved and checked under `<state>/browser` first.
        let StatePaths {
            profile: state_profile,
            downloads,
            tmp: state_tmp,
        } = state_paths(state)?;
        let (profile, tmp) = (state_profile, state_tmp);
        chromium_path_check(&tmp)?;

        let (to_child, child_read) = UnixStream::pair()
            .map_err(|error| format!("could not make the command pipe: {error}"))?;
        let (child_write, from_child) = UnixStream::pair()
            .map_err(|error| format!("could not make the reply pipe: {error}"))?;
        from_child
            .set_read_timeout(Some(Duration::from_secs(20)))
            .map_err(|error| format!("could not set the browser timeout: {error}"))?;

        let child_read_fd = child_read.as_raw_fd();
        let child_write_fd = child_write.as_raw_fd();
        let mut command = Command::new(&binary);
        let headed = std::env::var("HADES_HEADED")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let mut launch = launch_args(headed, no_sandbox_requested());
        let mut proxy_auth = None;
        if let Ok(proxy) = std::env::var("HADES_PROXY")
            && !proxy.is_empty()
        {
            let (proxy_url, auth) = split_proxy(&proxy);
            proxy_auth = auth;
            launch.push(format!("--proxy-server={proxy_url}"));
        }
        if let Ok(bypass) = std::env::var("HADES_PROXY_BYPASS")
            && !bypass.is_empty()
        {
            launch.push(format!("--proxy-bypass-list={bypass}"));
        }
        if let Ok(agent) = std::env::var("HADES_USER_AGENT")
            && !agent.is_empty()
        {
            launch.push(format!("--user-agent={agent}"));
        }
        command
            .args(&launch)
            .arg(format!("--user-data-dir={}", profile.display()));
        // A minimal environment, because Chromium inherits whatever the harness was started with. It needs
        // a home for its profile and a PATH to find helpers; it has no reason to see a provider key or a
        // HADES_* variable. HOME points at the browser's own directory, so a page cannot read the harness'
        // config through the default home path even if a scheme policy is ever loosened.
        command.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command
            .env("HOME", state.join("browser"))
            .env("TMPDIR", &tmp)
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8");
        if headed && let Some(display) = std::env::var_os("DISPLAY") {
            command.env("DISPLAY", display);
        }
        // The patched binary reads its fingerprint from the seed and the platform flag. A stock Chrome
        // ignores both arguments but still gives the same CDP surface.
        if is_patched(&binary) {
            command
                .arg(format!("--fingerprint={}", seed()))
                .arg("--fingerprint-platform=windows");
            if let Ok(ip) = std::env::var("HADES_WEBRTC_IP")
                && !ip.is_empty()
            {
                command.arg(format!("--fingerprint-webrtc-ip={ip}"));
            }
        }
        // Chromium's own words are the only account of a launch that fails before the first CDP reply, and
        // discarding them turned a named error into "the browser closed the pipe". The log is always kept,
        // truncated on each launch, because the launch that fails is the one whose words matter.
        let log = state.join("browser").join("chromium.log");
        let stderr = match std::fs::File::create(&log) {
            Ok(file) => Stdio::from(file),
            Err(_) => Stdio::null(),
        };
        // Operator diagnostics only: extra Chromium flags for a host whose launch fails and whose log is not
        // enough. Never set by the model, and never persisted.
        if let Ok(extra) = std::env::var("HADES_CHROME_EXTRA") {
            for flag in extra.split_whitespace() {
                command.arg(flag);
            }
        }
        command
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        unsafe {
            command.pre_exec(move || {
                // `dup2` onto the same descriptor is a no-op, and the pair was created close-on-exec: a pipe
                // whose end already sat on 3 or 4 would stay close-on-exec and be gone by the time Chromium
                // looked for it. Clearing the flag is what makes "the descriptor is on 3" true after exec.
                if dup2(child_read_fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if fcntl(3, F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if dup2(child_write_fd, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if fcntl(4, F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        if std::env::var("HADES_BROWSER_DEBUG").as_deref() == Ok("1") {
            eprintln!(
                "browser: {} {:?} (fds {child_read_fd}->3, {child_write_fd}->4)",
                binary.display(),
                launch
            );
        }
        let child = command
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", binary.display()))?;
        drop(child_read);
        drop(child_write);

        let mut session = Session {
            child,
            to: to_child,
            from: BufReader::new(from_child),
            id: 0,
            session_id: String::new(),
            target_id: String::new(),
            tabs: Vec::new(),
            current: 0,
            state: state.to_path_buf(),
            refs: HashMap::new(),
            ref_by_backend: HashMap::new(),
            frame_id: String::new(),
            frame_contexts: HashMap::new(),
            frame_sessions: HashMap::new(),
            oopif_targets: std::collections::HashSet::new(),
            default_context: String::new(),
            proxy_auth: None,
            ref_labels: HashMap::new(),
            next_ref: 0,
            closed_tabs: std::collections::HashSet::new(),
            dialog: None,
            file_chooser: None,
            network: Vec::new(),
            blocked: Vec::new(),
            humanize: std::env::var("HADES_HUMANIZE")
                .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            dialog_accept: std::env::var("HADES_DIALOG_ACCEPT")
                .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            downloads: downloads.clone(),
            events: VecDeque::new(),
            policy,
        };

        let version = match session.call("Browser.getVersion", json!({}), None) {
            Ok(version) => version,
            Err(said) => {
                // A launch that died before the first reply has a reason, and it is in the log. "The browser
                // closed the pipe" is the symptom; the sandbox, a missing library or a locked profile is the
                // cause, and an operator cannot act on the symptom.
                let complaint = chromium_complaint(state);
                let status = match session.child.try_wait() {
                    Ok(Some(status)) => Some(format!("exit {status}")),
                    _ => None,
                };
                session.close();
                return Err(match (complaint, status) {
                    (Some(complaint), Some(status)) => format!("{said} ({status}): {complaint}"),
                    (Some(complaint), None) => format!("{said}: {complaint}"),
                    (None, Some(status)) => format!("{said} ({status})"),
                    (None, None) => said,
                });
            }
        };
        let product = version
            .get("product")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let _ = product;

        let listed = session.call("Target.getTargets", json!({}), None)?;
        let existing = listed
            .get("targetInfos")
            .and_then(Value::as_array)
            .and_then(|infos| {
                infos
                    .iter()
                    .find(|info| info.get("type").and_then(Value::as_str) == Some("page"))
            })
            .map(|info| {
                (
                    info.get("targetId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    info.get("browserContextId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                )
            });
        let (target, default_context) = match existing {
            Some((target, context)) => (target, context),
            None => {
                let created =
                    session.call("Target.createTarget", json!({"url": "about:blank"}), None)?;
                (
                    created
                        .get("targetId")
                        .and_then(Value::as_str)
                        .ok_or("the browser made no target")?
                        .to_string(),
                    String::new(),
                )
            }
        };
        session.default_context = default_context;
        let attached = session.call(
            "Target.attachToTarget",
            json!({"targetId": target, "flatten": true}),
            None,
        )?;
        session.session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("the browser made no session")?
            .to_string();
        session.target_id = target.clone();
        session.tabs.push(Tab {
            target_id: target,
            session_id: session.session_id.clone(),
            title: String::new(),
            url: "about:blank".to_string(),
            context_id: session.default_context.clone(),
        });
        session.current = 0;
        session.proxy_auth = proxy_auth;
        session.enable_page()?;
        if session.proxy_auth.is_some() {
            let _ = session.page_call(
                "Fetch.enable",
                json!({
                    "patterns": [{"urlPattern": "*", "requestStage": "Request"}],
                    "handleAuthRequests": true
                }),
            );
        }
        session.call(
            "Browser.setDownloadBehavior",
            json!({
                "behavior": "allow",
                "downloadPath": session.downloads.display().to_string(),
                "eventsEnabled": true
            }),
            None,
        )?;
        Ok(session)
    }

    fn page_call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let session = self.session_id.clone();
        self.call(method, params, Some(&session))
    }

    fn enable_page(&mut self) -> Result<(), String> {
        self.page_call("Page.enable", json!({}))?;
        self.page_call("Runtime.enable", json!({}))?;
        self.page_call("DOM.enable", json!({}))?;
        self.page_call("Accessibility.enable", json!({}))?;
        self.page_call("Network.enable", json!({}))?;
        self.page_call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": 1440, "height": 900, "deviceScaleFactor": 1, "mobile": false}),
        )?;
        if let Ok(timezone) = std::env::var("HADES_TIMEZONE")
            && !timezone.is_empty()
        {
            let _ = self.page_call(
                "Emulation.setTimezoneOverride",
                json!({"timezoneId": timezone}),
            );
        }
        if let Ok(locale) = std::env::var("HADES_LOCALE")
            && !locale.is_empty()
        {
            let _ = self.page_call("Emulation.setLocaleOverride", json!({"locale": locale}));
        }
        let _ = self.page_call(
            "Page.setInterceptFileChooserDialog",
            json!({"enabled": true}),
        );
        Ok(())
    }

    fn refresh_tabs(&mut self) -> Result<(), String> {
        let listed = self.call("Target.getTargets", json!({}), None)?;
        let infos = listed
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut tabs = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for old in &self.tabs {
            if self.closed_tabs.contains(&old.target_id) {
                continue;
            }
            let info = infos.iter().find(|info| {
                info.get("targetId").and_then(Value::as_str) == Some(old.target_id.as_str())
            });
            let info = match info {
                Some(info) => info,
                None => continue,
            };
            if info.get("type").and_then(Value::as_str) != Some("page") {
                continue;
            }
            seen.insert(old.target_id.clone());
            tabs.push(Tab {
                target_id: old.target_id.clone(),
                session_id: old.session_id.clone(),
                title: info
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|title| !title.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| old.title.clone()),
                url: info
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|url| !url.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| old.url.clone()),
                context_id: info
                    .get("browserContextId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some(old.context_id.clone()))
                    .unwrap_or_default(),
            });
        }
        for info in &infos {
            if info.get("type").and_then(Value::as_str) != Some("page") {
                continue;
            }
            let target_id = match info.get("targetId").and_then(Value::as_str) {
                Some(target_id) if !seen.contains(target_id) => target_id.to_string(),
                _ => continue,
            };
            if self.closed_tabs.contains(&target_id) {
                continue;
            }
            seen.insert(target_id.clone());
            tabs.push(Tab {
                target_id,
                session_id: String::new(),
                title: info
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                url: info
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                context_id: info
                    .get("browserContextId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
        if tabs.is_empty() {
            tabs.push(Tab {
                target_id: self.target_id.clone(),
                session_id: self.session_id.clone(),
                title: String::new(),
                url: String::new(),
                context_id: String::new(),
            });
        }
        self.current = tabs
            .iter()
            .position(|tab| tab.target_id == self.target_id)
            .unwrap_or(0);
        self.tabs = tabs;
        if self.tabs[self.current].session_id.is_empty() {
            self.attach_current()?;
        }
        // A click, a tab switch or a redirect all change the URL without calling `open`. The check runs
        // here, on every refresh, so those paths are covered by the same rule.
        self.guard_current()?;
        Ok(())
    }

    /// The scheme of the tab the harness would act on, checked against the session's policy.
    fn guard_current(&self) -> Result<(), String> {
        let tab = self
            .tabs
            .get(self.current)
            .ok_or_else(|| "the browser has no current tab".to_string())?;
        self.policy.check(&tab.url)
    }

    fn attach_current(&mut self) -> Result<(), String> {
        let target = self.tabs[self.current].target_id.clone();
        let attached = self.call(
            "Target.attachToTarget",
            json!({"targetId": target, "flatten": true}),
            None,
        )?;
        let session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("the browser made no session for that tab")?
            .to_string();
        self.tabs[self.current].session_id = session_id.clone();
        self.session_id = session_id;
        self.target_id = target;
        self.enable_page()?;
        self.clear_frame();
        Ok(())
    }

    fn switch_to(&mut self, index: usize) -> Result<String, String> {
        self.refresh_tabs()?;
        if index >= self.tabs.len() {
            return Err(format!(
                "there is no tab {index}; there are {} tabs",
                self.tabs.len()
            ));
        }
        self.current = index;
        self.session_id = self.tabs[index].session_id.clone();
        self.target_id = self.tabs[index].target_id.clone();
        if self.session_id.is_empty() {
            self.attach_current()?;
        }
        self.clear_frame();
        self.settle(2);
        self.observe()
    }

    fn new_tab(&mut self, url: &str, context: &str) -> Result<String, String> {
        let mut parameters = json!({"url": if url.is_empty() { "about:blank" } else { url }});
        if !context.is_empty() {
            parameters["browserContextId"] = json!(context);
        }
        let created = self.call("Target.createTarget", parameters, None)?;
        let target = created
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or("the browser made no target")?
            .to_string();
        let attached = self.call(
            "Target.attachToTarget",
            json!({"targetId": target, "flatten": true}),
            None,
        )?;
        let session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("the browser made no session for the new tab")?
            .to_string();
        self.tabs.push(Tab {
            target_id: target.clone(),
            session_id: session_id.clone(),
            title: String::new(),
            url: if url.is_empty() { "about:blank" } else { url }.to_string(),
            context_id: context.to_string(),
        });
        self.current = self.tabs.len() - 1;
        self.session_id = session_id;
        self.target_id = target;
        self.clear_frame();
        self.enable_page()?;
        self.settle(2);
        self.observe()
    }

    fn contexts_text(&mut self) -> Result<String, String> {
        self.refresh_tabs()?;
        let listed = self.call("Target.getBrowserContexts", json!({}), None)?;
        let ids = listed
            .get("browserContextIds")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let default_tabs = self
            .tabs
            .iter()
            .filter(|tab| tab.context_id.is_empty() || tab.context_id == self.default_context)
            .count();
        let mut said = format!("contexts:\n  default: {default_tabs} tabs\n");
        for id in ids {
            let Some(id) = id.as_str() else { continue };
            let count = self.tabs.iter().filter(|tab| tab.context_id == id).count();
            said.push_str(&format!("  {id}: {count} tabs\n"));
        }
        Ok(said)
    }

    fn new_context(&mut self, url: &str) -> Result<String, String> {
        let created = self.call("Target.createBrowserContext", json!({}), None)?;
        let context = created
            .get("browserContextId")
            .and_then(Value::as_str)
            .ok_or("the browser made no context")?
            .to_string();
        let _ = self.call(
            "Browser.setDownloadBehavior",
            json!({
                "behavior": "allow",
                "downloadPath": self.downloads.display().to_string(),
                "eventsEnabled": true,
                "browserContextId": context
            }),
            None,
        );
        self.new_tab(url, &context)
    }

    fn close_context(&mut self, context: &str) -> Result<String, String> {
        if context.is_empty() || context == "default" || context == self.default_context {
            return Err("the default context cannot close".to_string());
        }
        self.refresh_tabs()?;
        let targets: Vec<String> = self
            .tabs
            .iter()
            .filter(|tab| tab.context_id == context)
            .map(|tab| tab.target_id.clone())
            .collect();
        if targets.is_empty() {
            return Err(format!("there is no context {context}"));
        }
        let current_closed = self
            .tabs
            .iter()
            .any(|tab| tab.context_id == context && tab.target_id == self.target_id);
        for target in &targets {
            self.closed_tabs.insert(target.clone());
            let _ = self.call("Target.closeTarget", json!({"targetId": target}), None);
        }
        let _ = self.call(
            "Target.disposeBrowserContext",
            json!({"browserContextId": context}),
            None,
        );
        self.tabs.retain(|tab| tab.context_id != context);
        if current_closed {
            if self.tabs.is_empty() {
                return self.new_tab("about:blank", "");
            }
            self.current = self.tabs.len() - 1;
            self.session_id = self.tabs[self.current].session_id.clone();
            self.target_id = self.tabs[self.current].target_id.clone();
            if self.session_id.is_empty() {
                self.attach_current()?;
            }
            self.clear_frame();
            self.settle(2);
            self.observe()
        } else {
            Ok(format!("context closed: {context}"))
        }
    }

    fn close_tab(&mut self, index: Option<usize>) -> Result<String, String> {
        self.refresh_tabs()?;
        let index = index.unwrap_or(self.current);
        if index >= self.tabs.len() {
            return Err(format!(
                "there is no tab {index}; there are {} tabs",
                self.tabs.len()
            ));
        }
        if self.tabs.len() == 1 {
            return Err("the last tab cannot close; use close to end the browser".to_string());
        }
        let target = self.tabs[index].target_id.clone();
        self.closed_tabs.insert(target.clone());
        let _ = self.call("Target.closeTarget", json!({"targetId": target}), None);
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(50));
            let listed = self.call("Target.getTargets", json!({}), None)?;
            let still_there = listed
                .get("targetInfos")
                .and_then(Value::as_array)
                .map(|infos| {
                    infos.iter().any(|info| {
                        info.get("targetId").and_then(Value::as_str) == Some(target.as_str())
                    })
                })
                .unwrap_or(false);
            if !still_there {
                break;
            }
        }
        let was_current = index == self.current;
        self.tabs.remove(index);
        if was_current {
            self.current = index.min(self.tabs.len() - 1);
            self.session_id = self.tabs[self.current].session_id.clone();
            self.target_id = self.tabs[self.current].target_id.clone();
            if self.session_id.is_empty() {
                self.attach_current()?;
            }
            self.clear_frame();
            self.settle(2);
            self.observe()
        } else {
            if index < self.current {
                self.current -= 1;
            }
            Ok(format!("closed tab {index}"))
        }
    }

    fn downloads_text(&self) -> String {
        let mut entries = Vec::new();
        if let Ok(listed) = std::fs::read_dir(&self.downloads) {
            for entry in listed.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let meta = match entry.metadata() {
                    Ok(meta) => meta,
                    Err(_) => continue,
                };
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|age| age.as_secs())
                    .unwrap_or(0);
                entries.push((modified, meta.len(), path));
            }
        }
        entries.sort_by_key(|(modified, _, _)| std::cmp::Reverse(*modified));
        if entries.is_empty() {
            return format!("downloads: none in {}", self.downloads.display());
        }
        let mut said = format!("downloads: {}\n", self.downloads.display());
        for (modified, size, path) in entries.iter().take(20) {
            said.push_str(&format!(
                "  {} ({size} bytes, {modified})\n",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unnamed")
            ));
        }
        said
    }

    fn frames(&mut self) -> Result<Vec<FrameRow>, String> {
        let tree = self.page_call("Page.getFrameTree", json!({}))?;
        let mut frames = Vec::new();
        if let Some(root) = tree.get("frameTree") {
            collect_frames(root, 0, &mut frames);
        }
        if let Ok(listed) = self.call("Target.getTargets", json!({}), None)
            && let Some(infos) = listed.get("targetInfos").and_then(Value::as_array)
        {
            for info in infos {
                if info.get("type").and_then(Value::as_str) != Some("iframe") {
                    continue;
                }
                let Some(id) = info.get("targetId").and_then(Value::as_str) else {
                    continue;
                };
                if frames.iter().any(|frame| frame.id == id) {
                    continue;
                }
                frames.push(FrameRow {
                    id: id.to_string(),
                    url: info
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    name: info
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    depth: 1,
                });
            }
        }
        Ok(frames)
    }

    fn frames_text(&mut self) -> Result<String, String> {
        let frames = self.frames()?;
        let mut said = String::from("frames:\n");
        for (index, frame) in frames.iter().enumerate() {
            let marker = if (self.frame_id.is_empty() && index == 0) || self.frame_id == frame.id {
                " current"
            } else {
                ""
            };
            said.push_str(&format!(
                "  [{index}]{marker} {}{} | {}",
                "  ".repeat(frame.depth),
                if frame.name.is_empty() {
                    "unnamed"
                } else {
                    &frame.name
                },
                frame.url
            ));
            said.push_str(&format!(" (id {})\n", frame.id));
        }
        Ok(said)
    }

    fn select_frame(&mut self, args: &Value) -> Result<String, String> {
        let frames = self.frames()?;
        let id = if let Some(id) = args.get("frame").and_then(Value::as_str) {
            if id == "main" {
                String::new()
            } else {
                id.to_string()
            }
        } else if let Some(index) = args.get("index").and_then(Value::as_u64) {
            frames
                .get(index as usize)
                .map(|frame| frame.id.clone())
                .ok_or_else(|| format!("there is no frame {index}"))?
        } else {
            return Err("frame needs a frame id or an index from `frames`".to_string());
        };
        if !id.is_empty() && !frames.iter().any(|frame| frame.id == id) {
            return Err(format!("there is no frame {id}"));
        }
        self.frame_id = id;
        self.clear_refs();
        match self.observe() {
            Ok(page) => Ok(page),
            Err(said) => {
                self.frame_id.clear();
                self.clear_refs();
                match self.observe() {
                    Ok(page) => Ok(format!("could not enter that frame: {said}\n\n{page}")),
                    Err(_) => Err(said),
                }
            }
        }
    }

    fn network_text(&self) -> String {
        if self.network.is_empty() {
            return "network: no requests since the browser started".to_string();
        }
        let start = self.network.len().saturating_sub(50);
        let mut said = format!(
            "network: {} requests, showing {}\n",
            self.network.len(),
            self.network.len() - start
        );
        for entry in &self.network[start..] {
            let status = entry
                .status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "---".to_string());
            let error = if entry.error.is_empty() {
                String::new()
            } else {
                format!(" ({})", entry.error)
            };
            said.push_str(&format!(
                "  {} {} {} {} {}{}\n",
                entry.request_id,
                entry.method,
                status,
                entry.resource_type,
                clip(&entry.url, 180),
                error
            ));
        }
        if !self.blocked.is_empty() {
            said.push_str(&format!("blocked: {}\n", self.blocked.join(", ")));
        }
        said
    }

    fn network_body(&mut self, request_id: &str, full: bool) -> Result<String, String> {
        let response =
            self.page_call("Network.getResponseBody", json!({"requestId": request_id}))?;
        let body = response
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let body = if response
            .get("base64Encoded")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            String::from_utf8_lossy(&base64_decode(&body)?).to_string()
        } else {
            body
        };
        Ok(clip(&body, if full { MAX_FULL } else { MAX_EVAL }))
    }

    fn block(&mut self, patterns: Vec<String>) -> Result<String, String> {
        self.page_call("Network.setBlockedURLs", json!({"urls": patterns}))?;
        self.blocked = patterns.clone();
        if patterns.is_empty() {
            Ok("blocked: none".to_string())
        } else {
            Ok(format!("blocked: {}", patterns.join(", ")))
        }
    }

    fn cookies_text(&mut self) -> Result<String, String> {
        let response = self.page_call("Network.getAllCookies", json!({}))?;
        let cookies = response
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if cookies.is_empty() {
            return Ok("cookies: none".to_string());
        }
        let mut said = format!("cookies: {}\n", cookies.len());
        for cookie in cookies.iter().take(50) {
            let name = cookie.get("name").and_then(Value::as_str).unwrap_or("");
            let value = cookie.get("value").and_then(Value::as_str).unwrap_or("");
            let domain = cookie.get("domain").and_then(Value::as_str).unwrap_or("");
            let path = cookie.get("path").and_then(Value::as_str).unwrap_or("/");
            let mut flags = Vec::new();
            if cookie
                .get("secure")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                flags.push("secure");
            }
            if cookie
                .get("httpOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                flags.push("httpOnly");
            }
            said.push_str(&format!(
                "  {name}={} domain={domain} path={path}{}\n",
                clip(value, 80),
                if flags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", flags.join(","))
                }
            ));
        }
        Ok(said)
    }

    fn cookie_set(&mut self, args: &Value) -> Result<String, String> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or("cookie_set needs a name")?;
        let value = args.get("value").and_then(Value::as_str).unwrap_or("");
        let mut parameters = json!({"name": name, "value": value, "path": "/"});
        for key in ["domain", "url", "path", "same_site"] {
            if let Some(value) = args.get(key).and_then(Value::as_str) {
                let key = if key == "same_site" { "sameSite" } else { key };
                parameters[key] = json!(value);
            }
        }
        for key in ["secure", "http_only"] {
            if let Some(value) = args.get(key).and_then(Value::as_bool) {
                let key = if key == "http_only" { "httpOnly" } else { key };
                parameters[key] = json!(value);
            }
        }
        let response = self.page_call("Network.setCookie", parameters)?;
        if response
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            Ok(format!("cookie set: {name}"))
        } else {
            Err("the browser refused that cookie".to_string())
        }
    }

    fn cookie_delete(&mut self, args: &Value) -> Result<String, String> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or("cookie_delete needs a name")?;
        let mut parameters = json!({"name": name});
        for key in ["domain", "url", "path"] {
            if let Some(value) = args.get(key).and_then(Value::as_str) {
                parameters[key] = json!(value);
            }
        }
        self.page_call("Network.deleteCookies", parameters)?;
        Ok(format!("cookie deleted: {name}"))
    }

    fn storage_text(&mut self, kind: &str) -> Result<String, String> {
        let store = if kind == "session" {
            "sessionStorage"
        } else {
            "localStorage"
        };
        let value = self.evaluate(&format!(
            "(() => Object.fromEntries(Object.entries({store})))()"
        ))?;
        let text = serde_json::to_string(&value).unwrap_or_default();
        Ok(clip(&text, MAX_EVAL))
    }

    fn storage_set(&mut self, args: &Value, kind: &str) -> Result<String, String> {
        let key = args
            .get("key")
            .and_then(Value::as_str)
            .ok_or("storage_set needs a key")?;
        let value = args.get("value").and_then(Value::as_str).unwrap_or("");
        let store = if kind == "session" {
            "sessionStorage"
        } else {
            "localStorage"
        };
        let script = format!(
            "(() => {{ {store}.setItem({}, {}); return true; }})()",
            serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
        );
        let _ = self.evaluate(&script)?;
        Ok(format!("{store} set: {key}"))
    }

    fn storage_clear(&mut self, kind: &str) -> Result<String, String> {
        let script = match kind {
            "local" => "(() => { localStorage.clear(); return true; })()",
            "session" => "(() => { sessionStorage.clear(); return true; })()",
            _ => "(() => { localStorage.clear(); sessionStorage.clear(); return true; })()",
        };
        let _ = self.evaluate(script)?;
        Ok(format!("{kind} storage cleared"))
    }

    fn tabs_text(&self) -> String {
        let mut said = String::from("tabs:\n");
        for (index, tab) in self.tabs.iter().enumerate() {
            let marker = if index == self.current {
                " current"
            } else {
                ""
            };
            said.push_str(&format!(
                "  [{index}]{marker} {} | {}{}\n",
                if tab.title.is_empty() {
                    "untitled"
                } else {
                    &tab.title
                },
                tab.url,
                if tab.context_id.is_empty() || tab.context_id == self.default_context {
                    String::new()
                } else {
                    format!(" [context {}]", tab.context_id)
                }
            ));
        }
        said
    }

    fn close(&mut self) {
        let _ = self.call("Browser.close", json!({}), None);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn send_only(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<u64, String> {
        self.id += 1;
        let id = self.id;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.to
            .write_all(message.to_string().as_bytes())
            .map_err(|error| format!("could not write {method}: {error}"))?;
        self.to
            .write_all(&[0])
            .map_err(|error| format!("could not finish {method}: {error}"))?;
        self.to
            .flush()
            .map_err(|error| format!("could not flush {method}: {error}"))?;
        Ok(id)
    }

    fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        let id = self.send_only(method, params, session)?;
        loop {
            let reply = self.read()?;
            if reply.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = reply.get("error") {
                    return Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("the browser refused the call")
                        .to_string());
                }
                return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
            }
            self.record_event(reply);
        }
    }

    fn record_event(&mut self, event: Value) {
        let method = event
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if method == "Fetch.requestPaused" {
            let request_id = event
                .get("params")
                .and_then(|params| params.get("requestId"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let session = self.session_id.clone();
            let _ = self.send_only(
                "Fetch.continueRequest",
                json!({"requestId": request_id}),
                Some(&session),
            );
        }
        if method == "Fetch.authRequired" {
            let parameters = event.get("params").cloned().unwrap_or(Value::Null);
            let request_id = parameters
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let session = self.session_id.clone();
            let response = match &self.proxy_auth {
                Some((user, password)) => json!({
                    "response": "ProvideCredentials",
                    "username": user,
                    "password": password
                }),
                None => json!({"response": "CancelAuth"}),
            };
            let _ = self.send_only(
                "Fetch.continueWithAuth",
                json!({
                    "requestId": request_id,
                    "authChallengeResponse": response
                }),
                Some(&session),
            );
        }
        if method == "Network.requestWillBeSent" {
            let parameters = event.get("params").cloned().unwrap_or(Value::Null);
            let request_id = parameters
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let request = parameters.get("request").cloned().unwrap_or(Value::Null);
            self.network.push(NetworkEntry {
                request_id,
                method: request
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                url: request
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                resource_type: parameters
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                status: None,
                mime: String::new(),
                error: String::new(),
            });
            if self.network.len() > 200 {
                self.network.remove(0);
            }
        }
        if method == "Network.responseReceived" {
            let parameters = event.get("params").cloned().unwrap_or(Value::Null);
            let request_id = parameters
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let response = parameters.get("response").cloned().unwrap_or(Value::Null);
            if let Some(entry) = self
                .network
                .iter_mut()
                .find(|entry| entry.request_id == request_id)
            {
                entry.status = response.get("status").and_then(Value::as_i64);
                entry.mime = response
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
            }
        }
        if method == "Network.loadingFailed" {
            let parameters = event.get("params").cloned().unwrap_or(Value::Null);
            let request_id = parameters
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some(entry) = self
                .network
                .iter_mut()
                .find(|entry| entry.request_id == request_id)
            {
                entry.error = parameters
                    .get("errorText")
                    .and_then(Value::as_str)
                    .unwrap_or("failed")
                    .to_string();
            }
        }
        if method == "Page.fileChooserOpened" {
            let parameters = event.get("params").cloned().unwrap_or(Value::Null);
            let backend = parameters
                .get("backendNodeId")
                .and_then(Value::as_i64)
                .and_then(|backend| {
                    self.refs
                        .iter()
                        .find(|(_, (_, node))| *node == backend)
                        .map(|(reference, _)| reference.clone())
                });
            self.file_chooser = Some(backend.unwrap_or_default());
        }
        if method == "Page.javascriptDialogOpening" {
            let parameters = event.get("params").cloned().unwrap_or(Value::Null);
            self.dialog = Some(Dialog {
                kind: parameters
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("dialog")
                    .to_string(),
                message: parameters
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
            let accepted = self.dialog_accept;
            let session = self.session_id.clone();
            let _ = self.send_only(
                "Page.handleJavaScriptDialog",
                json!({"accept": accepted}),
                Some(&session),
            );
        }
        if self.events.len() > 500 {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    fn read(&mut self) -> Result<Value, String> {
        let mut bytes = Vec::new();
        let read = self
            .from
            .read_until(0, &mut bytes)
            .map_err(|error| format!("could not read a reply: {error}"))?;
        if read == 0 {
            return Err("the browser closed the pipe".to_string());
        }
        if bytes.last() == Some(&0) {
            bytes.pop();
        }
        serde_json::from_slice(&bytes).map_err(|error| format!("bad browser reply: {error}"))
    }

    fn evaluate(&mut self, script: &str) -> Result<Value, String> {
        let mut parameters = json!({
            "expression": script,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true,
        });
        let frame = self.frame_id.clone();
        let context = self.frame_context(&frame)?;
        let session = if frame.is_empty() {
            Some(self.session_id.clone())
        } else {
            self.frame_sessions.get(&frame).cloned()
        };
        if let Some(context) = context {
            parameters["contextId"] = json!(context);
        }
        let reply = self.call("Runtime.evaluate", parameters, session.as_deref())?;
        if let Some(details) = reply.get("exceptionDetails") {
            let said = details
                .get("exception")
                .and_then(|error| error.get("description"))
                .and_then(Value::as_str)
                .or_else(|| details.get("text").and_then(Value::as_str))
                .unwrap_or("the script threw");
            return Err(said.to_string());
        }
        Ok(reply
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    fn act(&mut self, action: &str, args: &Value) -> Result<String, String> {
        match action {
            "observe" => {
                let text_limit = args
                    .get("text_limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(MAX_TEXT as u64)
                    .min(50_000) as usize;
                let item_limit = args
                    .get("item_limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(MAX_ITEMS as u64)
                    .min(500) as usize;
                self.observe_limited(text_limit, item_limit)
            }
            "open" => {
                let url = args
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or("open needs a url")?;
                self.open(url)
            }
            "click" => {
                let before = self.tabs.len();
                let outcome = self.click(args)?;
                let _ = self.refresh_tabs();
                let page = self.observe()?;
                let mut said = format!("{outcome}\n\n{page}");
                if self.tabs.len() > before {
                    said.push_str("\na new tab opened; `tabs` lists it and `tab N` switches");
                }
                Ok(said)
            }
            "type" => {
                let outcome = self.type_text(args)?;
                let page = self.observe()?;
                Ok(format!("{outcome}\n\n{page}"))
            }
            "press" => {
                let key = args
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or("press needs a key")?;
                self.press(key)?;
                std::thread::sleep(Duration::from_millis(500));
                self.observe()
            }
            "tabs" => {
                self.refresh_tabs()?;
                Ok(self.tabs_text())
            }
            "downloads" => Ok(self.downloads_text()),
            "frames" => self.frames_text(),
            "frame" => self.select_frame(args),
            "network" => Ok(self.network_text()),
            "network_body" => {
                let request_id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("network_body needs a request id")?;
                let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
                self.network_body(request_id, full)
            }
            "block" => {
                let text = args
                    .get("text")
                    .or_else(|| args.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let patterns: Vec<String> = text
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|pattern| !pattern.is_empty())
                    .map(str::to_string)
                    .collect();
                self.block(patterns)
            }
            "unblock" => self.block(Vec::new()),
            "cookies" => self.cookies_text(),
            "cookie_set" => self.cookie_set(args),
            "cookie_delete" => self.cookie_delete(args),
            "cookie_clear" => {
                self.page_call("Network.clearBrowserCookies", json!({}))?;
                Ok("cookies cleared".to_string())
            }
            "storage" => {
                let kind = args.get("kind").and_then(Value::as_str).unwrap_or("local");
                self.storage_text(kind)
            }
            "storage_set" => {
                let kind = args.get("kind").and_then(Value::as_str).unwrap_or("local");
                self.storage_set(args, kind)
            }
            "storage_clear" => {
                let kind = args.get("kind").and_then(Value::as_str).unwrap_or("both");
                self.storage_clear(kind)
            }
            "upload" => {
                let reference = args
                    .get("ref")
                    .and_then(Value::as_str)
                    .ok_or("upload needs a ref to a file input")?;
                let path = args
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or("upload needs a file path")?;
                let path = std::path::Path::new(path);
                // Reading a local file into a page is the same capability as loading one with `file://`,
                // so it is behind the same switch. Without this the deny policy could be walked around by
                // uploading the file into a page the model controls and reading it back with `eval`.
                if !self.policy.allow_local {
                    return Err(
                        "the browser is not allowed to read local files: `upload` is refused"
                            .to_string(),
                    );
                }
                if !path.is_file() {
                    return Err(format!("{} is not a file", path.display()));
                }
                let backend = self
                    .backend_for_ref(reference)
                    .ok_or("that ref is not a file input on the current page")?;
                let mut parameters = json!({
                    "files": [path.display().to_string()],
                    "backendNodeId": backend
                });
                if let Some(frame) = self.frame_for_ref(reference)
                    && let Some(context) = self.frame_context(&frame)?
                {
                    parameters["executionContextId"] = json!(context);
                }
                let frame = self.frame_for_ref(reference).unwrap_or_default();
                let session = if frame.is_empty() {
                    Some(self.session_id.clone())
                } else {
                    self.frame_sessions.get(&frame).cloned()
                };
                self.call("DOM.setFileInputFiles", parameters, session.as_deref())?;
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("the file");
                let page = self.observe()?;
                Ok(format!("uploaded {name}\n\n{page}"))
            }
            "new_tab" => {
                let url = args.get("url").and_then(Value::as_str).unwrap_or("");
                let context = args.get("context").and_then(Value::as_str).unwrap_or("");
                self.new_tab(url, context)
            }
            "contexts" => self.contexts_text(),
            "new_context" => {
                let url = args.get("url").and_then(Value::as_str).unwrap_or("");
                self.new_context(url)
            }
            "close_context" => {
                let context = args
                    .get("context")
                    .and_then(Value::as_str)
                    .ok_or("close_context needs a context id")?;
                self.close_context(context)
            }
            "tab" => {
                let index = args
                    .get("index")
                    .or_else(|| args.get("tab"))
                    .and_then(Value::as_u64)
                    .ok_or("tab needs an index")? as usize;
                self.switch_to(index)
            }
            "close_tab" => {
                let index = args
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|index| index as usize);
                self.close_tab(index)
            }
            "wait" => self.wait(args),
            "screenshot" => self.screenshot(args),
            "eval" => {
                let script = args
                    .get("script")
                    .and_then(Value::as_str)
                    .ok_or("eval needs a script")?;
                match self.evaluate(script) {
                    Ok(value) => {
                        let text = serde_json::to_string(&value).unwrap_or_default();
                        let limit = if args.get("full").and_then(Value::as_bool).unwrap_or(false) {
                            MAX_FULL
                        } else {
                            MAX_EVAL
                        };
                        Ok(clip(&text, limit))
                    }
                    Err(said) => match self.observe() {
                        Ok(page) => Ok(format!("the script failed: {said}\n\n{page}")),
                        Err(_) => Err(said),
                    },
                }
            }
            "close" => {
                self.close();
                Ok("browser closed".to_string())
            }
            other => Err(format!("unknown action {other}")),
        }
    }

    fn open(&mut self, url: &str) -> Result<String, String> {
        // Before `Page.navigate`, not after: a refused scheme must cost no request and leave no rendered
        // text in the observation for a later turn to read.
        self.policy.check(url)?;
        self.clear_frame();
        let reply = self.page_call("Page.navigate", json!({"url": url}))?;
        if let Some(error) = reply.get("errorText").and_then(Value::as_str) {
            return Err(error.to_string());
        }
        self.settle(30);
        self.observe()
    }

    fn settle(&mut self, seconds: u64) {
        let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
        while std::time::Instant::now() < deadline {
            if let Ok(ready) = self.evaluate("document.readyState")
                && ready.as_str() == Some("complete")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        std::thread::sleep(Duration::from_millis(400));
    }

    fn observe(&mut self) -> Result<String, String> {
        self.observe_limited(MAX_TEXT, MAX_ITEMS)
    }

    fn observe_limited(&mut self, text_limit: usize, item_limit: usize) -> Result<String, String> {
        let _ = self.refresh_tabs();
        let page = self.evaluate(PAGE)?;
        let title = page
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let url = page
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // The text is in this process, but it has not been returned. Refuse before it becomes an
        // observation: a redirect into `file://` is as much a read as an `open` was.
        self.policy.check(&url)?;
        let text = page
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let headings = page
            .get("headings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let dom_focus = page
            .get("focus")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let frame = self.frame_id.clone();
        let session = if frame.is_empty() {
            Some(self.session_id.clone())
        } else {
            let _ = self.frame_context(&frame)?;
            self.frame_sessions.get(&frame).cloned()
        };
        let tree = if frame.is_empty() {
            self.page_call("Accessibility.getFullAXTree", json!({}))?
        } else {
            self.call(
                "Accessibility.getFullAXTree",
                json!({"frameId": frame}),
                session.as_deref(),
            )?
        };
        let nodes = tree
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        let mut lines = Vec::new();
        let mut focused = String::new();
        for node in &nodes {
            if node.get("ignored").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let role = ax_string(node, "role");
            if !is_interactive(&role) {
                continue;
            }
            let backend = match node.get("backendDOMNodeId").and_then(Value::as_i64) {
                Some(backend) => backend,
                None => continue,
            };
            if !seen.insert(backend) {
                continue;
            }
            if lines.len() >= item_limit {
                break;
            }
            let name = ax_string(node, "name");
            let value = ax_string(node, "value");
            let reference = self.ref_for(backend, &name);
            let mut line = format!("  [{reference}] {role}");
            if !name.is_empty() {
                line.push(' ');
                line.push_str(&quote(&name));
            }
            if !value.is_empty() {
                line.push_str(&format!(" value={}", quote(&value)));
            }
            if let Some(url) = ax_property_text(node, "url")
                && !url.is_empty()
            {
                line.push_str(&format!(" -> {url}"));
            }
            if ax_property_bool(node, "focused").unwrap_or(false) {
                focused = reference.clone();
            }
            if let Some(checked) = ax_property_text(node, "checked")
                && !checked.is_empty()
                && checked != "false"
            {
                line.push_str(&format!(" ({checked})"));
            }
            if ax_property_bool(node, "disabled").unwrap_or(false) {
                line.push_str(" (disabled)");
            }
            lines.push(line);
        }

        let mut said = format!("title: {title}\nurl: {url}\n");
        if let Some(chooser) = self.file_chooser.take() {
            if chooser.is_empty() {
                said = format!("file chooser: use `upload` with a ref and a path\n\n{said}");
            } else {
                said = format!("file chooser: use `upload` with ref `{chooser}`\n\n{said}");
            }
        }
        if let Some(dialog) = self.dialog.take() {
            let message = dialog.message.replace('\n', " ");
            said = format!(
                "dialog: {} {} (handled)\n\n{said}",
                dialog.kind,
                quote(&message)
            );
        }
        let focus = if focused.is_empty() {
            dom_focus
        } else {
            focused
        };
        if !focus.is_empty() {
            said.push_str(&format!("focus: {focus}\n"));
        }
        if !headings.is_empty() {
            said.push_str("\nheadings:\n");
            for heading in headings.iter().take(MAX_HEADINGS) {
                if let Some(heading) = heading.as_str() {
                    said.push_str(&format!("  {heading}\n"));
                }
            }
        }
        said.push_str("\ninteractive:\n");
        if lines.is_empty() {
            said.push_str("  none\n");
        }
        for line in lines {
            said.push_str(&line);
            said.push('\n');
        }
        said.push_str("\ntext:\n");
        said.push_str(&clip(&text, text_limit));
        if self.tabs.len() > 1 {
            said.push_str(&format!(
                "\ntabs: {} (current {})\n",
                self.tabs.len(),
                self.current
            ));
        }
        Ok(said)
    }

    fn ref_for(&mut self, backend: i64, name: &str) -> String {
        let frame = self.frame_id.clone();
        if let Some(reference) = self.ref_by_backend.get(&(frame.clone(), backend)) {
            return reference.clone();
        }
        self.next_ref += 1;
        let slug = slug(name);
        let reference = if slug.is_empty() {
            format!("e{}", self.next_ref)
        } else {
            format!("e{} {slug}", self.next_ref)
        };
        self.refs
            .insert(reference.clone(), (frame.clone(), backend));
        self.ref_by_backend
            .insert((frame, backend), reference.clone());
        if !name.is_empty() {
            self.ref_labels.insert(reference.clone(), name.to_string());
        }
        reference
    }

    fn backend_for_ref(&self, reference: &str) -> Option<i64> {
        self.refs.get(reference).map(|(_, backend)| *backend)
    }

    fn frame_for_ref(&self, reference: &str) -> Option<String> {
        self.refs
            .get(reference)
            .map(|(frame, _)| frame.clone())
            .filter(|frame| !frame.is_empty())
    }

    fn attach_oopif(&mut self, frame: &str) -> Result<Option<String>, String> {
        if let Some(session) = self.frame_sessions.get(frame) {
            return Ok(Some(session.clone()));
        }
        let listed = self.call("Target.getTargets", json!({}), None)?;
        let infos = listed
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for info in infos {
            if info.get("type").and_then(Value::as_str) != Some("iframe") {
                continue;
            }
            let target_id = match info.get("targetId").and_then(Value::as_str) {
                Some(target_id) => target_id.to_string(),
                None => continue,
            };
            if self.oopif_targets.contains(&target_id) {
                continue;
            }
            let attached = match self.call(
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
                None,
            ) {
                Ok(attached) => attached,
                Err(_) => continue,
            };
            let session = match attached.get("sessionId").and_then(Value::as_str) {
                Some(session) => session.to_string(),
                None => continue,
            };
            self.oopif_targets.insert(target_id);
            let _ = self.call("Page.enable", json!({}), Some(&session));
            let _ = self.call("Runtime.enable", json!({}), Some(&session));
            let _ = self.call("DOM.enable", json!({}), Some(&session));
            let _ = self.call("Accessibility.enable", json!({}), Some(&session));
            let tree = match self.call("Page.getFrameTree", json!({}), Some(&session)) {
                Ok(tree) => tree,
                Err(_) => continue,
            };
            let mut ids = Vec::new();
            if let Some(root) = tree.get("frameTree") {
                collect_frame_ids(root, &mut ids);
            }
            for id in ids {
                self.frame_sessions.insert(id, session.clone());
            }
            if self.frame_sessions.contains_key(frame) {
                return Ok(Some(session));
            }
        }
        Ok(None)
    }

    fn frame_context(&mut self, frame: &str) -> Result<Option<i64>, String> {
        if frame.is_empty() {
            return Ok(None);
        }
        if let Some(context) = self.frame_contexts.get(frame) {
            return Ok(Some(*context));
        }
        let session = self.frame_sessions.get(frame).cloned();
        if let Some(session) = session {
            let world = self.call(
                "Page.createIsolatedWorld",
                json!({"frameId": frame, "worldName": "hades"}),
                Some(&session),
            )?;
            let context = world
                .get("executionContextId")
                .and_then(Value::as_i64)
                .ok_or("the frame made no execution context")?;
            self.frame_contexts.insert(frame.to_string(), context);
            return Ok(Some(context));
        }
        match self.page_call(
            "Page.createIsolatedWorld",
            json!({"frameId": frame, "worldName": "hades"}),
        ) {
            Ok(world) => {
                let context = world
                    .get("executionContextId")
                    .and_then(Value::as_i64)
                    .ok_or("the frame made no execution context")?;
                self.frame_contexts.insert(frame.to_string(), context);
                self.frame_sessions
                    .insert(frame.to_string(), self.session_id.clone());
                Ok(Some(context))
            }
            Err(main_error) => match self.attach_oopif(frame)? {
                Some(session) => {
                    let world = self.call(
                        "Page.createIsolatedWorld",
                        json!({"frameId": frame, "worldName": "hades"}),
                        Some(&session),
                    )?;
                    let context = world
                        .get("executionContextId")
                        .and_then(Value::as_i64)
                        .ok_or("the frame made no execution context")?;
                    self.frame_contexts.insert(frame.to_string(), context);
                    Ok(Some(context))
                }
                None => Err(format!(
                    "the frame is not in this process and no OOPIF target matched it: {main_error}"
                )),
            },
        }
    }

    fn resolve_ref(&mut self, reference: &str) -> Result<Option<String>, String> {
        let (frame, backend) = match self.refs.get(reference) {
            Some((frame, backend)) => (frame.clone(), *backend),
            None => return Ok(None),
        };
        let mut parameters = json!({"backendNodeId": backend});
        let context = self.frame_context(&frame)?;
        let session = if frame.is_empty() {
            Some(self.session_id.clone())
        } else {
            self.frame_sessions.get(&frame).cloned()
        };
        if let Some(context) = context {
            parameters["executionContextId"] = json!(context);
        }
        let resolved = self.call("DOM.resolveNode", parameters, session.as_deref())?;
        Ok(resolved
            .get("object")
            .and_then(|object| object.get("objectId"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    fn human_click(&mut self, reference: &str) -> Result<bool, String> {
        let backend = match self.refs.get(reference) {
            Some((_, backend)) => *backend,
            None => return Ok(false),
        };
        let object_id = match self.resolve_ref(reference)? {
            Some(object_id) => object_id,
            None => return Ok(false),
        };
        let frame = self.frame_for_ref(reference).unwrap_or_default();
        let session = if frame.is_empty() {
            Some(self.session_id.clone())
        } else {
            self.frame_sessions.get(&frame).cloned()
        };
        let _ = self.call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": "function(){ this.scrollIntoView({block:'center', inline:'center'}); }",
                "returnByValue": true,
            }),
            session.as_deref(),
        );
        let mut model_parameters = json!({"backendNodeId": backend});
        if let Some(context) = self.frame_context(&frame)? {
            model_parameters["executionContextId"] = json!(context);
        }
        let model = match self.call("DOM.getBoxModel", model_parameters, session.as_deref()) {
            Ok(model) => model,
            Err(_) => return Ok(false),
        };
        let quad = match model
            .get("model")
            .and_then(|model| model.get("content"))
            .and_then(Value::as_array)
        {
            Some(quad) if quad.len() >= 8 => quad,
            _ => return Ok(false),
        };
        let numbers: Vec<f64> = quad.iter().filter_map(Value::as_f64).collect();
        if numbers.len() < 8 {
            return Ok(false);
        }
        let x = (numbers[0] + numbers[2] + numbers[4] + numbers[6]) / 4.0;
        let y = (numbers[1] + numbers[3] + numbers[5] + numbers[7]) / 4.0;
        let start_x = x - 70.0;
        let start_y = y - 40.0;
        for step in 0..=10 {
            let t = step as f64 / 10.0;
            let eased = t * t * (3.0 - 2.0 * t);
            let wobble = (t * std::f64::consts::PI * 2.0).sin() * 3.0;
            let _ = self.call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseMoved",
                    "x": start_x + (x - start_x) * eased + wobble,
                    "y": start_y + (y - start_y) * eased,
                    "button": "none",
                }),
                session.as_deref(),
            );
            std::thread::sleep(Duration::from_millis(8));
        }
        for kind in ["mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": kind,
                    "x": x,
                    "y": y,
                    "button": "left",
                    "clickCount": 1,
                }),
                session.as_deref(),
            )?;
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(true)
    }

    fn human_type(
        &mut self,
        object_id: &str,
        text: &str,
        session: Option<&str>,
    ) -> Result<(), String> {
        let _ = self.call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": "function(){ this.focus(); }",
                "returnByValue": true,
            }),
            session,
        );
        let characters: Vec<char> = text.chars().collect();
        let total = characters.len();
        let mut index = 0;
        while index < total {
            let take = if total > 500 { 8 } else { 1 };
            let end = (index + take).min(total);
            let chunk: String = characters[index..end].iter().collect();
            self.call("Input.insertText", json!({"text": chunk}), session)?;
            std::thread::sleep(Duration::from_millis(if total > 500 { 4 } else { 18 }));
            index = end;
        }
        Ok(())
    }

    fn clear_refs(&mut self) {
        self.refs.clear();
        self.ref_by_backend.clear();
        self.ref_labels.clear();
        self.next_ref = 0;
    }

    fn clear_frame(&mut self) {
        self.frame_id.clear();
        self.frame_contexts.clear();
        self.frame_sessions.clear();
        self.oopif_targets.clear();
        self.clear_refs();
    }

    fn click(&mut self, args: &Value) -> Result<String, String> {
        let reference = args.get("ref").and_then(Value::as_str);
        let selector = args.get("selector").and_then(Value::as_str);
        let text = args.get("text").and_then(Value::as_str);
        if reference.is_none() && selector.is_none() && text.is_none() {
            return Err("click needs a ref, a selector, or visible text".to_string());
        }
        if let Some(reference) = reference {
            if self.humanize && self.human_click(reference)? {
                self.settle(10);
                let label = self
                    .ref_labels
                    .get(reference)
                    .map(String::as_str)
                    .unwrap_or("the element");
                return Ok(format!("clicked {label}"));
            }
            match self.resolve_ref(reference) {
                Ok(Some(object_id)) => {
                    let frame = self.frame_for_ref(reference).unwrap_or_default();
                    let session = if frame.is_empty() {
                        Some(self.session_id.clone())
                    } else {
                        self.frame_sessions.get(&frame).cloned()
                    };
                    let result = self.call(
                        "Runtime.callFunctionOn",
                        json!({
                            "objectId": object_id,
                            "functionDeclaration": "function(){ this.scrollIntoView({block:'center', inline:'center'}); this.click(); return ((this.innerText || this.value || '').trim().slice(0, 80)); }",
                            "returnByValue": true,
                            "userGesture": true,
                        }),
                        session.as_deref(),
                    )?;
                    if let Some(details) = result.get("exceptionDetails") {
                        let said = details
                            .get("exception")
                            .and_then(|error| error.get("description"))
                            .and_then(Value::as_str)
                            .unwrap_or("the click threw");
                        let page = self.observe()?;
                        return Ok(format!("{said}\n\n{page}"));
                    }
                    self.settle(10);
                    let label = result
                        .get("result")
                        .and_then(|result| result.get("value"))
                        .and_then(Value::as_str)
                        .filter(|label| !label.is_empty())
                        .or_else(|| self.ref_labels.get(reference).map(String::as_str))
                        .unwrap_or("the element");
                    return Ok(format!("clicked {label}"));
                }
                Ok(None) => {
                    let page = self.observe()?;
                    return Ok(format!(
                        "that ref is not on the page; here is the current page:\n\n{page}"
                    ));
                }
                Err(said) => {
                    let page = self.observe()?;
                    return Ok(format!("could not resolve that ref: {said}\n\n{page}"));
                }
            }
        }
        let script = format!(
            r#"(() => {{
                const ref = {};
                const selector = {};
                const text = {};
                let el = null;
                if (ref) {{
                    el = Array.from(document.querySelectorAll('[data-hades-ref]')).find(e => e.getAttribute('data-hades-ref') === ref);
                }} else if (selector) {{
                    el = document.querySelector(selector);
                }} else if (text) {{
                    const wanted = text.toLowerCase();
                    el = Array.from(document.querySelectorAll('a,button,[role="button"],[role="link"],[role="tab"]'))
                        .find(e => ((e.innerText || '').trim().toLowerCase().includes(wanted)));
                }}
                if (!el) return {{ok: false, error: 'no element matches that ref, selector or text'}};
                el.scrollIntoView({{block: 'center', inline: 'center'}});
                el.click();
                return {{ok: true, label: ((el.innerText || el.value || '').trim().slice(0, 80))}};
            }})()"#,
            serde_json::to_string(&reference).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(&selector).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(&text).unwrap_or_else(|_| "null".into()),
        );
        let result = self.evaluate(&script)?;
        if result.get("ok").and_then(Value::as_bool) != Some(true) {
            let said = result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the click found nothing");
            let page = self.observe()?;
            return Ok(format!("{said}\n\n{page}"));
        }
        self.settle(10);
        Ok(format!(
            "clicked {}",
            result
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("the element")
        ))
    }

    fn type_text(&mut self, args: &Value) -> Result<String, String> {
        let reference = args.get("ref").and_then(Value::as_str);
        let selector = args.get("selector").and_then(Value::as_str);
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or("type needs the text to enter")?;
        if reference.is_none() && selector.is_none() {
            return Err("type needs a ref or a selector".to_string());
        }
        if let Some(reference) = reference {
            if self.humanize
                && let Some(object_id) = self.resolve_ref(reference)?
            {
                let frame = self.frame_for_ref(reference).unwrap_or_default();
                let session = if frame.is_empty() {
                    Some(self.session_id.clone())
                } else {
                    self.frame_sessions.get(&frame).cloned()
                };
                self.human_type(&object_id, text, session.as_deref())?;
                let label = self
                    .ref_labels
                    .get(reference)
                    .map(String::as_str)
                    .unwrap_or("the field");
                return Ok(format!("typed into {label}"));
            }
            match self.resolve_ref(reference) {
                Ok(Some(object_id)) => {
                    let frame = self.frame_for_ref(reference).unwrap_or_default();
                    let session = if frame.is_empty() {
                        Some(self.session_id.clone())
                    } else {
                        self.frame_sessions.get(&frame).cloned()
                    };
                    let result = self.call(
                        "Runtime.callFunctionOn",
                        json!({
                            "objectId": object_id,
                            "functionDeclaration": "function(text){ this.focus(); if (this.isContentEditable) { this.textContent = text; } else if (this.tagName === 'SELECT') { this.value = text; } else { const proto = this.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype; const setter = Object.getOwnPropertyDescriptor(proto, 'value').set; setter.call(this, text); } this.dispatchEvent(new Event('input', {bubbles:true})); this.dispatchEvent(new Event('change', {bubbles:true})); return this.value || ''; }",
                            "arguments": [{"value": text}],
                            "returnByValue": true,
                            "userGesture": true,
                        }),
                        session.as_deref(),
                    )?;
                    if let Some(details) = result.get("exceptionDetails") {
                        let said = details
                            .get("exception")
                            .and_then(|error| error.get("description"))
                            .and_then(Value::as_str)
                            .unwrap_or("the field refused the text");
                        let page = self.observe()?;
                        return Ok(format!("{said}\n\n{page}"));
                    }
                    let label = self
                        .ref_labels
                        .get(reference)
                        .map(String::as_str)
                        .unwrap_or("the field");
                    return Ok(format!("typed into {label}"));
                }
                Ok(None) => {
                    let page = self.observe()?;
                    return Ok(format!(
                        "that ref is not on the page; here is the current page:\n\n{page}"
                    ));
                }
                Err(said) => {
                    let page = self.observe()?;
                    return Ok(format!("could not resolve that ref: {said}\n\n{page}"));
                }
            }
        }
        let script = format!(
            r#"(() => {{
                const ref = {};
                const selector = {};
                const text = {};
                let el = null;
                if (ref) {{
                    el = Array.from(document.querySelectorAll('[data-hades-ref]')).find(e => e.getAttribute('data-hades-ref') === ref);
                }} else {{
                    el = document.querySelector(selector);
                }}
                if (!el) return {{ok: false, error: 'no field matches that ref or selector'}};
                el.focus();
                if (el.isContentEditable) {{
                    el.textContent = text;
                }} else if (el.tagName === 'SELECT') {{
                    el.value = text;
                }} else {{
                    const proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
                    const setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
                    setter.call(el, text);
                }}
                el.dispatchEvent(new Event('input', {{bubbles: true}}));
                el.dispatchEvent(new Event('change', {{bubbles: true}}));
                return {{ok: true, name: (el.getAttribute('name') || el.getAttribute('aria-label') || el.tagName.toLowerCase())}};
            }})()"#,
            serde_json::to_string(&reference).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(&selector).unwrap_or_else(|_| "null".into()),
            serde_json::to_string(text).unwrap_or_else(|_| "null".into()),
        );
        let result = self.evaluate(&script)?;
        if result.get("ok").and_then(Value::as_bool) != Some(true) {
            let said = result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the field was not found");
            let page = self.observe()?;
            return Ok(format!("{said}\n\n{page}"));
        }
        Ok(format!(
            "typed into {}",
            result
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("the field")
        ))
    }

    fn press(&mut self, key: &str) -> Result<(), String> {
        let (code, windows) = match key {
            "Enter" => ("Enter", 13),
            "Tab" => ("Tab", 9),
            "Escape" => ("Escape", 27),
            "Backspace" => ("Backspace", 8),
            "ArrowDown" => ("ArrowDown", 40),
            "ArrowUp" => ("ArrowUp", 38),
            other => (other, 0),
        };
        self.page_call(
            "Input.dispatchKeyEvent",
            json!({
                "type": "rawKeyDown",
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": windows,
                "nativeVirtualKeyCode": windows,
            }),
        )?;
        self.page_call(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": windows,
                "nativeVirtualKeyCode": windows,
            }),
        )?;
        Ok(())
    }

    fn wait(&mut self, args: &Value) -> Result<String, String> {
        let text = args.get("text").and_then(Value::as_str);
        let url = args.get("url").and_then(Value::as_str);
        let selector = args.get("selector").and_then(Value::as_str);
        if text.is_none() && url.is_none() && selector.is_none() {
            return Err("wait needs text, url, or selector".to_string());
        }
        let seconds = args
            .get("timeout_s")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .min(60);
        let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
        while std::time::Instant::now() < deadline {
            let condition = if let Some(text) = text {
                format!(
                    "(document.body ? document.body.innerText : '').includes({})",
                    serde_json::to_string(text).unwrap_or_default()
                )
            } else if let Some(url) = url {
                format!(
                    "location.href.includes({})",
                    serde_json::to_string(url).unwrap_or_default()
                )
            } else {
                format!(
                    "document.querySelector({}) !== null",
                    serde_json::to_string(selector.unwrap_or("")).unwrap_or_default()
                )
            };
            if self.evaluate(&condition)?.as_bool() == Some(true) {
                return self.observe();
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let page = self.observe()?;
        Ok(format!(
            "the condition did not happen within {seconds}s\n\n{page}"
        ))
    }

    fn screenshot(&mut self, args: &Value) -> Result<String, String> {
        let requested = args.get("path").and_then(Value::as_str);
        let path = resolve_shot(&self.state, requested)?;
        let reply = self.page_call(
            "Page.captureScreenshot",
            json!({"format": "png", "captureBeyondViewport": false}),
        )?;
        let data = reply
            .get("data")
            .and_then(Value::as_str)
            .ok_or("the browser returned no image")?;
        let bytes = base64_decode(data)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
        }
        write_no_follow(&path, &bytes)?;
        Ok(format!("saved to {}", path.display()))
    }
}

/// Write a screenshot without following a symlink at the final name.
///
/// `resolve_shot` refuses traversal and a symlink it can see, but a link can be created between that check
/// and the write. `O_NOFOLLOW` makes the refusal the kernel's decision rather than a second check.
fn write_no_follow(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::io::FromRawFd as _;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "the screenshot path contains a NUL".to_string())?;
    // SAFETY: `open` with a NUL-terminated path we own; the descriptor is checked before it is wrapped.
    let descriptor = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644,
        )
    };
    if descriptor < 0 {
        return Err(format!(
            "could not write {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: the descriptor is open for writing and is owned by this `File` from here on.
    let mut file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    file.write_all(bytes)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// The flags Chromium is started with.
///
/// Extracted so the sandbox question is a test rather than a line buried in a constructor. Chromium's
/// sandbox is the boundary between a page and the account running the browser; a product that disables it
/// by default has traded that boundary for convenience on one machine.
/// The last thing Chromium said before it died, minus the crashpad noise it emits on every host.
///
/// The launch log is written to `<state>/browser/chromium.log` on every start. This reads it back only when
/// the first CDP call fails, so a healthy launch pays nothing.
fn chromium_complaint(state: &Path) -> Option<String> {
    let text = std::fs::read_to_string(state.join("browser").join("chromium.log")).ok()?;
    text.lines()
        .rev()
        .find(|line| {
            !line.contains("crashpad") && (line.contains(":FATAL:") || line.contains(":ERROR:"))
        })
        .map(|line| {
            // Drop the timestamp and pid prefix: `[pid:tid:date:time:SEVERITY:file] message`.
            match line.find("] ") {
                Some(index) => line[index + 2..].trim().to_string(),
                None => line.trim().to_string(),
            }
        })
}

fn launch_args(headed: bool, no_sandbox: bool) -> Vec<String> {
    let mut args = Vec::new();
    if !headed {
        args.push("--headless=new".to_string());
    }
    args.push("--remote-debugging-pipe".to_string());
    if no_sandbox {
        args.push("--no-sandbox".to_string());
    }
    let optional: [&str; 0] = [];
    args.extend(optional.iter().map(|argument| argument.to_string()));
    args
}

/// A host that cannot give Chromium a sandbox opts out by name.
///
/// The opt-out is an environment variable rather than a fallback: a silent downgrade when the first launch
/// fails would make the weakest configuration the one every host ends up with.
fn no_sandbox_requested() -> bool {
    std::env::var("HADES_BROWSER_NO_SANDBOX")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// The longest Unix socket path the kernel accepts: `sun_path` is 108 bytes including the terminator.
const SUN_PATH_MAX: usize = 107;

/// What Chromium appends to `TMPDIR` for its process-singleton socket.
///
/// It is a fixed string plus a six-character suffix, so the room left for `TMPDIR` is what decides whether a
/// state directory can host a browser at all.
const CHROMIUM_SOCKET: &str = "/.org.chromium.Chromium.XXXXXX/SingletonSocket";

/// Check that `TMPDIR` leaves room for Chromium's socket before a launch is attempted.
///
/// Chromium binds a Unix socket under `TMPDIR`; when the path is longer than the kernel allows, `bind` fails
/// inside Chromium and it aborts with `SIGTRAP` and no message, which reads as "the browser closed the pipe"
/// from here. Measured on this machine: a state directory of 88 characters died this way, 61 or fewer worked.
/// Refusing here names the number instead of letting a trap explain it.
fn chromium_path_check(tmp: &Path) -> Result<(), String> {
    let length = tmp.to_string_lossy().len() + CHROMIUM_SOCKET.len();
    if length > SUN_PATH_MAX {
        return Err(format!(
            "the browser cannot start: {} is {} characters, and Chromium's socket needs {} more, past the              kernel's {SUN_PATH_MAX}-byte limit for a Unix socket. Use a shorter state directory (--state),              or point HADES_HOME at one.",
            tmp.display(),
            tmp.to_string_lossy().len(),
            CHROMIUM_SOCKET.len()
        ));
    }
    Ok(())
}

/// Where a screenshot may be written: under the state's `shots` directory, and nowhere else.
///
/// The path arrives from the model, so it is treated as hostile: an absolute path, a `..` component, or a
/// prefix component is refused by name. The parent is created and then canonicalized, because a symlink
/// under `shots` pointing elsewhere would otherwise pass a textual prefix test.
fn resolve_shot(state: &Path, requested: Option<&str>) -> Result<PathBuf, String> {
    const REFUSED: &str = "a screenshot path must stay inside the state's shots directory";
    let shots = state.join("shots");
    std::fs::create_dir_all(&shots)
        .map_err(|error| format!("could not make {}: {error}", shots.display()))?;
    let shots = std::fs::canonicalize(&shots)
        .map_err(|error| format!("could not resolve {}: {error}", shots.display()))?;
    let path = match requested {
        Some(raw) => {
            let relative = Path::new(raw);
            if relative.is_absolute()
                || relative.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(REFUSED.to_string());
            }
            let path = shots.join(relative);
            // A subdirectory can be a symlink even when every component is a plain name: resolve the
            // parent and keep the result inside the shots directory.
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
                let resolved = std::fs::canonicalize(parent)
                    .map_err(|error| format!("could not resolve {}: {error}", parent.display()))?;
                if !resolved.starts_with(&shots) {
                    return Err(REFUSED.to_string());
                }
            }
            // And the final name may be a symlink, which `fs::write` would follow out of the directory.
            if std::fs::symlink_metadata(&path)
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(false)
            {
                return Err("a screenshot may not be written through a symlink".to_string());
            }
            path
        }
        None => {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or(0);
            // Under `shots`, not `browser`: the `image` tool reads state/shots by policy and nothing
            // under state/browser, so the default is the loop that works - save here, look at it
            // there, with the path the policy itself granted.
            shots.join(format!("shot-{stamp}.png"))
        }
    };
    let parent = path.parent().ok_or(REFUSED)?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
    let parent = std::fs::canonicalize(parent)
        .map_err(|error| format!("could not resolve {}: {error}", parent.display()))?;
    if !parent.starts_with(&shots) {
        return Err(REFUSED.to_string());
    }
    Ok(path)
}

const PAGE: &str = r#"(() => {
    const compact = (value) => (value || '').trim().replace(/\s+/g, ' ');
    const headings = Array.from(document.querySelectorAll('h1,h2,h3'))
        .filter((heading) => {
            const box = heading.getBoundingClientRect();
            return box.width > 0 && box.height > 0;
        })
        .slice(0, 12)
        .map((heading) => heading.tagName.toLowerCase() + ': ' + compact(heading.innerText).slice(0, 80));
    const active = document.activeElement;
    const label = active && active.getAttribute('aria-label');
    return {
        title: document.title,
        url: location.href,
        text: (document.body ? document.body.innerText : '').slice(0, 6000),
        headings,
        focus: active && active !== document.body
            ? active.tagName.toLowerCase() + (label ? ' ' + label : '')
            : ''
    };
})()"#;

fn collect_frame_ids(node: &Value, ids: &mut Vec<String>) {
    if let Some(id) = node
        .get("frame")
        .and_then(|frame| frame.get("id"))
        .and_then(Value::as_str)
    {
        ids.push(id.to_string());
    }
    if let Some(children) = node.get("childFrames").and_then(Value::as_array) {
        for child in children {
            collect_frame_ids(child, ids);
        }
    }
}

fn collect_frames(node: &Value, depth: usize, frames: &mut Vec<FrameRow>) {
    let Some(frame) = node.get("frame") else {
        return;
    };
    if let Some(id) = frame.get("id").and_then(Value::as_str) {
        frames.push(FrameRow {
            id: id.to_string(),
            url: frame
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            name: frame
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            depth,
        });
    }
    if let Some(children) = node.get("childFrames").and_then(Value::as_array) {
        for child in children {
            collect_frames(child, depth + 1, frames);
        }
    }
}

fn split_proxy(proxy: &str) -> (String, Option<(String, String)>) {
    let Some((before, host)) = proxy.rsplit_once('@') else {
        return (proxy.to_string(), None);
    };
    let userinfo = before
        .rsplit_once("://")
        .map(|(_, userinfo)| userinfo)
        .unwrap_or(before);
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    if user.is_empty() {
        return (proxy.to_string(), None);
    }
    let scheme = proxy
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .unwrap_or("http");
    (
        format!("{scheme}://{host}"),
        Some((percent_decode(user), percent_decode(password))),
    )
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

fn ax_string(node: &Value, key: &str) -> String {
    node.get(key)
        .and_then(|field| field.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn ax_property<'a>(node: &'a Value, key: &str) -> Option<&'a Value> {
    node.get("properties")?
        .as_array()?
        .iter()
        .find(|property| property.get("name").and_then(Value::as_str) == Some(key))?
        .get("value")
}

fn ax_property_bool(node: &Value, key: &str) -> Option<bool> {
    let value = ax_property(node, key)?.get("value")?;
    value
        .as_bool()
        .or_else(|| value.as_str().map(|text| text == "true"))
}

fn ax_property_text(node: &Value, key: &str) -> Option<String> {
    let value = ax_property(node, key)?.get("value")?;
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(flag) = value.as_bool() {
        return Some(flag.to_string());
    }
    None
}

fn is_interactive(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "checkbox"
            | "radio"
            | "combobox"
            | "listbox"
            | "option"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "tab"
            | "switch"
            | "slider"
            | "spinbutton"
            | "treeitem"
    )
}

fn slug(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    let slug = clean
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ");
    slug.chars()
        .take(32)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max * 2 / 3).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(max / 3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n[... cut ...]\n{tail}")
}

fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.subsec_nanos() as u64)
        .unwrap_or(0)
        % 90_000
        + 10_000
}

fn is_patched(binary: &Path) -> bool {
    binary.to_string_lossy().contains(".cloakbrowser")
}

/// Where a distribution usually puts Chromium, checked before the PATH search.
const KNOWN_BROWSERS: [&str; 8] = [
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/usr/lib/chromium/chromium",
    "/usr/lib/chromium-browser/chromium-browser",
    "/opt/google/chrome/chrome",
    "/snap/bin/chromium",
];

/// The names a browser answers to on PATH.
const PATH_BROWSERS: [&str; 5] = [
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "chrome",
];

fn find_chrome() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("HADES_CHROME") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("HADES_CHROME is not a file: {}", path.display()));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let cache = PathBuf::from(home).join(".cloakbrowser");
        if let Ok(entries) = std::fs::read_dir(&cache) {
            let mut candidates: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path().join("chrome"))
                .filter(|path| path.is_file())
                .collect();
            candidates.sort_by_key(|path| {
                path.metadata()
                    .and_then(|meta| meta.modified())
                    .unwrap_or(UNIX_EPOCH)
            });
            if let Some(path) = candidates.pop() {
                return Ok(path);
            }
        }
    }
    for candidate in KNOWN_BROWSERS {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    // Distributions disagree about the name and the package layout, and a browser installed by a package
    // manager is usually on PATH even when it is not in /usr/bin under a name this file guessed. The
    // search is last so an explicit install still wins.
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths) {
            for name in PATH_BROWSERS {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    Err("no Chromium found; set HADES_CHROME to a browser binary".to_string())
}

fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return Err("the screenshot was not valid base64".to_string()),
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

unsafe extern "C" {
    fn dup2(oldfd: i32, newfd: i32) -> i32;
    fn fcntl(fd: i32, command: i32, ...) -> i32;
}

/// `F_SETFD`, from `fcntl.h`. The number is the same on every architecture Linux supports, but it is named
/// so a reader does not have to look it up.
const F_SETFD: i32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture page is a `file://` URL. The session opts in explicitly; production never calls this.
    fn allow_local(state: &std::path::Path) {
        let key = session_key(state);
        let mut held = match SESSIONS.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(session) = held.get_mut(&key) {
            session.session.policy = BrowserPolicy::permit_local();
            session.last_used = Instant::now();
        } else {
            match Session::start_with(state, BrowserPolicy::permit_local()) {
                Ok(session) => {
                    held.insert(
                        key,
                        Held {
                            session,
                            last_used: Instant::now(),
                        },
                    );
                }
                Err(said) => panic!("the fixture browser did not start: {said}"),
            }
        }
    }

    static LIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the live-browser lock and tell the product this host cannot supply a Chromium sandbox.
    ///
    /// Unprivileged user namespaces are refused here (`kernel.apparmor_restrict_unprivileged_userns` is 1),
    /// so the fixture opts out explicitly and the product default stays sandboxed. Taking both in one place
    /// is what keeps a live test from failing for a reason that belongs to the host.
    /// A state directory short enough for Chromium's socket.
    ///
    /// The cargo target directory is deep (`…/hades-target/scratch/process-N/…`), and Chromium aborts with a
    /// bare `SIGTRAP` when its socket path passes 107 bytes. The test state lives in `/tmp` so the suite
    /// tests the browser rather than the length of the build directory; `chromium_path_check` is what refuses
    /// a long state in production, and it has its own test.
    fn live_state(name: &str) -> PathBuf {
        let state =
            PathBuf::from("/tmp").join(format!("hades-browser-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).expect("a live state");
        state
    }

    /// Set an environment variable for the length of a test and put it back on the way out, including when
    /// the test panics. A leaked `HADES_HEADED` made every later live test launch headed Chromium with no
    /// display, and the failures read as browser bugs rather than as one test's leftovers.
    struct EnvVar(&'static str, Option<std::ffi::OsString>);

    impl EnvVar {
        fn set(name: &'static str, value: &str) -> EnvVar {
            let previous = std::env::var_os(name);
            // SAFETY: every live test holds LIVE, so no other test reads or writes the environment here.
            unsafe {
                std::env::set_var(name, value);
            }
            EnvVar(name, previous)
        }
    }

    impl Drop for EnvVar {
        fn drop(&mut self) {
            // SAFETY: same lock as above.
            unsafe {
                match &self.1 {
                    Some(value) => std::env::set_var(self.0, value),
                    None => std::env::remove_var(self.0),
                }
            }
        }
    }

    fn live() -> std::sync::MutexGuard<'static, ()> {
        // SAFETY: every test that starts a browser holds this lock, and nothing reads the variable while
        // another test runs.
        unsafe {
            std::env::set_var("HADES_BROWSER_NO_SANDBOX", "1");
        }
        LIVE.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    #[test]
    fn chromium_keeps_its_sandbox_by_default() {
        let args = launch_args(false, false);
        assert!(!args.iter().any(|arg| arg == "--no-sandbox"), "{args:?}");
    }

    #[test]
    fn the_host_can_opt_out_of_the_sandbox_explicitly() {
        // Chromium's sandbox needs unprivileged user namespaces, which some hosts refuse. Opting out is a
        // decision a host makes out loud, not the shape the product ships with.
        let args = launch_args(false, true);
        assert!(args.iter().any(|arg| arg == "--no-sandbox"), "{args:?}");
        let headed = launch_args(true, false);
        assert!(
            !headed.contains(&"--headless=new".to_string()),
            "{headed:?}"
        );
    }

    #[test]
    fn every_browser_path_is_under_the_state_directory() {
        let state = crate::scratch::root().join("browser-state");
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).expect("a state directory");
        let paths = state_paths(&state).expect("state paths");
        let root = std::fs::canonicalize(state.join("browser")).expect("the browser root");
        for path in [&paths.profile, &paths.downloads, &paths.tmp] {
            assert!(
                path.starts_with(&root),
                "{} escaped {}",
                path.display(),
                root.display()
            );
        }
        // A symlink where one of the directories should be is refused rather than followed.
        std::fs::remove_dir_all(root.join("downloads")).expect("remove");
        std::os::unix::fs::symlink("/tmp", root.join("downloads")).expect("symlink");
        let said =
            state_paths(&state).expect_err("a link out of the root is not a download directory");
        assert!(said.contains("outside"), "{said}");
    }

    #[test]
    fn two_states_get_two_session_keys() {
        assert_ne!(
            session_key(Path::new("/a/state")),
            session_key(Path::new("/b/state"))
        );
        // One directory spelled two ways is one key once it exists.
        let state = crate::scratch::root().join("browser-keys");
        std::fs::create_dir_all(&state).expect("a state directory");
        let resolved = std::fs::canonicalize(&state).expect("resolves");
        assert_eq!(session_key(&resolved), session_key(&resolved));
        assert_eq!(session_key(&state), session_key(&resolved));
    }

    #[test]
    fn an_unused_session_goes_stale_at_the_idle_limit() {
        let now = Instant::now();
        assert!(!stale(now, now));
        assert!(!stale(now, now - (IDLE - Duration::from_secs(1))));
        assert!(stale(now, now - IDLE));
        assert!(stale(now, now - (IDLE + Duration::from_secs(1))));
    }

    #[test]
    fn a_state_too_deep_for_chromiums_socket_is_refused_with_the_numbers() {
        // Chromium binds `<TMPDIR>/.org.chromium.Chromium.XXXXXX/SingletonSocket`; past 107 bytes the bind
        // fails inside Chromium and it aborts without a message. The check has to happen here, where the
        // reason can be printed.
        let short = Path::new("/tmp/hades/tmp");
        assert!(chromium_path_check(short).is_ok());
        let long = Path::new(
            "/tmp/aaaaaaaaaaaaaaaaaaaa/bbbbbbbbbbbbbbbbbbbb/cccccccccccccccccccc/dddddddddddddddddddd/tmp",
        );
        let error = chromium_path_check(long).expect_err("a socket path past the limit");
        assert!(error.contains("107"), "{error}");
        assert!(error.contains("shorter state"), "{error}");
        // The boundary: exactly at the limit passes, one byte more fails.
        let room = SUN_PATH_MAX - CHROMIUM_SOCKET.len();
        let exact = format!("/tmp/{}", "x".repeat(room - 5));
        assert_eq!(exact.len() + CHROMIUM_SOCKET.len(), SUN_PATH_MAX);
        assert!(chromium_path_check(Path::new(&exact)).is_ok());
        let over = format!("/tmp/{}", "x".repeat(room - 4));
        assert!(chromium_path_check(Path::new(&over)).is_err());
    }

    /// The list has to name the packages distributions actually ship, not only the one this was built on.
    #[test]
    fn common_browser_locations_are_known() {
        for expected in [
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/google-chrome",
            "/opt/google/chrome/chrome",
            "/snap/bin/chromium",
        ] {
            assert!(
                KNOWN_BROWSERS.contains(&expected),
                "{expected} is missing from the search"
            );
        }
        assert!(PATH_BROWSERS.contains(&"chromium"));
    }

    #[test]
    fn a_screenshot_decodes() {
        let bytes = base64_decode("aGVsbG8=").expect("base64");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn a_screenshot_cannot_escape_the_shots_directory() {
        let state = crate::scratch::root().join("shots-confined");
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).expect("a scratch state");
        let error =
            resolve_shot(&state, Some("../../outside.png")).expect_err("a traversal is refused");
        assert!(error.contains("shots"), "{error}");
        let error = resolve_shot(&state, Some("/tmp/outside.png"))
            .expect_err("an absolute path is refused");
        assert!(error.contains("shots"), "{error}");
        let inside = resolve_shot(&state, Some("sub/page.png")).expect("an inside path");
        let shots = std::fs::canonicalize(state.join("shots")).expect("the shots directory");
        assert!(inside.starts_with(&shots), "{}", inside.display());
    }

    /// A symlink is not a name inside the directory, whatever it points at: one at the final name and one
    /// standing in for a subdirectory both have to be refused, and the kernel has to refuse the write too.
    #[test]
    fn a_screenshot_cannot_be_written_through_a_symlink() {
        let state = crate::scratch::root().join("shots-symlink");
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).expect("a scratch state");
        let outside = state.join("outside.png");
        std::fs::write(&outside, b"untouched").expect("a file outside shots");

        let shots = state.join("shots");
        std::fs::create_dir_all(&shots).expect("the shots directory");
        let link = shots.join("page.png");
        std::os::unix::fs::symlink(&outside, &link).expect("a symlink at the file name");
        let error = resolve_shot(&state, Some("page.png"))
            .expect_err("a symlink at the final name is refused");
        assert!(error.contains("symlink"), "{error}");

        // A directory that is really a symlink out of the shots tree is the same trick one level up.
        let elsewhere = state.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("a directory outside shots");
        std::os::unix::fs::symlink(&elsewhere, shots.join("sub")).expect("a symlinked directory");
        let error = resolve_shot(&state, Some("sub/page.png"))
            .expect_err("a symlinked directory is refused");
        assert!(error.contains("shots"), "{error}");

        // And the write itself: even if a link appears after the check, `O_NOFOLLOW` refuses it.
        let error =
            write_no_follow(&link, b"not the target").expect_err("a symlink is not followed");
        assert!(
            error.contains("symlink") || error.contains("Too many"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(&outside).expect("the target is still there"),
            b"untouched"
        );
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    #[ignore = "starts a real Chromium; run with --ignored"]
    fn a_live_page_is_observable() {
        let _guard = live();
        let state = live_state("live");
        let opened = tool(
            &json!({"action": "open", "url": "https://example.com"}),
            &state,
        );
        assert!(opened.contains("title: Example Domain"), "{opened}");
        assert!(opened.contains("interactive:"), "{opened}");
        let observed = tool(&json!({"action": "observe"}), &state);
        assert!(observed.contains("https://example.com"), "{observed}");
        let _ = tool(&json!({"action": "close"}), &state);
    }

    #[test]
    #[ignore = "starts a real Chromium; run with --ignored"]
    fn a_live_page_can_be_clicked_and_typed() {
        let _guard = live();
        let state = live_state("live-actions");
        allow_local(&state);
        let page = state.join("page.html");
        std::fs::write(
            &page,
            r#"<title>inner</title><button onclick="document.title='clicked'">Send</button><input id=q>"#,
        )
        .expect("a page");
        let url = format!("file://{}", page.display());
        let opened = tool(&json!({"action": "open", "url": url}), &state);
        assert!(opened.contains("button \"Send\""), "{opened}");
        assert!(opened.contains("textbox"), "{opened}");
        let clicked = tool(&json!({"action": "click", "text": "Send"}), &state);
        assert!(clicked.contains("title: clicked"), "{clicked}");
        let reopened = tool(&json!({"action": "open", "url": url}), &state);
        assert!(reopened.contains("e1 send"), "{reopened}");
        let by_ref = tool(&json!({"action": "click", "ref": "e1 send"}), &state);
        assert!(by_ref.contains("title: clicked"), "{by_ref}");
        let typed = tool(
            &json!({"action": "type", "selector": "#q", "text": "hello"}),
            &state,
        );
        assert!(typed.contains("value=\"hello\""), "{typed}");
        let typed_by_ref = tool(
            &json!({"action": "type", "ref": "e2", "text": "world"}),
            &state,
        );
        assert!(typed_by_ref.contains("value=\"world\""), "{typed_by_ref}");
        let stable = tool(&json!({"action": "observe"}), &state);
        assert!(stable.contains("[e1 send]"), "{stable}");
        let waited = tool(
            &json!({"action": "wait", "text": "Send", "timeout_s": 3}),
            &state,
        );
        assert!(waited.contains("title: clicked"), "{waited}");
        let stale = tool(&json!({"action": "click", "ref": "e999 gone"}), &state);
        assert!(stale.contains("that ref is not on the page"), "{stale}");
        assert!(stale.contains("title: clicked"), "{stale}");
        let bad = tool(
            &json!({"action": "eval", "script": "throw new Error('boom')"}),
            &state,
        );
        assert!(bad.contains("the script failed"), "{bad}");
        assert!(bad.contains("title: clicked"), "{bad}");
        let shot = state.join("shots").join("shot.png");
        let saved = tool(&json!({"action": "screenshot", "path": "shot.png"}), &state);
        assert!(saved.contains("saved to"), "{saved}");
        let bytes = std::fs::read(&shot).expect("a screenshot");
        assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']), "PNG magic");

        let _ = tool(&json!({"action": "close"}), &state);
    }

    #[test]
    #[ignore = "starts a real Chromium; run with --ignored"]
    fn a_popup_becomes_a_tab_the_model_can_switch_to() {
        let _guard = live();
        let state = live_state("live-tabs");
        allow_local(&state);
        let second = state.join("second.html");
        std::fs::write(&second, "<title>second</title><p>second page</p>").expect("a page");
        let first = state.join("first.html");
        std::fs::write(
            &first,
            format!(
                "<title>first</title><a href=\"{}\" target=\"_blank\">Open second</a>",
                second.display()
            ),
        )
        .expect("a page");
        let opened = tool(
            &json!({"action": "open", "url": format!("file://{}", first.display())}),
            &state,
        );
        assert!(opened.contains("title: first"), "{opened}");
        let clicked = tool(&json!({"action": "click", "text": "Open second"}), &state);
        assert!(clicked.contains("a new tab opened"), "{clicked}");
        let listed = tool(&json!({"action": "tabs"}), &state);
        assert!(listed.contains("[1]"), "{listed}");
        let switched = tool(&json!({"action": "tab", "index": 1}), &state);
        assert!(switched.contains("title: second"), "{switched}");
        let closed = tool(&json!({"action": "close_tab", "index": 1}), &state);
        assert!(closed.contains("title: first"), "{closed}");
        let last = tool(&json!({"action": "close_tab", "index": 0}), &state);
        assert!(last.contains("last tab cannot close"), "{last}");
        let _ = tool(&json!({"action": "close"}), &state);
    }

    #[test]
    #[ignore = "starts a real Chromium; run with --ignored"]
    fn a_dialog_is_handled_and_an_upload_lands() {
        let _guard = live();
        let state = live_state("live-dialog");
        allow_local(&state);
        let page = state.join("page.html");
        std::fs::write(
            &page,
            "<title>dialog</title><button onclick=\"alert('hello')\">Alert</button><input id=f type=file>",
        )
        .expect("a page");
        let url = format!("file://{}", page.display());
        let opened = tool(&json!({"action": "open", "url": url}), &state);
        assert!(opened.contains("title: dialog"), "{opened}");
        let clicked = tool(&json!({"action": "click", "text": "Alert"}), &state);
        assert!(clicked.contains("dialog: alert"), "{clicked}");
        assert!(clicked.contains("hello"), "{clicked}");
        let reference = last_ref(&opened);
        let upload = state.join("upload.txt");
        std::fs::write(&upload, "payload").expect("a file");
        let uploaded = tool(
            &json!({"action": "upload", "ref": reference, "path": upload.display().to_string()}),
            &state,
        );
        assert!(uploaded.contains("uploaded upload.txt"), "{uploaded}");
        let count = tool(
            &json!({"action": "eval", "script": "document.querySelector('#f').files.length"}),
            &state,
        );
        assert_eq!(count.trim(), "1", "{count}");
        let _ = tool(&json!({"action": "close"}), &state);
    }

    fn first_ref(observation: &str) -> String {
        let start = observation.find('[').expect("a ref") + 1;
        let end = observation[start..].find(']').expect("a ref") + start;
        observation[start..end].to_string()
    }

    fn last_ref(observation: &str) -> String {
        let mut found = None;
        let mut rest = observation;
        while let Some(start) = rest.find('[') {
            let after = &rest[start + 1..];
            let Some(end) = after.find(']') else { break };
            found = Some(after[..end].to_string());
            rest = &after[end + 1..];
        }
        found.expect("a ref")
    }

    #[test]
    #[ignore = "starts a real Chromium; run with --ignored"]
    fn a_download_lands_in_the_state_directory() {
        let _guard = live();
        let state = live_state("live-download");
        allow_local(&state);
        let page = state.join("page.html");
        std::fs::write(
            &page,
            "<title>download</title><a download=\"sample.txt\" href=\"data:text/plain,hello\">Download</a>",
        )
        .expect("a page");
        let url = format!("file://{}", page.display());
        let opened = tool(&json!({"action": "open", "url": url}), &state);
        assert!(opened.contains("title: download"), "{opened}");
        let clicked = tool(&json!({"action": "click", "text": "Download"}), &state);
        assert!(clicked.contains("clicked"), "{clicked}");
        let mut listed = String::new();
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            listed = tool(&json!({"action": "downloads"}), &state);
            if listed.contains("sample.txt") {
                break;
            }
        }
        assert!(listed.contains("sample.txt"), "{listed}");
        let _ = tool(&json!({"action": "close"}), &state);
    }

    #[test]
    #[ignore = "starts a headed Chromium under a display; run with xvfb-run"]
    fn a_headed_browser_opens_a_page() {
        let _guard = live();
        let _headed = EnvVar::set("HADES_HEADED", "1");
        let state = live_state("live-headed");
        let opened = tool(
            &json!({"action": "open", "url": "https://example.com"}),
            &state,
        );
        assert!(opened.contains("title: Example Domain"), "{opened}");
        let _ = tool(&json!({"action": "close"}), &state);
    }

    #[test]
    #[ignore = "starts a real Chromium and a cross-origin server; run with --ignored"]
    fn a_cross_origin_frame_can_be_observed_and_clicked() {
        let _guard = live();
        let state = live_state("live-oopif");
        let site = state.join("site");
        std::fs::create_dir_all(&site).expect("a site");
        std::fs::write(
            site.join("inner.html"),
            r#"<title>inner</title><button onclick="document.title='clicked'">Inside</button>"#,
        )
        .unwrap();
        std::fs::write(
            site.join("outer.html"),
            r#"<title>outer</title><iframe src="http://127.0.0.2:PORT/inner.html"></iframe>"#,
        )
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let outer = std::fs::read_to_string(site.join("outer.html"))
            .unwrap()
            .replace("PORT", &port.to_string());
        std::fs::write(site.join("outer.html"), outer).unwrap();
        let mut server = std::process::Command::new("python3")
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "0.0.0.0",
                "--directory",
                site.to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        let url = format!("http://127.0.0.1:{port}/outer.html");
        let opened = tool(&json!({"action": "open", "url": url}), &state);
        assert!(opened.contains("title: outer"), "{opened}");
        std::thread::sleep(std::time::Duration::from_millis(700));
        let frames = tool(&json!({"action": "frames"}), &state);
        assert!(frames.contains("inner"), "{frames}");
        let inner = tool(&json!({"action": "frame", "index": 1}), &state);
        assert!(inner.contains("title: inner"), "{inner}");
        assert!(inner.contains("button \"Inside\""), "{inner}");
        let reference = first_ref(&inner);
        let clicked = tool(&json!({"action": "click", "ref": reference}), &state);
        assert!(clicked.contains("title: clicked"), "{clicked}");
        let main = tool(&json!({"action": "frame", "frame": "main"}), &state);
        assert!(main.contains("title: outer"), "{main}");
        let _ = tool(&json!({"action": "close"}), &state);
        let _ = server.kill();
        let _ = server.wait();
    }

    #[test]
    #[ignore = "starts a real Chromium and an authenticated proxy; run with --ignored"]
    fn proxy_credentials_are_answered() {
        use std::io::Read;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let _guard = live();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a proxy port");
        let port = listener.local_addr().expect("an address").port();
        listener.set_nonblocking(true).expect("a nonblocking proxy");
        let running = Arc::new(AtomicBool::new(true));
        let stop = running.clone();
        let proxy = std::thread::spawn(move || {
            while stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buffer = [0u8; 4096];
                        let read = stream.read(&mut buffer).unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                        let authorized = request
                            .to_ascii_lowercase()
                            .contains("proxy-authorization: basic ");
                        let (status, extra, body) = if authorized {
                            (
                                "200 OK",
                                "",
                                "<title>proxied</title><h1>Proxy</h1>".to_string(),
                            )
                        } else {
                            (
                                "407 Proxy Authentication Required",
                                "Proxy-Authenticate: Basic realm=\"hades\"\r\n",
                                String::new(),
                            )
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\n{extra}Content-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        unsafe {
            std::env::set_var("HADES_PROXY", format!("http://user:pass@127.0.0.1:{port}"));
        }
        let state = live_state("live-proxy");
        let opened = tool(
            &json!({"action": "open", "url": "http://proxy.test/"}),
            &state,
        );
        assert!(opened.contains("title: proxied"), "{opened}");
        let _ = tool(&json!({"action": "close"}), &state);
        unsafe {
            std::env::remove_var("HADES_PROXY");
        }
        running.store(false, Ordering::SeqCst);
        let _ = proxy.join();
    }

    #[test]
    #[ignore = "starts a real Chromium; run with --ignored"]
    fn a_browser_context_isolates_its_tab() {
        let _guard = live();
        let state = live_state("live-context");
        allow_local(&state);
        let page = state.join("page.html");
        std::fs::write(&page, "<title>default</title><p>default</p>").expect("a page");
        let opened = tool(
            &json!({"action": "open", "url": format!("file://{}", page.display())}),
            &state,
        );
        assert!(opened.contains("title: default"), "{opened}");
        // `data:` is refused by the scheme policy; an isolated context is tested with a page on disk, which is
        // the same surface a real second context would open.
        let other = state.join("other.html");
        std::fs::write(&other, "<title>context</title><p>context</p>").expect("a page");
        let context = tool(
            &json!({"action": "new_context", "url": format!("file://{}", other.display())}),
            &state,
        );
        assert!(context.contains("title: context"), "{context}");
        let contexts = tool(&json!({"action": "contexts"}), &state);
        assert!(contexts.contains("default: 1 tabs"), "{contexts}");
        let id = first_context(&contexts);
        assert!(!id.is_empty(), "{contexts}");
        let closed = tool(&json!({"action": "close_context", "context": id}), &state);
        assert!(closed.contains("title: default"), "{closed}");
        let default = tool(
            &json!({"action": "close_context", "context": "default"}),
            &state,
        );
        assert!(
            default.contains("default context cannot close"),
            "{default}"
        );
        let _ = tool(&json!({"action": "close"}), &state);
    }

    fn first_context(contexts: &str) -> String {
        for line in contexts.lines() {
            let line = line.trim();
            if line.starts_with("  ") || line.is_empty() || line.starts_with("contexts:") {
                continue;
            }
            if let Some((id, _)) = line.split_once(':')
                && !id.trim().is_empty()
                && id.trim() != "default"
            {
                return id.trim().to_string();
            }
        }
        String::new()
    }

    #[test]
    #[ignore = "starts a real Chromium; run with --ignored"]
    fn a_same_origin_frame_can_be_observed_and_clicked() {
        let _guard = live();
        let state = live_state("live-iframe");
        allow_local(&state);
        std::fs::write(
            state.join("inner.html"),
            r#"<title>inner</title><button onclick="document.title='clicked'">Inside</button>"#,
        )
        .unwrap();
        std::fs::write(
            state.join("outer.html"),
            r#"<title>outer</title><iframe name=inner src="inner.html"></iframe>"#,
        )
        .unwrap();
        let opened = tool(
            &json!({"action": "open", "url": format!("file://{}", state.join("outer.html").display())}),
            &state,
        );
        assert!(opened.contains("title: outer"), "{opened}");
        assert!(opened.contains("interactive:\n  none"), "{opened}");
        let frames = tool(&json!({"action": "frames"}), &state);
        assert!(frames.contains("inner"), "{frames}");
        let inner = tool(&json!({"action": "frame", "index": 1}), &state);
        assert!(inner.contains("title: inner"), "{inner}");
        assert!(inner.contains("button \"Inside\""), "{inner}");
        let reference = first_ref(&inner);
        let clicked = tool(&json!({"action": "click", "ref": reference}), &state);
        assert!(clicked.contains("title: clicked"), "{clicked}");
        let main = tool(&json!({"action": "frame", "frame": "main"}), &state);
        assert!(main.contains("title: outer"), "{main}");
        let _ = tool(&json!({"action": "close"}), &state);
    }

    #[test]
    #[ignore = "starts a real Chromium; run with --ignored"]
    fn humanized_refs_click_and_type() {
        let _guard = live();
        unsafe {
            std::env::set_var("HADES_HUMANIZE", "1");
        }
        let state = live_state("live-human");
        allow_local(&state);
        let page = state.join("page.html");
        std::fs::write(
            &page,
            r#"<title>inner</title><button onclick="document.title='clicked'">Inside</button><input id=q>"#,
        )
        .expect("a page");
        let opened = tool(
            &json!({"action": "open", "url": format!("file://{}", page.display())}),
            &state,
        );
        let button = first_ref(&opened);
        let clicked = tool(&json!({"action": "click", "ref": button}), &state);
        assert!(clicked.contains("title: clicked"), "{clicked}");
        let input = last_ref(&clicked);
        let long = "human".repeat(120);
        let typed = tool(
            &json!({"action": "type", "ref": input, "text": long}),
            &state,
        );
        assert!(typed.contains("typed into"), "{typed}");
        let length = tool(
            &json!({"action": "eval", "script": "document.querySelector('#q').value.length"}),
            &state,
        );
        assert_eq!(length.trim(), "600", "{length}");
        let _ = tool(&json!({"action": "close"}), &state);
        unsafe {
            std::env::remove_var("HADES_HUMANIZE");
        }
    }

    #[test]
    #[ignore = "starts a real Chromium and a local HTTP server; run with --ignored"]
    fn network_storage_and_cookies_are_visible() {
        let _guard = live();
        let state = live_state("live-network");
        let site = state.join("site");
        std::fs::create_dir_all(&site).expect("a site");
        std::fs::write(
            site.join("index.html"),
            "<title>net</title><h1>Net</h1><script>localStorage.setItem('seed','one')</script><img src=\"/missing.png\">",
        )
        .expect("a page");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().expect("an address").port();
        drop(listener);
        let mut server = std::process::Command::new("python3")
            .args([
                "-m",
                "http.server",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
                "--directory",
                site.to_str().expect("a path"),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("a server");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let base = format!("http://127.0.0.1:{port}/");
        let opened = tool(&json!({"action": "open", "url": base}), &state);
        assert!(opened.contains("title: net"), "{opened}");

        let network = tool(&json!({"action": "network"}), &state);
        assert!(network.contains("GET"), "{network}");
        let request_id = first_request_id(&network);
        let body = tool(&json!({"action": "network_body", "id": request_id}), &state);
        assert!(body.contains("Net"), "{body}");

        let storage = tool(&json!({"action": "storage"}), &state);
        assert!(storage.contains("seed"), "{storage}");
        let _ = tool(
            &json!({"action": "storage_set", "key": "two", "value": "2"}),
            &state,
        );
        let storage = tool(&json!({"action": "storage"}), &state);
        assert!(storage.contains("two"), "{storage}");
        let _ = tool(&json!({"action": "storage_clear"}), &state);

        let cookie = tool(
            &json!({"action": "cookie_set", "name": "session", "value": "abc", "url": base}),
            &state,
        );
        assert!(cookie.contains("cookie set"), "{cookie}");
        let cookies = tool(&json!({"action": "cookies"}), &state);
        assert!(cookies.contains("session=abc"), "{cookies}");
        let _ = tool(&json!({"action": "cookie_clear"}), &state);

        let blocked = tool(&json!({"action": "block", "text": "*missing.png*"}), &state);
        assert!(blocked.contains("blocked"), "{blocked}");
        let _ = tool(&json!({"action": "open", "url": base}), &state);
        let network = tool(&json!({"action": "network"}), &state);
        assert!(network.contains("missing.png"), "{network}");
        assert!(network.contains("blocked"), "{network}");
        let _ = tool(&json!({"action": "unblock"}), &state);

        let _ = tool(&json!({"action": "close"}), &state);
        let _ = server.kill();
        let _ = server.wait();
    }

    fn first_request_id(network: &str) -> String {
        for line in network.lines() {
            if line.contains(" GET ") {
                return line.split_whitespace().next().unwrap_or("").to_string();
            }
        }
        String::new()
    }

    #[test]
    fn a_ref_slug_keeps_the_words_the_floor_reads() {
        assert_eq!(slug("Buy now!"), "buy now");
        assert_eq!(slug("  Send  message  "), "send message");
        assert_eq!(
            slug("A very long button label that runs on"),
            "a very long button"
        );
    }

    #[test]
    fn a_patched_binary_is_recognised() {
        assert!(is_patched(Path::new(
            "/opt/.cloakbrowser/chromium-146/chrome"
        )));
        assert!(!is_patched(Path::new("/usr/bin/google-chrome")));
    }
}
