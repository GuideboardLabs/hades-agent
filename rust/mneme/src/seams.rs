//! The two seams. A capability with one implementation does not earn a trait, so there are two.

use std::path::{Path, PathBuf};

use crate::date::Date;
use crate::fault::Fault;

/// The day is injected, never read from the ambient world, so the suite runs offline and the same
/// notes are written on every machine.
pub trait Clock {
    fn today(&self) -> Date;
}

/// The one seam that touches the disk. Paths come in as the vault built them, and the store never
/// invents one: it does what the vault asks and reports what the disk said.
pub trait NoteStore {
    /// Whether anything is at this path. Picking a free name is built on this.
    fn exists(&self, path: &Path) -> bool;
    /// The whole text of a note.
    fn read(&self, path: &Path) -> Result<String, Fault>;
    /// Write a note, replacing whatever was there.
    fn write(&self, path: &Path, text: &str) -> Result<(), Fault>;
    /// Create a folder and its parents.
    fn make_dirs(&self, path: &Path) -> Result<(), Fault>;
    /// Remove a note. The only destructive call, and the vault makes it once, after the copy that
    /// replaces it is already on disk.
    fn remove(&self, path: &Path) -> Result<(), Fault>;
    /// The `*.md` files directly inside a folder, by name. A folder that is not there lists as empty,
    /// because the vault asks for folders that do not exist yet.
    fn list(&self, folder: &Path) -> Result<Vec<PathBuf>, Fault>;
    /// Every `*.md` at any depth under a folder, by name, subfolders included and symlinked folders not
    /// followed. The audit needs this and `list` cannot do it: a note that should not be there is
    /// exactly a note several folders down.
    fn walk(&self, folder: &Path) -> Result<Vec<PathBuf>, Fault>;
    /// Write the note at this path out in the form a person reads. A store that is the disk has already
    /// done it and does nothing; a store that is a database writes the one file, so a path the curator
    /// hands back is a file and not a promise that `export` will make one later.
    fn export(&self, _path: &Path) -> Result<(), Fault> {
        Ok(())
    }
    /// Remove the file `export` wrote for a note. `remove` is the record's side; this is the tree's,
    /// and a store that is the disk has already done it there, so the default is nothing. A record
    /// store that skips this leaves the note's old markdown standing after a move, and the Inbox a
    /// person reads would hold a copy the record no longer has.
    fn remove_export(&self, _path: &Path) -> Result<(), Fault> {
        Ok(())
    }
}
