//! The index: a view of the frontmatter, so it can never drift from it.
//!
//! It reads like a WEFT map: the spine first (topics by category), then the work that hangs off it
//! (research by topic), then what a project became (products), then the two queues. Markers are
//! signage and are left out, so a fresh clone reads as empty rather than as six fake notes.

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
    let area_of = |note: &Note| -> Option<Area> {
        let relative = note.path.strip_prefix(root).ok()?;
        let first = relative.components().next()?.as_os_str().to_string_lossy();
        Area::ALL
            .into_iter()
            .find(|area| area.folder() == first.as_ref())
    };

    let mut topics: BTreeMap<&str, Vec<&Note>> = BTreeMap::new();
    let mut research: BTreeMap<&str, Vec<&Note>> = BTreeMap::new();
    let mut projects: Vec<&Note> = Vec::new();
    let mut products: Vec<&Note> = Vec::new();
    let mut outbox: Vec<&Note> = Vec::new();

    for note in notes {
        if note.is_marker() {
            continue;
        }
        match area_of(note) {
            Some(Area::Topics) => topics.entry(note.category()).or_default().push(note),
            Some(Area::Research) => research.entry(note.topic()).or_default().push(note),
            Some(Area::Projects) => projects.push(note),
            Some(Area::Products) => products.push(note),
            Some(Area::Outbox) => outbox.push(note),
            _ => {}
        }
    }

    let mut lines: Vec<String> = vec![
        "# WEFT: index".to_string(),
        String::new(),
        format!("Generated {generated} from note frontmatter. Do not edit."),
        String::new(),
        "The 26 categories are in [`TAXONOMY.md`](TAXONOMY.md). Every research note, project and \
         product names a topic; every project names its research."
            .to_string(),
        String::new(),
        "## Topics".to_string(),
        String::new(),
    ];
    if topics.is_empty() {
        lines.push(
            "No topics yet. A topic is `50 Topics/<category>/<topic>/index.md`; everything else \
             hangs off one."
                .to_string(),
        );
        lines.push(String::new());
    } else {
        for (category, found) in &topics {
            lines.push(format!("### {category} ({})", found.len()));
            for note in found {
                lines.push(entry_line(note, root, true));
            }
            lines.push(String::new());
        }
    }

    lines.push("## Research".to_string());
    lines.push(String::new());
    if research.is_empty() {
        lines.push("No research yet.".to_string());
        lines.push(String::new());
    } else {
        for (topic, found) in &research {
            lines.push(format!("### {topic} ({})", found.len()));
            for note in found {
                lines.push(entry_line(note, root, true));
            }
            lines.push(String::new());
        }
    }

    lines.push("## Projects".to_string());
    lines.push(String::new());
    if projects.is_empty() {
        lines.push("No projects yet.".to_string());
        lines.push(String::new());
    } else {
        for note in &projects {
            let research = note.research().len();
            lines.push(format!(
                "{} ({} research note(s))",
                entry_line(note, root, true),
                research
            ));
        }
        lines.push(String::new());
    }

    lines.push("## Products".to_string());
    lines.push(String::new());
    if products.is_empty() {
        lines.push("No products yet. A product is a project that stopped moving.".to_string());
        lines.push(String::new());
    } else {
        for note in &products {
            lines.push(format!(
                "{} (from `{}`{})",
                entry_line(note, root, true),
                note.from_project(),
                if note.status().is_empty() {
                    String::new()
                } else {
                    format!(", {}", note.status())
                }
            ));
        }
        lines.push(String::new());
    }

    lines.push("## Outbox".to_string());
    lines.push(String::new());
    if outbox.is_empty() {
        lines.push(
            "Empty. Messages for other agents are left here, and nothing reads them yet."
                .to_string(),
        );
    } else {
        lines.push(format!(
            "{} message(s), not read. This folder is a channel nothing consumes yet.",
            outbox.len()
        ));
    }
    lines.push(String::new());

    lines.push("## Inbox".to_string());
    lines.push(String::new());
    lines.push(format!(
        "{} candidate(s), {} stale after {INBOX_EXPIRY_DAYS} days.",
        inbox.iter().filter(|note| !note.is_marker()).count(),
        stale.iter().filter(|note| !note.is_marker()).count()
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
