//! Checking a pack's claims about the machine before the pack is installed.
//!
//! A pack is written on one computer by one person. Its docs, its flags and its numbers describe
//! that computer: an 8 GiB card, a port nobody else had taken, weights on a local disk, an agent
//! config that pointed at a live endpoint. Installing it elsewhere copies those circumstances too,
//! and the person who installed it then reads advice that is quietly wrong for their machine.
//!
//! `pack check` is the middle man. A pack may declare `requires.assumptions`: a list of claims,
//! each with the check that decides it and the remedy if it is false. This module gathers what the
//! machine actually is, evaluates every claim against those facts, and prints a verdict per claim
//! with its remedy. It changes nothing. Exit 0 means every hard claim holds, 3 means one does not,
//! 4 means something could not be decided here.
//!
//! The split is deliberate: [`facts`] touches the system once, [`evaluate`] is a pure function of a
//! claim and those facts. That is what makes the decisions testable without a particular GPU.

use std::collections::HashSet;
use std::fmt::Write;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};

use crate::pack::{Assumption, Pack, Severity};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

// ----------------------------------------------------------------------------- facts

#[derive(Debug, Clone, PartialEq)]
pub struct Gpu {
    pub name: String,
    pub vendor: String,
    pub vram_gib: f64,
    pub vram_used_gib: Option<f64>,
    pub driver: String,
}

#[derive(Debug, Clone)]
pub struct Facts {
    pub gpu: Option<Gpu>,
    pub physical_cores: u32,
    pub logical_cores: u32,
    pub ram_gib: f64,
    pub home: PathBuf,
}

impl Facts {
    fn to_value(&self) -> Value {
        json!({
            "gpu": self.gpu.as_ref().map(|gpu| json!({
                "name": gpu.name,
                "vendor": gpu.vendor,
                "vram_gib": gpu.vram_gib,
                "vram_used_gib": gpu.vram_used_gib,
                "driver": gpu.driver,
            })),
            "physical_cores": self.physical_cores,
            "logical_cores": self.logical_cores,
            "ram_gib": self.ram_gib,
            "home": self.home.display().to_string(),
        })
    }
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The card a pack is most likely to be fitted against: the one with the most memory.
fn gpu_from_sysfs() -> Option<Gpu> {
    let mut found: Vec<Gpu> = Vec::new();
    let entries = std::fs::read_dir("/sys/class/drm").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(suffix) = name.strip_prefix("card") else {
            continue;
        };
        if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let device = entry.path().join("device");
        if !device.exists() {
            continue;
        }
        let total = read_text(&device.join("mem_info_vram_total"));
        let Ok(total) = total.parse::<u64>() else {
            continue;
        };
        if total == 0 {
            continue;
        }
        let used = read_text(&device.join("mem_info_vram_used"))
            .parse::<u64>()
            .ok();
        let vendor = match read_text(&device.join("vendor")).as_str() {
            "0x1002" => "AMD",
            "0x10de" => "NVIDIA",
            "0x8086" => "Intel",
            _ => "unknown",
        };
        let driver = std::fs::read_link(device.join("driver"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .unwrap_or_default();
        found.push(Gpu {
            name: gpu_name(&device, vendor),
            vendor: vendor.to_string(),
            vram_gib: round2(total as f64 / GIB),
            vram_used_gib: used.map(|bytes| round2(bytes as f64 / GIB)),
            driver,
        });
    }
    found.sort_by(|a, b| b.vram_gib.total_cmp(&a.vram_gib));
    found.into_iter().next()
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// A card name from `lspci`, because sysfs carries ids rather than words. Falls back to the vendor.
fn gpu_name(device: &Path, vendor: &str) -> String {
    let pci = std::fs::canonicalize(device)
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_default();
    if !pci.is_empty()
        && let Ok(output) = Command::new("lspci").arg("-s").arg(&pci).output()
        && output.status.success()
    {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if let Some((_, tail)) = text.split_once("controller: ") {
            return tail.trim().to_string();
        }
    }
    format!("{vendor} GPU")
}

fn gpu_from_nvidia_smi() -> Option<Gpu> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,memory.used,driver_version",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?;
    let parts: Vec<&str> = line.split(',').map(str::trim).collect();
    if parts.len() < 3 {
        return None;
    }
    let total = parts[1].parse::<f64>().ok()?;
    let used = parts[2].parse::<f64>().ok();
    Some(Gpu {
        name: parts[0].to_string(),
        vendor: "NVIDIA".to_string(),
        vram_gib: round2(total / 1024.0),
        vram_used_gib: used.map(|mib| round2(mib / 1024.0)),
        driver: parts.get(3).copied().unwrap_or("").to_string(),
    })
}

fn physical_cores() -> u32 {
    let text = read_text(Path::new("/proc/cpuinfo"));
    let mut pairs: HashSet<(String, String)> = HashSet::new();
    let mut physical: Option<String> = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "physical id" => physical = Some(value.trim().to_string()),
            "core id" => {
                if let Some(package) = &physical {
                    pairs.insert((package.clone(), value.trim().to_string()));
                }
            }
            _ => {}
        }
    }
    if !pairs.is_empty() {
        return pairs.len() as u32;
    }
    std::thread::available_parallelism()
        .map(|count| count.get() as u32)
        .unwrap_or(1)
}

