//! What the model is given, built under a budget.
//!
//! The rule worth keeping: **a section that does not fit is dropped whole and named, never cut in half.**
//! A model handed the first two thirds of a rule follows the wrong rule, and nobody can tell afterwards
//! whether it was the model or the truncation. So the bundle lists what it left out, and the turn's log
//! carries that list.

use serde_json::Value;
use session_tree::{Clock, EntryId, Kind, Session, SessionStore};

use crate::fault::Fault;
use crate::profile::Profile;
use crate::task::{Budget, TaskSpec};
use crate::verify::State;
use crate::{PATCH, SHELL, VERIFY};

/// One piece of the prompt, with the name it is dropped under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    name: String,
    text: String,
}

impl Section {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Section {
            name: name.into(),
            text: text.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// What fits, what did not, and the size of the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    sections: Vec<Section>,
    dropped: Vec<String>,
    tokens: usize,
}

impl Bundle {
    pub fn new(sections: Vec<Section>, dropped: Vec<String>, tokens: usize) -> Self {
        Bundle {
            sections,
            dropped,
            tokens,
        }
    }

    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// The names of the sections that did not fit, in the order they were offered.
    pub fn dropped(&self) -> &[String] {
        &self.dropped
    }

    pub fn tokens(&self) -> usize {
        self.tokens
    }

    /// The bundle as one prompt: named sections, because a model that knows which part it is reading
    /// argues with the right part.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for section in &self.sections {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str("## ");
            out.push_str(section.name());
            out.push('\n');
            out.push_str(section.text());
        }
        out
    }
}

/// A rough size, four characters to a token.
///
/// A placeholder with a seam behind it: the real count needs the model's tokenizer, and the only reason
/// this is not it is that the model is not wired in yet. It is deliberately on the *generous* side, so a
/// budget that fits here fits there.
pub fn estimate(text: &str) -> usize {
    let chars = text.chars().count();
    if chars == 0 { 0 } else { chars.div_ceil(4) }
}

/// What an accepted attempt established, which every later attempt has to honour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
    text: String,
    /// The node that accepted it, so a constraint can be argued with rather than merely obeyed.
    from: String,
}

