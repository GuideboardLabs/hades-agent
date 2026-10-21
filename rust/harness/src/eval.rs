//! Replaying the failure corpus.
//!
//! A case is a task, an oracle, a pinned starting tree and the conditions it failed under. Eval copies the
//! tree to a throwaway directory, runs the check once, and only then asks the model. A green start means
//! the case is `stale`: it no longer starts red, so replaying it would measure nothing. A red start runs
//! `hades run` with the case's goal, check, policy, model and full bounds. The model gets the same goal and
//! the same start, and has to solve it again.
//!
//! `--against` runs the same cases twice, once under the current harness and once under a candidate. That
//! is the adoption gate's evidence: a candidate that fixes the target and breaks a case that used to pass
//! is a regression, whatever the probe said.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::cases::{self, Case, ResultLine};
use crate::distill;
use crate::task::TaskSpec;
use crate::verify::{CommandVerifier, State, Verifier};

/// How long a case's check may run when the caller names no other bound.
pub const CHECK_MINUTES: u64 = 10;

/// What a caller asked eval to do.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Case ids to run. Empty means every case in the selected profiles.
    pub cases: Vec<String>,
    /// Profile ids to read cases from. Empty means every profile with a ledger.
    pub profiles: Vec<String>,
    /// The harness to run as the base arm. `None` is the profile's current harness.
    pub harness: Option<PathBuf>,
    /// The candidate harness. `Some` makes this a two-arm comparison.
    pub against: Option<PathBuf>,
    /// A model preset to override the case's recorded model. The recorded value is a description, not a
    /// preset name, so it is reported rather than passed.
    pub model: Option<String>,
    /// An attempt ceiling for this run, overriding each case's stored one.
    pub attempts: Option<u32>,
    /// A wall-clock ceiling for this run, overriding each case's stored one.
    pub minutes: Option<u64>,
    pub check_minutes: u64,
    pub include_partial: bool,
    pub include_stale: bool,
    pub dry_run: bool,
    pub keep: bool,
    /// Raw flags forwarded to each replay, for values only the caller knows (an endpoint, a key, a
    /// temperature). The library does not interpret them; the child `hades run` does.
    pub passthrough: Vec<String>,
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub case: String,
    pub profile: String,
    pub goal: String,
    pub base: String,
    pub candidate: Option<String>,
    pub class: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub rows: Vec<Row>,
    pub broken: usize,
    pub regressions: usize,
    pub code: i32,
    pub json: Value,
    pub text: String,
}

