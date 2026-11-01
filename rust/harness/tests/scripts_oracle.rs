//! The scripts a person is told to run, and whether the tree lets them run it.
//!
//! A shell script committed `0644` works under `bash scripts/x.sh`, which is how CI calls it, and fails with
//! `Permission denied` for the person the docs tell to type `./scripts/x.sh`. The suite's own deadline was
//! committed that way: the bound existed, and the command a person was given could not reach it.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[test]
fn every_shell_script_in_the_tree_is_executable() {
    // The workspace root, resolved from this crate so the test does not care where cargo was started.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the workspace root above the harness crate")
        .to_path_buf();
    let mut offenders: Vec<PathBuf> = Vec::new();
    walk(&root, &root, &mut offenders);
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "these scripts are not executable, so `./{}` fails with Permission denied: {offenders:?}. \
         Fix with `chmod +x <file>` and `git update-index --chmod=+x <file>`.",
        offenders
            .first()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    );
}

/// Every `.sh` under the workspace, skipping what a build wrote there: an artifact's modes are the packer's
/// business and are checked where the artifact is made.
fn walk(root: &Path, directory: &Path, offenders: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(name.as_ref(), "target" | "dist" | ".git" | "node_modules") {
                continue;
            }
            walk(root, &path, offenders);
            continue;
        }
        if !name.ends_with(".sh") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.permissions().mode() & 0o111 == 0 {
            offenders.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
}
