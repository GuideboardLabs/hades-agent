//! The path policy: where the file tools may point, decided on the real filesystem.
//!
//! A path check made on the path's text is a check against a story, not against the machine. The text
//! says `sub/../note.txt`, the filesystem says a symlink to `/etc`, and the two agree only until
//! something finds the places where they differ. So this gate answers a different question, and only
//! this one: what would the kernel actually open, and is that under a root this operation may use.
//!
//! The check, in the order the steps earned their place:
//!
//! 1. a relative path is resolved against the workspace root, which is the first root of the
//!    operation's list,
//! 2. `..` is folded lexically, so an escape is judged after resolution rather than read off the text,
//! 3. the deepest ancestor that exists is canonicalized, which answers for every symlink under it in
//!    one call, and the names after it, none of which exist as themselves, are walked one at a time,
//!    following a symlink exactly where the kernel would follow it,
//! 4. the result is accepted only if it is under a permitted root, compared by components, which is
//!    what makes `/work2` not inside `/work`.
//!
//! A refusal names the boundary that refused, because a ledger that sees only "refused" cannot later
//! tell an outside path from an escape: outside a permitted root, a symlink escape, a `..` escape, a
//! prefix collision.
//!
//! What this is not, said at the same length. It is a gate before the open, not the open: a symlink
//! swapped in between the check and the write is the runtime cage's business (`crate::cage`), not this
//! one's. The caller opens the path this returns, never the path it asked about, because the returned
//! path is the one that was checked, and a `..` met after a symlink is folded against the resolved
//! path, not against the story. And `floor::protected` stays a second gate on top: the policy knows
//! where a write may land, the floor knows which writes would rewrite the thing doing the checking,
//! and a write has to pass both.
//!
//! The lexical fold is the chosen behaviour, and it is a ruling, not an oversight. The fold happens
//! before the filesystem is asked anything, so `door/../secret.txt` names `<root>/secret.txt` here
//! even when `door` is a symlink, which is not the kernel's resolution of the string: the kernel walks
//! `door`, follows it, and looks for `secret.txt` behind it. That difference is closed on the caller's
//! side of the gate, in one direction only: every caller MUST open the path the policy returns, not
//! the path it was asked about, because the returned path is the one whose resolution was actually
//! walked and whose containment was actually decided; to open the requested path instead is to step
//! around the gate after it has spoken.

use std::path::{Component, Path, PathBuf};

/// How many symlink hops one resolution may take. The kernel ends a chain of more than 40 with `ELOOP`,
/// so a bound above that is never what fires first; its job is to make the walk honest about work it
/// cannot finish, with an error that names the path instead of a number to look up.
const MAX_SYMLINK_HOPS: usize = 64;

/// Which files the tools may point at, checked the way the kernel will interpret them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPolicy {
    /// What a read may point at. The first root is the workspace root: relative reads resolve against it.
    pub read_roots: Vec<PathBuf>,
    /// What a write or a create may point at. The first root is the workspace root for those.
    pub write_roots: Vec<PathBuf>,
}

impl PathPolicy {
    /// Check a path a read is about to take, and return the path to open.
    pub fn read(&self, path: &Path) -> Result<PathBuf, String> {
        self.checked(path, &self.read_roots, "read")
    }

    /// Check a path a write is about to take, and return the path to open.
    ///
    /// The target may not exist yet, which is the ordinary case: the check resolves what does exist
    /// around it (its parent, or the deepest ancestor that does) instead of refusing to answer.
    pub fn write(&self, path: &Path) -> Result<PathBuf, String> {
        self.checked(path, &self.write_roots, "write")
    }

    /// Check a path a create is about to take. A create is a write to the policy, same roots and same
    /// gate; the separate name is so the activity ledger can tell a new file from an overwrite.
    pub fn create(&self, path: &Path) -> Result<PathBuf, String> {
        self.checked(path, &self.write_roots, "write")
    }

