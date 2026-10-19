//! Reading a run as evidence, and writing the composition down.
//!
//! The harness tuning loop needs two things from a finished run: what it cost, and whether the check
//! passed. It needs one thing from the editor: a candidate prompt. This module is the part of that loop
//! that can be read and tested without a model: the receipt, the transcript we are willing to show an
//! editor (with the check redacted), the choice between candidates, and the library note.
//!
//! Why redact the check: an editor that can read `--verify "grep -q 42 out.txt"` will write a prompt that
//! makes the model produce `42` in `out.txt`, which is not the task. The editor may see that a run failed
//! and what the model did; it may not see the string it is being graded by. The candidate is then probed
//! against the real check, which the editor never saw.

use std::path::Path;

use serde_json::Value;

use crate::verify::Progress;

/// What one finished run cost and whether it passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub passed: bool,
    pub answers: usize,
    pub millis: u64,
    pub answer_bytes: usize,
    pub check: String,
    /// The run stopped at an approval question and nobody was there to answer it. Different from a check
    /// that failed, and different from a probe that crashed: the arm was never measured.
    pub awaiting: bool,
    /// The check's partial report, when it opted into the contract.
    pub progress: Option<Progress>,
    /// The protected holdout check, when a person supplied one. `None` means unmeasured.
    pub holdout: Option<bool>,
}

impl Receipt {
    pub fn line(&self) -> String {
        format!(
            "{} answer(s), {} ms, {} bytes, check {}{}",
            self.answers,
            self.millis,
            self.answer_bytes,
            if self.check.is_empty() {
                "not run"
            } else {
                self.check.as_str()
            },
            self.progress
                .as_ref()
                .map(|progress| format!(" ({})", progress.line()))
                .unwrap_or_default()
        )
    }
}

/// Read one session log into a receipt. A missing or unreadable log is a run that did not happen, which is
/// a failure with a reason rather than a panic.
pub fn receipt(log: &Path) -> Receipt {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut answers = 0;
    let mut millis = 0;
    let mut answer_bytes = 0;
    let mut check = String::new();
    let mut awaiting = false;
    let mut progress: Option<Progress> = None;

    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = entry
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = entry.get("payload").unwrap_or(&Value::Null);
        match kind {
            "tool_call" => {
                if let Some(model) = payload.get("model") {
                    answers += 1;
                    millis += model.get("millis").and_then(Value::as_u64).unwrap_or(0);
                }
                if let Some(program) = payload.get("program").and_then(Value::as_str) {
                    answer_bytes += program.len();
                }
            }
            "tool_result" if payload.get("tool").and_then(Value::as_str) == Some("verify") => {
                check = payload
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                progress = payload.get("progress").and_then(Progress::from_value);
            }
            "checkpoint"
                if payload.get("node").and_then(Value::as_str) == Some("awaiting")
                    || payload.get("condition").and_then(Value::as_str) == Some("Asked") =>
            {
                awaiting = true;
            }
            _ => {}
        }
    }

    Receipt {
        passed: check == "passed",
        answers,
        millis,
        answer_bytes,
        check,
        awaiting,
        progress,
        holdout: None,
    }
}

/// A transcript an editor may see: what the model answered and what came back, with every verify program
/// replaced by its outcome and the check text redacted wherever it appears.
pub fn transcript(log: &Path, check: &str, cap: usize) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut said = String::new();
    let mut last_tool = String::new();

    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = entry
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = entry.get("payload").unwrap_or(&Value::Null);
        match kind {
            "tool_call" => {
                let tool = payload.get("tool").and_then(Value::as_str).unwrap_or("?");
                let program = payload.get("program").and_then(Value::as_str).unwrap_or("");
                last_tool = tool.to_string();
                if tool == "verify" {
                    said.push_str("\n[the check ran; its program is withheld]\n");
                } else {
                    said.push_str(&format!(
                        "\n--- {tool}\n{}\n",
                        redact(program, check).trim_end()
                    ));
                }
            }
            "tool_result" => {
                let output = payload.get("output").and_then(Value::as_str).unwrap_or("");
                let state = payload.get("state").and_then(Value::as_str).unwrap_or("");
                if last_tool == "verify" {
                    said.push_str(&format!("[check: {state}]\n"));
                } else if !output.trim().is_empty() {
                    let shown = clip(redact(output, check).trim_end(), 400);
                    said.push_str(&format!("{shown}\n"));
                }
            }
            "checkpoint"
                if payload.get("condition").and_then(Value::as_str) == Some("Exhausted") =>
            {
                said.push_str("\n[the run exhausted its attempts]\n");
            }
            _ => {}
        }
        if said.len() > cap {
            break;
        }
    }
    clip(&said, cap)
}

