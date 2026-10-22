//! The six areas. Literal and lowercase on disk; the door names in the product are flavoured, and
//! that is the whole point of keeping the keys plain.

use std::fmt;

/// One of the six areas, in the order the vault walks them: the Inbox first, the spine last.
///
/// `Products` sits between Projects and Topics because a product is what a project becomes when it
/// stops moving: it keeps its research, it keeps its topic, and it is still a first-class thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Area {
    Inbox,
    Outbox,
    Research,
    Projects,
    Products,
    Topics,
}

impl Area {
    pub const ALL: [Area; 6] = [
        Area::Inbox,
        Area::Outbox,
        Area::Research,
        Area::Projects,
        Area::Products,
        Area::Topics,
    ];

    /// The folder on disk. The numbers leave room for a new door without renumbering an installed
    /// vault, which is the one thing a first-release vault layout must get right.
    pub fn folder(self) -> &'static str {
        match self {
            Area::Inbox => "00 Inbox",
            Area::Outbox => "10 Outbox",
            Area::Research => "20 Research",
            Area::Projects => "30 Projects",
            Area::Products => "40 Products",
            Area::Topics => "50 Topics",
        }
    }

    /// The key a caller names it by.
    pub fn key(self) -> &'static str {
        match self {
            Area::Inbox => "inbox",
            Area::Outbox => "outbox",
            Area::Research => "research",
            Area::Projects => "projects",
            Area::Products => "products",
            Area::Topics => "topics",
        }
    }

    /// Whether a note in this area has to name a topic. Topics themselves are the spine, so the
    /// requirement starts with the work that hangs off it.
    pub fn needs_topic(self) -> bool {
        matches!(self, Area::Research | Area::Projects | Area::Products)
    }

    /// A one-line description, seeded as the marker in a fresh vault.
    pub fn purpose(self) -> &'static str {
        match self {
            Area::Inbox => {
                "Captures land here and only here. Nothing is knowledge until a person files it."
            }
            Area::Outbox => {
                "Work that leaves the machine, and messages for other agents. Nothing reads it yet."
            }
            Area::Research => {
                "Findings, one note per question, each with its topic and its provenance."
            }
            Area::Projects => "Hubs. Every project carries a topic and at least one research note.",
            Area::Products => {
                "A project that stopped moving: closed, inactive, or frozen as its own artifact."
            }
            Area::Topics => {
                "The spine. One category per static type, and a tree of general topics under it."
            }
        }
    }

    /// Exact and lowercase: an area is a literal, so `Research` is not one. This is the boundary
    /// where a string stops being a string, which is why nothing downstream has to check.
    pub fn parse(key: &str) -> Option<Area> {
        Area::ALL.into_iter().find(|area| area.key() == key)
    }
}

impl fmt::Display for Area {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

#[cfg(test)]
mod tests {
    use super::Area;

    #[test]
    fn every_area_has_a_distinct_folder_and_key() {
        let mut folders: Vec<&str> = Area::ALL.iter().map(|area| area.folder()).collect();
        folders.sort();
        folders.dedup();
        assert_eq!(folders.len(), Area::ALL.len());
        assert_eq!(Area::parse("products"), Some(Area::Products));
        assert_eq!(Area::parse("Products"), None);
    }
}
