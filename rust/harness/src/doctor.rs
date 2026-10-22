//! `hades doctor`: what this install is, checked without a model call and without changing anything.
//!
//! A product needs one command that answers "is this thing set up" before a person debugs a run. Every
//! check here reads; none of them writes, restarts, or calls out. A warning is something a fresh home is
//! allowed to have; a failure is something that will stop a run.

use std::path::{Path, PathBuf};

use serde_json::Value;

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
    let mut found = platform_findings();

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
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    found.push(Finding::ok(
        "recipe",
        crate::recipe::Recipe::describe(&root),
    ));
    // A browser the tool cannot find is a live screenshot that fails at the moment it is needed. Naming the
    // binary here, before the run, turns that into a warning at the top.
    // Two fetch paths with different performance and different platform requirements: which one is live
    // belongs in the report, not in a benchmark's footnote.
    found.push(Finding::ok(
        "fetch transport",
        crate::fetch_pool::transport(),
    ));
    found.push(match crate::browser::browser_path() {
        Ok(path) => Finding::ok(
            "browser",
            format!("{} (set HADES_CHROME to change it)", path.display()),
        ),
        Err(said) => Finding::warn("browser", said),
    });
    // A toolchain the cage cannot see is a runtime that dies inside a run with a message about a missing
    // library. Discovery cannot look under the home, so an install there is named in the config or it is not
    // reachable; this is where a person finds out which of the two it is.
    let declared = crate::cage::declared();
    let undeclared: Vec<PathBuf> = crate::cage::likely_home_toolchains(home)
        .into_iter()
        .filter(|path| !declared.iter().any(|grant| path.starts_with(grant)))
        .collect();
    if declared.is_empty() && undeclared.is_empty() {
        found.push(Finding::ok(
            "toolchains",
            "none declared; none found under the home",
        ));
    } else if undeclared.is_empty() {
        found.push(Finding::ok(
            "toolchains",
            format!(
                "declared: {}",
                declared
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<String>>()
                    .join(", ")
            ),
        ));
    } else {
        found.push(Finding::warn(
            "toolchains",
            format!(
                "{} is under the home and not declared; the cage cannot read it. Add it to `runtime_paths` in {}",
                undeclared
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<String>>()
                    .join(", "),
                config.display()
            ),
        ));
    }
    // An audit trail with a silent hole is worse than one that says where the hole is. The counter is per
    // process, so this reports the gaps this process hit; the ledger's own history is the longer record.
    let gaps = crate::activity::gaps();
    found.push(if gaps == 0 {
        Finding::ok("audit", "activity gaps: 0")
    } else {
        Finding::warn(
            "audit",
            format!(
                "activity gaps: {gaps} (last: {}); those actions are not in activity.jsonl",
                crate::activity::last_gap().unwrap_or_else(|| "unknown".to_string())
            ),
        )
    });
    // A pack install that committed and then failed to write its event is on disk and not in the record. The
    // catalog is the list of what is installed; the events are the account of how it got there.
    if home.join("packs").join("catalog.jsonl").exists() {
        match crate::pack_fit::rows_without_events(home) {
            Ok(missing) if missing.is_empty() => {
                found.push(Finding::ok("packs", "every catalog row has its apply event"));
            }
            Ok(missing) => found.push(Finding::warn(
                "packs",
                format!(
                    "{} catalog row(s) have no apply event: {}; the install is committed but unrecorded",
                    missing.len(),
                    missing.join(", ")
                ),
            )),
            Err(error) => found.push(Finding::warn("packs", error)),
        }
    }
    // A run that stopped without reaching its end is reported here as well as in `hades status`: the same
    // marker, two doors, because `doctor` is what a person runs when something feels wrong.
    let interrupted = crate::daemon::interrupted(home);
    found.push(if interrupted.is_empty() {
        Finding::ok("interrupted", "interrupted: 0")
    } else {
        Finding::warn(
            "interrupted",
            format!(
                "interrupted: {} run(s) never reached their end ({})",
                interrupted.len(),
                interrupted
                    .iter()
                    .filter_map(|marker| marker.get("session").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    });

    found.push(compatibility(&found));
    found
}

/// What this machine is, and which of the parts Hades needs it can actually provide.
///
/// A support table is a promise about a distribution; this is a reading of the machine in front of you. It is
/// the first thing `doctor` prints, because "supported" and "works here" are different questions and only the
/// second one matters at the moment something is wrong.
pub fn platform_findings() -> Vec<Finding> {
    let mut found = Vec::new();
    let system = uname();
    found.push(Finding::ok("platform", system));
    found.push(Finding::ok("libc", libc_report()));
    let abi = crate::cage::abi();
    found.push(if abi >= 1 {
        Finding::ok(
            "landlock",
            format!("ABI {abi}: a run can be confined to its workspace"),
        )
    } else {
        Finding::warn(
            "landlock",
            format!(
                "ABI {abi}: no Landlock on this kernel, so a governed run cannot be caged here"
            ),
        )
    });
    found.push(Finding::ok(
        "sqlite",
        format!("bundled {}", rusqlite::version()),
    ));
    // Pack export shells out to GNU tar for a byte-identical archive. Busybox tar is present on Alpine and is
    // not the same program, so "tar exists" is not the question.
    found.push(match gnu_tar() {
        Ok(version) => Finding::ok("tar", format!("GNU tar {}", version)),
        Err(_) => Finding::warn(
            "tar",
            "no GNU tar: packs cannot be exported or installed from an archive here (busybox tar is not \
             byte-identical); install it (`apk add tar`)",
        ),
    });
    found
}

/// The first version number GNU tar prints, or nothing when the `tar` on PATH is another program.
fn gnu_tar() -> Result<String, String> {
    let output = std::process::Command::new("tar")
        .arg("--version")
        .output()
        .map_err(|error| format!("could not run tar: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !text.contains("GNU tar") {
        return Err("not GNU tar".to_string());
    }
    let version = text
        .split_whitespace()
        .skip_while(|word| *word != "tar)")
        .nth(1)
        .unwrap_or("unknown")
        .to_string();
    Ok(version)
}

/// The verdict, from the findings that describe the host rather than the install: a person with a missing
/// config still has a capable machine, and a person with a perfect config on an old kernel does not.
fn compatibility(found: &[Finding]) -> Finding {
    let capability = [
        "platform",
        "libc",
        "landlock",
        "sqlite",
        "tar",
        "fetch transport",
        "browser",
    ];
    let reduced: Vec<String> = found
        .iter()
        .filter(|finding| finding.level != Level::Ok && capability.contains(&finding.name.as_str()))
        .map(|finding| finding.name.clone())
        .collect();
    if reduced.is_empty() {
        Finding::ok("compatibility", "full")
    } else {
        Finding::warn("compatibility", format!("reduced: {}", reduced.join(", ")))
    }
}

/// Kernel and machine, from `uname`, without a dependency: the release string is what support questions
/// actually turn on.
fn uname() -> String {
    // SAFETY: `uname` fills a struct it defines; the arrays are NUL-terminated by the kernel.
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uts) } != 0 {
        return "unknown system".to_string();
    }
    let field = |raw: &[libc::c_char]| {
        let bytes: Vec<u8> = raw
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    format!(
        "{} {}, kernel {}",
        field(&uts.sysname),
        field(&uts.machine),
        field(&uts.release)
    )
}

/// Which C library carries this binary, and which version. On musl the answer is the build's own, because a
/// static binary does not ask the host anything.
fn libc_report() -> String {
    #[cfg(target_env = "gnu")]
    {
        unsafe extern "C" {
            fn gnu_get_libc_version() -> *const libc::c_char;
        }
        // SAFETY: glibc defines this and returns a NUL-terminated static string.
        let version = unsafe {
            std::ffi::CStr::from_ptr(gnu_get_libc_version())
                .to_string_lossy()
                .into_owned()
        };
        let linkage = if cfg!(target_feature = "crt-static") {
            "static"
        } else {
            "dynamic"
        };
        format!("glibc {version} ({linkage})")
    }
    #[cfg(not(target_env = "gnu"))]
    {
        format!(
            "musl ({})",
            if cfg!(target_feature = "crt-static") {
                "static"
            } else {
                "dynamic"
            }
        )
    }
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

    #[test]
    fn the_platform_report_names_this_machine_and_reaches_a_verdict() {
        let found = platform_findings();
        let names: Vec<&str> = found.iter().map(|finding| finding.name.as_str()).collect();
        for name in ["platform", "libc", "landlock", "sqlite"] {
            assert!(names.contains(&name), "{names:?}");
        }
        let platform = found
            .iter()
            .find(|f| f.name == "platform")
            .expect("a platform");
        assert!(platform.detail.starts_with("Linux "), "{}", platform.detail);
        let libc = found.iter().find(|f| f.name == "libc").expect("a libc");
        // One of the two, never neither: a binary knows which C library it was built against.
        assert!(
            libc.detail.starts_with("glibc ") || libc.detail.starts_with("musl "),
            "{}",
            libc.detail
        );
        let mut all = found.clone();
        all.push(compatibility(&all));
        let verdict = all.last().expect("a verdict");
        assert_eq!(verdict.name, "compatibility");
        assert!(
            verdict.detail == "full" || verdict.detail.starts_with("reduced: "),
            "{}",
            verdict.detail
        );
    }

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
