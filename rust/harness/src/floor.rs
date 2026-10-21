//! The hard floor: five kinds of action that are never the agent's to take on its own.
//!
//! A permanent delete, money moving, a credential or permission change, new security access, and a
//! message that reaches a person. The source that produced this list measured a forbidden action
//! attempted in 17.4% of runs at maximum reasoning effort, so a sentence in the prompt is not the
//! enforcement. This is: [`check`] runs before the tool does, at both doors.
//!
//! - **The chat loop** (`agent::execute`) has nobody to ask, so the refusal is returned to the model as
//!   the tool's result: it says what was refused, why, and that the person decides.
//! - **A governed turn** (`turn::Turn::once`) asks the broker once the program is visible. The question
//!   is asked even when the policy says allow, and even when a standing answer already allowed the
//!   shell for that session. The answer a person gave to "may the shell run" is not an answer to "may
//!   this be deleted".
//!
//! Stated limits, because a floor that overstates itself is worse than none. This reads the tool name
//! and its arguments as text. It cannot see what a page does after a control whose name says nothing
//! ("Continue"), a program that builds a command out of pieces at run time, or a delete performed by a
//! binary whose name is not on a list. It reads literal redirection targets, so `> "$FILE"` is outside
//! it, and Landlock does not mediate `access(2)`, so `test -r` answers about a file the read itself
//! cannot open. It is a floor, not a sandbox, and the lists below are the whole of what it knows.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// The five categories. One name, one stable action string, one place to change either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Deleting something that cannot be brought back.
    Delete,
    /// Any movement of money: a charge, a payout, a transfer, an order.
    Money,
    /// Changing a password, key, token, mode or owner.
    Credential,
    /// Granting a new way in: an SSH key, a user, a group, a firewall rule, a cloud IAM binding.
    Access,
    /// Reaching a person: mail, a chat API, a social post.
    Outbound,
}

impl Category {
    /// Every category, in the order a person reads them.
    ///
    /// This is the only list: the guard digest and the candidate refusal both iterate it, so a category
    /// cannot be enforced by the floor and invisible to a generation's guard. The `match` is exhaustive on
    /// purpose — adding a variant does not compile until it is named here, which is the tripwire an array
    /// alone would not give.
    pub const fn all() -> [Category; 5] {
        match Category::Delete {
            Category::Delete => {}
            Category::Money => {}
            Category::Credential => {}
            Category::Access => {}
            Category::Outbound => {}
        }
        [
            Category::Delete,
            Category::Money,
            Category::Credential,
            Category::Access,
            Category::Outbound,
        ]
    }

    /// What this category is called in a refusal or a question.
    pub fn phrase(self) -> &'static str {
        match self {
            Category::Delete => "a permanent delete",
            Category::Money => "money moving",
            Category::Credential => "a credential or permission change",
            Category::Access => "new security access",
            Category::Outbound => "a message to a person",
        }
    }

    /// The action string the approval broker records. Stable, because a session-scoped answer is looked
    /// up by it: changing this spelling silently re-asks every question.
    pub fn action(self) -> &'static str {
        match self {
            Category::Delete => "permanently delete",
            Category::Money => "move money",
            Category::Credential => "change a credential or permission",
            Category::Access => "grant new security access",
            Category::Outbound => "send a message to a person",
        }
    }
}

/// How the floor describes itself in a receipt.
///
/// The floor reads the tool name and its arguments as text. Calling that "safety: on" would claim a
/// sandbox this code does not have, so the receipt names the mechanism and the cage is what bounds reads
/// and writes.
pub const HEURISTIC: &str = "command-text heuristic";

/// How a strict policy describes itself. It refuses a short list of destructive primitives it can see in
/// the command text, and it does not claim to catch an encoding it cannot read.
pub const STRICT_DENY_LIST: &str = "strict deny list";

/// The receipt line for a run. One string, so a caller cannot print half of it.
pub fn receipt() -> &'static str {
    "floor: command-text heuristic (not a sandbox)"
}

/// The words a strict policy refuses. This is a deny list over literal text: `rm -rf` typed out is caught,
/// `r''m -rf` and `$(printf rm)` are not, and the receipt says `strict deny list` rather than "sandbox".
const DESTRUCTIVE: [&str; 9] = [
    "rm -rf",
    "rm -fr",
    "rm --recursive --force",
    "mkfs",
    "shred ",
    "of=/dev/sd",
    "of=/dev/nvme",
    "of=/dev/vd",
    "of=/dev/hd",
];