impl Constraint {
    pub fn new(text: impl Into<String>, from: impl Into<String>) -> Self {
        Constraint {
            text: text.into(),
            from: from.into(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn from(&self) -> &str {
        &self.from
    }
}

/// One attempt that did not pass, kept because the next attempt should not repeat it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Try {
    program: String,
    output: String,
    state: String,
    /// What the tool itself observed, as opposed to what the check said. `None` when the log being read
    /// predates this field: a missing observation is not the same fact as an exit of zero.
    exit_status: Option<i32>,
    printed_nothing: bool,
    /// How far the check got, when it opted into the progress contract. A bare `state` string is the
    /// binary fallback, not the same fact as `0/0`.
    progress: Option<crate::verify::Progress>,
}

impl Try {
    pub fn new(
        program: impl Into<String>,
        output: impl Into<String>,
        state: impl Into<String>,
    ) -> Self {
        Try {
            program: program.into(),
            output: output.into(),
            state: state.into(),
            exit_status: None,
            printed_nothing: false,
            progress: None,
        }
    }

    /// What the harness saw when it ran the program, kept next to what the check made of it. The difference
    /// between "the check failed" and "the command did not run" is the whole reason this is here.
    pub fn observed(mut self, exit_status: i32, printed_nothing: bool) -> Self {
        self.exit_status = Some(exit_status);
        self.printed_nothing = printed_nothing;
        self
    }

    pub fn progressed(mut self, progress: Option<crate::verify::Progress>) -> Self {
        self.progress = progress;
        self
    }

    pub fn progress(&self) -> Option<&crate::verify::Progress> {
        self.progress.as_ref()
    }

    /// The sentence the next prompt carries, above the check's own output.
    pub fn observation(&self) -> String {
        match (self.exit_status, self.printed_nothing) {
            (Some(code), true) => format!("the program exited {code} and printed nothing."),
            (Some(code), false) => format!("the program exited {code}."),
            (None, true) => "the program printed nothing.".to_string(),
            (None, false) => String::new(),
        }
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn output(&self) -> &str {
        &self.output
    }

    pub fn state(&self) -> &str {
        &self.state
    }
}

/// The constraints an accepted result establishes, in one place.
///
/// The walk records these the moment it reaches the accept node, and reading a log back has to rebuild the
/// same ones. Two spellings of "what was accepted" would drift, and the drift would show up as a resumed run
/// being told something a live run would not have been.
pub fn accepted(command: &str, program: &str) -> Vec<Constraint> {
    let mut constraints = vec![Constraint::new(
        format!("an answer satisfied this check: {command}"),
        "accept",
    )];
    if !program.trim().is_empty() {
        constraints.push(Constraint::new(
            format!("the answer that passed: {}", one_line(program, 300)),
            "accept",
        ));
    }
    constraints
}

/// One string field of an entry payload, empty when it is absent. Payloads arrive as `Value` because a log
/// is a log: a field an older writer did not have reads as nothing rather than as a broken entry.
fn field(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Text on one line, cut at the head. Flattening the whitespace matters because a program is written in
/// lines and a prompt is read in sections, so an unflattened one reads as several.
pub(crate) fn one_line(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        return flat;
    }
    let head: String = flat.chars().take(limit).collect();
    format!("{head}...")
}

/// How much of the return path a prompt will carry. Bounded, because a run that fails twenty times must not
/// build a twenty-times-larger prompt: the last few attempts are what the next one can act on.
const TRIES_KEPT: usize = 3;
/// A program is cut at its head: the start of a program is its intent, and the tail of a long one is
/// usually the same few closing lines.
const PROGRAM_CHARS: usize = 600;
const PLAN_CHARS: usize = 1200;
/// What the last attempt printed is cut at *both* ends, because an attempt's output has two halves that
/// matter. The failures are at the bottom (a test runner's list, a compiler's error, a check's summary) and
/// the material it gathered is at the top (the file contents a `cat` printed). Keeping only the top threw
/// the failure away and the model repeated its exploration; keeping only the bottom threw the source away
/// and it repeated its exploration for a different reason. Both were real loops against a frontier model.
/// The middle is what goes: a long log's middle is repetition.
const OUTPUT_CHARS: usize = 3000;
/// How the output budget is split: the failure is the more valuable half, so the tail gets more.
const OUTPUT_TAIL_SHARE: usize = 5;

/// The return path: what this run has established, as context for what it does next.
///
/// This is the half of the loop most systems leave out. A run that fails and tries again has learned
/// something by failing, and a second attempt handed exactly the context of the first is a second roll of
/// the same dice rather than a second attempt. Constraints come from accepted results and outrank history;
/// tries come from failures and are capped, so the prompt grows with what is still useful rather than with
/// how long the run has been going.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Learned {
    /// What the last compaction said, verbatim. A cut that leaves no note leaves the next run blind to
    /// everything before it, which is a compaction that costs evidence rather than context.
    summary: Option<String>,
    /// The plan the session wrote, kept across a compaction because it governs every later attempt.
    plan: Option<String>,
    constraints: Vec<Constraint>,
    tries: Vec<Try>,
    /// The last entry this reading covered. A compaction may not cut past it: the run it belongs to has not
    /// happened yet, and a summary of work in progress would be a summary of a guess.
    up_to: Option<EntryId>,
}

impl Learned {
    pub fn new() -> Self {
        Learned::default()
    }

    pub fn constraints(&self) -> &[Constraint] {
        &self.constraints
    }

    pub fn tries(&self) -> &[Try] {
        &self.tries
    }

    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    pub fn plan(&self) -> Option<&str> {
        self.plan.as_deref()
    }

    /// The end of what this reading covers, for the compaction that shortens it.
    pub fn up_to(&self) -> Option<EntryId> {
        self.up_to
    }

    /// The size of what history contributes to a bundle. The number a compaction is decided on.
    pub fn tokens(&self) -> usize {
        self.sections()
            .iter()
            .map(|section| estimate(section.text()))
            .sum()
    }

    /// An accepted result becomes a constraint: the thing that worked stops being a possibility and starts
    /// being a premise.
    pub fn accept(&mut self, constraint: Constraint) {
        self.constraints.push(constraint);
    }

    /// An attempt that did not pass becomes something the next attempt is told about.
    pub fn tried(&mut self, attempt: Try) {
        self.tries.push(attempt);
    }

    /// Whether anything has been learned, so a caller can leave the sections out entirely.
    pub fn is_empty(&self) -> bool {
        self.summary.is_none()
            && self.plan.is_none()
            && self.constraints.is_empty()
            && self.tries.is_empty()
    }

    /// Read a session back into what the next turn is told.
    ///
    /// A resumed run has to see what the run before it learned, or resuming means starting over with a longer
    /// file. Three readings, one per kind of entry that carries anything forward:
    ///
    /// - a check that did not pass becomes a `Try`, carrying the program and the output the log already
    ///   holds, so the next attempt is told what the last one did rather than being handed the same prompt;
    /// - a checkpoint that passed becomes the same constraints the walk records live, built by the same
    ///   function, so a resumed run is told exactly what a live one would have been told;
    /// - a compaction is where the reading starts, and `Session::view()` already begins there, so a
    ///   compacted session reads back shorter while the log keeps every entry.
    ///
    /// This is the function that made "a session survives a kill and resumes" checkable. Without it the
    /// harness could reopen a log and still hand the next turn a prompt that knew nothing about it.
    pub fn from_log<S, C>(session: &Session<S, C>) -> Self
    where
        S: SessionStore + Clone,
        C: Clock + Clone,
    {
        let mut learned = Learned::new();
        // The work a check is about: the program, and what it printed. Both are already in the log, and the
        // `Try` the walk builds live carries the two joined in this order.
        let mut work: Option<String> = None;
        let mut work_output = String::new();
        let mut work_status: Option<i32> = None;

        for entry in session.view() {
            if entry.kind == Kind::Compaction {
                learned.summary = Some(field(&entry.payload, "summary"));
                continue;
            }
            learned.up_to = Some(entry.id);
            if entry.kind == Kind::Plan {
                learned.plan = Some(field(&entry.payload, "plan"));
                continue;
            }
            let tool = field(&entry.payload, "tool");
            if entry.kind == Kind::ToolCall && (tool == SHELL || tool == PATCH) {
                // A program and an edit are both "the work" a check is about. The edit is read from the
                // rendered text, because a resumed run has to be shown the same thing the live one saw.
                work = Some(if tool == PATCH {
                    field(&entry.payload, "rendered")
                } else {
                    field(&entry.payload, "program")
                });
                work_output.clear();
            } else if entry.kind == Kind::ToolResult && (tool == SHELL || tool == PATCH) {
                work_output = field(&entry.payload, "output");
                work_status = entry
                    .payload
                    .get("status")
                    .and_then(Value::as_i64)
                    .map(|code| code as i32);
            } else if entry.kind == Kind::ToolResult && field(&entry.payload, "tool") == VERIFY {
                let state = field(&entry.payload, "state");
                // Anything but a pass is something the next attempt should be told about, which is the same
                // rule the walk applies: a check that could not run is not a pass either.
                if !state.is_empty() && state != State::Passed.name() {
                    let tried = Try::new(
                        work.clone().unwrap_or_default(),
                        format!("{}\n{}", field(&entry.payload, "output"), work_output),
                        state,
                    );
                    learned.tried(match work_status {
                        Some(code) => tried.observed(code, work_output.trim().is_empty()),
                        None => tried,
                    });
                }
            } else if entry.kind == Kind::Checkpoint
                && field(&entry.payload, "check") == State::Passed.name()
            {
                for constraint in accepted(
                    &field(&entry.payload, "command"),
                    work.as_deref().unwrap_or_default(),
                ) {
                    learned.accept(constraint);
                }
            }
        }

        learned
    }

    /// The sections the return path contributes. A premise outranks a history, so constraints come first.
    pub fn sections(&self) -> Vec<Section> {
        let mut sections = Vec::new();

        if let Some(plan) = &self.plan {
            sections.push(Section::new(
                "the plan you wrote",
                cut_head(plan, PLAN_CHARS),
            ));
        }

        if let Some(summary) = &self.summary {
            sections.push(Section::new(
                "summary of compacted history",
                summary.clone(),
            ));
        }

        if !self.constraints.is_empty() {
            let mut text = String::new();
            for constraint in &self.constraints {
                text.push_str("- ");
                text.push_str(constraint.text());
                text.push_str(" (");
                text.push_str(constraint.from());
                text.push_str(")\n");
            }
            sections.push(Section::new("already established", text));
        }

        if !self.tries.is_empty() {
            let recent: Vec<&Try> = self.tries.iter().rev().take(TRIES_KEPT).collect();
            let mut text = String::new();
            for (index, attempt) in recent.iter().rev().enumerate() {
                text.push_str(&format!(
                    "attempt {} did not pass ({}).\n{}its program was:\n{}\nand the check said:\n{}\n\n",
                    index + 1,
                    attempt.state(),
                    match attempt.observation() {
                        said if said.is_empty() => String::new(),
                        said => format!("{said}\n"),
                    },
                    cut_head(attempt.program(), PROGRAM_CHARS),
                    cut_both(attempt.output(), OUTPUT_CHARS)
                ));
            }
            sections.push(Section::new("what was already tried", text));
        }

        sections
    }
}

/// The first `limit` characters, marked when it was cut.
pub fn cut_head(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(limit).collect();
    format!("{head}\n[cut after {limit} characters]")
}

/// Both ends, with the middle cut and the cut announced. The end carries the failure and the start carries
/// what the attempt found out; a long output's middle is the part nobody needs twice.
fn cut_both(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= limit {
        return trimmed.to_string();
    }
    let tail = limit * OUTPUT_TAIL_SHARE / 10;
    let head = limit - tail;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    let cut = chars.len() - head - tail;
    format!("{start}\n[... {cut} characters cut from the middle ...]\n{end}")
}

/// The map takes at most this fraction of the remaining budget, and only if it gets this many tokens.
pub const MAP_SHARE: usize = 2;
pub const MIN_MAP_TOKENS: usize = 200;

/// The sections a turn offers, in the order they are offered: the fixed ones, then what the run has
/// learned, then the repo map with whatever is left.
///
/// The map goes last because it is the largest thing here by an order of magnitude, and a builder that
/// offers it first offers only a map. What the run learned goes before it because it is smaller and worth
/// more once there is any.
pub fn sections(
    task: &TaskSpec,
    profile: &Profile,
    budget: usize,
    learned: &Learned,
    cache: &mut crate::repomap::Cache,
) -> Result<Vec<Section>, Fault> {
    let mut sections = fixed(task, profile);
    sections.extend(learned.sections());

    let used: usize = sections.iter().map(|s| estimate(s.text())).sum();
    let left = budget.saturating_sub(used).min(budget / MAP_SHARE);
    if left >= MIN_MAP_TOKENS
        && let Some(section) =
            crate::repomap::section_cached(task.root(), task.goal(), left, cache)?
    {
        sections.push(section);
    }
    Ok(sections)
}

/// The sections that are in every prompt, in the order a model reads best.
fn fixed(task: &TaskSpec, profile: &Profile) -> Vec<Section> {
    let mut files = String::new();
    if let Ok(listing) = std::fs::read_dir(task.root()) {
        let mut names: Vec<String> = listing
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            files.push_str(&name);
            files.push('\n');
        }
    }

    vec![
        Section::new("goal", task.goal()),
        Section::new("root", task.root().to_path_buf().display().to_string()),
        Section::new("files", files),
        Section::new(
            "check",
            format!("the check for this work is: {}", profile.verify),
        ),
    ]
}

/// Building context is a capability with more than one implementation: this one packs text, and a repo
/// map is the next one.
pub trait Context {
    fn build(&self, task: &TaskSpec, budget: &Budget) -> Result<Bundle, Fault>;
}

/// Packs whole sections in the order offered, skipping what does not fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budgeted {
    sections: Vec<Section>,
}

