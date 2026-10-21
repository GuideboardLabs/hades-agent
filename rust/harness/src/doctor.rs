//! `hades doctor`: what this install is, checked without a model call and without changing anything.
//!
//! A product needs one command that answers "is this thing set up" before a person debugs a run. Every
//! check here reads; none of them writes, restarts, or calls out. A warning is something a fresh home is
//! allowed to have; a failure is something that will stop a run.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub name: String,
    pub detail: String,
}

impl Finding {
    fn ok(name: &str, detail: impl Into<String>) -> Finding {
        Finding {
            level: Level::Ok,
            name: name.to_string(),
            detail: detail.into(),
        }
    }
    fn warn(name: &str, detail: impl Into<String>) -> Finding {
        Finding {
            level: Level::Warn,
            name: name.to_string(),
            detail: detail.into(),
        }
    }
    fn fail(name: &str, detail: impl Into<String>) -> Finding {
        Finding {
            level: Level::Fail,
            name: name.to_string(),
            detail: detail.into(),
        }
    }
}

/// What the cage can and cannot contain on this kernel, the safety floor's own claim, and any host socket
/// the cage cannot close. Read-only: nothing here opens a socket or changes a rule.
pub fn security_findings(root: &Path) -> Vec<Finding> {
    let mut found = Vec::new();
    let cage = crate::cage::Cage::around(root);
    found.push(Finding::ok("cage", cage.report()));
    found.push(Finding::ok(
        "safety floor",
        "command-text heuristic; not a sandbox (the cage is the boundary)",
    ));
    let candidates: Vec<PathBuf> = crate::cage::SOCKETS.iter().map(PathBuf::from).collect();
    found.extend(socket_findings(&candidates));
    found
}

/// One warning per Unix socket a caged program could reach. A socket that is not there is not a finding.
pub fn socket_findings(candidates: &[PathBuf]) -> Vec<Finding> {
    let abi = crate::cage::abi();
    crate::cage::scan_sockets(candidates)
        .into_iter()
        .filter(|(_, state)| *state == crate::cage::SocketState::Present)
        .map(|(path, _)| {
            Finding::warn(
                "socket",
                format!(
                    "unix socket at {}: a caged program can reach it and landlock abi {abi} does not \
                     refuse the connect (use `--cage strict` to drop /run)",
                    path.display()
                ),
            )
        })
        .collect()
}

/// The checks, in the order a person would debug them. `home` is the Hades home, `config` the path the CLI
/// would read, and `binary` the executable that is running.
pub fn check(home: &Path, config: &Path, binary: &Path, services: bool) -> Vec<Finding> {
    let mut found = Vec::new();

    if home.is_dir() {
        let probe = home.join(".doctor-write-probe");
        match std::fs::write(&probe, b"probe") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                found.push(Finding::ok(
                    "home",
                    format!("{} is writable", home.display()),
                ));
            }
            Err(error) => found.push(Finding::fail(
                "home",
                format!("{} is not writable: {error}", home.display()),
            )),
        }
    } else {
        found.push(Finding::warn(
            "home",
            format!(
                "{} does not exist yet (hades init makes it)",
                home.display()
            ),
        ));
    }

    match std::fs::read_to_string(config) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(_) => found.push(Finding::ok(
                "config",
                format!("{} parses", config.display()),
            )),
            Err(error) => found.push(Finding::fail(
                "config",
                format!("{} is not valid JSON: {error}", config.display()),
            )),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => found.push(Finding::warn(
            "config",
            format!("no file at {} (hades init writes one)", config.display()),
        )),
        Err(error) => found.push(Finding::fail(
            "config",
            format!("{} cannot be read: {error}", config.display()),
        )),
    }

    let env = home.join(".env");
    match std::fs::read_to_string(&env) {
        Ok(text) => {
            let keys = text
                .lines()
                .filter(|line| {
                    let line = line.trim();
                    !line.starts_with('#')
                        && line
                            .split_once('=')
                            .is_some_and(|(_, value)| !value.trim().is_empty())
                })
                .count();
            if keys == 0 {
                found.push(Finding::warn(
                    "keys",
                    format!("{} has no values set", env.display()),
                ));
            } else {
                found.push(Finding::ok(
                    "keys",
                    format!("{keys} value(s) in {} (not printed)", env.display()),
                ));
            }
        }
        Err(_) => found.push(Finding::warn(
            "keys",
            format!(
                "no {}; a cloud model will fail without a key",
                env.display()
            ),
        )),
    }

    let sessions = home.join("sessions");
    let count = std::fs::read_dir(&sessions)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|kind| kind == "jsonl"))
                .count()
        })
        .unwrap_or(0);
    found.push(Finding::ok(
        "sessions",
        format!("{count} log(s) under {}", sessions.display()),
    ));

    let (jobs, problems) = crate::cron::load(home);
    let detail = format!(
        "{} job(s){}",
        jobs.len(),
        if problems.is_empty() {
            String::new()
        } else {
            format!(", {} file(s) refused", problems.len())
        }
    );
    if problems.is_empty() {
        found.push(Finding::ok("jobs", detail));
    } else {
        found.push(Finding::warn("jobs", detail));
    }

    let builtin = crate::skills::builtin(home).len();
    let installed = crate::skills::installed(home).len();
    let proposed = crate::skills::proposed(home).len();
    found.push(Finding::ok(
        "skills",
        format!(
            "{builtin} built-in, {installed} installed, {proposed} proposal(s) waiting for review"
        ),
    ));

    let rows = crate::scoreboard::load(home);
    found.push(Finding::ok(
        "scoreboard",
        format!("{} run(s) recorded", rows.len()),
    ));

    if binary.is_file() {
        found.push(Finding::ok("binary", format!("{}", binary.display())));
    } else {
        found.push(Finding::warn(
            "binary",
            format!(
                "{} is not a file (the installer puts it at ~/.local/bin/hades)",
                binary.display()
            ),
        ));
    }

    if services {
        for unit in ["hades-daemon", "hades-gateway"] {
            let status = std::process::Command::new("systemctl")
                .args(["--user", "is-active", unit])
                .output();
            match status {
                Ok(output) => {
                    let said = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if said == "active" {
                        found.push(Finding::ok(unit, "active"));
                    } else {
                        found.push(Finding::warn(unit, format!("not active ({said})")));
                    }
                }
                Err(error) => found.push(Finding::warn(
                    unit,
                    format!("could not ask systemctl: {error}"),
                )),
            }
        }
    }

    // The security section is read-only and cheap, so it runs on every `doctor` rather than behind a flag:
    // a limit nobody sees is a limit that gets assumed away.
    found.extend(security_findings(home));

    found
}

