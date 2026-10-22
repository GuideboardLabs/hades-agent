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

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_string())
}

static SERVER: Mutex<Option<Child>> = Mutex::new(None);

/// Look at one image and answer one question about it.
pub fn describe(path: &Path, question: &str) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if bytes.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    ensure_server()?;
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
    let body = json!({
        "model": "qwen3vl",
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
    let reply = crate::http::post(
        &format!("http://127.0.0.1:{PORT}/v1/chat/completions"),
        &body.to_string(),
        &[("Content-Type", "application/json".to_string())],
        Duration::from_secs(240),
    )
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
        return Ok(());
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
        .args(["--user", "stop", "hades-vision"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let child = Command::new("systemd-run")
        .args([
            "--user",
            "--unit",
            "hades-vision",
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
        .arg(PORT.to_string())
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
    let mut child = child;
    let started = Instant::now();
    let mut checked_unit = Instant::now();
    loop {
        if healthy() {
            *held = Some(child);
            return Ok(());
        }
        // `systemd-run` exits as soon as the transient unit is started, so a non-zero status here is the
        // launch itself failing: no user manager, no binary, a rejected property. Waiting four minutes
        // for a server that was never started is the bug this replaces.
        if let Ok(Some(status)) = child.try_wait() {
            let said = child
                .stderr
                .take()
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
        // The unit can also fail after `systemd-run` returns; asking systemd is cheaper than waiting out
        // the clock, and its last lines say why.
        if checked_unit.elapsed() >= Duration::from_secs(15) {
            checked_unit = Instant::now();
            if let Some(reason) = unit_failure() {
                let _ = child.kill();
                return Err(reason);
            }
        }
        if started.elapsed() >= Duration::from_secs(240) {
            let _ = child.kill();
            return Err("the vision server did not become healthy in four minutes".to_string());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Why the vision unit is not running, in systemd's own words, or `None` while it is starting or up.
fn unit_failure() -> Option<String> {
    let failed = Command::new("systemctl")
        .args(["--user", "is-failed", "hades-vision"])
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
            "hades-vision",
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
            "http://127.0.0.1:8123/health",
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

    #[test]
    fn the_encoder_matches_the_known_vector() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
    }
}