impl Budgeted {
    pub fn new(sections: Vec<Section>) -> Self {
        Budgeted { sections }
    }

    /// The sections this builder will offer, for a caller that wants to show them.
    pub fn sections(&self) -> &[Section] {
        &self.sections
    }
}

impl Context for Budgeted {
    fn build(&self, _task: &TaskSpec, budget: &Budget) -> Result<Bundle, Fault> {
        let mut kept = Vec::new();
        let mut dropped = Vec::new();
        let mut tokens = 0usize;

        for section in &self.sections {
            let cost = estimate(section.text());
            if tokens + cost <= budget.context_tokens() {
                tokens += cost;
                kept.push(section.clone());
            } else {
                dropped.push(section.name().to_string());
            }
        }

        Ok(Bundle::new(kept, dropped, tokens))
    }
}

#[cfg(test)]
mod tests {

    /// A scratch base outside the source tree: the build directory when cargo names one, the temp
    /// directory otherwise. Never `<manifest>/../target`, which belongs to the build and, on a machine
    /// with a nearly full root filesystem, is the wrong disk to fill.
    fn scratch_base() -> std::path::PathBuf {
        crate::scratch::root()
    }

    use super::*;

    fn task() -> TaskSpec {
        TaskSpec::new("a goal", "/tmp")
    }