fn ram_gib() -> f64 {
    let text = read_text(Path::new("/proc/meminfo"));
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:")
            && let Some(kib) = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<f64>().ok())
        {
            return round2(kib / 1024.0 / 1024.0);
        }
    }
    0.0
}

/// What this machine actually is. One read, shared by every claim.
pub fn facts() -> Facts {
    let gpu = gpu_from_sysfs().or_else(gpu_from_nvidia_smi);
    Facts {
        gpu,
        physical_cores: physical_cores(),
        logical_cores: std::thread::available_parallelism()
            .map(|count| count.get() as u32)
            .unwrap_or(1),
        ram_gib: ram_gib(),
        home: std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/")),
    }
}

// ----------------------------------------------------------------------------- verdicts

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Conflict,
    Unknown,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Conflict => "conflict",
            Status::Unknown => "unknown",
        }
    }

    fn mark(self) -> &'static str {
        match self {
            Status::Ok => "ok   ",
            Status::Conflict => "FALSE",
            Status::Unknown => "?    ",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub id: String,
    pub claim: String,
    pub severity: Severity,
    pub status: Status,
    pub detail: String,
    pub resolution: Option<String>,
}

impl Verdict {
    fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "claim": self.claim,
            "severity": match self.severity { Severity::Hard => "hard", Severity::Soft => "soft" },
            "status": self.status.name(),
            "detail": self.detail,
            "resolution": self.resolution,
        })
    }
}

fn expand(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

fn port_free(port: u16) -> bool {
    let address: SocketAddr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).is_err()
}

fn socket_alive(url: &str) -> Option<bool> {
    let rest = url.split_once("://").map(|(_, tail)| tail).unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = authority.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let target = format!("{host}:{port}");
    let mut addresses = target.to_socket_addrs().ok()?;
    let address = addresses.next()?;
    Some(TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).is_ok())
}

