//! The modes the record sets itself, at creation.
//!
//! `create_dir_all` and `fs::write` take their mode from the process umask, which on a desktop is `022` or
//! `002`: the six-area tree, the charter and the index would be group- or world-readable, and the 0700
//! `~/.hades` above them would be the only thing keeping the record private. A store pointed somewhere
//! that is not Hades' home (`HADES_HOME=/tmp/x`, a library user calling [`crate::SqliteStore::open`]) has
//! no such parent, so the modes are set here instead of assumed.
//!
//! `harness::scratch` does the same job for the harness' own state; the crates below the harness cannot
//! depend on it, so the two small functions live here. The rule they carry is the same one: a directory
//! this store owns is 0700 and a file it owns is 0600, whether it created it now or found it at a looser
//! mode an earlier version left.

use std::fs;
use std::io;
use std::path::Path;

/// Create `path` if it is absent, and make sure a directory this store owns is 0700.
pub(crate) fn state_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::symlink_metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Make a file this store owns 0600. Called after it exists, so it also covers a file an earlier version
/// wrote at the umask.
pub(crate) fn private_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::symlink_metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

/// Create a file only when it is absent, at 0600 from the first instant.
///
/// The database is opened by SQLite, which creates it at the umask; creating the empty file here first
/// means the record is never readable by the group for even the moment between creation and a `chmod`.
/// An existing file is left to [`private_file`].
pub(crate) fn create_private(path: &Path) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}