    /// What the tool saw is not what the check said, and the next attempt needs both.
    #[test]
    fn a_try_carries_the_observation_above_the_checks_output() {
        let mut learned = Learned::new();
        learned.tried(Try::new("false", "the check said no", "failed").observed(1, true));
        let text: String = learned
            .sections()
            .iter()
            .map(|section| section.text().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("the program exited 1 and printed nothing."),
            "{text}"
        );
        let at = text.find("the program exited").expect("the sentence");
        let check = text.find("the check said no").expect("the output");
        assert!(
            at < check,
            "the observation comes before the check's output"
        );
    }

    #[test]
    fn a_section_that_does_not_fit_is_dropped_whole_and_named() {
        let builder = Budgeted::new(vec![
            Section::new("goal", "g"),
            Section::new("big", "x".repeat(400)),
            Section::new("small", "y"),
        ]);
        let bundle = builder.build(&task(), &Budget::new(20)).expect("a bundle");
        assert_eq!(bundle.sections().len(), 2);
        assert_eq!(bundle.dropped(), ["big"]);
        assert_eq!(bundle.sections()[0].name(), "goal");
    }

    #[test]
    fn a_bundle_names_its_sections_so_a_model_can_argue_with_the_right_one() {
        let builder = Budgeted::new(vec![
            Section::new("goal", "make it work"),
            Section::new("check", "cargo test"),
        ]);
        let text = builder
            .build(&task(), &Budget::new(1000))
            .expect("a bundle")
            .text();
        assert!(text.starts_with("## goal\nmake it work"), "{text}");
        assert!(text.contains("## check\ncargo test"), "{text}");
    }

