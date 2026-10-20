//! Where a test writes.
//!
//! Test scratch used to live at `<manifest>/../target/scratch`, which is inside the source tree and on
//! whatever filesystem the checkout is on. On this machine that is the root filesystem, with a few
//! gigabytes free, while the build directory is on a much larger disk; a suite that fills the root
//! filesystem fails the next build rather than the test that wrote too much.
//!
//! So a test writes under [`CARGO_TARGET_DIR`] when cargo names one, and under the machine's temp
//! directory otherwise. Never under `<manifest>/../target`: that path is the one the build uses for its
//! own artifacts, and a test that invents a directory there is a test that can fill the wrong disk.
//!
//! Each test binary gets one root named after its pid, and a root whose process is gone is pruned the
//! next time a test binary starts. Within a process, [`unique`] hands out a fresh directory per call.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// The directory test scratch lives under.
pub fn root() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let base = base();
        let _ = std::fs::create_dir_all(&base);
        prune(&base);
        let root = base.join(format!("process-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a scratch root for this test binary");
        root
    })
    .clone()
}

/// Where a fresh directory for one test lives, created and empty.
pub fn unique(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = root().join(format!("{name}-{}", NEXT.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("a scratch directory");
    path
}

fn base() -> PathBuf {
    if let Ok(dir) = std::env::var("CARGO_TARGET_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir).join("scratch");
    }
    let temp = std::env::temp_dir().join("hades-scratch");
    // The live install's own tree is refused by Hades' boundary, and a test that points its state there
    // would be asking the guard to make an exception. A temp directory inside it is therefore not
    // usable; Hades' own home is.
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        let refused = [
            home.join(".hermes"),
            home.join("Documents").join("ObsidianVault"),
        ];
        if refused.iter().any(|guarded| temp.starts_with(guarded)) {
            return home.join(".hades").join("test-scratch");
        }
    }
    temp
}

/// Remove roots whose process is gone. A crashed test binary leaves its directory behind otherwise, and
/// a scratch directory that only ever grows is the same problem one release later.
fn prune(base: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(pid) = name.strip_prefix("process-") else {
            continue;
        };
        if pid == std::process::id().to_string() {
            continue;
        }
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Make a state directory private, and keep an existing one private. One creator, because four modules
/// calling `create_dir_all` with the process umask is four chances for a key to be world-readable.
pub fn state_dir(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::symlink_metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Set a state file to 0600. Called after the file exists, so it covers a file an earlier version made.
pub fn private_file(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::symlink_metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The S0 claim: no test writes under `<manifest>/../target`. That directory belongs to the build,
    /// and on a machine whose root filesystem is nearly full it is the wrong disk to fill.
    #[test]
    fn scratch_is_never_under_the_source_trees_target_directory() {
        let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target");
        let root = root();
        let clean_target = target.canonicalize().unwrap_or(target.clone());
        let clean_root = root.canonicalize().unwrap_or(root.clone());
        assert!(
            !clean_root.starts_with(&clean_target),
            "test scratch {} is under the build's target {}",
            clean_root.display(),
            clean_target.display()
        );
        // A directory left by an older checkout is not evidence about this one. What matters is that a
        // scratch directory made now lands outside the source tree and adds nothing under `target`.
        let leftover: Vec<std::ffi::OsString> = std::fs::read_dir(target.join("scratch"))
            .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
            .unwrap_or_default();
        let probe = unique("source-tree-probe");
        let afterwards: Vec<std::ffi::OsString> = std::fs::read_dir(target.join("scratch"))
            .map(|entries| entries.flatten().map(|entry| entry.file_name()).collect())
            .unwrap_or_default();
        assert_eq!(
            leftover, afterwards,
            "a test wrote under the source tree's target"
        );
        assert!(!clean_root.starts_with(&clean_target));
        let _ = std::fs::remove_dir_all(&probe);
        let first = unique("probe");
        let second = unique("probe");
        assert_ne!(first, second, "two tests never share a directory");
        assert!(first.is_dir() && second.is_dir());
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    #[test]
    fn the_root_is_the_build_directory_when_cargo_names_one() {
        // The gate sets CARGO_TARGET_DIR, and then the scratch root is inside it. A plain `cargo test`
        // does not set it, and then the temp directory (or, when the temp directory is inside Hades'
        // own refused tree, a directory under the Hades home) is the answer. Every one of those is
        // outside the source tree, which is the fact that matters.
        if let Ok(dir) = std::env::var("CARGO_TARGET_DIR")
            && !dir.trim().is_empty()
        {
            assert!(
                root().starts_with(PathBuf::from(dir).join("scratch")),
                "{} is not under the build directory",
                root().display()
            );
            return;
        }
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let tree = manifest.join("..");
        assert!(
            !root().starts_with(tree.canonicalize().unwrap_or_else(|_| tree.to_path_buf())),
            "{} is inside the source tree",
            root().display()
        );
    }
}
