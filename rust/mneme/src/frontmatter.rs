//! The frontmatter: the small, documented subset of YAML the vault writes, as named fields.
//!
//! Scalars, and comma separated lists in square brackets, and nothing else. A note without
//! frontmatter is not an error; it simply has no metadata yet.

use std::collections::BTreeMap;

/// The order the vault writes keys in: the ones it names, then anything a hand edit added, sorted.
pub const ORDER: [&str; 13] = [
    "title",
    "summary",
    "status",
    "category",
    "topic",
    "parent",
    "topics",
    "projects",
    "research",
    "from_project",
    "source_url",
    "provenance",
    "captured",
];

/// A value the vault does not give a name to. Kept as the kind it was, so a key someone typed by hand
/// survives a rewrite instead of being quietly dropped on the next move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    Scalar(String),
    List(Vec<String>),
}

/// The metadata of one note: named fields for the documented keys, so durability reads off the record
/// rather than probing a dictionary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub status: Option<String>,
    /// A topic's fixed door, one of [`crate::topic::Category`]. Empty on every other kind of note.
    pub category: Option<String>,
    /// The primary topic, by slug. Research, projects and products name one; topics are one.
    pub topic: Option<String>,
    /// A subtopic's parent topic slug. Empty for a root topic.
    pub parent: Option<String>,
    pub topics: Vec<String>,
    pub projects: Vec<String>,
    /// The research notes a project stands on. A durable project names at least one.
    pub research: Vec<String>,
    /// The project a product came from. A durable product names one.
    pub from_project: Option<String>,
    pub source_url: Option<String>,
    pub provenance: Option<String>,
    pub captured: Option<String>,
    /// Keys the vault does not name, in key order.
    pub extra: BTreeMap<String, Field>,
}

impl Frontmatter {
    /// Read the header off the text and hand back the body that follows it. Text with no header is all
    /// body.
    pub fn parse(text: &str) -> (Frontmatter, &str) {
        let Some((raw, body)) = split(text) else {
            return (Frontmatter::default(), text);
        };
        let mut frontmatter = Frontmatter::default();
        let lines: Vec<&str> = raw.lines().collect();
        let mut index = 0;
        while index < lines.len() {
            let line = lines[index];
            let left = line.trim_start();
            if left.is_empty() || left.starts_with('#') {
                index += 1;
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                index += 1;
                continue;
            };
            let key = key.trim().to_lowercase();
            let value = value.trim();
            // A hand-written block list is a list: `topics:` followed by `- one` lines. Without this,
            // the key parses as an empty scalar and every topic a person typed is silently dropped on
            // the next import, which is exactly the path an edited note takes.
            if value.is_empty() {
                let mut items: Vec<String> = Vec::new();
                let mut look = index + 1;
                while look < lines.len() {
                    let candidate = lines[look].trim();
                    let Some(item) = candidate.strip_prefix("- ") else {
                        break;
                    };
                    match parse_value(item.trim()) {
                        Field::Scalar(text) if !text.is_empty() => items.push(text),
                        Field::List(more) => items.extend(more),
                        _ => {}
                    }
                    look += 1;
                }
                if !items.is_empty() {
                    frontmatter.put(&key, Field::List(items));
                    index = look;
                    continue;
                }
            }
            frontmatter.put(&key, parse_value(value));
            index += 1;
        }
        (frontmatter, body)
    }

    /// Render in a stable key order, so two runs over the same note are byte-identical and a diff
    /// shows only what changed.
    pub fn render(&self) -> String {
        let mut lines: Vec<String> = vec!["---".to_string()];
        for key in ORDER {
            if let Some(field) = self.named(key) {
                lines.push(field_line(key, &field));
            }
        }
        for (key, field) in &self.extra {
            lines.push(field_line(key, field));
        }
        lines.push("---".to_string());
        lines.join("\n") + "\n"
    }

    /// Set one key from a parsed value, by name.
    fn put(&mut self, key: &str, field: Field) {
        match key {
            "title" => self.title = as_text(field),
            "summary" => self.summary = as_text(field),
            "status" => self.status = as_text(field),
            "category" => self.category = as_text(field),
            "topic" => self.topic = as_text(field),
            "parent" => self.parent = as_text(field),
            "research" => self.research = as_list(field),
            "from_project" => self.from_project = as_text(field),
            "source_url" => self.source_url = as_text(field),
            "provenance" => self.provenance = as_text(field),
            "captured" => self.captured = as_text(field),
            "topics" => self.topics = as_list(field),
            "projects" => self.projects = as_list(field),
            _ => {
                self.extra.insert(key.to_string(), field);
            }
        }
    }