/// The block device behind a path, and whether it calls itself rotational.
fn path_is_local(path: &Path) -> Option<bool> {
    let target = if path.exists() {
        std::fs::canonicalize(path).ok()?
    } else {
        return None;
    };
    let mounts = read_text(Path::new("/proc/mounts"));
    let mut best: Option<(String, usize)> = None;
    for line in mounts.lines() {
        let mut parts = line.split_whitespace();
        let Some(device) = parts.next() else { continue };
        let Some(mount) = parts.next() else { continue };
        let mount = mount.replace("\\040", " ");
        if target.starts_with(&mount) {
            let depth = mount.len();
            if best.as_ref().is_none_or(|(_, seen)| depth >= *seen) {
                best = Some((device.to_string(), depth));
            }
        }
    }
    let (device, _) = best?;
    if !device.starts_with("/dev/") {
        return Some(true); // overlay, tmpfs, a bind: not a spinning disk we can name
    }
    let name = Path::new(&device)
        .file_name()?
        .to_string_lossy()
        .to_string();
    let base: String = name
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '-')
        .filter(|c| *c != '-')
        .collect();
    let block = format!("/sys/block/{base}/queue/rotational");
    let flag = read_text(Path::new(&block));
    Some(flag != "1")
}

fn service_active(unit: &str) -> Option<bool> {
    let output = Command::new("systemctl")
        .args(["--user", "is-active", unit])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(text == "active")
}

