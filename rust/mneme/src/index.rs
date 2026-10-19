//! The index: a view of the frontmatter, so it can never drift from it.

use std::collections::BTreeMap;
use std::path::Path;

use crate::INBOX_EXPIRY_DAYS;
use crate::area::Area;
use crate::note::{Note, relative_posix};

/// The generated file, at the root of the vault.
pub const INDEX_NAME: &str = "INDEX.md";

/// Render the index from notes that have already been read.
///
/// Paths are vault-relative and never absolute: an index that leaks the machine it was built on is not
/// something anyone can publish. The Inbox is counted rather than listed, because it is a queue and
/// not part of the graph.
pub fn render(
    generated: &str,
    root: &Path,
    notes: &[Note],
    inbox: &[Note],
    stale: &[Note],
) -> String {
    let mut by_topic: BTreeMap<&str, Vec<&Note>> = BTreeMap::new();
    let mut by_project: BTreeMap<&str, Vec<&Note>> = BTreeMap::new();
    for note in notes {
        let Ok(relative) = note.path.strip_prefix(root) else {
            continue;
        };
        let Some(first) = relative.components().next() else {
            continue;
        };
        let folder = first.as_os_str().to_string_lossy();
        if folder.eq_ignore_ascii_case(Area::Inbox.folder()) {
            continue;
        }
        for topic in note.topics() {
            by_topic.entry(topic.as_str()).or_default().push(note);
        }
        for project in note.projects() {
            by_project.entry(project.as_str()).or_default().push(note);
        }
    }

    let mut lines: Vec<String> = vec![
        "# Mneme index".to_string(),
        String::new(),
        format!("Generated {generated} from note frontmatter. Do not edit."),
        String::new(),
        "## Topics".to_string(),
        String::new(),
    ];
    if by_topic.is_empty() {
        lines.push(
            "No topics yet. A topic is a note in `40 Topics`; everything else links to it."
                .to_string(),
        );
        lines.push(String::new());
    } else {
        for (topic, found) in &by_topic {
            lines.push(format!("### {topic} ({})", found.len()));
            for note in found {
                lines.push(entry_line(note, root, true));
            }
            lines.push(String::new());
        }
    }

    lines.push("## Projects".to_string());
    lines.push(String::new());
    if by_project.is_empty() {
        lines.push("No projects yet.".to_string());
        lines.push(String::new());
    } else {
        for (project, found) in &by_project {
            lines.push(format!("### {project} ({})", found.len()));
            for note in found {
                lines.push(entry_line(note, root, false));
            }
            lines.push(String::new());
        }
    }

    lines.push("## Inbox".to_string());
    lines.push(String::new());
    lines.push(format!(
        "{} candidate(s), {} stale after {INBOX_EXPIRY_DAYS} days.",
        inbox.len(),
        stale.len()
    ));
    lines.push(String::new());
    lines.join("\n")
}

/// One bullet: the title, where the note lives, and, under a topic, why it is there.
fn entry_line(note: &Note, root: &Path, with_summary: bool) -> String {
    let mut line = format!("- {} ({})", note.title(), relative_posix(&note.path, root));
    if with_summary && !note.summary().is_empty() {
        line.push_str(&format!(": {}", note.summary()));
    }
    line
}
