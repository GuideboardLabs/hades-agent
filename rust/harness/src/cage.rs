//! The cage: what a program the harness runs is allowed to touch.
//!
//! The tool runs a program. Without a cage that program runs with the caller's rights, which means a model
//! that writes `cat ~/.hermes/config.yaml` reads the API keys, and one that writes `rm -rf ~` is stopped
//! only if the policy asked a person first. Both are avoidable, and the mechanism that avoids them here is
//! Landlock: an unprivileged process confines *itself*, and everything it goes on to start, to a set of
//! paths and access rights. No root, no sysctl, no namespace.
//!
//! Why not namespaces or a container, measured on the machine this was built for: unprivileged user namespaces are refused
//! (`unshare -n` is EPERM, `bwrap` fails with "setting up uid map: Permission denied") because
//! `kernel.apparmor_restrict_unprivileged_userns` is 1. `landlock-restrict` as a separate helper is not
//! installed either. Landlock needs none of those things, and it was reachable from a plain unprivileged
//! process.
//!
//! What the cage is, precisely. Deny by default, for the filesystem and for TCP: read and execute under a
//! fixed list of system directories (a program cannot start without `/usr` and `/lib`), read and write under
//! the task's root and under `/dev`, and no TCP at all.
//!
//! What it is not, in the same breath. It does not hide anything: a denied path still exists and the program
//! gets `EACCES`, not `ENOENT`, so a program can still learn that `~/.ssh` exists. It does not confine UDP,
//! raw sockets, or anything that is not TCP. It does not hide other processes or make `/proc` lie, and it
//! cannot remove access it grants more widely than a program needs. It bounds what a program can read and
//! write, which is where the damage that costs money happens, and it says so rather than being called a
//! sandbox.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::fault::Fault;

/// The syscall numbers the Landlock API uses.
///
/// Named from `libc` where the pinned crate has them, because the number is per-architecture and writing
/// x86-64's down by hand is how the crate came to not compile on ARM64 at all. Landlock was added to the
/// generic and x86-64 tables as 444, 445 and 446; the assertion below keeps the fallback honest.
const SYS_CREATE_RULESET: libc::c_long = libc::SYS_landlock_create_ruleset as libc::c_long;
const SYS_ADD_RULE: libc::c_long = libc::SYS_landlock_add_rule as libc::c_long;
const SYS_RESTRICT_SELF: libc::c_long = libc::SYS_landlock_restrict_self as libc::c_long;

/// Ask for the ABI version rather than a rule set.
const WANT_VERSION: usize = 1;
/// A rule about a path and what may be done beneath it.
const PATH_BENEATH: usize = 1;

/// The filesystem rights, as the kernel numbers them.
const EXECUTE: u64 = 1 << 0;
const WRITE_FILE: u64 = 1 << 1;
const READ_FILE: u64 = 1 << 2;
const READ_DIR: u64 = 1 << 3;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_CHAR: u64 = 1 << 6;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SOCK: u64 = 1 << 9;
const MAKE_FIFO: u64 = 1 << 10;
const MAKE_BLOCK: u64 = 1 << 11;
const MAKE_SYM: u64 = 1 << 12;
const REFER: u64 = 1 << 13;
const TRUNCATE: u64 = 1 << 14;
const IOCTL_DEV: u64 = 1 << 15;
/// Look up a pathname Unix socket. Since Landlock ABI 9 (Linux 7.1); the value is the kernel's own
/// `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`, read from the UAPI rather than guessed.
pub const LANDLOCK_ACCESS_FS_RESOLVE_UNIX: u64 = 1 << 16;

/// Every right this knows about. Handling all of them is what makes the cage deny by default.
const ALL_RIGHTS: u64 = EXECUTE
    | WRITE_FILE
    | READ_FILE
    | READ_DIR
    | REMOVE_DIR
    | REMOVE_FILE
    | MAKE_CHAR
    | MAKE_DIR
    | MAKE_REG
    | MAKE_SOCK
    | MAKE_FIFO
    | MAKE_BLOCK
    | MAKE_SYM
    | REFER
    | TRUNCATE
    | IOCTL_DEV;