    /// The one-root policy every caller in this crate wants: `root` is the only read root and the only
    /// write root, whether the caller is the chat loop or a governed turn.
    ///
    /// Nothing here is resolved yet: [`PathPolicy::checked`] canonicalizes the roots per check, on
    /// purpose, so a root that exists at the check rather than at construction is what answers.
    pub fn for_root(root: &Path) -> PathPolicy {
        PathPolicy {
            read_roots: vec![root.to_path_buf()],
            write_roots: vec![root.to_path_buf()],
        }
    }

    /// The whole check, once, for one operation and the roots it is permitted by.
    ///
    /// The roots are canonicalized here rather than once at construction. The fields are public, so the
    /// policy cannot vouch for what they hold: a root deleted after the policy was built would
    /// otherwise stay a permit to a directory that is no longer there. The cost is a stat per root per
    /// call, which is nothing next to the write it decides about.
    fn checked(&self, path: &Path, roots: &[PathBuf], kind: &str) -> Result<PathBuf, String> {
        let workspace = roots.first().ok_or_else(|| {
            format!(
                "refused by the path policy: no {kind} root is configured, so there is nothing to \
                 resolve {} against",
                path.display()
            )
        })?;
        let requested = if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.join(path)
        };
        let folded = fold(&requested);
        let mut hops = 0;
        let resolved = resolve_through(&folded, None, &mut hops)?;
        let permitted = canonical_roots(roots)?;
        if permitted.iter().any(|root| resolved.starts_with(root)) {
            return Ok(resolved);
        }
        Err(refusal(&requested, &folded, &resolved, &permitted, kind))
    }
}

/// Why the resolved path was refused, named so the activity ledger can tell the four apart later.
///
/// The order is part of the meaning. A symlink escape is asked first, because a `..` behind a link is
/// both and the link is the part that lied. A `..` escape is asked next, by comparing the resolved
/// path with the same path without its `..` components: when that pretense is inside a root and the
/// resolution is not, the dots are what carried the request out. A prefix collision is asked before
/// the plain refusal, because a path that touches a root only by name, `/work2` beside `/work`,
/// deserves the error that says exactly what happened.
fn refusal(
    requested: &Path,
    folded: &Path,
    resolved: &Path,
    permitted: &[PathBuf],
    kind: &str,
) -> String {
    if permitted.iter().any(|root| folded.starts_with(root)) {
        return format!(
            "refused by the path policy: {} resolves through a symlink to {}, outside the permitted \
             {kind} roots",
            requested.display(),
            resolved.display()
        );
    }
    if requested
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        let plain = without_parent_dirs(requested);
        if permitted.iter().any(|root| plain.starts_with(root)) {
            return format!(
                "refused by the path policy: {} leaves the permitted {kind} roots through `..`, \
                 landing at {}",
                requested.display(),
                resolved.display()
            );
        }
    }
    for root in permitted {
        let root_text = root.to_string_lossy();
        if resolved.to_string_lossy().starts_with(&*root_text) {
            return format!(
                "refused by the path policy: {} is not under {} by path components, only by name: a \
                 prefix collision",
                resolved.display(),
                root.display()
            );
        }
    }
    format!(
        "refused by the path policy: {} is outside the permitted {kind} roots",
        resolved.display()
    )
}

/// The configured roots, verified to exist and resolved to their real spelling.
///
/// A root that cannot be resolved is a broken configuration, and a gate that shrugs at one has already
/// answered a question it could not see. So this fails closed, and the refusal says which root.
fn canonical_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    roots
        .iter()
        .map(|root| {
            std::fs::canonicalize(root).map_err(|error| {
                format!(
                    "refused by the path policy: the permitted root {} could not be resolved for a \
                     containment check: {error}",
                    root.display()
                )
            })
        })
        .collect()
}