/// Replay the selected cases. The caller prints `text` and exits `code`.
pub fn execute(state: &Path, options: &Options) -> Result<Report, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0);
    let work_root = state.join("eval").join(stamp.to_string());

    let profiles = if options.profiles.is_empty() {
        cases::profiles(state)
    } else {
        options
            .profiles
            .iter()
            .map(|profile| cases::safe_profile(profile))
            .collect()
    };

    let mut selected: Vec<Case> = Vec::new();
    for profile in &profiles {
        for case in cases::load(state, profile) {
            if case.status == "retired" {
                continue;
            }
            if !options.cases.is_empty() && !options.cases.iter().any(|id| id == &case.id) {
                continue;
            }
            if case.partial && !options.include_partial {
                continue;
            }
            if case.status == "stale" && !options.include_stale {
                continue;
            }
            selected.push(case);
        }
    }
    if !options.cases.is_empty() {
        for id in &options.cases {
            if !selected.iter().any(|case| &case.id == id) {
                return Err(format!(
                    "`{id}` is not a case this run would replay: it is missing, retired, partial or stale \
                     (use --include-partial or --include-stale for the last two)"
                ));
            }
        }
    }

    let arms: Vec<(&str, Option<PathBuf>)> = match &options.against {
        Some(candidate) => vec![
            ("base", options.harness.clone()),
            ("candidate", Some(candidate.clone())),
        ],
        None => vec![("base", options.harness.clone())],
    };

    if options.dry_run {
        let worst = selected
            .iter()
            .map(|case| {
                let attempts = case.bounds.attempts as u64;
                let width = case.bounds.width.max(1) as u64;
                attempts * width
            })
            .sum::<u64>()
            * arms.len() as u64;
        let mut text = format!(
            "dry run: {} case(s), {} arm(s), at most {worst} model call(s)\nprobes: {} pre-check(s), no model call\n",
            selected.len(),
            arms.len(),
            selected.len() * arms.len()
        );
        for case in &selected {
            text.push_str(&format!(
                "  {}  {}  {} attempt(s){}  {}\n",
                case.id,
                case.bounds.line(),
                case.bounds.attempts,
                if case.attack.is_some() {
                    "  + attack"
                } else {
                    ""
                },
                clip(&case.goal, 90)
            ));
        }
        if selected.is_empty() {
            text.push_str("  (nothing selected)\n");
        }
        return Ok(Report {
            rows: Vec::new(),
            broken: 0,
            regressions: 0,
            code: 0,
            json: json!([]),
            text,
        });
    }

    let mut rows = Vec::new();
    let mut broken = 0usize;
    let mut regressions = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for case in &selected {
        let pinned = match cases::pinned_tree(state, case) {
            Ok(tree) => tree,
            Err(said) => {
                broken += 1;
                failures.push(format!("{}: {said}", case.id));
                let result = ResultLine {
                    id: case.id.clone(),
                    arm: "base".to_string(),
                    outcome: "broken".to_string(),
                    attack: None,
                    attempts: 0,
                    spent: 0,
                    seconds_milli: 0,
                    session: String::new(),
                    state: work_root.display().to_string(),
                    harness: None,
                    model: case.model.clone(),
                    at: now(),
                };
                let _ = cases::append_result(state, &case.profile, &result);
                rows.push(Row {
                    case: case.id.clone(),
                    profile: case.profile.clone(),
                    goal: case.goal.clone(),
                    base: "broken".to_string(),
                    candidate: options.against.as_ref().map(|_| "broken".to_string()),
                    class: "broken".to_string(),
                });
                continue;
            }
        };

        let mut outcomes: Vec<(&str, String)> = Vec::new();
        for (arm, harness) in &arms {
            let outcome = replay_arm(
                state,
                options,
                case,
                &pinned,
                arm,
                harness.as_deref(),
                &work_root,
                stamp,
            );
            match outcome {
                Ok(result) => {
                    if result.outcome == "broken" {
                        broken += 1;
                        failures.push(format!(
                            "{} ({arm}): the case could not be replayed",
                            case.id
                        ));
                    }
                    outcomes.push((arm, result.outcome.clone()));
                    let _ = cases::append_result(state, &case.profile, &result);
                }
                Err(said) => {
                    broken += 1;
                    failures.push(format!("{} ({arm}): {said}", case.id));
                    outcomes.push((arm, "broken".to_string()));
                }
            }
        }

        let base = outcomes
            .iter()
            .find(|(arm, _)| *arm == "base")
            .map(|(_, outcome)| outcome.clone())
            .unwrap_or_else(|| "broken".to_string());
        let candidate = options.against.as_ref().and_then(|_| {
            outcomes
                .iter()
                .find(|(arm, _)| *arm == "candidate")
                .map(|(_, outcome)| outcome.clone())
        });
        let class = classify(case, &base, candidate.as_deref());
        if class == "regression" {
            regressions += 1;
        }
        rows.push(Row {
            case: case.id.clone(),
            profile: case.profile.clone(),
            goal: case.goal.clone(),
            base,
            candidate,
            class,
        });
    }

    let code = if broken > 0 {
        2
    } else if regressions > 0 {
        1
    } else {
        0
    };
    let json = json!(
        rows.iter()
            .map(|row| json!({
                "case": row.case,
                "profile": row.profile,
                "goal": row.goal,
                "base": row.base,
                "candidate": row.candidate,
                "class": row.class,
                "attack": case_attack(&selected, &row.case),
            }))
            .collect::<Vec<Value>>()
    );

    let mut text = format!(
        "eval {stamp}: {} case(s), {} arm(s)\n",
        rows.len(),
        arms.len()
    );
    for row in &rows {
        text.push_str(&format!(
            "  {:<26} {:<8} {:<9} {:<9} {}\n    {}\n",
            row.case,
            row.profile,
            row.base,
            row.candidate.as_deref().unwrap_or("-"),
            row.class,
            clip(&row.goal, 100)
        ));
    }
    if !failures.is_empty() {
        text.push_str("\nbroken:\n");
        for said in &failures {
            text.push_str(&format!("  {said}\n"));
        }
    }
    if options.against.is_some() {
        text.push_str(&format!("\nregressions: {regressions}, broken: {broken}\n"));
    }

    Ok(Report {
        rows,
        broken,
        regressions,
        code,
        json,
        text,
    })
}

