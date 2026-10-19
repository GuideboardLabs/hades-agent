//! The fence: a safety predicate, and the only copy of it. Two copies of a safety rule drift apart
//! and the drift is silent.
//!
//! Fenced material is never read, never indexed and never written, so the check is on the **path**
//! and not on the contents. A rule that reads the contents has already read them.

use std::collections::HashSet;
use std::path::{Component, Path};

/// True when every part of a fence is a part of the path.
///
/// Matching is by path part, never by substring, so a fence of `2-Areas/Family` does not fence
/// `2-Areas/Family-history`, and one part is enough to fence a whole subtree.
pub fn fenced(path: &Path, fences: &[String]) -> bool {
    let parts = path_parts(path);
    fences.iter().any(|fence| {
        let tokens = path_parts(Path::new(fence));
        !tokens.is_empty() && tokens.is_subset(&parts)
    })
}

/// The parts of a path, lowercased, the way `Path.parts` reads them.
fn path_parts(path: &Path) -> HashSet<String> {
    path.components().map(named_part).collect()
}

fn named_part(component: Component<'_>) -> String {
    match component {
        Component::RootDir => "/".to_string(),
        Component::CurDir => ".".to_string(),
        Component::ParentDir => "..".to_string(),
        Component::Prefix(prefix) => prefix.as_os_str().to_string_lossy().to_lowercase(),
        Component::Normal(name) => name.to_string_lossy().to_lowercase(),
    }
}