/// What the filesystem would be asked to touch, with every symlink followed.
///
/// The first step is the only cheap one: the deepest ancestor that exists is resolved wholesale, one
/// `canonicalize` call that answers for every symlink under it at once. For a target that does not
/// exist, a write's usual case, that ancestor is the target's parent, so this is the old rule in its
/// general form: canonicalize the parent, join what came after it.
///
/// The names behind that ancestor cannot be answered by a single call, because none of them exist as
/// themselves. They are walked one at a time, and a symlink at any of them is followed to a bound.
/// Every path this returns is fully resolved, so the containment decision is about a path the kernel
/// could actually resolve the same way.
fn resolve_through(
    path: &Path,
    requested: Option<&Path>,
    hops: &mut usize,
) -> Result<PathBuf, String> {
    // The input is folded, so the components here are the filesystem root and plain names only.
    let components: Vec<Component> = path.components().collect();
    let mut prefix_len = components.len();
    while prefix_len > 1
        && !components[..prefix_len]
            .iter()
            .collect::<PathBuf>()
            .exists()
    {
        prefix_len -= 1;
    }
    let prefix: PathBuf = components[..prefix_len].iter().collect();
    let origin = requested.unwrap_or(path);
    let mut current = match std::fs::canonicalize(&prefix) {
        Ok(base) => base,
        Err(error)
            if std::fs::symlink_metadata(&prefix)
                .map(|meta| meta.is_symlink())
                .unwrap_or(false) =>
        {
            // Fail closed: a path whose existing part cannot be resolved cannot be proven contained.
            // A prefix that exists as a symlink but reads through to nothing is the likeliest face of
            // that: it hangs, and a hang at the head of the walk is a broken chain, not a plain miss,
            // so the ledger names it what it is.
            return Err(format!(
                "refused by the path policy: the symlink {} could not be resolved for a containment \
                 check (symlink escape): {error}",
                prefix.display()
            ));
        }
        Err(error) => {
            // Fail closed: a path whose existing part cannot be resolved cannot be proven contained.
            return Err(format!(
                "refused by the path policy: the status of {} could not be resolved for a containment \
                 check: {error}",
                prefix.display()
            ));
        }
    };
    for component in &components[prefix_len..] {
        current = follow_leaf(&current.join(component.as_os_str()), origin, hops)?;
    }
    Ok(current)
}

/// Follow the last name of a path until it is no longer a symlink, and return where it lands.
///
/// The name may not exist, which is the ordinary case for a file about to be created. When a link is
/// followed and its land exists whole, `canonicalize` answers for every link in it at once, which is
/// what keeps an absolute target from being read back with its `..` still in the text. When the land
/// hangs, its text is not a resolution, so the walk continues on it the way it continued through the
/// original name: the same hop budget and the same refused-for, so the whole chain, however deeply it
/// is chased, can be ended by one bound.
fn follow_leaf(start: &Path, requested: &Path, hops: &mut usize) -> Result<PathBuf, String> {
    let current = start.to_path_buf();
    let metadata = match std::fs::symlink_metadata(&current) {
        // A name that is simply not there is not an error for a gate to raise: the caller's open
        // will raise a better one.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(current),
        Err(error) => {
            return Err(format!(
                "refused by the path policy: the status of {} could not be read for a containment \
                 check: {error}",
                current.display()
            ));
        }
        Ok(metadata) => metadata,
    };
    if !metadata.is_symlink() {
        return Ok(current);
    }
    *hops += 1;
    if *hops > MAX_SYMLINK_HOPS {
        // The bound stands in for the kernel's ELOOP here, and it is named on the path the caller
        // asked about, not the one the chase happens to be standing on.
        return Err(format!(
            "refused by the path policy: more than {MAX_SYMLINK_HOPS} symlinks were followed from \
             {}, which no open would survive either",
            requested.display()
        ));
    }
    let target = std::fs::read_link(&current).map_err(|error| {
        format!(
            "refused by the path policy: the symlink {} could not be read for a containment check: \
             {error}",
            current.display()
        )
    })?;
    let joined = if target.is_absolute() {
        target
    } else {
        current
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .join(target)
    };
    // A relative target is resolved against the link's own directory, and an absolute one against
    // itself, but either may textually carry `..`, and `read_link` returns it as text. Folding here
    // is what the kernel does when it walks the link, and it keeps a `..` out of the path that the
    // containment check is about to compare.
    let joined = fold(&joined);
    match std::fs::canonicalize(&joined) {
        // The land exists whole: `canonicalize` answers for every link in it at once, which is what
        // keeps an absolute target from being read back with its `..` still in the text.
        Ok(landed) => Ok(landed),
        // The land hangs: its text names something that does not exist, so `canonicalize` cannot
        // answer for it, and its text is not a resolution either. A name inside the text may sit
        // behind a link of its own, so pasting the text into the answer would hand the containment
        // check a path the kernel would never arrive at. The land is resolved the way every other
        // path is, through the same walk; `hops` is carried through so the budget stays one budget
        // for the whole chain, and the bound, however deep the chase, ends it.
        Err(_) => resolve_through(&joined, Some(requested), hops),
    }
}

