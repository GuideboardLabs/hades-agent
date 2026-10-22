//! Vision: a local pair of model files that can look at a picture and answer a question about it.
//!
//! The chat model is text-only, so seeing is a separate call to a separate model. This module owns that
//! call and the server in front of it: Qwen3-VL 8B in GGUF with its projector, served by llama.cpp on a
//! loopback port, started on first use and kept for the life of the process. It is a file dependency on
//! this machine's model tree, not a service the gateway expects to already be running.

use serde_json::{Value, json};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The Vulkan build, because the CPU build turns one screenshot into a minute of work. Override with
/// HADES_VISION_BIN when the build moves.
const DEFAULT_BINARY: &str = "llama-server";
const DEFAULT_MODEL: &str = "models/Qwen3VL-8B-Instruct-Q4_K_M.gguf";
const DEFAULT_PROJECTOR: &str = "models/mmproj-Qwen3VL-8B-Instruct-Q8_0.gguf";
const PORT: u16 = 8123;

/// The transient unit that runs the local server. It is named after the port, so two processes using
/// different ports do not stop each other's server: one unit per endpoint, not one per user.
fn unit() -> String {
    format!("hades-vision-{}", port())
}

/// The port the local server listens on. A host that already has something on 8123, or that wants two
/// models at once, sets `HADES_VISION_PORT`; the default stays where every earlier install expects it.
fn port() -> u16 {
    std::env::var("HADES_VISION_PORT")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(PORT)
}

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_string())
}

static SERVER: Mutex<Option<Child>> = Mutex::new(None);

/// A configured OpenAI-compatible endpoint for the image tool.
struct Remote {
    url: String,
    model: String,
    key_env: Option<String>,
}

/// Where the image tool sends its question when it is not starting a local model.
///
/// The environment wins over the config so one run can point at another server without editing a file;
/// the config is the durable form. Nothing here reads a key value: `key_env` names the variable.
fn remote() -> Result<Option<Remote>, String> {
    if let Ok(url) = std::env::var("HADES_VISION_URL") {
        let url = url.trim().to_string();
        if !url.is_empty() {
            let model = std::env::var("HADES_VISION_MODEL_NAME").map_err(|_| {
                "HADES_VISION_URL is set but HADES_VISION_MODEL_NAME is not: an endpoint has to say which \
                 model to ask for"
                    .to_string()
            })?;
            let key_env = std::env::var("HADES_VISION_KEY_ENV")
                .ok()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty());
            return Ok(Some(Remote {
                url,
                model,
                key_env,
            }));
        }
    }
    let home = crate::boundary::hades_home()?;
    let path = home.join(crate::config::NAME);
    let config = crate::config::load(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    Ok(config
        .and_then(|config| config.vision)
        .map(|vision| Remote {
            url: vision.url,
            model: vision.model,
            key_env: vision.key_env,
        }))
}

/// Look at one image and answer one question about it.
pub fn describe(path: &Path, question: &str) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if bytes.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    // A full screenshot costs the encoder seconds per thousand pixels; 768 px wide keeps ordinary interface
    // text legible and stays near four seconds on the card. The original is untouched.
    let (bytes, picture) = shrink(path).unwrap_or((bytes, path.to_path_buf()));
    let mime = match picture
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "image/jpeg",
    };
    let data = base64(&bytes);

    let configured = remote()?;
    let (url, model, key_env) = match configured {
        Some(remote) => (remote.url, remote.model, remote.key_env),
        None => {
            ensure_server()?;
            (
                format!("http://127.0.0.1:{}/v1/chat/completions", port()),
                "qwen3vl".to_string(),
                None,
            )
        }
    };

    let body = json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": question},
                {"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{data}")}}
            ]
        }],
        "max_tokens": 500,
        "temperature": 0.2
    });
    let mut headers = vec![("Content-Type", "application/json".to_string())];
    if let Some(name) = key_env {
        let key = crate::model::secret(&name)?;
        headers.push(("Authorization", format!("Bearer {key}")));
    }
    let reply = crate::http::post(&url, &body.to_string(), &headers, Duration::from_secs(240))
        .map_err(|fault| format!("the vision server did not answer: {fault:?}"))?;
    if !reply.ok() {
        return Err(format!(
            "the vision server answered {}: {}",
            reply.status,
            clip(&reply.body, 300)
        ));
    }
    let parsed: Value = serde_json::from_str(&reply.body)
        .map_err(|error| format!("the vision server answered with something else: {error}"))?;
    parsed
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .ok_or_else(|| "the vision server answered with nothing".to_string())
}

