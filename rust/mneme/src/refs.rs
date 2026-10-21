//! Resolving one note's reference to another.
//!
//! A reference is written by a person, so it is allowed to be the full record path, the slug below its
//! area, or the bare name at the end. The canonical form is the slug below the area without `.md`, and
//! that is what a creator stores. A bare name is accepted only when exactly one note has it: two notes
//! with the same file name are a question the resolver refuses to answer by guessing.

use std::collections::BTreeSet;

/// What one reference resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResearchRef {
    /// The canonical slug below the area, without `.md`.
    Found(String),
    Missing,
    /// Every candidate, sorted, so the refusal can name the choice the writer has to make.
    Ambiguous(Vec<String>),
}

impl ResearchRef {
    pub fn found(&self) -> Option<&str> {
        match self {
            ResearchRef::Found(slug) => Some(slug),
            _ => None,
        }
    }

    /// The sentence a refusal or an audit line uses.
    pub fn reason(&self, reference: &str) -> Option<String> {
        match self {
            ResearchRef::Found(_) => None,
            ResearchRef::Missing => Some(format!("`{reference}` does not name an existing note")),
            ResearchRef::Ambiguous(candidates) => Some(format!(
                "`{reference}` is ambiguous: it could be {}",
                candidates
                    .iter()
                    .map(|candidate| format!("`{candidate}`"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            )),
        }
    }
}

/// Normalise the forms a person writes. `folder` is the area folder, e.g. `20 Research`.
pub fn clean(reference: &str, folder: &str) -> String {
    let mut text = reference.trim();
    if let Some(rest) = text.strip_prefix(folder) {
        text = rest.trim_start_matches('/');
    }
    if let Some(rest) = text.strip_prefix("./") {
        text = rest;
    }
    text = text.strip_suffix(".md").unwrap_or(text);
    text = text.strip_suffix("/index").unwrap_or(text);
    text.trim_matches('/').to_string()
}

/// Resolve a reference against the canonical slugs below one area.
///
/// Exact match wins. Otherwise a reference matches a slug that ends with `/<reference>`, and only a
/// single such slug is accepted.
pub fn resolve(slugs: &BTreeSet<String>, reference: &str, folder: &str) -> ResearchRef {
    let wanted = clean(reference, folder);
    if wanted.is_empty() {
        return ResearchRef::Missing;
    }
    if slugs.contains(&wanted) {
        return ResearchRef::Found(wanted);
    }
    let suffix = format!("/{wanted}");
    let candidates: Vec<String> = slugs
        .iter()
        .filter(|slug| slug.ends_with(&suffix))
        .cloned()
        .collect();
    match candidates.len() {
        0 => ResearchRef::Missing,
        1 => ResearchRef::Found(candidates.into_iter().next().unwrap_or_default()),
        _ => ResearchRef::Ambiguous(candidates),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slugs(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn a_full_path_and_a_bare_name_find_the_same_note() {
        let set = slugs(&["technical/ai-inference/kv-cache"]);
        for reference in [
            "technical/ai-inference/kv-cache",
            "20 Research/technical/ai-inference/kv-cache.md",
            "kv-cache",
        ] {
            assert_eq!(
                resolve(&set, reference, "20 Research"),
                ResearchRef::Found("technical/ai-inference/kv-cache".to_string()),
                "{reference}"
            );
        }
    }

    #[test]
    fn two_notes_with_the_same_name_are_ambiguous_not_guessed() {
        let set = slugs(&["technical/a/note", "technical/b/note"]);
        assert_eq!(
            resolve(&set, "note", "20 Research"),
            ResearchRef::Ambiguous(vec![
                "technical/a/note".to_string(),
                "technical/b/note".to_string()
            ])
        );
    }

    #[test]
    fn a_project_index_and_its_name_resolve_to_the_same_project() {
        let set = slugs(&["loom"]);
        for reference in ["loom", "loom/index", "30 Projects/loom/index.md"] {
            assert_eq!(
                resolve(&set, reference, "30 Projects"),
                ResearchRef::Found("loom".to_string()),
                "{reference}"
            );
        }
    }
}