fn clip(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}

/// Remove the check's own text. The editor is told a check exists, never what it is.
pub fn redact(text: &str, check: &str) -> String {
    let check = check.trim();
    if check.is_empty() {
        return text.to_string();
    }
    text.replace(check, "<the check>")
}

/// One row of the composition table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub receipt: Receipt,
    pub note: String,
}

/// The best row: a passing row beats a failing one, then fewer milliseconds, then fewer answers. `base` is
/// the reference and is never chosen as an adoption target, because copying the base over itself is not a
/// composition.
pub fn choose(rows: &[Row]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (index, row) in rows.iter().enumerate() {
        if row.name == "base" {
            continue;
        }
        let better = match best {
            None => true,
            Some(at) => {
                let current = &rows[at];
                match (row.receipt.passed, current.receipt.passed) {
                    (true, false) => true,
                    (false, true) => false,
                    (true, true) => match (row.receipt.holdout, current.receipt.holdout) {
                        (Some(true), Some(false)) => true,
                        (Some(false), Some(true)) => false,
                        _ => {
                            let row_progress = row.receipt.progress.as_ref().map(Progress::ratio);
                            let current_progress =
                                current.receipt.progress.as_ref().map(Progress::ratio);
                            match (row_progress, current_progress) {
                                (Some(left), Some(right)) if left != right => left > right,
                                _ => {
                                    (row.receipt.millis, row.receipt.answers)
                                        < (current.receipt.millis, current.receipt.answers)
                                }
                            }
                        }
                    },
                    _ => {
                        // A failure that got further is the more useful row. Without a progress report the
                        // two are equal, so the old cost tie-break still decides.
                        let row_progress = row.receipt.progress.as_ref().map(Progress::ratio);
                        let current_progress =
                            current.receipt.progress.as_ref().map(Progress::ratio);
                        match (row_progress, current_progress) {
                            (Some(left), Some(right)) if left != right => left > right,
                            (Some(_), None) => true,
                            (None, Some(_)) => false,
                            _ => {
                                (row.receipt.millis, row.receipt.answers)
                                    < (current.receipt.millis, current.receipt.answers)
                            }
                        }
                    }
                }
            }
        };
        if better {
            best = Some(index);
        }
    }
    best
}

/// Whether the chosen candidate beats the reference on the metric that matters: a pass where the base
/// failed, or an equal pass for less. A tie keeps the base, because an edit with no evidence is churn.
pub fn beats_base(base: &Row, candidate: &Row) -> bool {
    // A candidate that fails the protected holdout is not adoptable, even when it passes the check it was
    // tuned against. That is the whole point of keeping a second check out of the editor's view.
    if candidate.receipt.holdout == Some(false) {
        return false;
    }
    match (candidate.receipt.passed, base.receipt.passed) {
        (true, false) => true,
        (false, true) => false,
        (true, true) => match (candidate.receipt.holdout, base.receipt.holdout) {
            (Some(true), Some(false)) => true,
            (Some(false), Some(true)) => false,
            _ => {
                (candidate.receipt.millis, candidate.receipt.answers)
                    < (base.receipt.millis, base.receipt.answers)
            }
        },
        (false, false) => false,
    }
}