/// Landlock rejects rights introduced after the running kernel's ABI. Match both
/// the handled mask and every grant to the available ABI, as the kernel API requires.
pub fn fs_rights_for_abi(abi: isize) -> u64 {
    let mut rights = ALL_RIGHTS;
    if abi < 5 {
        rights &= !IOCTL_DEV;
    }
    if abi < 3 {
        rights &= !TRUNCATE;
    }
    if abi < 2 {
        rights &= !REFER;
    }
    if abi >= 9 {
        rights |= LANDLOCK_ACCESS_FS_RESOLVE_UNIX;
    }
    rights
}

/// The rights a *readable* path needs: read, list, execute, and, where the kernel can enforce it, the
/// right to look up a pathname Unix socket under it. Without the last one, a program inside the cage could
/// not connect to a socket the same rule says it may read, and a program outside it could.
pub fn read_rights_for_abi(abi: isize) -> u64 {
    let mut rights = READ;
    if abi >= 9 {
        rights |= LANDLOCK_ACCESS_FS_RESOLVE_UNIX;
    }
    rights
}

/// Reading and running: what a program needs to start and to read the system it lives on.
const READ: u64 = EXECUTE | READ_FILE | READ_DIR;

/// The rights a rule about a *file* may carry. A file cannot be made, removed or referred, so passing the
/// directory mask to a file is refused with EINVAL by the kernel. That is what made the first version grant
/// `/dev` as a directory and lose `/dev/null` with it: the mask was wrong, not the idea.
const FILE_RIGHTS: u64 = READ_FILE | WRITE_FILE;

/// The device files a program cannot work without. `/dev/null` is the one that matters: a shell redirects
/// to it constantly, and a program that cannot write to it dies on the way to doing anything.
const DEVICES: &[(&str, u64)] = &[
    ("/dev/null", FILE_RIGHTS),
    ("/dev/zero", READ_FILE),
    ("/dev/urandom", READ_FILE),
    ("/dev/random", READ_FILE),
    ("/dev/tty", FILE_RIGHTS),
];

/// The language runtimes a profile's check may need to start.
///
/// A program that cannot start an interpreter cannot do the work, and the failure is worse than a refusal:
/// the model sees a broken interpreter, concludes the box is broken, and spends its attempts diagnosing
/// instead of working. That happened for real, three attempts in a row, against a frontier model.
const RUNTIMES: &[&str] = &[
    "python3", "python", "node", "cargo", "rustc", "go", "make", "git",
];

/// The rights that apply as rules for TCP. Landlock does not cover UDP, which is stated, not implied.
const BIND_TCP: u64 = 1 << 0;
const CONNECT_TCP: u64 = 1 << 1;

/// The directories a program needs to exist at all. Deliberately short: everything here is readable by the
/// program, so a directory joins the list by being necessary, not by being convenient.
pub const SYSTEM: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/proc", "/sys", "/run",
];

/// The same list with the broad `/run` grant removed.
///
/// `/run` is where the host's own control sockets live: Docker, Podman, containerd, and the system bus. A
/// cage that grants it hands a program a way to ask a privileged daemon to do the work instead, and on
/// kernels before Landlock ABI 9 no filesystem rule can refuse the connect. Strict mode is for a caller who
/// would rather name the resolver than take that path.
pub const SYSTEM_STRICT: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/proc", "/sys",
];

/// The only `/run` paths strict mode keeps, and it keeps them by name.
///
/// A name lookup is the one thing a program cannot do without: `systemd-resolved` publishes its stub
/// resolver under `/run`, and without it every clone, install and fetch fails with a DNS error that looks
/// like a broken network rather than a cage. Both are sockets, not the directory, and neither can start a
/// container.
const STRICT_RESOLVE: &[&str] = &["/run/systemd/resolve", "/run/nscd"];

