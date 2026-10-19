//! The capability to write memory.
//!
//! One rule, from our own governance work: exactly one component writes durable memory. In Python that
//! is a promise backed by a test that scans the package for the write calls. Here it is a type. Moving a
//! note out of the Inbox takes a [`WritePermit`]; the type's field is private, so the only place one can
//! be built is [`crate::Vault::permit`]; and the holder it names is written into the note, so the audit
//! reads back who did it rather than being told.
//!
//! The boundary, stated plainly, because a guarantee is only worth what it excludes: Rust cannot stop a
//! caller from *asking* for a permit. What it stops is a move with no permit at all, and it makes every
//! asker name itself in one function, so the rule is a single place to read instead of a convention
//! spread across a package.

/// Who is asking to write memory.
///
/// One variant, because one component writes. As the engine grows it is a **seat** that holds a permit,
/// and the policy belongs with the seat: this type says who, and nothing here decides whether they may.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Holder {
    Curator,
}

impl Holder {
    pub fn name(self) -> &'static str {
        match self {
            Holder::Curator => "curator",
        }
    }
}

/// The permit itself: a value you can only hold because someone handed it to you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritePermit {
    holder: Holder,
}

impl WritePermit {
    /// Crate-private on purpose: this is the only constructor, so a permit cannot be forged outside the
    /// vault, and every one that exists came from [`crate::Vault::permit`].
    pub(crate) fn new(holder: Holder) -> Self {
        WritePermit { holder }
    }

    pub fn holder(&self) -> Holder {
        self.holder
    }
}