/// A strict check over the command text, after the contents of quoted strings are removed.
///
/// Stripping the quoted spans is what keeps `echo "rm -rf /"` from being refused; it is also what makes
/// `sh -c 'rm -rf /'` invisible, which is the limitation this function has and states.
pub fn strict(command: &str) -> Option<Floor> {
    let visible = unquoted(command).to_lowercase();
    for word in DESTRUCTIVE {
        if visible.contains(word) {
            return Some(Floor::new(Category::Delete, word));
        }
    }
    None
}

/// The command with the contents of single- and double-quoted spans removed.
fn unquoted(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut quote: Option<char> = None;
    for ch in command.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => {}
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None => out.push(ch),
        }
    }
    out
}

/// What tripped the floor, and why. The evidence is the matched word or endpoint, never the whole
/// command: a command can carry a token in a URL, and a refusal is copied into a chat or a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Floor {
    category: Category,
    evidence: String,
}

impl Floor {
    fn new(category: Category, evidence: impl Into<String>) -> Self {
        Floor {
            category,
            evidence: evidence.into(),
        }
    }

    pub fn category(&self) -> Category {
        self.category
    }

    /// The broker's action, for the question a governed run raises.
    pub fn action(&self) -> &'static str {
        self.category.action()
    }

    /// Why the question is being asked, in one sentence.
    pub fn reason(&self) -> String {
        format!(
            "the hard floor: {} ({})",
            self.category.phrase(),
            self.evidence
        )
    }

    /// What the model is told in a chat, where nobody can be asked.
    pub fn refusal(&self) -> String {
        format!(
            "Refused by the hard floor: {} ({}). This is never done on my own; a person decides it. \
             Tell the person what you would do and why, and stop. Do not look for another command \
             that reaches the same end.",
            self.category.phrase(),
            self.evidence
        )
    }
}

/// Check a tool call before it runs. `None` is the ordinary case: the call is not floored.
pub fn check(tool: &str, args: &Value, root: &Path) -> Option<Floor> {
    match tool {
        "shell" => {
            let command = args.get("command").and_then(Value::as_str)?;
            shell(command, root)
        }
        "write_file" | "patch" => {
            let path = args.get("path").and_then(Value::as_str)?;
            written_path(path, root)
        }
        "browser" => page(args),
        _ => None,
    }
}

fn shell(command: &str, root: &Path) -> Option<Floor> {
    // Redirection is a write, and the segment it lives in is often not a write command at all:
    // `echo key > ~/.hermes/config.yaml` reads as `echo`. The targets are checked before the command
    // words, and a target the shell sends to a device or to another descriptor is not a file write.
    for target in redirections(command) {
        if let Some(floor) = written_path(&target, root) {
            return Some(floor);
        }
    }
    for segment in segments(command) {
        if let Some(floor) = one_segment(&segment, root) {
            return Some(floor);
        }
    }
    None
}

/// The files a command redirects output into.
///
/// This is a scanner, not a parser: it walks the bytes once, ignores everything inside single or double
/// quotes (a `>` in a commit message is not a redirect), and reads the word after `>` or `>>`. `2>&1`,
/// `&>`, `/dev/null` and `/dev/stdout` are descriptors or devices, not files, and are skipped. A target
/// assembled at run time (`> "$FILE"`) is outside what this can see, which is stated in the module note.
fn redirections(command: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '>' => {
                if chars.peek() == Some(&'>') {
                    chars.next();
                }
                while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                    chars.next();
                }
                let mut target = String::new();
                while let Some(c) = chars.peek() {
                    if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>' | ')' | '(') {
                        break;
                    }
                    target.push(*c);
                    chars.next();
                }
                let device = target.starts_with('&')
                    || matches!(target.as_str(), "/dev/null" | "/dev/stdout" | "/dev/stderr");
                if !target.is_empty() && !device {
                    found.push(target);
                }
            }
            None => {}
        }
    }
    found
}