/// One line per finding, and a count at the end. The exit code is the answer: 1 when anything failed.
pub fn render(found: &[Finding]) -> (String, i32) {
    let mut text = String::new();
    for finding in found {
        let mark = match finding.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "fail",
        };
        text.push_str(&format!("{mark} {:<12} {}\n", finding.name, finding.detail));
    }
    let failed = found.iter().filter(|f| f.level == Level::Fail).count();
    let warned = found.iter().filter(|f| f.level == Level::Warn).count();
    text.push_str(&format!(
        "\n{} ok, {warned} warning(s), {failed} failure(s)\n",
        found.len() - failed - warned
    ));
    (text, i32::from(failed > 0))
}

/// The executable path this process is running, without resolving symlinks: the installer installs a real
/// file, and a symlink elsewhere is worth seeing as it is.
pub fn running_binary() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("hades"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = crate::scratch::root().join(format!("doctor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch home");
        dir
    }

    #[test]
    fn a_fresh_home_warns_but_does_not_fail() {
        let home = scratch("fresh");
        let found = check(&home, &home.join("config.json"), &home.join("hades"), false);
        assert!(
            found.iter().all(|finding| finding.level != Level::Fail),
            "{found:?}"
        );
        let (text, code) = render(&found);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("no file at"), "{text}");
    }

    #[test]
    fn a_corrupt_config_is_a_failure_and_sets_the_exit_code() {
        let home = scratch("corrupt");
        std::fs::write(home.join("config.json"), "{not json").expect("the bad file");
        let found = check(&home, &home.join("config.json"), &home.join("hades"), false);
        assert!(found.iter().any(|finding| finding.level == Level::Fail));
        let (text, code) = render(&found);
        assert_eq!(code, 1, "{text}");
    }

    #[test]
    fn a_written_config_parses_and_no_secret_is_printed() {
        let home = scratch("configured");
        std::fs::write(home.join("config.json"), r#"{"version":1}"#).expect("a config");
        std::fs::write(home.join(".env"), "HADES_API_KEY=secret-value\n").expect("an env");
        let found = check(&home, &home.join("config.json"), &home.join("hades"), false);
        let (text, _) = render(&found);
        assert!(text.contains("1 value(s)"), "{text}");
        assert!(!text.contains("secret-value"), "{text}");
    }

    #[test]
    fn a_reachable_docker_socket_is_a_warning_and_an_absent_one_is_not() {
        let home = scratch("socket");
        let sock = home.join("docker.sock");
        std::fs::write(&sock, b"").expect("a socket file");
        let found = socket_findings(&[sock.clone(), home.join("absent.sock")]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].level, Level::Warn);
        assert!(found[0].detail.contains("docker.sock"), "{:?}", found[0]);
        // The report names the limit rather than implying the cage closes it.
        let (text, _) = render(&found);
        assert!(text.contains("unix socket"), "{text}");
    }

    #[test]
    fn the_security_section_names_the_abi_and_the_floor() {
        let found = security_findings(Path::new("/work"));
        let (text, _) = render(&found);
        assert!(text.contains("landlock"), "{text}");
        assert!(text.contains("unix sockets"), "{text}");
        assert!(text.contains("command-text heuristic"), "{text}");
    }
}