fn case_attack(selected: &[Case], id: &str) -> Option<String> {
    selected
        .iter()
        .find(|case| case.id == id)
        .and_then(|case| case.attack.clone())
}

/// The classification the gate reads. `holds` is both passing, `fixed` is base failing and candidate
/// passing, `regression` is the reverse. `base drifted` is the base arm failing a case whose last base
/// result passed, which is a change in the world rather than in the candidate.
fn classify(case: &Case, base: &str, candidate: Option<&str>) -> String {
    let Some(candidate) = candidate else {
        return match base {
            "pass" => "pass".to_string(),
            "stale" => "stale".to_string(),
            "broken" => "broken".to_string(),
            "blocked" => "blocked".to_string(),
            _ => "still failing".to_string(),
        };
    };
    if case.status == "fixed" && base != "pass" {
        return "base drifted".to_string();
    }
    match (base, candidate) {
        ("pass", "pass") => "holds".to_string(),
        ("pass", _) => "regression".to_string(),
        (_, "pass") => "fixed".to_string(),
        _ => "still failing".to_string(),
    }
}

/// One arm, one case: copy the tree, prove the start red, run the goal, keep the log, report the outcome.
#[allow(clippy::too_many_arguments)]
fn replay_arm(
    state: &Path,
    options: &Options,
    case: &Case,
    pinned: &Path,
    arm: &str,
    harness: Option<&Path>,
    work_root: &Path,
    stamp: u128,
) -> Result<ResultLine, String> {
    let work = work_root.join(format!("{arm}-{}", case.id));
    let tree = work.join("tree");
    if work.exists() {
        fs::remove_dir_all(&work)
            .map_err(|error| format!("could not clear {}: {error}", work.display()))?;
    }
    cases::copy_dir(pinned, &tree)?;
    let eval_state = work.join("state");
    let profile = crate::profile::named(&case.profile).ok_or_else(|| {
        format!(
            "the case names profile `{}`, which this build does not ship",
            case.profile
        )
    })?;
    let harness_digest = crate::harness_files::Files::load(
        harness
            .map(|path| path.to_string_lossy().to_string())
            .as_deref(),
        profile.id.as_str(),
    )
    .ok()
    .map(|files| files.prompt().digest);

    let verifier = CommandVerifier::with_deadline(
        crate::engine::LIMIT,
        std::time::Duration::from_secs(options.check_minutes.max(1) * 60),
    );
    let task = TaskSpec::new(case.goal.clone(), tree.clone());
    let started = Instant::now();
    let pre = verifier.verify(&task, &case.check);
    let base_result = |outcome: &str, session: &str, attempts: u32, spent: u64| ResultLine {
        id: case.id.clone(),
        arm: arm.to_string(),
        outcome: outcome.to_string(),
        attack: None,
        attempts,
        spent,
        seconds_milli: started.elapsed().as_millis() as u64,
        session: session.to_string(),
        state: eval_state.display().to_string(),
        harness: harness_digest.clone(),
        model: options.model.clone().unwrap_or_else(|| case.model.clone()),
        at: now(),
    };

    if pre.state == State::Passed {
        return Ok(base_result("stale", "", 0, 0));
    }
    if !pre.state.is_a_result() {
        return Ok(base_result("broken", "", 0, 0));
    }

    let label = format!("eval-{stamp}-{arm}-{}", case.id);
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not find this binary to replay with: {error}"))?;
    let mut command = Command::new(exe);
    command
        .arg("run")
        .arg(&case.goal)
        .arg("--root")
        .arg(&tree)
        .arg("--state")
        .arg(&eval_state)
        .arg("--session")
        .arg(&label)
        .arg("--verify")
        .arg(&case.check)
        .arg("--profile")
        .arg(&case.profile)
        .arg("--attempts")
        .arg(options.attempts.unwrap_or(case.bounds.attempts).to_string())
        .arg("--minutes")
        .arg(options.minutes.unwrap_or(case.bounds.minutes).to_string())
        .arg("--policy")
        .arg(&case.policy)
        .arg("--baseline")
        .arg("off")
        .arg("--shadow")
        .arg("off")
        .arg("--case")
        .arg("off")
        .arg("--check-minutes")
        .arg(options.check_minutes.max(1).to_string())
        .args(&options.passthrough);
    if let Some(tokens) = case.bounds.tokens {
        command.arg("--budget-tokens").arg(tokens.to_string());
    }
    if case.bounds.width > 1 {
        command.arg("--branch").arg(case.bounds.width.to_string());
    }
    if let Some(model) = &options.model {
        command.arg("--model").arg(model);
    }
    if let Some(dir) = harness {
        command.arg("--harness").arg(dir);
    }
    if let Some(attack) = &case.attack {
        command.arg("--attack").arg(attack);
    }
    let output = command
        .output()
        .map_err(|error| format!("could not start the replay: {error}"))?;
    let log = eval_state.join("sessions").join(format!("{label}.jsonl"));
    if !log.is_file() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "the replay wrote no session; it said: {}",
            clip(said.trim(), 400)
        ));
    }
    let facts = distill::load(&eval_state, &label)
        .map_err(|error| format!("the replay's log will not read: {error}"))?;
    let outcome = if facts
        .ended
        .as_deref()
        .is_some_and(|ended| ended == "awaiting")
    {
        "blocked"
    } else if facts.accepted {
        "pass"
    } else {
        "fail"
    };
    let receipt = crate::tuning::receipt(&log);
    let spent = receipt.prompt_tokens + receipt.generated_tokens;

    // The result cites this session. Deleting the log would leave a regression with no evidence, so it is
    // copied into the case before the throwaway state is removed.
    let kept = cases::profile_dir(state, &case.profile)
        .join(&case.id)
        .join("runs")
        .join(format!("{label}.jsonl"));
    if let Some(parent) = kept.parent() {
        crate::scratch::state_dir(parent)
            .map_err(|error| format!("could not make {}: {error}", parent.display()))?;
    }
    fs::copy(&log, &kept).map_err(|error| format!("could not keep {}: {error}", kept.display()))?;
    let _ = crate::scratch::private_file(&kept);

    let mut result = base_result(outcome, &label, facts.attempts_used, spent);
    result.attack = attack_outcome(&log);

    if !options.keep {
        let _ = fs::remove_dir_all(&work);
    }
    Ok(result)
}