/// The control sockets a host may expose to a program that can reach `/run` or `/var/run`.
pub const SOCKETS: &[&str] = &[
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/run/podman/podman.sock",
    "/run/containerd/containerd.sock",
];

/// A socket the harness found on the host, and whether it is there at all.
///
/// Presence is the fact this can establish without opening anything: whether the caller may connect also
/// depends on its group membership, and a doctor that guessed that would be wrong on half the machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketState {
    Absent,
    Present,
}

/// Scan the named sockets. Only presence is reported; nothing is opened.
pub fn scan_sockets(candidates: &[PathBuf]) -> Vec<(PathBuf, SocketState)> {
    candidates
        .iter()
        .map(|path| {
            let state = match std::fs::metadata(path) {
                Ok(_) => SocketState::Present,
                Err(_) => SocketState::Absent,
            };
            (path.clone(), state)
        })
        .collect()
}

/// What a program the harness runs is allowed to touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cage {
    writable: Vec<PathBuf>,
    readable: Vec<PathBuf>,
    /// Single files with their own rights, which directories cannot express.
    files: Vec<(PathBuf, u64)>,
    network: bool,
}

impl Cage {
    /// The cage a turn runs under: the task's root writable, the system readable, no network.
    pub fn around(root: impl Into<PathBuf>) -> Cage {
        Cage {
            writable: vec![root.into()],
            // Multiarch library directories and other default system mounts vary by
            // distribution. An absent default needs no grant. Explicit workspace and
            // scratch paths still have to exist when plan() builds their rules.
            readable: SYSTEM
                .iter()
                .map(PathBuf::from)
                .filter(|path| !matches!(std::fs::metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound))
                .collect(),
            files: DEVICES
                .iter()
                .map(|(path, rights)| (PathBuf::from(path), *rights))
                .collect(),
            network: false,
        }
    }

    /// Let the program start the language runtimes it needs, read-only and executable.
    ///
    /// The prefixes are discovered by asking, in the parent (which is not caged), where each runtime on
    /// `PATH` lives: its own directory, its prefix, and for a Python, the two prefixes it reports, because a
    /// virtual environment's interpreter loads its standard library from a directory that is not under the
    /// environment at all. Discovery cannot look under the home directory without becoming a grant of the
    /// home directory, so a toolchain installed there is named in the config and added by
    /// [`Cage::with_runtime_paths`]. Nothing at or above the home directory is ever granted, so the config
    /// and the key file stay denied; what is granted is read and execute, never write.
    pub fn with_runtimes(mut self) -> Cage {
        for path in runtime_paths().into_iter().chain(declared_toolchains()) {
            self = self.grant_toolchain(path);
        }
        self
    }