/// The library note: what was tried, what it cost, what was adopted and why. Written even when nothing is
/// adopted, because a failed candidate is the evidence that the base is still the best harness we have.
pub fn experience(
    goal: &str,
    check: &str,
    rows: &[Row],
    adopted: Option<&str>,
    lesson: &str,
    candidate_dirs: &[(String, String)],
) -> String {
    let mut text = format!("# Harness tune: {}\n\n", one_line(goal));
    text.push_str(&format!(
        "- check: `{}`\n- adopted: `{}`\n\n",
        if check.trim().is_empty() {
            "none"
        } else {
            check.trim()
        },
        adopted.unwrap_or("none (base kept)")
    ));
    text.push_str("| variant | check | answers | model ms | answer bytes | note |\n");
    text.push_str("|---|---|---|---|---|---|\n");
    for row in rows {
        text.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            row.name,
            if row.receipt.check.is_empty() {
                "not run"
            } else {
                row.receipt.check.as_str()
            },
            row.receipt.answers,
            row.receipt.millis,
            row.receipt.answer_bytes,
            one_line(&row.note)
        ));
    }
    if !candidate_dirs.is_empty() {
        text.push_str("\n## Candidates\n\n");
        for (name, dir) in candidate_dirs {
            text.push_str(&format!("- `{name}`: `{dir}`\n"));
        }
    }
    if !lesson.trim().is_empty() {
        text.push_str(&format!("\n## Lesson\n\n{}\n", lesson.trim()));
    }
    text
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The editor's answer: one JSON object, with the check's own text forbidden anywhere in it.
pub fn candidate_from(
    reply: &str,
    check: &str,
) -> Result<(String, String, String, String), String> {
    let text = reply.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .unwrap_or(text)
        .trim_end_matches("```")
        .trim();
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| format!("the editor did not answer with JSON: {error}"))?;
    let system = value
        .get("system")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if system.is_empty() {
        return Err("the editor's candidate has no system prompt".to_string());
    }
    if system.len() > crate::harness_files::CAP {
        return Err("the editor's system prompt is over 32 KiB".to_string());
    }
    let guidance = value
        .get("guidance")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if guidance.len() > crate::harness_files::CAP {
        return Err("the editor's tool guidance is over 32 KiB".to_string());
    }
    if !check.trim().is_empty()
        && (system.contains(check.trim()) || guidance.contains(check.trim()))
    {
        return Err("the editor wrote the check into its candidate".to_string());
    }
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("no reason given")
        .trim()
        .to_string();
    let lesson = value
        .get("lesson")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    Ok((system, guidance, reason, lesson))
}

/// Copy the prompt and guidance a target holds into `versions/<seconds>/` before an adoption overwrites
/// them. Returns the directory, or `None` when there was nothing to keep. A version is a plain directory:
/// restoring is a copy back, and nothing depends on a manifest that could itself be wrong.
pub fn keep_version(target: &Path) -> Result<Option<std::path::PathBuf>, String> {
    let files: Vec<&str> = ["system.md", "tool_guidance.md"]
        .into_iter()
        .filter(|name| target.join(name).is_file())
        .collect();
    if files.is_empty() {
        return Ok(None);
    }
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let dir = target.join("versions").join(seconds.to_string());
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not make {}: {error}", dir.display()))?;
    for name in files {
        std::fs::copy(target.join(name), dir.join(name))
            .map_err(|error| format!("could not keep the previous {name}: {error}"))?;
    }
    Ok(Some(dir))
}

/// Copy one candidate over a harness directory: the system prompt always, the guidance when the candidate
/// has one. A candidate that drops the guidance removes the target's, because dropping it is part of the
/// edit and leaving the old file behind would make the adopted harness a blend of two.
pub fn adopt(target: &Path, source: &Path) -> Result<(), String> {
    std::fs::create_dir_all(target)
        .map_err(|error| format!("could not make {}: {error}", target.display()))?;
    std::fs::copy(source.join("system.md"), target.join("system.md")).map_err(|error| {
        format!(
            "could not copy the adopted prompt into {}: {error}",
            target.display()
        )
    })?;
    let guidance = source.join("tool_guidance.md");
    if guidance.is_file() {
        std::fs::copy(&guidance, target.join("tool_guidance.md"))
            .map_err(|error| format!("could not copy the adopted guidance: {error}"))?;
    } else {
        let stale = target.join("tool_guidance.md");
        match std::fs::remove_file(&stale) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "the candidate drops the guidance but {} could not be removed: {error}",
                    stale.display()
                ));
            }
        }
    }
    Ok(())
}

