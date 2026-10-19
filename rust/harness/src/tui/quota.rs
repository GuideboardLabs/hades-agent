//! Provider account quotas. No inferred plan limits, browser cookies, or credentials in argv.
use super::backend::Options;
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Default)]
pub struct Window {
    pub label: String,
    pub used: f64,
    pub limit: f64,
    pub unit: String,
    pub reset: String,
}
impl Window {
    pub fn fraction(&self) -> f64 {
        if self.limit > 0.0 {
            (self.used / self.limit).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
    pub fn description(&self) -> String {
        if self.unit == "USD" {
            format!("{} ${:.2} / ${:.2} used", self.label, self.used, self.limit)
        } else {
            format!("{} {:.0}% used", self.label, self.fraction() * 100.0)
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct Quota {
    pub provider: String,
    pub windows: Vec<Window>,
    pub note: String,
    pub available: bool,
}
impl Quota {
    pub fn pending(provider: String) -> Self {
        Self {
            provider,
            note: "Checking account usage…".into(),
            ..Self::default()
        }
    }
}

pub fn poll(options: &Options) -> Quota {
    let provider = options.provider();
    let url = match provider.as_str() {
        "Ollama Cloud" => "https://ollama.com/api/balance",
        "OpenRouter" => "https://openrouter.ai/api/v1/key",
        "Local model" => return Quota { provider, note: "Local inference · no account quota".into(), available: true, windows: Vec::new() },
        _ => return Quota { provider, note: "Account quota is not exposed by this provider; session usage is tracked separately.".into(), ..Quota::default() },
    };
    let result = (|| {
        let client = options.client()?;
        let key = client
            .authorization()?
            .ok_or("No API credential configured for account usage")?;
        let value = get(url, &key)?;
        parse(&provider, &value)
    })();
    result.unwrap_or_else(|note| Quota {
        provider,
        note,
        available: false,
        windows: Vec::new(),
    })
}

fn get(url: &str, key: &str) -> Result<Value, String> {
    if key.chars().any(char::is_control) {
        return Err("Credential contains invalid control characters".into());
    }
    let config = format!(
        "header = \"Authorization: Bearer {}\"\n",
        key.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let mut child = Command::new("curl")
        .args([
            "-q",
            "-fsS",
            "--connect-timeout",
            "4",
            "--max-time",
            "8",
            "--max-filesize",
            "65536",
            "--proto",
            "=https",
            "--config",
            "-",
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Account usage needs curl".to_string())?;
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(config.as_bytes()).is_err()
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Could not query account usage".into());
    }
    let output = child
        .wait_with_output()
        .map_err(|_| "Account usage request failed".to_string())?;
    if !output.status.success() {
        return Err("Account usage unavailable (credential, rate limit, or connection); retrying in one minute.".into());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| "Provider returned an unreadable usage response".into())
}

pub fn parse(provider: &str, value: &Value) -> Result<Quota, String> {
    let mut quota = Quota {
        provider: provider.into(),
        available: true,
        ..Quota::default()
    };
    let finite = |v: &Value| v.as_f64().filter(|n| n.is_finite() && *n >= 0.0);
    if provider == "Ollama Cloud" {
        let included = &value["included"];
        if let (Some(balance), Some(limit)) = (
            finite(&included["balance_usd"]),
            finite(&included["allowance_usd"]),
        ) {
            if limit > 0.0 {
                quota.windows.push(Window {
                    label: "Monthly".into(),
                    used: (limit - balance).max(0.0),
                    limit,
                    unit: "USD".into(),
                    reset: included
                        .pointer("/period/until")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                });
            }
            quota.note = format!("${balance:.2} included credits remaining");
        } else {
            for (key, label) in [("session", "Session"), ("weekly", "Weekly")] {
                if let Some(remaining) =
                    finite(&included[key]["remaining_percent"]).filter(|n| *n <= 100.0)
                {
                    quota.windows.push(Window {
                        label: label.into(),
                        used: 100.0 - remaining,
                        limit: 100.0,
                        unit: "%".into(),
                        reset: included[key]["resets_at"].as_str().unwrap_or("").into(),
                    });
                }
            }
            if quota.windows.is_empty() {
                return Err("Provider balance response has no supported quota fields".into());
            }
            quota.note = "Legacy plan · account-wide usage".into();
        }
        if let Some(purchased) = finite(&value["purchased"]["balance_usd"]) {
            quota
                .note
                .push_str(&format!(" · ${purchased:.2} purchased"));
        }
    } else if provider == "OpenRouter" {
        let data = &value["data"];
        let budget = &data["effective_budget"];
        if let (Some(limit), Some(used)) =
            (finite(&budget["limit_usd"]), finite(&budget["spend_usd"]))
        {
            if limit > 0.0 {
                quota.windows.push(Window {
                    label: budget["reset_interval"].as_str().unwrap_or("Budget").into(),
                    used,
                    limit,
                    unit: "USD".into(),
                    reset: budget["resets_at"].as_str().unwrap_or("").into(),
                });
            }
        } else if let (Some(limit), Some(remaining)) =
            (finite(&data["limit"]), finite(&data["limit_remaining"]))
            && limit > 0.0
        {
            quota.windows.push(Window {
                label: "API key".into(),
                used: (limit - remaining).max(0.0),
                limit,
                unit: "USD".into(),
                reset: data["limit_reset"].as_str().unwrap_or("").into(),
            });
        }
        if let Some(used) = finite(&data["usage"]) {
            quota.note = format!("${used:.2} reported key usage");
        } else if quota.windows.is_empty() {
            return Err("No account usage returned by OpenRouter".into());
        }
        if quota.windows.is_empty() {
            quota.note.push_str(" · no key budget set");
        }
    } else {
        return Err("Unsupported provider quota".into());
    }
    Ok(quota)
}