/// An absolute path with `.` and `..` folded away, without touching the filesystem.
///
/// `canonicalize` cannot answer for a file that does not exist yet, and the containment question has
/// to be asked about a target the caller is still going to create. Popping past the filesystem root
/// ends the walk with the remainder under `/`, which is where the kernel would have put it too, and
/// where the containment check will send it back from.
fn fold(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if !out.is_absolute() {
        out = PathBuf::from("/").join(out);
    }
    out
}

/// The same path with every `..` and `.` component dropped rather than folded.
///
/// This is only ever asked by the [`refusal`] that names a `..` escape: the path as it would read
/// without the dots is compared with the roots, and when that pretend path is inside and the resolved
/// one is not, the dots are the whole difference.
fn without_parent_dirs(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| !matches!(component, Component::ParentDir | Component::CurDir))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A workspace under the system temp directory, named for the test and the process so two tests
    /// running beside each other cannot collide, with both roots pointing at it. The `Drop` is the
    /// cleanup: a test that panics half way must not leave a tree behind either.
    struct TempTree {
        base: PathBuf,
    }

    impl TempTree {
        fn new(test: &str) -> TempTree {
            let base = std::env::temp_dir()
                .join(format!("hades-path-policy-{test}-{}", std::process::id()));
            // A stale tree from a killed run of the same test in the same process would otherwise
            // carry symlinks into a fresh test.
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(base.join("work")).expect("the temp workspace could be made");
            TempTree { base }
        }

        fn workspace(&self) -> PathBuf {
            self.base.join("work")
        }

        /// The policy under test: the workspace is the one root, for reads and for writes.
        fn policy(&self) -> PathPolicy {
            let canonical = fs::canonicalize(self.workspace()).expect("the workspace exists");
            PathPolicy {
                read_roots: vec![canonical.clone()],
                write_roots: vec![canonical],
            }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_read_outside_the_roots_is_refused() {
        let tree = TempTree::new("read-outside");
        let away = tree.base.join("away");
        fs::create_dir_all(&away).expect("a directory outside the workspace");
        let secret = away.join("secret.txt");
        fs::write(&secret, "no").expect("a file outside the workspace");

        let error = tree
            .policy()
            .read(&secret)
            .expect_err("outside the read roots");
        assert!(error.contains("outside"), "{error}");
    }

    #[test]
    fn a_write_outside_the_roots_is_refused() {
        let tree = TempTree::new("write-outside");
        let away = tree.base.join("away");
        fs::create_dir_all(&away).expect("a directory outside the workspace");

        let error = tree
            .policy()
            .write(&away.join("made.txt"))
            .expect_err("outside the write roots");
        assert!(error.contains("outside"), "{error}");
    }

    #[test]
    fn a_patch_outside_the_roots_is_refused() {
        let tree = TempTree::new("patch-outside");
        let away = tree.base.join("away");
        fs::create_dir_all(&away).expect("a directory outside the workspace");
        let file = away.join("lib.rs");
        fs::write(&file, "fn main() {}\n").expect("an existing file a patch would edit");

        // A patch is a write to the policy: it changes an existing file, so it answers to the same gate.
        let error = tree
            .policy()
            .write(&file)
            .expect_err("outside the write roots");
        assert!(error.contains("outside"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_system_file_is_refused() {
        use std::os::unix::fs::symlink;

        let tree = TempTree::new("symlink-file");
        symlink("/etc/passwd", tree.workspace().join("secret")).expect("the symlink");

        let error = tree
            .policy()
            .read(&tree.workspace().join("secret"))
            .expect_err("a link out of the root");
        assert!(error.contains("symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_to_tmp_is_refused() {
        use std::os::unix::fs::symlink;

        let tree = TempTree::new("symlink-dir");
        symlink("/tmp", tree.workspace().join("jail")).expect("the symlink");
        let policy = tree.policy();

        let read = policy
            .read(&tree.workspace().join("jail"))
            .expect_err("a link out of the root");
        assert!(read.contains("symlink"), "{read}");
        let write = policy
            .write(&tree.workspace().join("jail").join("made.txt"))
            .expect_err("a write behind the link");
        assert!(write.contains("symlink"), "{write}");
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_under_a_symlinked_parent_is_refused() {
        use std::os::unix::fs::symlink;

        let tree = TempTree::new("symlink-parent");
        let away = tree.base.join("away");
        fs::create_dir_all(&away).expect("a directory outside the workspace");
        symlink(&away, tree.workspace().join("door")).expect("the symlink");

        let error = tree
            .policy()
            .create(&tree.workspace().join("door").join("made.txt"))
            .expect_err("the parent is a link out of the root");
        assert!(error.contains("symlink"), "{error}");
        assert!(
            !away.join("made.txt").exists(),
            "nothing was made behind the door"
        );
    }

    #[test]
    fn a_path_that_leaves_the_roots_through_two_dots_is_refused() {
        let tree = TempTree::new("dotdot-outside");
        fs::create_dir_all(tree.workspace().join("sub")).expect("a subdirectory");

        // As text the path stays beside the workspace; folded, it is one above it, which no root covers.
        let asked = tree.workspace().join("..").join("above.txt");
        let error = tree
            .policy()
            .write(&asked)
            .expect_err("the fold leaves the root");
        assert!(error.contains("`..`"), "{error}");
    }

    #[test]
    fn two_dots_that_stay_inside_the_roots_are_accepted() {
        let tree = TempTree::new("dotdot-inside");
        fs::create_dir_all(tree.workspace().join("sub")).expect("a subdirectory");
        let note = tree.workspace().join("kept.txt");
        fs::write(&note, "stays").expect("a file named through a ..");

        // An escape is judged after resolution, so dots that fold back inside are not a refusal.
        let resolved = tree
            .policy()
            .read(&tree.workspace().join("sub").join("..").join("kept.txt"))
            .expect("the fold stays inside");
        assert_eq!(resolved, fs::canonicalize(&note).expect("the file exists"));
    }

    #[test]
    fn a_name_that_shares_the_root_prefix_is_refused() {
        let tree = TempTree::new("prefix-collision");
        fs::create_dir_all(tree.base.join("work2")).expect("a sibling one character apart");
        let policy = tree.policy();

        let asked = tree.base.join("work2").join("notes.txt");
        let error = policy
            .write(&asked)
            .expect_err("a sibling of the root, not a child of it");
        assert!(error.contains("prefix collision"), "{error}");
    }

    #[test]
    fn a_nested_path_is_accepted() {
        let tree = TempTree::new("nested-ok");
        fs::create_dir_all(tree.workspace().join("src/deep")).expect("a deep directory");
        let note = tree.workspace().join("src/deep/note.txt");
        fs::write(&note, "here").expect("a nested file");
        let policy = tree.policy();

        // Relative paths are resolved against the workspace, and the returned path is the real file.
        let read = policy
            .read(Path::new("src/deep/note.txt"))
            .expect("inside the read roots");
        assert_eq!(read, fs::canonicalize(&note).expect("the note exists"));
        assert_eq!(
            fs::read_to_string(&read).expect("the note is readable"),
            "here",
        );

        // A create answers for a target that does not exist, and the path it returns is the file itself.
        let created = policy
            .create(Path::new("src/deep/new.txt"))
            .expect("a new file inside the write roots");
        assert_eq!(
            created,
            fs::canonicalize(tree.workspace().join("src/deep"))
                .expect("the deep directory exists")
                .join("new.txt")
        );
        fs::write(&created, "made").expect("the resolved path is the file itself");
        assert_eq!(
            fs::read_to_string(&created).expect("the created file is readable"),
            "made",
        );

        let written = policy
            .write(Path::new("src/deep/note.txt"))
            .expect("an existing file inside the write roots");
        assert_eq!(written, fs::canonicalize(&note).expect("the note exists"));

        // The workspace itself is inside its own root, or no listing could ever start.
        let root = policy.read(Path::new("")).expect("the workspace root");
        assert_eq!(
            root,
            fs::canonicalize(tree.workspace()).expect("the workspace exists")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_that_walks_out_through_another_link_is_refused() {
        use std::os::unix::fs::symlink;

        let tree = TempTree::new("dangle-escape");
        // A real directory outside every root, so a landing there is an escape and not a missing file.
        let outside = tree.base.join("outside-esc");
        fs::create_dir_all(&outside).expect("a real directory outside the workspace");
        symlink(&outside, tree.workspace().join("b")).expect("the door out of the workspace");
        // L1 hangs: its target text `b/c` names a `c` that does not exist, so the land cannot be
        // canonicalized whole.
        symlink("b/c", tree.workspace().join("L1")).expect("the dangling link");
        let policy = tree.policy();

        // Walked the way the kernel walks, the chain lands at outside-esc/c: the raw target text reads
        // like it stays inside, the walk ends up outside. The refusal is about where it lands.
        let read = policy
            .read(&tree.workspace().join("L1"))
            .expect_err("the chain lands outside the root");
        assert!(read.contains("symlink"), "{read}");
        assert!(read.contains("outside-esc"), "{read}");

        // And a create behind the same door is refused too, so nothing outside is ever written.
        let created = policy
            .create(&tree.workspace().join("L1").join("note.txt"))
            .expect_err("a create at the landing writes outside the root");
        assert!(created.contains("symlink"), "{created}");
        assert!(created.contains("outside-esc"), "{created}");
        assert!(
            !outside.join("c").exists(),
            "nothing was made at the landing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_that_hangs_off_a_link_inside_the_root_is_accepted() {
        use std::os::unix::fs::symlink;

        let tree = TempTree::new("dangle-inside");
        fs::create_dir_all(tree.workspace().join("inner"))
            .expect("a directory inside the workspace");
        symlink("inner", tree.workspace().join("b")).expect("an ordinary link to it");
        symlink("b/c", tree.workspace().join("L1")).expect("the dangling link to it");
        let policy = tree.policy();

        // L1 dangles off `b`, which resolves to `inner` when the chain is followed: the kernel would
        // open <root>/inner/c and say ENOENT, so the gate answers with exactly that path, a resolved
        // prefix plus the missing tail, never the raw target text.
        let inner =
            fs::canonicalize(tree.workspace().join("b")).expect("b resolves inside the root");
        let read = policy
            .read(&tree.workspace().join("L1"))
            .expect("the chain lands inside the root");
        assert_eq!(read, inner.join("c"));

        // A create behind the same link answers with the same shape.
        let created = policy
            .create(&tree.workspace().join("L1").join("note.txt"))
            .expect("the landing stays inside the root");
        assert_eq!(created, inner.join("c").join("note.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn a_cyclic_dangling_chain_is_refused_on_the_hop_budget() {
        use std::os::unix::fs::symlink;

        let tree = TempTree::new("dangle-cycle");
        // The chase runs d -> e/c/d -> e/c/e/c/d -> ..., always in the walk's own tail, never a
        // canonicalizable land: the bound, not the kernel's ELOOP, is what has to end it.
        symlink("e/c", tree.workspace().join("d")).expect("the cycle");
        symlink("d", tree.workspace().join("e")).expect("the cycle back-edge");

        let error = tree
            .policy()
            .read(&tree.workspace().join("d"))
            .expect_err("the chain never lands");
        assert!(error.contains("symlinks were followed"), "{error}");
        // The bound is named on the path the caller asked about, `<workspace>/d`, which a chase path
        // (assembled from `e/c` and `d`) never contains.
        assert!(error.contains("work/d"), "{error}");
    }
}