/// What the log said about the attack, when the case carries one. The verifier writes it beside the check.
fn attack_outcome(log: &Path) -> Option<String> {
    let text = fs::read_to_string(log).ok()?;
    for line in text.lines().rev() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("kind").and_then(Value::as_str) == Some("tool_result")
            && entry.pointer("/payload/tool").and_then(Value::as_str) == Some("verify")
            && let Some(attack) = entry.pointer("/payload/attack").and_then(Value::as_str)
        {
            return Some(attack.to_string());
        }
    }
    None
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

fn clip(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let cut: String = text.chars().take(cap).collect();
    format!("{cut}…")
}

/// Parse the `eval` command's arguments into options. Kept here so the surface and the library cannot
/// disagree about a default.
#[allow(clippy::too_many_arguments)]
pub fn options_from(
    cases: Vec<String>,
    profiles: Vec<String>,
    harness: Option<PathBuf>,
    against: Option<PathBuf>,
    model: Option<String>,
    attempts: Option<u32>,
    minutes: Option<u64>,
    check_minutes: Option<u64>,
    include_partial: bool,
    include_stale: bool,
    dry_run: bool,
    keep: bool,
    passthrough: Vec<String>,
) -> Options {
    Options {
        cases,
        profiles,
        harness,
        against,
        model,
        attempts,
        minutes,
        check_minutes: check_minutes.unwrap_or(CHECK_MINUTES),
        include_partial,
        include_stale,
        dry_run,
        keep,
        passthrough,
    }
}