/// A directory name from a goal: lowercase words, hyphens, at most 40 characters.
pub fn slug(goal: &str) -> String {
    let mut slug = String::new();
    for word in goal.split_whitespace() {
        let word: String = word
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if word.is_empty() {
            continue;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word);
        if slug.len() >= 40 {
            break;
        }
    }
    if slug.is_empty() {
        "tune".to_string()
    } else {
        slug.truncate(40);
        slug.trim_end_matches('-').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn log(name: &str, body: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/scratch")
            .join(format!("hades-tune-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("session.jsonl");
        std::fs::write(&path, body).expect("log");
        path
    }

    const RUN: &str = concat!(
        r#"{"kind":"tool_call","payload":{"tool":"shell","program":"echo hi","model":{"millis":1000}}}"#,
        "\n",
        r#"{"kind":"tool_result","payload":{"tool":"shell","output":"hi\n","state":"ok"}}"#,
        "\n",
        r#"{"kind":"tool_call","payload":{"tool":"verify","program":"grep -q 42 out.txt"}}"#,
        "\n",
        r#"{"kind":"tool_result","payload":{"tool":"verify","output":"exit 0\n","state":"passed"}}"#,
        "\n",
    );

    #[test]
    fn a_receipt_reads_answers_cost_and_the_check() {
        let path = log("receipt", RUN);
        let receipt = receipt(&path);
        assert!(receipt.passed);
        assert_eq!(receipt.answers, 1);
        assert_eq!(receipt.millis, 1000);
        assert_eq!(receipt.check, "passed");
        assert!(receipt.answer_bytes > 0);
        assert!(!receipt.awaiting);
    }

    #[test]
    fn a_probe_that_stopped_at_a_question_is_marked_awaiting_not_failed() {
        const PARKED: &str = concat!(
            r#"{"kind":"tool_call","payload":{"tool":"shell","program":"rm -rf build","model":{"millis":10}}}"#,
            "\n",
            r#"{"kind":"approval_request","payload":{"action":"permanently delete","reason":"the hard floor"}}"#,
            "\n",
            r#"{"kind":"checkpoint","payload":{"node":"awaiting","condition":"Asked","attempt":1}}"#,
            "\n",
        );
        let path = log("parked", PARKED);
        let receipt = receipt(&path);
        assert!(receipt.awaiting, "{receipt:?}");
        assert!(!receipt.passed);
        assert_eq!(receipt.answers, 1);
        assert!(receipt.check.is_empty(), "no verify ran");
    }

    #[test]
    fn the_transcript_withholds_the_check_and_redacts_it() {
        let path = log("transcript", RUN);
        let said = transcript(&path, "grep -q 42 out.txt", 4000);
        assert!(said.contains("echo hi"), "{said}");
        assert!(!said.contains("grep -q 42"), "{said}");
        assert!(said.contains("[the check ran"), "{said}");
        assert!(said.contains("[check: passed]"), "{said}");
    }

    #[test]
    fn redaction_is_exact_and_harmless_when_empty() {
        assert_eq!(redact("run a; then b", "a"), "run <the check>; then b");
        assert_eq!(redact("nothing", ""), "nothing");
    }

    fn row(name: &str, passed: bool, millis: u64, answers: usize) -> Row {
        Row {
            name: name.to_string(),
            receipt: Receipt {
                passed,
                answers,
                millis,
                answer_bytes: 10,
                check: if passed {
                    "passed".into()
                } else {
                    "failed".into()
                },
                awaiting: false,
                progress: None,
                holdout: None,
            },
            note: String::new(),
        }
    }

    #[test]
    fn a_pass_beats_a_faster_failure_and_a_cheaper_pass_wins_between_passes() {
        let base = row("base", false, 100, 1);
        let inv = row("invariant", true, 900, 2);
        let dom = row("domain", false, 50, 1);
        let rows = vec![base.clone(), inv.clone(), dom];
        assert_eq!(choose(&rows), Some(1));
        assert!(beats_base(&base, &inv));

        let base = row("base", true, 900, 2);
        let cheap = row("invariant", true, 800, 1);
        assert!(beats_base(&base, &cheap));
        let same = row("invariant", true, 900, 2);
        assert!(!beats_base(&base, &same));
    }

    #[test]
    fn the_experience_note_carries_the_table_and_the_lesson() {
        let rows = vec![row("base", true, 900, 2), row("invariant", true, 800, 1)];
        let note = experience(
            "build stats.sh",
            "test -f stats.sh",
            &rows,
            Some("invariant"),
            "say where to write",
            &[("invariant".into(), "/tmp/inv".into())],
        );
        assert!(note.contains("| base | passed | 2 | 900 |"), "{note}");
        assert!(note.contains("adopted: `invariant`"), "{note}");
        assert!(note.contains("say where to write"), "{note}");
    }

    #[test]
    fn a_candidate_is_one_json_object_and_may_not_name_the_check() {
        let good = r#"```json
{"reason":"needs a plan","system":"Plan first.","guidance":"","lesson":"say it"}
```"#;
        let (system, guidance, reason, _) =
            candidate_from(good, "grep -q 42 out.txt").expect("parsed");
        assert_eq!(system, "Plan first.");
        assert!(guidance.is_empty());
        assert_eq!(reason, "needs a plan");

        let leak = r#"{"reason":"x","system":"Run grep -q 42 out.txt","guidance":""}"#;
        let said = candidate_from(leak, "grep -q 42 out.txt").expect_err("refused");
        assert!(said.contains("wrote the check"), "{said}");

        let empty = r#"{"reason":"x","system":"   ","guidance":""}"#;
        assert!(candidate_from(empty, "").is_err());
        assert!(candidate_from("not json", "").is_err());
    }

    #[test]
    fn adoption_copies_the_prompt_and_drops_stale_guidance() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/scratch")
            .join(format!("hades-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("source");
        let target = root.join("target");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::write(source.join("system.md"), "new prompt").expect("system");
        std::fs::write(target.join("system.md"), "old prompt").expect("old system");
        std::fs::write(target.join("tool_guidance.md"), "stale").expect("stale");
        adopt(&target, &source).expect("adopted");
        assert_eq!(
            std::fs::read_to_string(target.join("system.md")).expect("read"),
            "new prompt"
        );
        assert!(
            !target.join("tool_guidance.md").exists(),
            "stale guidance survived"
        );
        std::fs::write(source.join("tool_guidance.md"), "new guidance").expect("guidance");
        adopt(&target, &source).expect("adopted again");
        assert_eq!(
            std::fs::read_to_string(target.join("tool_guidance.md")).expect("read"),
            "new guidance"
        );
    }

    #[test]
    fn a_slug_is_short_and_readable() {
        assert_eq!(
            slug("Build stats.sh, then run it!"),
            "build-statssh-then-run-it"
        );
        assert_eq!(slug("   "), "tune");
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/scratch")
            .join(format!("versions-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    #[test]
    fn an_adoption_keeps_the_prompt_it_replaces() {
        let target = scratch("target");
        std::fs::write(target.join("system.md"), "old prompt").expect("the old prompt");
        std::fs::write(target.join("tool_guidance.md"), "old guidance").expect("the old guidance");
        let kept = keep_version(&target)
            .expect("no error")
            .expect("something to keep");
        assert!(kept.starts_with(target.join("versions")));
        assert_eq!(
            std::fs::read_to_string(kept.join("system.md")).expect("the kept prompt"),
            "old prompt"
        );
    }

    #[test]
    fn a_target_with_nothing_to_keep_says_so() {
        let target = scratch("empty");
        assert!(keep_version(&target).expect("no error").is_none());
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    fn row_with(
        name: &str,
        passed: bool,
        millis: u64,
        progress: Option<(u32, u32)>,
        holdout: Option<bool>,
    ) -> Row {
        Row {
            name: name.to_string(),
            receipt: Receipt {
                passed,
                answers: 1,
                millis,
                answer_bytes: 10,
                check: if passed {
                    "passed".into()
                } else {
                    "failed".into()
                },
                awaiting: false,
                progress: progress.map(|(passed, total)| Progress {
                    gate: false,
                    passed,
                    total,
                    family: None,
                }),
                holdout,
            },
            note: String::new(),
        }
    }

    #[test]
    fn a_failure_that_got_further_is_the_better_row() {
        let base = row_with("base", false, 10, Some((1, 5)), None);
        let near = row_with("near", false, 900, Some((4, 5)), None);
        let quick = row_with("quick", false, 50, Some((1, 5)), None);
        let rows = vec![base, near, quick];
        assert_eq!(choose(&rows), Some(1), "4/5 beats 1/5 even when slower");
    }

    #[test]
    fn a_failed_holdout_is_never_adopted() {
        let base = row_with("base", true, 100, None, Some(true));
        let candidate = row_with("candidate", true, 50, None, Some(false));
        assert!(!beats_base(&base, &candidate));
    }

    #[test]
    fn a_passed_holdout_breaks_a_tie_on_cost() {
        let base = row_with("base", true, 100, None, Some(false));
        let candidate = row_with("candidate", true, 500, None, Some(true));
        assert!(beats_base(&base, &candidate));
    }
}