    #[test]
    fn an_accepted_result_becomes_a_constraint_and_a_failure_becomes_a_history() {
        let mut learned = Learned::new();
        assert!(learned.is_empty());
        assert!(learned.sections().is_empty());

        learned.accept(Constraint::new("retries is 3", "accept"));
        learned.tried(Try::new("echo 1", "exit 1", "failed"));

        let sections = learned.sections();
        assert_eq!(sections.len(), 2);
        // The premise outranks the history.
        assert_eq!(sections[0].name(), "already established");
        assert!(
            sections[0].text().contains("retries is 3"),
            "{:?}",
            sections[0]
        );
        assert_eq!(sections[1].name(), "what was already tried");
        assert!(sections[1].text().contains("echo 1"), "{:?}", sections[1]);
    }

    #[test]
    fn the_return_path_is_capped_so_a_long_run_does_not_grow_the_prompt() {
        let mut learned = Learned::new();
        for i in 0..20 {
            learned.tried(Try::new(format!("program {i}"), "x".repeat(5000), "failed"));
        }
        let sections = learned.sections();
        let history = &sections[0];
        assert!(history.text().contains("program 19"), "{:?}", history);
        // The ceiling is the arithmetic of the caps, not a number someone liked: three tries kept, each at
        // most a program and an output, plus the framing around them. Tying the assertion to the constants
        // means raising a cap fails here rather than silently growing every retry's prompt.
        let ceiling = TRIES_KEPT * (PROGRAM_CHARS + OUTPUT_CHARS) + 2000;
        assert!(
            history.text().chars().count() < ceiling,
            "history is {} chars, over the {ceiling} the caps allow",
            history.text().chars().count()
        );
    }