    /// Grant a toolchain the person named, read and execute only.
    ///
    /// A named path is a specific toolchain, so a path that is the home directory, anything above it, `/`,
    /// a non-directory or a path that no longer exists is refused rather than granted: a grant that
    /// resolves to `$HOME` is the whole filesystem with extra steps, and a typo should be a `doctor` line,
    /// not a cage that silently covers nothing.
    pub fn with_runtime_paths<I, P>(mut self, paths: I) -> Cage
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        for path in paths {
            self = self.grant_toolchain(path.into());
        }
        self
    }

    fn grant_toolchain(mut self, path: PathBuf) -> Cage {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if !path.is_dir() || !self.toolchain_acceptable(&path) {
            return self;
        }
        if !self.readable.contains(&path) {
            self.readable.push(path);
        }
        self
    }

    /// Whether a path may be granted as a toolchain: never `/`, never `$HOME` or anything above it, and
    /// never an ancestor of the workspace, which would grant the workspace's parents along with it.
    fn toolchain_acceptable(&self, path: &Path) -> bool {
        if path == Path::new("/") {
            return false;
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
            && (path == home || home.starts_with(path))
        {
            return false;
        }
        !self.writable.iter().any(|root| root.starts_with(path))
    }

    /// The cage a chat's shell runs under: the workspace writable, the system and language runtimes
    /// readable, network open, and everything else denied.
    ///
    /// Network stays open because a chat that cannot clone a repository or reach a package index cannot do
    /// the work people ask of it. What the cage denies is the read: a page that tells the model to read a key
    /// and post it gets `EACCES` on the read, and there is nothing to post. Writes outside the workspace and
    /// its scratch directory are denied too, which is why this cage is the read boundary and not an egress
    /// filter.
    pub fn chat(root: impl Into<PathBuf>, scratch: impl Into<PathBuf>) -> Cage {
        Cage::chat_with(root, scratch, true)
    }

    /// The chat cage with the network dial a role sets. A verifier or a memory curator has no reason to reach
    /// out, so its shell is closed; a patch profile keeps the network because a clone or an install needs it.
    pub fn chat_with(root: impl Into<PathBuf>, scratch: impl Into<PathBuf>, network: bool) -> Cage {
        let cage = Cage::around(root).also_writable(scratch).with_runtimes();
        if network { cage.with_network() } else { cage }
    }

    /// Another directory the program may write in. Needed by anything that wants a scratch directory, since
    /// `/tmp` is not writable from inside a cage that was not told about it.
    pub fn also_writable(mut self, path: impl Into<PathBuf>) -> Cage {
        self.writable.push(path.into());
        self
    }

    /// Let the program reach the network. Off unless a caller asks, and the caller is then on the hook for
    /// saying why.
    pub fn with_network(mut self) -> Cage {
        self.network = true;
        self
    }

    /// A cage with no broad `/run`: the system, the named resolver paths, and nothing that can ask a
    /// privileged daemon to act. Opt-in, because removing `/run` breaks a legitimate runtime that keeps
    /// something else there, and the doctor names the path when one is hit.
    pub fn strict(root: impl Into<PathBuf>) -> Cage {
        let mut cage = Cage::around(root);
        cage.readable = SYSTEM_STRICT
            .iter()
            .chain(STRICT_RESOLVE.iter())
            .map(PathBuf::from)
            .filter(|path| {
                !matches!(std::fs::metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            })
            .collect();
        cage
    }

    /// Whether this cage would let the caller read `path`, by the rules it holds rather than by trying.
    ///
    /// A writable directory is readable too, because the writable rule carries the whole mask. The check is
    /// textual over absolute grants, which is the same approximation the kernel makes: it is a report for
    /// `doctor` and a test seam, not a second security mechanism.
    pub fn grants_read(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        let under = |grant: &PathBuf| path == grant || path.starts_with(grant);
        self.readable.iter().any(under)
            || self.writable.iter().any(under)
            || self
                .files
                .iter()
                .any(|(grant, rights)| grant == path && rights & READ_FILE != 0)
    }

    /// How the cage's Unix-socket coverage reads on this kernel.
    ///
    /// Landlock ABI 9 (Linux 7.1) added `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`: a connect to a pathname socket
    /// created outside the domain is denied unless the path grants the right. Before ABI 9 there is no such
    /// rule, and abstract-namespace sockets are never covered by it. This says which of those is true here
    /// rather than letting "landlock" imply more than it does.
    pub fn socket_note(&self) -> String {
        let abi = abi();
        if abi >= 9 {
            "closed to pathname sockets outside the granted paths (landlock ABI 9 resolve_unix); abstract \
             sockets are not contained"
                .to_string()
        } else {
            format!(
                "not contained by this kernel (landlock abi {abi} has no pathname socket rule; abstract \
                 sockets were never covered)"
            )
        }
    }

    /// The cage's whole claim in readable form: what the kernel supports, what it closes, and what it does
    /// not. `describe()` is the one-line version for a log; this is the one a person reads in `doctor`.
    pub fn report(&self) -> String {
        let abi = abi();
        let tcp = if self.network {
            "open (the caller asked for the network)"
        } else if abi >= 4 {
            "closed"
        } else {
            "open (this kernel cannot close TCP)"
        };
        let mut text = format!(
            "landlock abi {abi}; tcp: {tcp}; unix sockets: {}; readable: {}; writable: {}",
            self.socket_note(),
            self.readable
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" "),
            self.writable
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" "),
        );
        if abi < 5 {
            text.push_str("; this kernel cannot restrict device ioctls");
        }
        if abi < 3 {
            text.push_str("; this kernel cannot restrict truncation");
        }
        text
    }

    /// What the cage allows, for a log or a surface that has to state it rather than imply it.
    pub fn describe(&self) -> String {
        let mut text = format!(
            "writable: {}; readable: {}; files: {}; network: {}",
            self.writable
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" "),
            self.readable
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" "),
            self.files
                .iter()
                .map(|(path, _)| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" "),
            if self.network { "open" } else { "closed" },
        );
        let version = abi();
        if !self.network && version < 4 {
            text.push_str(" (this kernel cannot close TCP)");
        }
        if version < 5 {
            text.push_str(" (this kernel cannot restrict device ioctls)");
        }
        if version < 3 {
            text.push_str(" (this kernel cannot restrict truncation)");
        }
        text.push_str(&format!("; unix sockets: {}", self.socket_note()));
        text
    }

    /// Build everything the child will need, here, before the fork.
    ///
    /// The hook that runs after a fork may not allocate: it runs in a process that may have several threads
    /// and a locked allocator, and the lock will never be released. So every byte the child passes to the
    /// kernel, and every file descriptor it passes, is made here and only used there.
    pub fn plan(&self) -> Result<Plan, Fault> {
        let abi = abi();
        if abi < 1 {
            return Err(Fault::harness(
                "this kernel has no Landlock, so a program cannot be caged; refusing to run it with the \
                 caller's rights instead"
                    .to_string(),
            ));
        }

        // `handled_access_net` and the size of the attribute are the ABI: a kernel older than 4 is told
        // about the filesystem only, and the caller is told that TCP stays open.
        let rights = fs_rights_for_abi(abi);
        let attr = if abi >= 4 {
            let mut attr = Vec::with_capacity(16);
            attr.extend_from_slice(&rights.to_le_bytes());
            let net = if self.network {
                0
            } else {
                BIND_TCP | CONNECT_TCP
            };
            attr.extend_from_slice(&net.to_le_bytes());
            attr
        } else {
            rights.to_le_bytes().to_vec()
        };

        let mut rules = Vec::new();
        for path in self.readable.iter() {
            rules.push(self.rule(path, read_rights_for_abi(abi))?);
        }
        for (path, rights) in self.files.iter() {
            // A device that is not there is not a mistake worth failing over: a container may have no
            // `/dev/tty`, and a program that never opens one should still run.
            if path.exists()
                && let Ok(rule) = self.rule(path, *rights)
            {
                rules.push(rule);
            }
        }
        for path in self.writable.iter() {
            rules.push(self.rule(path, rights)?);
        }

        Ok(Plan {
            attr,
            rules,
            describe: self.describe(),
        })
    }

    /// One path rule. The path must exist: a rule about a directory that is not there is a cage built on a
    /// mistake, and the honest time to say so is now.
    fn rule(&self, path: &Path, rights: u64) -> Result<(Vec<u8>, OwnedFd), Fault> {
        let file = std::fs::File::open(path).map_err(|error| {
            Fault::harness(format!(
                "the cage could not open {} to allow it: {error}",
                path.display()
            ))
        })?;
        let fd: OwnedFd = file.into();
        // A path rule is twelve bytes and packed: a u64, then an i32 file descriptor. Written by hand
        // because the layout is what the kernel reads, not what a Rust struct would lay out. A rule for a
        // single file rather than a directory was refused by this kernel with EINVAL, so rules are about
        // directories, and `/dev` is allowed whole rather than `/dev/null` alone.
        let mut rule = Vec::with_capacity(12);
        rule.extend_from_slice(&rights.to_le_bytes());
        rule.extend_from_slice(&fd.as_raw_fd().to_le_bytes());
        Ok((rule, fd))
    }
}

