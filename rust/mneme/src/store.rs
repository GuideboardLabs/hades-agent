//! One folder tree of markdown files, and nothing else.

use std::fs;
use std::path::{Path, PathBuf};

use crate::fault::Fault;
use crate::seams::NoteStore;

/// The note store over the real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileVault;

impl FileVault {
    pub fn new() -> Self {
        FileVault
    }

    fn io(path: &Path, detail: impl std::fmt::Display) -> Fault {
        Fault::Io {
            path: path.display().to_string(),
            detail: detail.to_string(),
        }
    }
}

impl NoteStore for FileVault {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn read(&self, path: &Path) -> Result<String, Fault> {
        fs::read_to_string(path).map_err(|error| FileVault::io(path, error))
    }

    fn write(&self, path: &Path, text: &str) -> Result<(), Fault> {
        fs::write(path, text).map_err(|error| FileVault::io(path, error))
    }

    fn make_dirs(&self, path: &Path) -> Result<(), Fault> {
        fs::create_dir_all(path).map_err(|error| FileVault::io(path, error))
    }

    fn remove(&self, path: &Path) -> Result<(), Fault> {
        fs::remove_file(path).map_err(|error| FileVault::io(path, error))
    }

    fn list(&self, folder: &Path) -> Result<Vec<PathBuf>, Fault> {
        let entries = match fs::read_dir(folder) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(FileVault::io(folder, error)),
        };
        let mut found: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let path = entry.map_err(|error| FileVault::io(folder, error))?.path();
            // Exactly `*.md`, the way a shell glob reads it: a directory named `notes.md` is not a note
            // either, and reads as one only when someone opens it.
            if path.extension() == Some(std::ffi::OsStr::new("md")) {
                found.push(path);
            }
        }
        found.sort();
        Ok(found)
    }

    fn walk(&self, folder: &Path) -> Result<Vec<PathBuf>, Fault> {
        let mut found: Vec<PathBuf> = Vec::new();
        let mut pending: Vec<PathBuf> = vec![folder.to_path_buf()];
        while let Some(current) = pending.pop() {
            let entries = match fs::read_dir(&current) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(FileVault::io(&current, error)),
            };
            for entry in entries {
                let entry = entry.map_err(|error| FileVault::io(&current, error))?;
                let kind = entry
                    .file_type()
                    .map_err(|error| FileVault::io(&current, error))?;
                let path = entry.path();
                if kind.is_dir() {
                    pending.push(path);
                } else if path.extension() == Some(std::ffi::OsStr::new("md")) {
                    found.push(path);
                }
            }
        }
        found.sort();
        Ok(found)
    }
}