fn config_valid(path: &Path) -> Option<Result<(), String>> {
    if !path.exists() {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    Some(match serde_json::from_str::<Value>(&text) {
        Ok(_) => Ok(()),
        Err(error) => Err(error.to_string()),
    })
}

fn arg_number(assumption: &Assumption) -> Option<f64> {
    assumption.arg.as_ref()?.trim().parse::<f64>().ok()
}

fn arg_text(assumption: &Assumption) -> Option<String> {
    assumption.arg.clone()
}

fn verdict(
    assumption: &Assumption,
    status: Status,
    detail: String,
    resolution: Option<String>,
) -> Verdict {
    Verdict {
        id: assumption.id.clone(),
        claim: assumption.claim.clone(),
        severity: assumption.severity,
        status,
        detail,
        resolution,
    }
}

/// Decide one claim against one set of facts. No install, no write, no network except a connect test
/// the claim itself asks for.
pub fn evaluate(assumption: &Assumption, facts: &Facts) -> Verdict {
    let fallback = assumption.resolution.clone();
    match assumption.check.as_str() {
        "gpu_present" => match &facts.gpu {
            Some(gpu) => verdict(
                assumption,
                Status::Ok,
                format!("{} ({:.2} GiB)", gpu.name, gpu.vram_gib),
                None,
            ),
            None => verdict(
                assumption,
                Status::Conflict,
                "no GPU found".to_string(),
                fallback,
            ),
        },
        "gpu_vendor" => {
            let want = arg_text(assumption).unwrap_or_default();
            match &facts.gpu {
                None => verdict(
                    assumption,
                    Status::Unknown,
                    "no GPU to ask".to_string(),
                    fallback,
                ),
                Some(gpu) if gpu.vendor.eq_ignore_ascii_case(&want) => {
                    verdict(assumption, Status::Ok, gpu.vendor.clone(), None)
                }
                Some(gpu) => verdict(
                    assumption,
                    Status::Conflict,
                    format!("this machine is {}", gpu.vendor),
                    fallback.or(Some(format!(
                        "translate the pack's advice to {}",
                        gpu.vendor
                    ))),
                ),
            }
        }
        "vram_at_least" => match (&facts.gpu, arg_number(assumption)) {
            (Some(gpu), Some(need)) if gpu.vram_gib >= need - 0.05 => verdict(
                assumption,
                Status::Ok,
                format!("{:.2} GiB", gpu.vram_gib),
                None,
            ),
            (Some(gpu), Some(need)) => verdict(
                assumption,
                Status::Conflict,
                format!("{:.2} GiB, pack assumed {need:.0}", gpu.vram_gib),
                fallback,
            ),
            (None, _) => verdict(
                assumption,
                Status::Unknown,
                "VRAM unreadable".to_string(),
                fallback,
            ),
            (_, None) => verdict(
                assumption,
                Status::Unknown,
                "the claim has no numeric `arg`".to_string(),
                Some("give the claim an `arg` such as 8".to_string()),
            ),
        },
        "vram_at_most" => match (&facts.gpu, arg_number(assumption)) {
            (Some(gpu), Some(limit)) if gpu.vram_gib <= limit => verdict(
                assumption,
                Status::Ok,
                format!("{:.2} GiB", gpu.vram_gib),
                None,
            ),
            (Some(gpu), Some(limit)) => verdict(
                assumption,
                Status::Conflict,
                format!(
                    "{:.2} GiB here, pack assumed at most {limit:.0}",
                    gpu.vram_gib
                ),
                fallback,
            ),
            (None, _) => verdict(
                assumption,
                Status::Unknown,
                "VRAM unreadable".to_string(),
                fallback,
            ),
            (_, None) => verdict(
                assumption,
                Status::Unknown,
                "the claim has no numeric `arg`".to_string(),
                None,
            ),
        },
        "vram_idle" => match &facts.gpu {
            None => verdict(
                assumption,
                Status::Unknown,
                "no GPU to ask".to_string(),
                fallback,
            ),
            Some(gpu) => match gpu.vram_used_gib {
                None => verdict(
                    assumption,
                    Status::Unknown,
                    "memory in use unreadable".to_string(),
                    fallback,
                ),
                Some(used) => {
                    let limit = arg_number(assumption).unwrap_or(0.9);
                    if used <= limit {
                        verdict(
                            assumption,
                            Status::Ok,
                            format!("{used:.2} GiB in use"),
                            None,
                        )
                    } else {
                        verdict(
                            assumption,
                            Status::Conflict,
                            format!("{used:.2} GiB already loaded"),
                            fallback,
                        )
                    }
                }
            },
        },
        "physical_cores" => {
            if facts.physical_cores == 0 {
                return verdict(
                    assumption,
                    Status::Unknown,
                    "core count unreadable".to_string(),
                    fallback,
                );
            }
            verdict(
                assumption,
                Status::Ok,
                format!(
                    "{} physical, {} logical",
                    facts.physical_cores, facts.logical_cores
                ),
                fallback.or(Some(format!("use {} threads", facts.physical_cores))),
            )
        }
        "port_free" => match assumption
            .arg
            .as_ref()
            .and_then(|text| text.trim().parse::<u16>().ok())
        {
            None => verdict(
                assumption,
                Status::Unknown,
                "the claim has no port `arg`".to_string(),
                None,
            ),
            Some(port) => {
                if port_free(port) {
                    verdict(assumption, Status::Ok, format!("{port} free"), None)
                } else {
                    verdict(
                        assumption,
                        Status::Conflict,
                        format!("{port} is in use"),
                        fallback,
                    )
                }
            }
        },
        "ram_at_least" => match arg_number(assumption) {
            None => verdict(
                assumption,
                Status::Unknown,
                "the claim has no numeric `arg`".to_string(),
                None,
            ),
            Some(_) if facts.ram_gib <= 0.0 => verdict(
                assumption,
                Status::Unknown,
                "RAM unreadable".to_string(),
                fallback,
            ),
            Some(need) => {
                if facts.ram_gib >= need {
                    verdict(
                        assumption,
                        Status::Ok,
                        format!("{:.1} GiB", facts.ram_gib),
                        None,
                    )
                } else {
                    verdict(
                        assumption,
                        Status::Conflict,
                        format!("{:.1} GiB, pack assumed {need:.0}", facts.ram_gib),
                        fallback,
                    )
                }
            }
        },
        "disk_local" => match arg_text(assumption) {
            None => verdict(
                assumption,
                Status::Unknown,
                "the claim names no path".to_string(),
                None,
            ),
            Some(text) => {
                let path = expand(&text, &facts.home);
                match path_is_local(&path) {
                    None => verdict(
                        assumption,
                        Status::Unknown,
                        format!("{} does not exist yet", path.display()),
                        fallback,
                    ),
                    Some(true) => verdict(
                        assumption,
                        Status::Ok,
                        format!("{} on a local disk", path.display()),
                        None,
                    ),
                    Some(false) => verdict(
                        assumption,
                        Status::Conflict,
                        format!("{} sits on a rotational or USB disk", path.display()),
                        fallback,
                    ),
                }
            }
        },
        "file_exists" => match arg_text(assumption) {
            None => verdict(
                assumption,
                Status::Unknown,
                "the claim names no path".to_string(),
                None,
            ),
            Some(text) => {
                let path = expand(&text, &facts.home);
                if path.exists() {
                    verdict(assumption, Status::Ok, path.display().to_string(), None)
                } else {
                    verdict(
                        assumption,
                        Status::Conflict,
                        format!("missing: {}", path.display()),
                        fallback,
                    )
                }
            }
        },
        "socket_alive" => match arg_text(assumption) {
            None => verdict(
                assumption,
                Status::Unknown,
                "the claim names no URL".to_string(),
                None,
            ),
            Some(url) => match socket_alive(&url) {
                None => verdict(
                    assumption,
                    Status::Unknown,
                    format!("{url} has no host:port to test"),
                    fallback,
                ),
                Some(true) => verdict(assumption, Status::Ok, format!("{url} answers"), None),
                Some(false) => verdict(
                    assumption,
                    Status::Conflict,
                    format!("{url} is not listening"),
                    fallback,
                ),
            },
        },
        "config_json" => match arg_text(assumption) {
            None => verdict(
                assumption,
                Status::Unknown,
                "the claim names no file".to_string(),
                None,
            ),
            Some(text) => {
                let path = expand(&text, &facts.home);
                match config_valid(&path) {
                    None => verdict(
                        assumption,
                        Status::Unknown,
                        format!("{} does not exist", path.display()),
                        fallback,
                    ),
                    Some(Ok(())) => verdict(
                        assumption,
                        Status::Ok,
                        format!("{} parses", path.display()),
                        None,
                    ),
                    Some(Err(error)) => verdict(
                        assumption,
                        Status::Conflict,
                        format!("{}: {error}", path.display()),
                        fallback,
                    ),
                }
            }
        },
        "service_active" => match arg_text(assumption) {
            None => verdict(
                assumption,
                Status::Unknown,
                "the claim names no unit".to_string(),
                None,
            ),
            Some(unit) => match service_active(&unit) {
                None => verdict(
                    assumption,
                    Status::Unknown,
                    "systemctl is not answering".to_string(),
                    fallback,
                ),
                Some(true) => verdict(assumption, Status::Ok, format!("{unit} active"), None),
                Some(false) => verdict(
                    assumption,
                    Status::Conflict,
                    format!("{unit} is not active"),
                    fallback,
                ),
            },
        },
        other => verdict(
            assumption,
            Status::Unknown,
            format!("no check named `{other}` in this build"),
            Some("this pack was written for a newer Hades".to_string()),
        ),
    }
}

pub fn evaluate_all(assumptions: &[Assumption], facts: &Facts) -> Vec<Verdict> {
    assumptions
        .iter()
        .map(|assumption| evaluate(assumption, facts))
        .collect()
}

/// 0 when every hard claim holds, 3 when one does not, 4 when something could not be decided.
pub fn exit_code(verdicts: &[Verdict]) -> i32 {
    let hard = verdicts
        .iter()
        .any(|v| v.status == Status::Conflict && v.severity == Severity::Hard);
    if hard {
        return 3;
    }
    if verdicts.iter().any(|v| v.status == Status::Unknown) {
        return 4;
    }
    0
}

// ----------------------------------------------------------------------------- report

fn machine_line(facts: &Facts) -> String {
    let gpu = match &facts.gpu {
        Some(gpu) => format!(
            "{} / {:.2} GiB VRAM{}",
            gpu.name,
            gpu.vram_gib,
            if gpu.driver.is_empty() {
                String::new()
            } else {
                format!(" / {}", gpu.driver)
            }
        ),
        None => "no GPU".to_string(),
    };
    format!(
        "{gpu} / {} cores / {:.1} GiB RAM",
        facts.physical_cores, facts.ram_gib
    )
}

pub fn report(pack: &Pack, facts: &Facts, verdicts: &[Verdict]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "pack:        {} {}",
        pack.manifest.id, pack.manifest.version
    );
    let _ = writeln!(out, "requires:    hades {}", pack.manifest.requires.hades);
    let _ = writeln!(out, "this machine: {}", machine_line(facts));
    if verdicts.is_empty() {
        let _ = writeln!(
            out,
            "\nthis pack declares no assumptions about the machine; nothing to check"
        );
        let _ = writeln!(
            out,
            "its docs were written somewhere; read them against this machine before trusting a number"
        );
        return out;
    }
    let _ = writeln!(out);
    for item in verdicts {
        let _ = writeln!(out, "[{}] {}: {}", item.status.mark(), item.id, item.claim);
        if !item.detail.is_empty() {
            let _ = writeln!(out, "          now: {}", item.detail);
        }
        if item.status != Status::Ok
            && let Some(resolution) = &item.resolution
        {
            let _ = writeln!(out, "          do:  {resolution}");
        }
    }
    let ok = verdicts.iter().filter(|v| v.status == Status::Ok).count();
    let false_count = verdicts
        .iter()
        .filter(|v| v.status == Status::Conflict)
        .count();
    let unknown = verdicts
        .iter()
        .filter(|v| v.status == Status::Unknown)
        .count();
    let _ = writeln!(
        out,
        "\n{ok} of {} claims hold, {false_count} false, {unknown} undecided",
        verdicts.len()
    );
    let hard: Vec<&Verdict> = verdicts
        .iter()
        .filter(|v| v.status == Status::Conflict && v.severity == Severity::Hard)
        .collect();
    if !hard.is_empty() {
        let names: Vec<&str> = hard.iter().map(|v| v.id.as_str()).collect();
        let _ = writeln!(
            out,
            "\nSTOP: {} must be settled before this pack's results mean anything.",
            names.join(", ")
        );
    }
    out
}

