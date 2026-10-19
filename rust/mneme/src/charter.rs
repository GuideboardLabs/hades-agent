//! The charter: the onboarding answers, and the text they render to.
//!
//! Plain text on purpose. It is read at the start of every session and edited by hand as often as it
//! is regenerated, so both directions have to work.
//!
//! Three answers, in the order the interview asks them: who the agent is, what the work is for, and
//! which topics matter. The topics are named here rather than seeded as notes: the first topics are a
//! statement of intent, and a note is something a run has actually read or written.

/// The file the answers live in, at the root of the vault.
pub const CHARTER_NAME: &str = "charter.md";

/// Who the agent is, what the work is for, and which topics matter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Charter {
    pub name: String,
    pub goal: String,
    pub topics: Vec<String>,
}

impl Charter {
    /// Whether anything was ever answered.
    pub fn is_set(&self) -> bool {
        !self.name.is_empty() || !self.goal.is_empty() || !self.topics.is_empty()
    }
}

/// The whole file, written from the three answers.
pub fn render(agent_name: &str, agent_goal: &str, topics: &[String]) -> String {
    let topics: Vec<&str> = topics
        .iter()
        .map(|topic| topic.trim())
        .filter(|topic| !topic.is_empty())
        .collect();
    let listed = if topics.is_empty() {
        "None named yet. Name one here by hand, with `hades init`, or by writing a note in `40 Topics`."
            .to_string()
    } else {
        topics
            .iter()
            .map(|topic| format!("- {topic}"))
            .collect::<Vec<String>>()
            .join("\n")
    };
    format!(
        "# Charter\n\n\
         Loaded at the start of every session. Edit it by hand, or run `hades init` again.\n\n\
         ## The agent\n\n\
         - name: {}\n\n\
         ## The goal\n\n\
         {}\n\n\
         ## Topics\n\n\
         {listed}\n\n\
         ## How to change this\n\n\
         Everything here is plain text on purpose. The name is what the agent is called; the goal is \
         the standing context every piece of work is judged against: what a session opens with, what \
         the dreamer's candidates are measured against, and what the self-audit checks the work \
         against. The topics are the spine of the graph: a note is filed against one, and a recalled \
         bundle is chosen by it.\n",
        agent_name.trim(),
        agent_goal.trim()
    )
}

/// Read the two answers back. Anything missing reads as empty rather than as an error: a vault
/// without a charter is a vault that has not been onboarded yet, which is a state and not a fault.
pub fn parse(text: &str) -> Charter {
    let mut charter = Charter::default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("- name:") {
            charter.name = rest.trim().to_string();
            break;
        }
    }
    if let Some(start) = body_of_heading(text, "## The goal") {
        let rest = &text[start..];
        let end = rest.find("\n## ").unwrap_or(rest.len());
        charter.goal = rest[..end].trim().to_string();
    }
    if let Some(start) = body_of_heading(text, "## Topics") {
        let rest = &text[start..];
        let end = rest.find("\n## ").unwrap_or(rest.len());
        charter.topics = rest[..end]
            .lines()
            .filter_map(|line| line.trim().strip_prefix("- "))
            .map(|topic| topic.trim().to_string())
            .filter(|topic| !topic.is_empty())
            .collect();
    }
    charter
}

/// The byte after a heading line and the blank lines under it, when that heading is in the text.
fn body_of_heading(text: &str, heading: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let content = line
            .strip_suffix('\n')
            .unwrap_or(line)
            .trim_end_matches('\r');
        if content.trim_end() == heading {
            let mut start = offset + line.len();
            while text[start..].starts_with('\n') {
                start += 1;
            }
            return Some(start);
        }
        offset += line.len();
    }
    None
}