/// One command, cut at the operators a shell treats as separators. `&` inside a query string splits a
/// URL, which costs a missed match on the tail and never invents one.
fn segments(command: &str) -> Vec<String> {
    command
        .replace("&&", ";")
        .replace("||", ";")
        .split(['\n', ';', '|', '&'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "nohup", "time", "command", "exec", "nice", "ionice", "stdbuf", "xargs",
];
const TAKES_VALUE: &[&str] = &["-u", "-g", "-p", "-n", "-C", "-U", "-r", "-t"];

/// The first word that is actually the command: wrappers, their flags, and leading assignments are not
/// the program. Without this, `sudo -u root rm x` reads as `sudo`.
fn command_word<'a>(words: &[&'a str]) -> Option<&'a str> {
    let mut index = 0;
    let mut wrapped = false;
    while index < words.len() {
        let word = words[index];
        if WRAPPERS.contains(&word) {
            wrapped = true;
            index += 1;
            continue;
        }
        if is_assignment(word) {
            index += 1;
            continue;
        }
        if wrapped && word.starts_with('-') {
            index += if TAKES_VALUE.contains(&word) { 2 } else { 1 };
            continue;
        }
        return Some(word.rsplit('/').next().unwrap_or(word));
    }
    None
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.contains('/')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

fn one_segment(segment: &str, root: &Path) -> Option<Floor> {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let word = command_word(&words)?.to_ascii_lowercase();
    let lower = segment.to_ascii_lowercase();
    let args: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();

    if word == "tee" {
        for arg in words.iter().skip(1) {
            if arg.starts_with('-') {
                continue;
            }
            if let Some(floor) = written_path(arg, root) {
                return Some(floor);
            }
        }
    }

    if let Some(floor) = deleting(&word, &args, &lower) {
        return Some(floor);
    }
    if let Some(floor) = money(&word, &args, &lower) {
        return Some(floor);
    }
    if let Some(floor) = credential(&word, &args, &lower, root) {
        return Some(floor);
    }
    if let Some(floor) = access(&word, &lower) {
        return Some(floor);
    }
    if let Some(floor) = outbound(&word, &args, &lower) {
        return Some(floor);
    }
    None
}

