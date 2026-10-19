//! The five areas. Literal and lowercase on disk; the door names in the product are flavoured, and
//! that is the whole point of keeping the keys plain.

use std::fmt;

/// One of the five areas, in the order the vault walks them: the Inbox first, the spine last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Area {
    Inbox,
    Outbox,
    Research,
    Projects,
    Topics,
}

impl Area {
    pub const ALL: [Area; 5] = [
        Area::Inbox,
        Area::Outbox,
        Area::Research,
        Area::Projects,
        Area::Topics,
    ];

    /// The folder on disk.
    pub fn folder(self) -> &'static str {
        match self {
            Area::Inbox => "00 Inbox",
            Area::Outbox => "10 Outbox",
            Area::Research => "20 Research",
            Area::Projects => "30 Projects",
            Area::Topics => "40 Topics",
        }
    }

    /// The key a caller names it by.
    pub fn key(self) -> &'static str {
        match self {
            Area::Inbox => "inbox",
            Area::Outbox => "outbox",
            Area::Research => "research",
            Area::Projects => "projects",
            Area::Topics => "topics",
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