pub fn report_json(pack: &Pack, facts: &Facts, verdicts: &[Verdict]) -> Value {
    json!({
        "pack": {
            "id": pack.manifest.id,
            "version": pack.manifest.version,
            "requires_hades": pack.manifest.requires.hades,
        },
        "facts": facts.to_value(),
        "verdicts": verdicts.iter().map(Verdict::to_value).collect::<Vec<Value>>(),
        "exit": exit_code(verdicts),
    })
}

// ----------------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::{Assumption, Severity};

    fn claim(id: &str, check: &str, arg: Option<&str>, severity: Severity) -> Assumption {
        Assumption {
            id: id.to_string(),
            claim: format!("{id} claim"),
            check: check.to_string(),
            arg: arg.map(str::to_string),
            severity,
            resolution: Some(format!("fix {id}")),
        }
    }

    fn rig(vram: f64, used: Option<f64>, vendor: &str) -> Facts {
        Facts {
            gpu: Some(Gpu {
                name: "test card".to_string(),
                vendor: vendor.to_string(),
                vram_gib: vram,
                vram_used_gib: used,
                driver: "test".to_string(),
            }),
            physical_cores: 6,
            logical_cores: 12,
            ram_gib: 31.3,
            home: PathBuf::from("/home/test"),
        }
    }

    #[test]
    fn a_bigger_card_satisfies_at_least_and_contradicts_at_most() {
        // "at least 8 GiB" is about whether it runs at all, so a 24 GiB card passes it. The claim
        // that the pack's *tuning* was written for 8 GiB is `vram_at_most`, and that one is false
        // here: the offload it prescribes is unnecessary on this machine.
        let facts = rig(24.0, Some(0.1), "NVIDIA");
        let verdicts = evaluate_all(
            &[
                claim("needs", "vram_at_least", Some("8"), Severity::Hard),
                claim("tuned", "vram_at_most", Some("8"), Severity::Soft),
            ],
            &facts,
        );
        assert_eq!(verdicts[0].status, Status::Ok);
        assert_eq!(verdicts[1].status, Status::Conflict);
        assert!(verdicts[1].detail.contains("24.00"));
        assert!(verdicts[1].resolution.is_some());
        assert_eq!(exit_code(&verdicts), 0, "a soft conflict still exits 0");
    }

    #[test]
    fn a_smaller_card_than_asserted_fails_the_hard_claim() {
        let facts = rig(8.0, Some(0.1), "AMD");
        let verdicts = evaluate_all(
            &[claim("needs", "vram_at_least", Some("24"), Severity::Hard)],
            &facts,
        );
        assert_eq!(verdicts[0].status, Status::Conflict);
        assert!(verdicts[0].detail.contains("pack assumed 24"));
        assert_eq!(exit_code(&verdicts), 3);
    }

    #[test]
    fn a_busy_card_is_a_hard_stop() {
        let facts = rig(8.0, Some(7.54), "AMD");
        let verdicts = evaluate_all(
            &[claim("idle", "vram_idle", Some("0.9"), Severity::Hard)],
            &facts,
        );
        assert_eq!(verdicts[0].status, Status::Conflict);
        assert_eq!(exit_code(&verdicts), 3);
    }

    #[test]
    fn an_idle_card_passes() {
        let facts = rig(8.0, Some(0.05), "AMD");
        let verdicts = evaluate_all(&[claim("idle", "vram_idle", None, Severity::Hard)], &facts);
        assert_eq!(verdicts[0].status, Status::Ok);
        assert_eq!(exit_code(&verdicts), 0);
    }

    #[test]
    fn a_vendor_mismatch_says_what_this_machine_is() {
        let facts = rig(24.0, None, "NVIDIA");
        let verdicts = evaluate_all(
            &[claim("amd", "gpu_vendor", Some("AMD"), Severity::Soft)],
            &facts,
        );
        assert_eq!(verdicts[0].status, Status::Conflict);
        assert!(verdicts[0].detail.contains("NVIDIA"));
    }

    #[test]
    fn no_gpu_is_a_conflict_rather_than_a_pass() {
        let facts = Facts {
            gpu: None,
            physical_cores: 6,
            logical_cores: 12,
            ram_gib: 31.3,
            home: PathBuf::from("/home/test"),
        };
        let verdicts = evaluate_all(&[claim("gpu", "gpu_present", None, Severity::Hard)], &facts);
        assert_eq!(verdicts[0].status, Status::Conflict);
        assert_eq!(exit_code(&verdicts), 3);
    }

    #[test]
    fn an_unknown_check_is_undecided_not_fine() {
        let facts = rig(8.0, None, "AMD");
        let verdicts = evaluate_all(
            &[claim("x", "does_not_exist", None, Severity::Soft)],
            &facts,
        );
        assert_eq!(verdicts[0].status, Status::Unknown);
        assert_eq!(exit_code(&verdicts), 4);
    }

    #[test]
    fn a_claim_without_its_argument_is_undecided() {
        let facts = rig(8.0, None, "AMD");
        let verdicts = evaluate_all(
            &[claim("ram", "ram_at_least", None, Severity::Soft)],
            &facts,
        );
        assert_eq!(verdicts[0].status, Status::Unknown);
    }

    #[test]
    fn cores_and_ram_report_the_machine() {
        let facts = rig(8.0, None, "AMD");
        let verdicts = evaluate_all(
            &[
                claim("cores", "physical_cores", None, Severity::Soft),
                claim("ram", "ram_at_least", Some("24"), Severity::Soft),
            ],
            &facts,
        );
        assert_eq!(verdicts[0].status, Status::Ok);
        assert!(verdicts[0].detail.contains("6 physical"));
        assert_eq!(verdicts[1].status, Status::Ok);
    }

    #[test]
    fn a_missing_file_conflicts_and_a_present_one_does_not() {
        let facts = rig(8.0, None, "AMD");
        let present = std::env::temp_dir().join("pack-check-present");
        std::fs::write(&present, b"x").expect("temp file");
        let verdicts = evaluate_all(
            &[
                claim(
                    "here",
                    "file_exists",
                    Some(present.to_str().expect("path")),
                    Severity::Hard,
                ),
                claim(
                    "gone",
                    "file_exists",
                    Some("/definitely/not/here"),
                    Severity::Soft,
                ),
            ],
            &facts,
        );
        assert_eq!(verdicts[0].status, Status::Ok);
        assert_eq!(verdicts[1].status, Status::Conflict);
        assert_eq!(exit_code(&verdicts), 0, "the missing file was a soft claim");
        let _ = std::fs::remove_file(&present);
    }

    #[test]
    fn a_held_port_reads_as_busy() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let facts = rig(8.0, None, "AMD");
        let verdicts = evaluate_all(
            &[claim(
                "port",
                "port_free",
                Some(&port.to_string()),
                Severity::Soft,
            )],
            &facts,
        );
        assert_eq!(
            verdicts[0].status,
            Status::Conflict,
            "we are holding that port"
        );
        drop(listener);
        let verdicts = evaluate_all(
            &[claim(
                "port",
                "port_free",
                Some(&port.to_string()),
                Severity::Soft,
            )],
            &facts,
        );
        assert_eq!(verdicts[0].status, Status::Ok, "the port was released");
    }

    #[test]
    fn report_names_the_remedy_and_the_hard_stop() {
        let facts = rig(8.0, Some(7.5), "AMD");
        let verdicts = evaluate_all(
            &[
                claim("idle", "vram_idle", Some("0.9"), Severity::Hard),
                claim("ram", "ram_at_least", Some("24"), Severity::Soft),
            ],
            &facts,
        );
        let text = report_with(&verdicts, &facts);
        assert!(text.contains("do:  fix idle"));
        assert!(text.contains("STOP: idle"));
        assert!(text.contains("1 of 2 claims hold"));
    }

    /// The report needs a `Pack`, which needs an archive; the formatting under test does not.
    fn report_with(verdicts: &[Verdict], facts: &Facts) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "this machine: {}", machine_line(facts));
        for item in verdicts {
            let _ = writeln!(out, "[{}] {}: {}", item.status.mark(), item.id, item.claim);
            if item.status != Status::Ok
                && let Some(resolution) = &item.resolution
            {
                let _ = writeln!(out, "          do:  {resolution}");
            }
        }
        let ok = verdicts.iter().filter(|v| v.status == Status::Ok).count();
        let _ = writeln!(out, "{ok} of {} claims hold", verdicts.len());
        let hard: Vec<&Verdict> = verdicts
            .iter()
            .filter(|v| v.status == Status::Conflict && v.severity == Severity::Hard)
            .collect();
        if !hard.is_empty() {
            let names: Vec<&str> = hard.iter().map(|v| v.id.as_str()).collect();
            let _ = writeln!(out, "STOP: {} must be settled", names.join(", "));
        }
        out
    }

    #[test]
    fn a_missing_home_path_expands_before_it_is_checked() {
        let path = expand("~/models", Path::new("/home/test"));
        assert_eq!(path, PathBuf::from("/home/test/models"));
        let absolute = expand("/var/lib", Path::new("/home/test"));
        assert_eq!(absolute, PathBuf::from("/var/lib"));
    }

    #[test]
    fn the_home_directory_is_never_a_rotational_surprise() {
        // /proc is a kernel filesystem with no /dev block device behind it; the check must not panic
        // or claim a spinning disk it cannot name.
        assert_eq!(path_is_local(Path::new("/proc/meminfo")), Some(true));
    }
}