fn deleting(word: &str, args: &[String], lower: &str) -> Option<Floor> {
    const BINS: &[&str] = &["rm", "rmdir", "unlink", "shred", "srm", "wipe", "trash-put"];
    if BINS.contains(&word) {
        return Some(Floor::new(
            Category::Delete,
            format!("the command runs `{word}`"),
        ));
    }
    if word.starts_with("mkfs") || word == "wipefs" {
        return Some(Floor::new(
            Category::Delete,
            format!("`{word}` destroys a filesystem"),
        ));
    }
    if word == "find" && (lower.contains("-delete") || lower.contains("-exec rm")) {
        return Some(Floor::new(
            Category::Delete,
            "`find` was told to delete what it matches",
        ));
    }
    if word == "dd" && args.iter().any(|arg| arg.starts_with("of=/dev/")) {
        return Some(Floor::new(Category::Delete, "`dd` writes over a device"));
    }
    if word == "truncate"
        && args.iter().any(|arg| arg == "-s" || arg == "--size")
        && lower.contains('0')
    {
        return Some(Floor::new(Category::Delete, "`truncate` empties a file"));
    }
    if word == "git"
        && lower.contains(" clean")
        && args
            .iter()
            .any(|arg| arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
    {
        return Some(Floor::new(
            Category::Delete,
            "`git clean -f` deletes untracked files",
        ));
    }
    if matches!(word, "sqlite3" | "psql" | "mysql" | "mariadb")
        && (lower.contains("drop table") || lower.contains("drop database"))
    {
        return Some(Floor::new(
            Category::Delete,
            "the database command drops a table or database",
        ));
    }
    None
}

const MONEY_BINS: &[&str] = &[
    "stripe",
    "paypal",
    "venmo",
    "cashapp",
    "coinbase",
    "binance",
    "kraken",
    "wise",
    "zelle",
    "plaid",
    "wallet",
    "bitcoin-cli",
    "solana",
    "ethereum",
];
const MONEY_VERBS: &[&str] = &[
    "pay",
    "charge",
    "charges",
    "transfer",
    "transfers",
    "payout",
    "payouts",
    "refund",
    "refunds",
    "withdraw",
    "withdrawal",
    "buy",
    "sell",
    "trade",
    "order",
    "orders",
    "invoice",
    "subscribe",
    "checkout",
    "purchase",
    "topup",
    "top-up",
];
const MONEY_ENDPOINTS: &[&str] = &[
    "api.stripe.com",
    "api.paypal.com",
    "paypal.com/v1",
    "api.coinbase.com",
    "api.binance.com",
    "api.kraken.com",
    "api.wise.com",
    "api.venmo.com",
    "api.paystack.co",
    "api.lemonsqueezy.com",
    "api.paddle.com",
];

fn money(word: &str, args: &[String], lower: &str) -> Option<Floor> {
    if MONEY_BINS.contains(&word)
        && let Some(verb) = MONEY_VERBS
            .iter()
            .find(|verb| args.iter().any(|arg| arg == **verb))
    {
        return Some(Floor::new(
            Category::Money,
            format!("`{word} {verb}` moves money"),
        ));
    }
    if matches!(word, "curl" | "wget" | "http")
        && sends(args)
        && let Some(endpoint) = MONEY_ENDPOINTS
            .iter()
            .find(|endpoint| lower.contains(**endpoint))
    {
        return Some(Floor::new(
            Category::Money,
            format!("the request goes to {endpoint}"),
        ));
    }
    None
}

const CREDENTIAL_BINS: &[&str] = &[
    "passwd", "chpasswd", "usermod", "setfacl", "visudo", "chattr", "gpasswd",
];
const CREDENTIAL_PATHS: &[&str] = &[
    ".aws/credentials",
    ".netrc",
    ".pgpass",
    ".docker/config.json",
    ".config/gh/hosts.yml",
    "/etc/sudoers",
    "/etc/shadow",
    "/etc/passwd",
    ".ssh/id_rsa",
    ".ssh/id_ed25519",
    ".ssh/id_ecdsa",
];

fn credential(word: &str, args: &[String], lower: &str, root: &Path) -> Option<Floor> {
    if CREDENTIAL_BINS.contains(&word) {
        return Some(Floor::new(
            Category::Credential,
            format!("the command runs `{word}`"),
        ));
    }
    if word == "gpg" && lower.contains("--export-secret") {
        return Some(Floor::new(
            Category::Credential,
            "the command exports a secret key",
        ));
    }
    // A mode or owner change is a permission change when it grants something, or when it reaches
    // outside the workspace. `chmod +x ./script.sh` is a build step, not a grant.
    if matches!(word, "chmod" | "chown" | "chgrp") {
        let grants = args.iter().any(|arg| {
            let arg = arg.trim_matches('"').trim_matches('\'');
            arg.contains("+s")
                || arg.contains("o+w")
                || arg.contains("a+w")
                || arg.contains("g+w")
                || arg == "777"
                || arg == "0777"
                || arg == "666"
                || arg == "0666"
        });
        let outside = args
            .iter()
            .skip(1)
            .any(|arg| arg.starts_with('/') && !Path::new(arg).starts_with(root));
        if grants || outside {
            return Some(Floor::new(
                Category::Credential,
                format!("`{word}` changes who can use something"),
            ));
        }
    }
    let path = CREDENTIAL_PATHS
        .iter()
        .find(|path| lower.contains(**path))
        .copied()
        .or_else(|| {
            args.iter()
                .find(|arg| arg.trim_end_matches('"').ends_with(".env"))
                .map(String::as_str)
        });
    if let Some(path) = path {
        let writes =
            lower.contains('>') || matches!(word, "cp" | "mv" | "install" | "tee" | "ln" | "sed");
        if writes {
            return Some(Floor::new(
                Category::Credential,
                format!("the command writes {path}"),
            ));
        }
    }
    // A write command's arguments are paths too, and the protected prefixes (the live install, the
    // agent's state, keys) are not on the credential list because they are not secrets: they are the
    // files whose change would rewrite the thing doing the checking.
    if matches!(
        word,
        "cp" | "mv" | "install" | "tee" | "ln" | "sed" | "truncate" | "dd" | "touch"
    ) {
        for arg in args.iter().skip(1) {
            if arg.starts_with('-') {
                continue;
            }
            if let Some(floor) = written_path(arg, root) {
                return Some(floor);
            }
        }
    }
    None
}

const ACCESS_BINS: &[&str] = &["ssh-copy-id", "useradd", "adduser", "groupadd", "chsh"];
const ACCESS_ENDPOINTS: &[&str] = &["api.telegram.org"];

fn access(word: &str, lower: &str) -> Option<Floor> {
    if ACCESS_BINS.contains(&word) {
        return Some(Floor::new(
            Category::Access,
            format!("the command runs `{word}`"),
        ));
    }
    let grants = match word {
        "tailscale" => lower.contains(" up"),
        "gh" => {
            lower.contains("auth login")
                || lower.contains("ssh-key add")
                || lower.contains("gpg-key add")
        }
        "aws" => {
            lower.contains("iam create")
                || lower.contains("iam attach")
                || lower.contains("iam put")
                || lower.contains("iam add")
        }
        "gcloud" => lower.contains("add-iam-policy-binding"),
        "az" => lower.contains("role assignment create"),
        "ufw" => lower.contains(" allow"),
        "iptables" => lower.contains(" -a") || lower.contains(" -i"),
        "nft" => lower.contains(" add"),
        _ => false,
    };
    if grants {
        return Some(Floor::new(
            Category::Access,
            format!("`{word}` opens a new way in"),
        ));
    }
    if lower.contains("authorized_keys")
        && !matches!(
            word,
            "cat" | "less" | "head" | "tail" | "grep" | "rg" | "ls" | "stat" | "test"
        )
    {
        return Some(Floor::new(
            Category::Access,
            "the command writes a list of authorized keys",
        ));
    }
    if ACCESS_ENDPOINTS
        .iter()
        .any(|endpoint| lower.contains(*endpoint))
        && lower.contains("setwebhook")
    {
        return Some(Floor::new(
            Category::Access,
            "the command registers a webhook for a bot",
        ));
    }
    None
}

const OUTBOUND_BINS: &[&str] = &[
    "mail",
    "mailx",
    "sendmail",
    "mutt",
    "neomutt",
    "msmtp",
    "swaks",
    "himalaya",
    "twurl",
    "toot",
    "tweet",
    "telegram-send",
];
const OUTBOUND_ENDPOINTS: &[&str] = &[
    "api.telegram.org",
    "discord.com/api/webhooks",
    "hooks.slack.com",
    "slack.com/api/chat.postmessage",
    "api.twilio.com",
    "api.sendgrid.com",
    "api.mailgun.net",
    "api.postmarkapp.com",
    "api.twitter.com",
    "api.x.com",
    "bsky.social",
    "graph.facebook.com",
    "api.linkedin.com",
    "api.resend.com",
    "api.brevo.com",
    "api.mailchimp.com",
    "api.zoom.us",
];

fn outbound(word: &str, args: &[String], lower: &str) -> Option<Floor> {
    if OUTBOUND_BINS.contains(&word) {
        return Some(Floor::new(
            Category::Outbound,
            format!("the command runs `{word}`"),
        ));
    }
    if word == "signal-cli" && lower.contains(" send") {
        return Some(Floor::new(
            Category::Outbound,
            "the command sends a Signal message",
        ));
    }
    if matches!(word, "python" | "python3") && lower.contains("smtplib") {
        return Some(Floor::new(
            Category::Outbound,
            "the script sends mail through smtplib",
        ));
    }
    if matches!(word, "curl" | "wget" | "http") && lower.contains("sendmessage") {
        return Some(Floor::new(
            Category::Outbound,
            "the request calls a send-message endpoint",
        ));
    }
    if matches!(word, "curl" | "wget" | "http")
        && sends(args)
        && let Some(endpoint) = OUTBOUND_ENDPOINTS
            .iter()
            .find(|endpoint| lower.contains(**endpoint))
    {
        return Some(Floor::new(
            Category::Outbound,
            format!("the request goes to {endpoint}"),
        ));
    }
    None
}

/// Whether a curl-like command is a write rather than a read. A GET to an API is a query; a request
/// with a body or an explicit method is an action.
fn sends(args: &[String]) -> bool {
    let body = args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-d" | "--data"
                | "--data-raw"
                | "--data-binary"
                | "--data-urlencode"
                | "--form"
                | "--upload-file"
        )
    });
    let method = args.windows(2).any(|pair| {
        matches!(pair[0].as_str(), "-x" | "--request")
            && matches!(pair[1].as_str(), "post" | "put" | "patch" | "delete")
    }) || args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-xpost"
                | "-xput"
                | "-xpatch"
                | "-xdelete"
                | "--request=post"
                | "--request=put"
                | "--request=patch"
                | "--request=delete"
        )
    });
    body || method
}