    /// The key as it would be written, or nothing when this note does not carry it. An empty list is
    /// the same as an absent key here, which is the one shape the vault never writes: a note without a
    /// topic cannot leave the Inbox.
    fn named(&self, key: &str) -> Option<Field> {
        match key {
            "title" => self.title.clone().map(Field::Scalar),
            "summary" => self.summary.clone().map(Field::Scalar),
            "status" => self.status.clone().map(Field::Scalar),
            "category" => self.category.clone().map(Field::Scalar),
            "topic" => self.topic.clone().map(Field::Scalar),
            "parent" => self.parent.clone().map(Field::Scalar),
            "research" if !self.research.is_empty() => Some(Field::List(self.research.clone())),
            "from_project" => self.from_project.clone().map(Field::Scalar),
            "topics" if !self.topics.is_empty() => Some(Field::List(self.topics.clone())),
            "projects" if !self.projects.is_empty() => Some(Field::List(self.projects.clone())),
            "source_url" => self.source_url.clone().map(Field::Scalar),
            "provenance" => self.provenance.clone().map(Field::Scalar),
            "captured" => self.captured.clone().map(Field::Scalar),
            _ => None,
        }
    }
}

/// The header block and the body after it: `---`, lines, `---`. A file that does not open with the
/// marker has no header at all, whatever else it contains.
fn split(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    let start = text.len() - rest.len();
    let mut offset = start;
    for line in rest.split_inclusive('\n') {
        if line
            .strip_suffix('\n')
            .unwrap_or(line)
            .trim_end_matches('\r')
            == "---"
        {
            return Some((&text[start..offset], &text[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// One value, as the subset reads it: a bracketed list, or a bare scalar with its quotes taken off.
fn parse_value(value: &str) -> Field {
    if value.len() >= 2 && value.starts_with('[') && value.ends_with(']') {
        Field::List(
            value[1..value.len() - 1]
                .split(',')
                .map(|item| item.trim().trim_matches(quote).to_string())
                .filter(|item| !item.is_empty())
                .collect(),
        )
    } else {
        Field::Scalar(value.trim_matches(quote).to_string())
    }
}

fn quote(character: char) -> bool {
    character == '"' || character == '\''
}

/// A named key read as text. A list on a key the vault writes as a scalar is joined with commas, which
/// is the one shape the vault itself never produces.
fn as_text(field: Field) -> Option<String> {
    Some(match field {
        Field::Scalar(text) => text,
        Field::List(items) => items.join(", "),
    })
}

/// A key read as a list, accepting a lone scalar as a list of one.
fn as_list(field: Field) -> Vec<String> {
    match field {
        Field::Scalar(text) if text.is_empty() => Vec::new(),
        Field::Scalar(text) => vec![text],
        Field::List(items) => items,
    }
}

fn field_line(key: &str, field: &Field) -> String {
    match field {
        Field::Scalar(text) => format!("{key}: {text}"),
        Field::List(items) => format!("{key}: [{}]", items.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::Frontmatter;

    #[test]
    fn a_hand_written_block_list_is_a_list() {
        let text = "---\ntitle: X\ntopics:\n  - memory\n  - systems\n---\n\nbody\n";
        let (frontmatter, body) = Frontmatter::parse(text);
        assert_eq!(
            frontmatter.topics,
            vec!["memory".to_string(), "systems".to_string()]
        );
        assert_eq!(body.trim(), "body");
        assert!(frontmatter.render().contains("topics: [memory, systems]"));
    }

    #[test]
    fn an_empty_key_is_empty_not_absent() {
        // The schema distinguishes a missing summary from one written as empty, because a note without
        // one is refused by the curator and a note with an empty one is refused for a different reason.
        let (frontmatter, _) = Frontmatter::parse("---\nsummary:\n---\n");
        assert_eq!(frontmatter.summary.as_deref(), Some(""));
    }
}
