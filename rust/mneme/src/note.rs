//! A note on disk, and the two readings the vault makes of it: what it is called, and whether it is
//! durable.

use std::path::{Path, PathBuf};

use crate::frontmatter::Frontmatter;

/// A note, with its metadata parsed out and its body kept whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub path: PathBuf,
    pub frontmatter: Frontmatter,
    pub body: String,
}

impl Note {
    pub fn new(
        path: impl Into<PathBuf>,
        frontmatter: Frontmatter,
        body: impl Into<String>,
    ) -> Self {
        Note {
            path: path.into(),
            frontmatter,
            body: body.into(),
        }
    }

    /// The title, falling back to the file name. A note is never nameless.
    pub fn title(&self) -> String {
        match self.frontmatter.title.as_deref() {
            Some(title) if !title.is_empty() => title.to_string(),
            _ => stem(&self.path),
        }
    }

    pub fn summary(&self) -> &str {
        self.frontmatter.summary.as_deref().unwrap_or("")
    }

    pub fn topics(&self) -> &[String] {
        &self.frontmatter.topics
    }

    pub fn projects(&self) -> &[String] {
        &self.frontmatter.projects
    }

    /// Summary, at least one topic, and provenance. All three, always. Without them a note cannot
    /// leave the Inbox, which is what keeps the graph free of orphans.
    pub fn is_durable(&self) -> bool {
        let source = self.frontmatter.source_url.as_deref().unwrap_or("");
        let provenance = self.frontmatter.provenance.as_deref().unwrap_or("");
        !self.summary().is_empty()
            && !self.topics().is_empty()
            && !(source.is_empty() && provenance.is_empty())
    }
}

/// The file name without its extension, which is what a note is called when it says nothing else.
pub fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A path as anything user-facing shows it: relative to the vault root, and always with forward
/// slashes. One implementation, because a path rendered two ways is a path that disagrees with itself.
pub fn relative_posix(path: &Path, root: &Path) -> String {
    let relative: PathBuf = path
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| PathBuf::from(path.file_name().unwrap_or_default()));
    relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// A file name from a heading: runs of lowercase letters and digits, at most `max_words` of them.
pub fn slugify(text: &str, max_words: usize) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    for character in text.to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            current.push(character);
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    let slug = words
        .iter()
        .take(max_words)
        .cloned()
        .collect::<Vec<String>>()
        .join("-");
    if slug.is_empty() {
        "untitled".to_string()
    } else {
        slug
    }
}