/// A path a tool writes. Reads are not floored: looking at a credential is not changing one.
fn written_path(path: &str, root: &Path) -> Option<Floor> {
    let path = path.trim_matches(|c| c == '"' || c == '\'');
    let lower = path.to_ascii_lowercase();
    if lower.contains("authorized_keys") {
        return Some(Floor::new(
            Category::Access,
            "the file is a list of authorized keys",
        ));
    }
    if let Some(found) = CREDENTIAL_PATHS
        .iter()
        .find(|found| lower.contains(**found))
    {
        return Some(Floor::new(
            Category::Credential,
            format!("the file is {found}"),
        ));
    }
    if lower.trim_end_matches('"').ends_with(".env") {
        return Some(Floor::new(Category::Credential, "the file holds secrets"));
    }
    if let Some(reason) = protected(path, root) {
        return Some(Floor::new(Category::Credential, reason));
    }
    None
}

/// The files that are not secrets but are the thing doing the checking: the live install, the agent's own
/// state, and the keys.
///
/// The workspace wins: on this box the agent's root is `~/.hades/work`, and every write inside it would
/// otherwise be its own state. A path is protected when it is under one of those directories **and** outside
/// the root the tool was given, which is the same shape as the rule the `audit`/`init` guard already uses.
pub(crate) fn protected(path: &str, root: &Path) -> Option<String> {
    let full = normalize(&expand(path, root));
    let root = normalize(root);
    if full.starts_with(&root) {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let checks = [
        (".hermes", "the file is part of the live install"),
        (".hades", "the file is the agent's own state"),
        (".ssh", "the file is a key or its configuration"),
    ];
    for (directory, reason) in checks {
        if full.starts_with(home.join(directory)) {
            return Some(reason.to_string());
        }
    }
    None
}

/// `~` and `$HOME`, then the workspace for a relative path, then `.` and `..` folded away. Lexical on
/// purpose: the file may not exist yet, so `canonicalize` cannot answer, and a `..` that escapes the root is
/// exactly the case this check exists to see.
fn expand(path: &str, root: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let raw = if path == "~" {
        home.clone().unwrap_or_else(|| PathBuf::from(path))
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.clone()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path))
    } else if path == "$HOME" {
        home.clone().unwrap_or_else(|| PathBuf::from(path))
    } else if let Some(rest) = path.strip_prefix("$HOME/") {
        home.clone()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    };
    if raw.is_absolute() {
        raw
    } else {
        root.join(raw)
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The words a page uses when a click, a keystroke or a script is about to reach a person. A page is
/// opaque to this check; the control the model named is the only evidence there is, so the match is on
/// whole words and a false positive costs one retry, not a lost message.
const PAGE_WORDS: &[&str] = &[
    "send", "publish", "tweet", "toot", "post", "reply", "checkout", "transfer", "withdraw",
];

fn page(args: &Value) -> Option<Floor> {
    let action = args.get("action").and_then(Value::as_str)?;
    // A URL scheme is a read primitive: `file://` renders the local filesystem, and the other three run
    // code the harness never parsed. This is a floor beside the browser's own policy, not a replacement
    // for it: the policy is what refuses before `Page.navigate`, and this is what an audit reads.
    if matches!(action, "open" | "new_tab" | "new_context") {
        // Trimmed first: the policy trims and refuses ` file:///x`, so an untrimmed read here would let
        // the refusal happen with no line in the audit trail.
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let scheme = url
            .split_once(':')
            .map(|(scheme, _)| scheme.to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(scheme.as_str(), "file" | "data" | "javascript" | "vbscript") {
            return Some(Floor::new(
                Category::Access,
                format!("the URL scheme `{scheme}:` reads the local filesystem"),
            ));
        }
    }
    if !matches!(action, "click" | "type" | "eval") {
        return None;
    }
    let target = ["text", "selector", "script", "ref", "label"]
        .iter()
        .filter_map(|key| args.get(*key).and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if target.contains("pay now") || target.contains("buy now") || target.contains("place order") {
        return Some(Floor::new(
            Category::Money,
            "the page control buys something",
        ));
    }
    let word = PAGE_WORDS.iter().find(|word| mentions(&target, word))?;
    Some(Floor::new(
        Category::Outbound,
        format!("the page control says `{word}`"),
    ))
}

fn mentions(text: &str, word: &str) -> bool {
    if word.contains(' ') {
        return text.contains(word);
    }
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| token == word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_receipt_names_the_floor_as_a_heuristic() {
        let line = receipt();
        assert!(line.contains(HEURISTIC), "{line}");
        assert!(line.contains("not a sandbox"), "{line}");
    }

    #[test]
    fn strict_refuses_a_destructive_primitive() {
        let floor = strict("rm -rf build && cargo build").expect("a refusal");
        assert_eq!(floor.category(), Category::Delete);
        assert!(floor.reason().contains("rm -rf"), "{}", floor.reason());
        // Quoted text is not a command: printing the words must not trip the list.
        assert!(strict("echo \"rm -rf / is refused\"").is_none());
    }

    #[test]
    fn strict_records_what_it_cannot_see() {
        // The plan's own example. Two adjacent quoted spans join to `rm -rf /` when a shell runs it, but
        // this reads text after stripping quoted spans, so it does not match. The assertion is that the
        // miss is recorded as a known miss rather than papered over.
        let joined = "sh -c 'r''m -rf /'";
        assert!(
            strict(joined).is_none(),
            "strict claimed a catch it cannot make: {joined}"
        );
        assert!(STRICT_DENY_LIST.contains("deny list"));
    }

    #[test]
    fn unquoted_drops_both_kinds_of_quote() {
        assert_eq!(unquoted("a 'b c' d \"e\""), "a  d ");
    }

    fn command(text: &str) -> Option<Floor> {
        check("shell", &json!({ "command": text }), Path::new("/work"))
    }

    #[test]
    fn a_local_or_script_url_scheme_is_floored() {
        for scheme in ["file", "data", "javascript", "vbscript"] {
            let args = json!({ "action": "open", "url": format!("{scheme}:///etc/passwd") });
            let floor = page(&args).unwrap_or_else(|| panic!("{scheme}: was not floored"));
            assert_eq!(floor.category(), Category::Access, "{scheme}");
            assert!(floor.reason().contains(scheme), "{}", floor.reason());
        }
        assert!(page(&json!({ "action": "open", "url": "https://example.com" })).is_none());
    }

    #[test]
    fn each_permanent_delete_is_seen() {
        for text in [
            "rm -rf build",
            "sudo rm build/out.txt",
            "sudo -u root rm /var/log/x",
            "find . -name '*.tmp' -delete",
            "git clean -fdx",
            "sqlite3 app.db 'drop table notes'",
        ] {
            let floor = command(text).unwrap_or_else(|| panic!("{text} was not floored"));
            assert_eq!(floor.category(), Category::Delete, "{text}");
        }
    }

    #[test]
    fn a_build_step_is_not_a_permission_change() {
        assert!(command("chmod +x ./script.sh").is_none());
        assert!(command("chmod 755 ./bin").is_none());
        assert!(command("chown root ./file").is_none());
        assert!(command("chmod 777 ./file").is_some());
        assert!(command("chmod 755 /etc/nginx").is_some());
    }

    #[test]
    fn money_is_a_verb_near_a_money_command() {
        assert!(command("stripe balance").is_none());
        assert!(command("stripe charges create --amount 100").is_some());
        assert!(
            command("curl -s https://api.stripe.com/v1/charges -u key:").is_none(),
            "a GET is a query"
        );
        assert!(command("curl -X POST https://api.stripe.com/v1/charges -d amount=100").is_some());
    }

    #[test]
    fn a_message_to_a_person_is_seen() {
        for text in [
            "mail -s hello person@example.com",
            "swaks --to person@example.com",
            "curl -X POST https://api.telegram.org/bot1/sendMessage -d chat_id=1",
            "python3 -c 'import smtplib'",
        ] {
            let floor = command(text).unwrap_or_else(|| panic!("{text} was not floored"));
            assert_eq!(floor.category(), Category::Outbound, "{text}");
        }
    }

    #[test]
    fn new_access_is_seen() {
        for text in [
            "ssh-copy-id person@host",
            "useradd someone",
            "gh ssh-key add key.pub",
            "echo key >> ~/.ssh/authorized_keys",
            "ufw allow 22",
            "gcloud projects add-iam-policy-binding p --role=r",
        ] {
            let floor = command(text).unwrap_or_else(|| panic!("{text} was not floored"));
            assert_eq!(floor.category(), Category::Access, "{text}");
        }
    }

    #[test]
    fn a_credential_write_is_seen_and_a_read_is_not() {
        assert!(command("cat ~/.aws/credentials").is_none());
        assert!(command("echo key > ~/.aws/credentials").is_some());
        assert!(command("cp token.txt ~/.aws/credentials").is_some());
        let floor = check(
            "write_file",
            &json!({ "path": "/home/p/.ssh/authorized_keys", "content": "ssh-rsa AAAA" }),
            Path::new("/work"),
        )
        .expect("a key list");
        assert_eq!(floor.category(), Category::Access);
    }

    #[test]
    fn a_patch_to_a_key_list_is_the_same_action_as_a_write() {
        let floor = check(
            "patch",
            &json!({ "path": "/home/p/.ssh/authorized_keys", "edits": [] }),
            Path::new("/work"),
        )
        .expect("a key list");
        assert_eq!(floor.category(), Category::Access);
    }

    #[test]
    fn the_wrapper_is_not_the_command() {
        // The first word is a wrapper, so the check has to look past it.
        assert!(command("sudo -u root apt-get install curl").is_none());
        // A quoted example is still text; a refusal that costs one retry is the right error direction.
        assert!(command("echo 'rm -rf /'").is_none());
        assert!(command("grep -rn 'rm -rf' src").is_none());
    }

    #[test]
    fn an_ordinary_command_is_not_floored() {
        for text in [
            "ls -la",
            "cargo test --workspace",
            "git status",
            "touch made.txt",
            "curl -s https://example.com",
        ] {
            assert!(command(text).is_none(), "{text} was floored");
        }
    }

    #[test]
    fn a_page_control_that_reaches_a_person_is_seen() {
        let root = Path::new("/work");
        let click = check(
            "browser",
            &json!({ "action": "click", "text": "Send" }),
            root,
        )
        .expect("a send control");
        assert_eq!(click.category(), Category::Outbound);
        let by_ref = check(
            "browser",
            &json!({ "action": "click", "ref": "e3 send" }),
            root,
        )
        .expect("a ref that carries a send label");
        assert_eq!(by_ref.category(), Category::Outbound);
        let order = check(
            "browser",
            &json!({ "action": "click", "text": "Buy now" }),
            root,
        )
        .expect("a checkout");
        assert_eq!(order.category(), Category::Money);
        assert!(
            check(
                "browser",
                &json!({ "action": "open", "url": "https://x.com" }),
                root
            )
            .is_none()
        );
        assert!(
            check(
                "browser",
                &json!({ "action": "click", "selector": "#search" }),
                root
            )
            .is_none(),
            "searching is not sending"
        );
        assert!(
            check(
                "browser",
                &json!({ "action": "click", "selector": "#postcode" }),
                root
            )
            .is_none(),
            "a word boundary, not a substring"
        );
    }

    #[test]
    fn the_refusal_names_the_category_and_the_person() {
        let floor = command("rm -rf build").expect("a delete");
        let said = floor.refusal();
        assert!(said.contains("permanent delete"), "{said}");
        assert!(said.contains("a person decides it"), "{said}");
        assert_eq!(floor.action(), "permanently delete");
    }

    #[test]
    fn a_shell_redirection_into_the_live_install_is_a_credential_change() {
        let home = std::env::var("HOME").expect("a home");
        for text in [
            format!("echo key > {home}/.hermes/config.yaml"),
            format!("printf y >> {home}/.ssh/authorized_keys"),
            format!("tee -a {home}/.hades/.env"),
            format!("cp key.txt {home}/.hermes/.env"),
        ] {
            let floor = command(&text).unwrap_or_else(|| panic!("{text} was not floored"));
            assert!(
                matches!(floor.category(), Category::Credential | Category::Access),
                "{text}: {floor:?}"
            );
        }
    }

    #[test]
    fn an_ordinary_redirection_and_a_descriptor_are_not_writes_to_protected_files() {
        for text in [
            "echo hi > out.txt",
            "printf hi >> out.txt",
            "cargo build 2>&1",
            "sh -c 'echo a > b' > out.txt",
            "echo done > /dev/null",
        ] {
            assert!(command(text).is_none(), "{text} was floored");
        }
    }

    #[test]
    fn a_relative_parent_that_escapes_the_workspace_is_resolved_before_the_check() {
        let home = PathBuf::from(std::env::var("HOME").expect("a home"));
        let root = home.join(".hades/work");
        let floor = check(
            "shell",
            &json!({ "command": "echo x > ../../.hermes/config.yaml" }),
            &root,
        )
        .expect("an escape");
        assert_eq!(floor.category(), Category::Credential);
        assert!(
            check("shell", &json!({ "command": "echo x > out.txt" }), &root).is_none(),
            "the workspace is writable"
        );
    }

    #[test]
    fn a_write_to_the_live_install_through_a_file_tool_is_also_floored() {
        let home = std::env::var("HOME").expect("a home");
        let floor = check(
            "write_file",
            &json!({ "path": format!("{home}/.hermes/config.yaml"), "content": "x" }),
            Path::new("/work"),
        )
        .expect("the live config");
        assert_eq!(floor.category(), Category::Credential);
    }
}