    #[test]
    fn a_long_output_keeps_both_ends_and_the_middle_goes() {
        // An attempt's output has two halves that matter: what it found out at the top and what failed at
        // the bottom. Cutting only one of them made a frontier model repeat its exploration twice, for two
        // different reasons, in two consecutive real runs.
        let text = format!(
            "SOURCE OF THE FUNCTION\n{}\nFAILED tests/test_calc.py::test_median_even",
            "m".repeat(5000)
        );
        let kept = cut_both(&text, 1000);
        // Both ends survive, the whole thing is bounded, and the cut says how much went.
        assert!(kept.starts_with("SOURCE OF THE FUNCTION"), "{kept}");
        assert!(kept.ends_with("test_median_even"), "{kept}");
        assert!(kept.contains("characters cut from the middle"), "{kept}");
        assert!(
            kept.chars().count() < 1100,
            "{} chars",
            kept.chars().count()
        );
        // The tail gets the larger share, because a failure is worth more than a listing.
        assert!(kept.find("FAILED").expect("the failure is there") > kept.chars().count() / 2);
        assert_eq!(cut_both("short", 1000), "short");
    }

    #[test]
    fn a_long_output_keeps_its_end_and_a_long_program_keeps_its_start() {
        // The end of an output is where the failure is, and the start of a program is its intent. Getting
        // this backwards cost three identical attempts in a real run: the check's summary was at the bottom
        // of the output and the head-cut threw it away.
        let long = format!(
            "{}\nFAILED tests/test_report.py::test_median_line",
            "x".repeat(4000)
        );
        let kept = cut_both(&long, 200);
        assert!(kept.contains("FAILED tests/test_report.py"), "{kept}");
        assert!(
            kept.contains("cut from the middle"),
            "the cut is announced: {kept}"
        );
        assert!(!kept.contains(&"x".repeat(300)), "the middle is gone");

        let program = format!("touch first.txt\n{}", "y".repeat(4000));
        let kept = cut_head(&program, 200);
        assert!(kept.starts_with("touch first.txt"), "{kept}");
        assert!(kept.contains("cut after 200"), "{kept}");

        // And nothing is marked when nothing was cut.
        assert_eq!(cut_both("short", 200), "short");
        assert_eq!(cut_head("short", 200), "short");
    }

    #[test]
    fn the_fixed_sections_come_first_and_anything_learned_comes_before_the_map() {
        // A root of its own, because the map is built against whatever the root is and /tmp is full of
        // other people's directories.
        let root = scratch_base().join(format!("hades-sections-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("a root");
        std::fs::write(root.join("a.rs"), "pub fn a() {}\n").expect("a file");
        let task = TaskSpec::new("a goal", &root);
        let profile = crate::profile::named("survey").expect("the profile");
        let mut learned = Learned::new();
        learned.accept(Constraint::new("a premise", "accept"));
        let mut cache = crate::repomap::Cache::new();
        let sections = sections(&task, &profile, 4000, &learned, &mut cache).expect("sections");
        let names: Vec<&str> = sections.iter().map(Section::name).collect();
        assert_eq!(&names[..4], ["goal", "root", "files", "check"]);
        assert_eq!(names[4], "already established");
        let _ = std::fs::remove_dir_all(&root);
    }
}