/// Toolchains a person declared in `runtime_paths`, in the config.
///
/// Discovery asks the `PATH`, and the `PATH` of a caged program is the parent's; a toolchain installed under
/// the home directory is therefore invisible to it, because walking the home is the grant the cage exists to
/// refuse. Naming the path in the config is the explicit way in, and this is where that list is held.
static DECLARED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Replace the declared toolchain list. Called once, when the config is loaded.
pub fn declare_toolchains(paths: impl IntoIterator<Item = PathBuf>) {
    let mut held = DECLARED.lock().unwrap_or_else(|poison| poison.into_inner());
    held.clear();
    held.extend(paths);
}

fn declared_toolchains() -> Vec<PathBuf> {
    DECLARED
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

/// The toolchain directories a config may name, as the report reads them.
pub fn declared() -> Vec<PathBuf> {
    declared_toolchains()
}

/// The directories a person is likely to have installed a runtime into, under the home directory.
///
/// `doctor` names these when they exist and are not declared: the cage cannot discover them, so an
/// undeclared one is a runtime that will fail inside a run with a message about a missing library.
pub fn likely_home_toolchains(home: &Path) -> Vec<PathBuf> {
    [
        ".cargo",
        ".rustup",
        ".nvm",
        ".pyenv",
        ".local/share/uv",
        ".local/share/pnpm",
        ".bun",
    ]
    .iter()
    .map(|name| home.join(name))
    .filter(|path| path.is_dir())
    .collect()
}

/// The directories a program needs in order to start the runtimes it needs, discovered by asking.
///
/// Asking the runtime is the only honest way: a virtual environment's interpreter is a symlink to a system
/// binary whose standard library lives under the environment, except when it does not, which is exactly the
/// case here (`sys.base_prefix` is inside `.hermes-runtime`, not inside `venv`). Guessing from the `PATH`
/// entry alone found one of the two and the interpreter still died with no `encodings` module.
///
/// The answer is cached for the life of the process: discovery starts a Python and asks it two questions,
/// and the chat's shell now builds a cage on every call. `PATH` does not change under a running gateway, so
/// asking twice would buy nothing and cost tens of milliseconds on every command.
fn runtime_paths() -> Vec<PathBuf> {
    static FOUND: OnceLock<Vec<PathBuf>> = OnceLock::new();
    FOUND.get_or_init(discover_runtimes).clone()
}

fn discover_runtimes() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut found: Vec<PathBuf> = Vec::new();

    for name in RUNTIMES {
        let Some(on_path) = which(name) else {
            continue;
        };
        // The directory the executable is in, and the prefix above it, which is what a language needs to
        // find its libraries. A binary directly in `/usr/bin` contributes `/usr/bin` and `/usr`, both
        // already readable, so this only ever adds anything for a runtime installed somewhere unusual.
        if let Some(dir) = on_path.parent() {
            found.push(dir.to_path_buf());
            if let Some(prefix) = dir.parent() {
                found.push(prefix.to_path_buf());
            }
        }
        // And, for anything that answers like a Python, the two prefixes it reports about itself.
        if name.starts_with("python")
            && let Some(reported) = ask_python(&on_path)
        {
            found.extend(reported);
        }
    }

    // A rustup toolchain is a shim plus two homes: the shim is found on `PATH`, but it reads
    // `<RUSTUP_HOME>/settings.toml` and the registry cache under `CARGO_HOME`, so granting only the bin
    // directory yields "could not read settings file: Permission denied" inside the cage. Only values
    // outside `$HOME` are taken: granting `~/.cargo` would hand the cage `credentials.toml`, and a
    // toolchain that lives under the home is a toolchain this cage does not cover (stated in its docs).
    if let Some(home) = &home {
        for key in ["CARGO_HOME", "RUSTUP_HOME"] {
            if let Some(value) = std::env::var_os(key) {
                let path = PathBuf::from(value);
                if !path.starts_with(home) {
                    found.push(path);
                }
            }
        }
    }

    let mut kept: Vec<PathBuf> = Vec::new();
    for path in found {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        // Never the home directory or anything above it: a cage that grants the home directory is not a
        // cage, and the config and the key file both live there.
        if path == Path::new("/") {
            continue;
        }
        if let Some(home) = &home
            && (path == *home || home.starts_with(&path))
        {
            continue;
        }
        if !path.is_dir() || kept.contains(&path) {
            continue;
        }
        kept.push(path);
    }
    kept
}