fn ensure_server() -> Result<(), String> {
    let mut held = match SERVER.lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    if healthy() {
        if serving_configured_model() {
            return Ok(());
        }
        // Something answers on this port, but it is not the model this process was told to use. Stop the
        // unit this process owns and start the configured one: a health endpoint says the process is up,
        // not which weights it has.
        stop_unit();
    }
    if let Some(mut child) = held.take() {
        let _ = child.kill();
    }
    let binary = env_or("HADES_VISION_BIN", DEFAULT_BINARY);
    let model = env_or("HADES_VISION_MODEL", DEFAULT_MODEL);
    let projector = env_or("HADES_VISION_PROJECTOR", DEFAULT_PROJECTOR);
    if !Path::new(&model).exists() || !Path::new(&projector).exists() {
        return Err(format!(
            "the vision models are not configured: set HADES_VISION_MODEL and HADES_VISION_PROJECTOR (expected {model} and {projector})"
        ));
    }
    let _ = Command::new("systemctl")
        .args(["--user", "stop", &unit()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let child = Command::new("systemd-run")
        .args([
            "--user",
            "--unit",
            &unit(),
            "--collect",
            "-q",
            "-p",
            "MemoryMax=22G",
            "--",
        ])
        .arg(&binary)
        .arg("-m")
        .arg(&model)
        .arg("--mmproj")
        .arg(&projector)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port().to_string())
        .arg("-c")
        .arg("4096")
        .arg("-ngl")
        .arg("99")
        // The name is llama.cpp's, not a spelling of ours: `VIDMEM`, and the typo cost nothing at
        // compile time and everything at run time.
        .env("GGML_VK_DISABLE_HOST_VISIBLE_VIDMEM", "1")
        .arg("--no-warmup")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start {binary}: {error}"))?;
    // `systemd-run` returns as soon as the transient unit exists. Exit 0 means the unit is starting, not
    // that the server has answered, so only a non-zero exit is a failed launch. Once it has exited there
    // is no child to keep, and the unit is stopped by name if the wait runs out.
    let mut child = Some(child);
    let started = Instant::now();
    let mut checked_unit = Instant::now();
    loop {
        if healthy() {
            if strict_identity() && served_model().is_none() {
                // The server answers but will not say which model it is, and the operator asked for
                // confirmed identity: stop it rather than reuse an endpoint nobody can name.
                stop_unit();
                return Err(format!(
                    "the server on port {} answers but does not report which model it loaded, and \
                     HADES_VISION_STRICT is set",
                    port()
                ));
            }
            if let Some(child) = child.take() {
                *held = Some(child);
            }
            return Ok(());
        }
        let exited = match child.as_mut() {
            Some(running) => match running.try_wait() {
                Ok(Some(status)) => Some((status, running.stderr.take())),
                _ => None,
            },
            None => None,
        };
        if let Some((status, pipe)) = exited {
            child = None;
            if !status.success() {
                let said = pipe
                    .and_then(|mut pipe| {
                        use std::io::Read;
                        let mut text = String::new();
                        pipe.read_to_string(&mut text).ok().map(|_| text)
                    })
                    .unwrap_or_default();
                return Err(format!(
                    "the vision server was not started: systemd-run exited {}: {}",
                    status.code().unwrap_or(-1),
                    clip(said.trim(), 300)
                ));
            }
        }
        // The unit can also fail after `systemd-run` returns; asking systemd is cheaper than waiting out
        // the clock, and its last lines say why.
        if checked_unit.elapsed() >= Duration::from_secs(15) {
            checked_unit = Instant::now();
            if let Some(reason) = unit_failure() {
                stop_unit();
                if let Some(mut child) = child.take() {
                    let _ = child.kill();
                }
                return Err(reason);
            }
        }
        if started.elapsed() >= Duration::from_secs(240) {
            stop_unit();
            if let Some(mut child) = child.take() {
                let _ = child.kill();
            }
            return Err("the vision server did not become healthy in four minutes".to_string());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Stop the transient unit. Used when the wait ran out and `systemd-run` is long gone: the child this
/// function started is only the launcher, and killing it would leave the server running.
fn stop_unit() {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", &unit()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Whether the server on the port is the model this process configured, as far as it says.
///
/// `/health` reports that something is listening; `/props` reports which weights it loaded. A build that
/// does not answer `/props` is accepted, because refusing it would break older servers without protecting
/// anything: the check is there for the case where two models share a port by accident.
fn serving_configured_model() -> bool {
    let Some(reported) = served_model() else {
        // A server that will not say which weights it loaded is accepted by default; with strict identity
        // on, it is refused, because an unidentified endpoint on the port is the case this check exists for.
        return !strict_identity();
    };
    let wanted = env_or("HADES_VISION_MODEL", DEFAULT_MODEL);
    let name = Path::new(&wanted)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    name.is_empty() || reported.ends_with(name)
}

/// Whether identity has to be confirmed rather than assumed. `HADES_VISION_STRICT` wins over the config
/// file, like the other vision settings.
fn strict_identity() -> bool {
    match std::env::var("HADES_VISION_STRICT") {
        Ok(value) => asked_for_strict_identity(&value),
        Err(_) => false,
    }
}

/// The spellings `HADES_VISION_STRICT` accepts. A value nobody recognises means the setting is wrong, and
/// the safe reading is the one that keeps the old behaviour rather than the one that changes it silently.
fn asked_for_strict_identity(value: &str) -> bool {
    matches!(value.trim(), "1" | "true" | "yes" | "on")
}

/// The model path the server reports, or `None` when it does not answer `/props`.
fn served_model() -> Option<String> {
    let probe = Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "2",
            &format!("http://127.0.0.1:{}/props", port()),
        ])
        .output()
        .ok()?;
    if !probe.status.success() {
        return None;
    }
    let value: Value = serde_json::from_slice(&probe.stdout).ok()?;
    value
        .get("model_path")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Why the vision unit is not running, in systemd's own words, or `None` while it is starting or up.
fn unit_failure() -> Option<String> {
    let failed = Command::new("systemctl")
        .args(["--user", "is-failed", &unit()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !failed.success() {
        return None;
    }
    let said = Command::new("journalctl")
        .args([
            "--user",
            "-u",
            &unit(),
            "-n",
            "5",
            "--no-pager",
            "-o",
            "cat",
        ])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    Some(format!(
        "the vision unit failed to start: {}",
        clip(&said, 400)
    ))
}

fn healthy() -> bool {
    // curl rather than a hand-rolled GET: the server may hold the connection, and a health probe must not
    // turn into a two-second blocking read on every poll.
    let probe = Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "2",
            &format!("http://127.0.0.1:{}/health", port()),
        ])
        .output();
    matches!(probe, Ok(out) if out.status.success() && out.stdout == b"200")
}

fn shrink(path: &Path) -> Option<(Vec<u8>, std::path::PathBuf)> {
    let target = std::env::temp_dir().join(format!(
        "hades-vision-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let status = Command::new("convert")
        .arg(path)
        .arg("-resize")
        .arg("768x")
        .arg(&target)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        let _ = std::fs::remove_file(&target);
        return None;
    }
    let bytes = std::fs::read(&target).ok()?;
    let _ = std::fs::remove_file(&target);
    if bytes.is_empty() {
        None
    } else {
        Some((bytes, target))
    }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (a << 16) | (b << 8) | c;
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_string()
    } else {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &text[..end])
    }
}

/// A test hook so the encoder is checked against a known vector rather than against itself.
pub fn base64_for_test(bytes: &[u8]) -> String {
    base64(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `systemd-run` exits 0 as soon as the unit exists, while the model is still loading. Treating that
    /// exit as a failed launch made every first local vision call fail; the wait has to continue against
    /// the health endpoint and the unit.
    #[test]
    fn a_successful_launch_keeps_waiting_for_the_model() {
        let _env = crate::test_env::hold();
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        // A port of this test's own: the default is a real service on some hosts, and a test that skips
        // when it is busy proves nothing about the branch.
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        let server = std::thread::spawn(move || {
            for (index, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer);
                // The first two probes land while the model is loading: close without an answer.
                if index < 2 {
                    continue;
                }
                let body = r#"{"status":"ok"}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
            }
        });

        let directory = std::env::temp_dir().join(format!(
            "hades-vision-cold-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or(0)
        ));
        let bin = directory.join("bin");
        std::fs::create_dir_all(&bin).expect("a bin directory");
        // A launcher that exits 0 at once, and a systemctl that reports no failed unit.
        std::fs::write(bin.join("systemd-run"), "#!/bin/sh\nexit 0\n").expect("a systemd-run");
        std::fs::write(bin.join("systemctl"), "#!/bin/sh\nexit 1\n").expect("a systemctl");
        std::fs::write(bin.join("journalctl"), "#!/bin/sh\nexit 1\n").expect("a journalctl");
        for name in ["systemd-run", "systemctl", "journalctl"] {
            let mut permissions = std::fs::metadata(bin.join(name))
                .expect("the script")
                .permissions();
            use std::os::unix::fs::PermissionsExt as _;
            permissions.set_mode(0o755);
            std::fs::set_permissions(bin.join(name), permissions).expect("an executable");
        }
        let model = directory.join("model.gguf");
        let projector = directory.join("mmproj.gguf");
        std::fs::write(&model, b"stub").expect("a model file");
        std::fs::write(&projector, b"stub").expect("a projector file");

        let old_path = std::env::var_os("PATH");
        let joined = std::env::join_paths(std::iter::once(bin.clone()).chain(
            std::env::split_paths(old_path.as_deref().unwrap_or_default()),
        ))
        .expect("a PATH");
        // SAFETY: no other test reads these variables; the stub endpoint test sets a different pair and does
        // not reach the local launcher.
        unsafe {
            std::env::set_var("PATH", &joined);
            std::env::set_var("HADES_VISION_PORT", port.to_string());
            std::env::set_var("HADES_VISION_BIN", "llama-server");
            std::env::set_var("HADES_VISION_MODEL", &model);
            std::env::set_var("HADES_VISION_PROJECTOR", &projector);
        }

        // One unit per port: two processes on different ports must not stop each other's server.
        assert_eq!(unit(), format!("hades-vision-{port}"));

        let started = Instant::now();
        let result = ensure_server();
        unsafe {
            std::env::remove_var("HADES_VISION_PORT");
            std::env::remove_var("HADES_VISION_BIN");
            std::env::remove_var("HADES_VISION_MODEL");
            std::env::remove_var("HADES_VISION_PROJECTOR");
            if let Some(path) = old_path {
                std::env::set_var("PATH", path);
            }
        }
        let waited = started.elapsed();

        assert!(
            result.is_ok(),
            "a launcher that exited 0 was treated as a failure: {result:?}"
        );
        assert!(
            waited >= Duration::from_millis(500),
            "the wait did not survive an unhealthy probe: {waited:?}"
        );
        let _ = std::fs::remove_dir_all(&directory);
        drop(server);
    }

    /// A configured endpoint is used instead of starting a local model, and the request it receives is
    /// the OpenAI vision shape. Without this, the image tool is tied to one machine's GGUF files.
    #[test]
    fn a_configured_endpoint_is_used_instead_of_a_local_server() {
        let _env = crate::test_env::hold();
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one connection");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let body_at = loop {
                let read = stream.read(&mut buffer).expect("a request");
                request.extend_from_slice(&buffer[..read]);
                if let Some(at) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break at + 4;
                }
                if read == 0 {
                    panic!("the client closed before finishing its headers");
                }
            };
            let head = String::from_utf8_lossy(&request[..body_at]).to_string();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .expect("a content length");
            while request.len() < body_at + length {
                let read = stream.read(&mut buffer).expect("a body");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let body = String::from_utf8_lossy(&request[body_at..]).to_string();
            let reply = r#"{"choices":[{"message":{"content":"a stub answer"}}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                reply.len(),
                reply
            )
            .expect("a reply");
            body
        });

        let directory = std::env::temp_dir().join(format!(
            "hades-vision-stub-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&directory).expect("a directory");
        let image = directory.join("shot.png");
        std::fs::write(&image, b"not really a png, which the stub does not mind")
            .expect("an image");

        // SAFETY: no test in this binary reads these variables concurrently: the only other vision test
        // checks the encoder, and the image tool's failure test never reaches the endpoint lookup.
        unsafe {
            std::env::set_var(
                "HADES_VISION_URL",
                format!("http://127.0.0.1:{port}/v1/chat/completions"),
            );
            std::env::set_var("HADES_VISION_MODEL_NAME", "stub-vision");
        }
        let said = describe(&image, "what is in this picture").expect("an answer");
        unsafe {
            std::env::remove_var("HADES_VISION_URL");
            std::env::remove_var("HADES_VISION_MODEL_NAME");
        }

        assert_eq!(said, "a stub answer");
        let body = handle.join().expect("the stub thread");
        assert!(body.contains("\"model\":\"stub-vision\""), "{body}");
        assert!(body.contains("image_url"), "{body}");
        assert!(body.contains("data:image/png;base64,"), "{body}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn strict_identity_is_asked_for_by_name() {
        for yes in ["1", "true", "yes", "on", " on "] {
            assert!(asked_for_strict_identity(yes), "{yes:?}");
        }
        for no in ["0", "false", "no", "", "strict"] {
            assert!(!asked_for_strict_identity(no), "{no:?}");
        }
    }

    #[test]
    fn the_encoder_matches_the_known_vector() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
    }
}