/// The first executable called `name` on the `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Where a Python keeps its standard library, asked rather than guessed.
fn ask_python(interpreter: &Path) -> Option<Vec<PathBuf>> {
    let output = std::process::Command::new(interpreter)
        .args([
            "-c",
            "import sys; print(sys.base_prefix); print(sys.prefix)",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let paths: Vec<PathBuf> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect();
    if paths.is_empty() { None } else { Some(paths) }
}

/// A cage, prepared for one child process.
///
/// Moved into the child's hook rather than shared, so the rules and their file descriptors have exactly one
/// owner and no clone can leave a descriptor open behind the child's back.
#[derive(Debug)]
pub struct Plan {
    attr: Vec<u8>,
    rules: Vec<(Vec<u8>, OwnedFd)>,
    describe: String,
}

impl Plan {
    /// What this plan allows, for the log.
    pub fn describe(&self) -> &str {
        &self.describe
    }

    /// Confine the calling process for good, and everything it starts after this.
    ///
    /// Runs after a fork and before an exec, so it allocates nothing, takes no lock and prints nothing: only
    /// system calls, and an error built from a number. Nothing here can be undone, which is the point: a
    /// program cannot lift its own cage, and neither can anything it runs.
    pub fn confine(&self) -> std::io::Result<()> {
        let ruleset = call(
            SYS_CREATE_RULESET,
            self.attr.as_ptr() as usize,
            self.attr.len(),
            0,
            0,
        );
        if ruleset < 0 {
            return Err(std::io::Error::from_raw_os_error(-ruleset as i32));
        }
        for (rule, _fd) in &self.rules {
            let added = call(
                SYS_ADD_RULE,
                ruleset as usize,
                PATH_BENEATH,
                rule.as_ptr() as usize,
                0,
            );
            if added < 0 {
                return Err(std::io::Error::from_raw_os_error(-added as i32));
            }
        }
        // The kernel refuses to confine a process that could still gain privileges from a set-user-id
        // binary, so this comes first. It is also true for its own sake.
        let set = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } as isize;
        if set < 0 {
            return Err(std::io::Error::from_raw_os_error(-set as i32));
        }
        let confined = call(SYS_RESTRICT_SELF, ruleset as usize, 0, 0, 0);
        if confined < 0 {
            return Err(std::io::Error::from_raw_os_error(-confined as i32));
        }
        Ok(())
    }
}

/// The kernel's Landlock ABI version, or a negative number when there is none.
pub fn abi() -> isize {
    call(SYS_CREATE_RULESET, 0, 0, WANT_VERSION, 0)
}

/// Whether a program can be caged at all here.
pub fn available() -> bool {
    abi() >= 1
}

/// One Linux system call.
///
/// The standard library does not expose these. `libc::syscall` does, with the per-architecture number in
/// its own table, so the same code compiles on x86-64 and on ARM64 and the number is never this file's
/// guess. Raw assembly would not set libc's errno; callers convert the negated return value with
/// `from_raw_os_error`.
fn call(number: libc::c_long, a: usize, b: usize, c: usize, d: usize) -> isize {
    // SAFETY: a system call with the arguments its own contract requires. Nothing is dereferenced that the
    // caller did not build, and the numbers come from libc's table for this architecture.
    unsafe {
        libc::syscall(
            number,
            a as libc::c_long,
            b as libc::c_long,
            c as libc::c_long,
            d as libc::c_long,
        ) as isize
    }
}
