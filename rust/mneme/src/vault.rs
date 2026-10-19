//! The vault itself: the operations, and the value that holds a store, a clock and a root.

use std::path::{Component, Path, PathBuf};

use crate::INBOX_EXPIRY_DAYS;
use crate::area::Area;
use crate::charter::{self, CHARTER_NAME, Charter};
use crate::date::Date;
use crate::fault::Fault;
use crate::fence::fenced;
use crate::frontmatter::Frontmatter;
use crate::index::{self, INDEX_NAME};
use crate::note::{Note, slugify};
use crate::permit::{Holder, WritePermit};
use crate::seams::{Clock, NoteStore};

/// What is known about a note when it is filed. Empty means the vault leaves that key as it found it,
/// except for the topics, which are the whole reason the move is allowed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filing {
    pub topics: Vec<String>,
    pub projects: Vec<String>,
    pub summary: Option<String>,
    pub source_url: Option<String>,
    pub provenance: Option<String>,
}

/// A vault: five areas under one root, over an injected store and clock.
pub struct Vault<S: NoteStore, C: Clock> {
    root: PathBuf,
    store: S,
    clock: C,
}

impl<S: NoteStore, C: Clock> Vault<S, C> {
    pub fn new(root: impl Into<PathBuf>, store: S, clock: C) -> Self {
        Vault {
            root: root.into(),
            store,
            clock,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Write one note out in the form a person reads. Default no-op for a store that is the disk.
    pub fn export(&self, path: &Path) -> Result<(), Fault> {
        self.store.export(path)
    }

    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// The folder of one area.
    pub fn area_path(&self, area: Area) -> PathBuf {
        self.root.join(area.folder())
    }

    /// Hand out the permit to write memory.
    ///
    /// The only constructor of a [`WritePermit`] in the program, and it takes the holder, so every move
    /// in the codebase names who is making it and there is one line to read when the question is who may
    /// write. In the product the curator is the only caller.
    pub fn permit(&self, holder: Holder) -> WritePermit {
        WritePermit::new(holder)
    }

    /// Create the five areas and an initial index. Idempotent, so running it twice is not an error.
    /// The onboarding answers, when any of them is given, become the charter.
    pub fn init(&self, answers: Option<(&str, &str, &[String])>) -> Result<PathBuf, Fault> {
        for area in Area::ALL {
            self.store.make_dirs(&self.area_path(area))?;
        }
        if let Some((name, goal, topics)) = answers
            && (!name.is_empty()
                || !goal.is_empty()
                || topics.iter().any(|topic| !topic.trim().is_empty()))
        {
            self.write_charter(name, goal, topics)?;
        }
        self.write_index(None, &[])?;
        Ok(self.root.clone())
    }

    /// The onboarding answers, written where a session will find them.
    pub fn write_charter(
        &self,
        agent_name: &str,
        agent_goal: &str,
        topics: &[String],
    ) -> Result<PathBuf, Fault> {
        let path = self.root.join(CHARTER_NAME);
        self.store
            .write(&path, &charter::render(agent_name, agent_goal, topics))?;
        Ok(path)
    }

    /// The two onboarding answers, empty when the charter was never written.
    pub fn read_charter(&self) -> Result<Charter, Fault> {
        let path = self.root.join(CHARTER_NAME);
        if !self.store.exists(&path) {
            return Ok(Charter::default());
        }
        Ok(charter::parse(&self.store.read(&path)?))
    }

    /// Write a capture into the Inbox. The only way into the vault.
    pub fn capture(
        &self,
        text: &str,
        title: Option<&str>,
        source_url: Option<&str>,
    ) -> Result<PathBuf, Fault> {
        let inbox = self.area_path(Area::Inbox);
        self.store.make_dirs(&inbox)?;
        let stamp = self.clock.today().to_iso();
        let stripped = text.trim();
        let first_line = stripped.lines().next().unwrap_or("");
        let chosen = match title {
            Some(title) if !title.is_empty() => title,
            _ if !first_line.is_empty() => first_line,
            _ => "capture",
        };
        let heading = chosen.trim();

        let mut frontmatter = Frontmatter {
            title: Some(heading.chars().take(80).collect()),
            captured: Some(stamp.clone()),
            ..Frontmatter::default()
        };
        if let Some(url) = source_url.filter(|url| !url.is_empty()) {
            frontmatter.source_url = Some(url.to_string());
        }

        let path = self.unique(inbox.join(format!("{stamp}-{}.md", slugify(heading, 6))))?;
        let written = self.write_note(&path, &frontmatter, text)?;
        self.write_index(None, &[])?;
        Ok(written)
    }

    /// Move a note out of the Inbox and into its home.
    ///
    /// Takes a permit, which is the one-writer rule as a type: a move cannot happen without a value only
    /// [`Vault::permit`] can build. The parameter is a witness, so the type is what matters and not the
    /// value; the holder it carries is read by the caller that stamps the note, one layer up, where the
    /// audit trail is written. The capture path stays permit-free because the Inbox is a queue rather
    /// than memory: entering an area is where a note becomes memory.
    ///
    /// Refuses to file an orphan: without at least one topic the note stays exactly where it is,
    /// because a durable note in the graph has a home by construction, and a move that has to be undone
    /// is worse than a capture that waits.
    pub fn file(
        &self,
        _permit: &WritePermit,
        note: &Path,
        area: Area,
        filing: &Filing,
    ) -> Result<PathBuf, Fault> {
        let topics = cleaned(&filing.topics);
        if topics.is_empty() {
            return Err(Fault::OrphanNote {
                path: note.display().to_string(),
            });
        }
        let path = if note.is_absolute() {
            note.to_path_buf()
        } else {
            self.root.join(note)
        };
        if !self.store.exists(&path) {
            return Err(Fault::MissingNote {
                path: path.display().to_string(),
            });
        }

        let (mut frontmatter, body) = self.parts(&path)?;
        frontmatter.topics = topics;
        if !filing.projects.is_empty() {
            frontmatter.projects = cleaned(&filing.projects);
        }
        if let Some(summary) = filing.summary.as_deref().filter(|text| !text.is_empty()) {
            frontmatter.summary = Some(summary.to_string());
        }
        if let Some(url) = filing.source_url.as_deref().filter(|text| !text.is_empty()) {
            frontmatter.source_url = Some(url.to_string());
        }
        if let Some(provenance) = filing.provenance.as_deref().filter(|text| !text.is_empty()) {
            frontmatter.provenance = Some(provenance.to_string());
        }
        if frontmatter.status.is_none() {
            frontmatter.status = Some("durable".to_string());
        }

        let candidate = self
            .area_path(area)
            .join(path.file_name().unwrap_or_default());
        let destination = if same_path(&candidate, &path) {
            candidate
        } else {
            self.unique(candidate)?
        };
        let written = self.write_note(&destination, &frontmatter, &body)?;
        if !same_path(&written, &path) {
            self.store.remove(&path)?;
        }
        self.write_index(None, &[])?;
        Ok(written)
    }

    /// Read one note: the frontmatter parsed, the body whole.
    pub fn read_note(&self, path: &Path) -> Result<Note, Fault> {
        let (frontmatter, body) = self.parts(path)?;
        Ok(Note::new(path, frontmatter, body))
    }

    /// Write a note: frontmatter first, then the body trimmed, with one blank line between.
    pub fn write_note(
        &self,
        path: &Path,
        frontmatter: &Frontmatter,
        body: &str,
    ) -> Result<PathBuf, Fault> {
        if let Some(parent) = path.parent() {
            self.store.make_dirs(parent)?;
        }
        let text = format!("{}\n{}\n", frontmatter.render(), body.trim());
        self.store.write(path, &text)?;
        Ok(path.to_path_buf())
    }

    /// Every note, in area order and then by name, skipping fenced paths. Read one folder at a time,
    /// because the Inbox alone is often what a caller wants.
    pub fn iter_notes(&self, area: Option<Area>, fences: &[String]) -> Result<Vec<Note>, Fault> {
        let folders: Vec<PathBuf> = match area {
            Some(area) => vec![self.area_path(area)],
            None => Area::ALL.iter().map(|area| self.area_path(*area)).collect(),
        };
        let mut notes: Vec<Note> = Vec::new();
        for folder in folders {
            for path in self.store.list(&folder)? {
                let relative = path.strip_prefix(&self.root).unwrap_or(&path);
                if fenced(relative, fences) {
                    continue;
                }
                notes.push(self.read_note(&path)?);
            }
        }
        Ok(notes)
    }

    /// Find notes by topic or project. Both filters, when given, must match. With neither, nothing is
    /// returned: the vault never dumps itself, and a caller that wants everything walks it.
    pub fn recall(&self, topic: Option<&str>, project: Option<&str>) -> Result<Vec<Note>, Fault> {
        let mut found: Vec<Note> = Vec::new();
        for note in self.iter_notes(None, &[])? {
            if let Some(topic) = topic
                && !note.topics().iter().any(|held| held == topic)
            {
                continue;
            }
            if let Some(project) = project
                && !note.projects().iter().any(|held| held == project)
            {
                continue;
            }
            if topic.is_some() || project.is_some() {
                found.push(note);
            }
        }
        Ok(found)
    }

    /// Inbox candidates that have sat past their welcome. Reported, never dropped: the Inbox is a
    /// human review queue and only its owner empties it.
    pub fn stale(&self, days: i64) -> Result<Vec<Note>, Fault> {
        let cutoff = self.clock.today().minus_days(days);
        let mut out: Vec<Note> = Vec::new();
        for note in self.iter_notes(Some(Area::Inbox), &[])? {
            let stamp = note.frontmatter.captured.clone().unwrap_or_default();
            let stamp = if stamp.is_empty() {
                date_prefix(&note.path)
            } else {
                stamp
            };
            let Some(captured) = Date::parse_iso(&stamp) else {
                continue;
            };
            if captured < cutoff {
                out.push(note);
            }
        }
        Ok(out)
    }

    /// The index as text: built from the notes themselves, so it cannot disagree with them.
    pub fn build_index(&self, generated: Option<&str>, fences: &[String]) -> Result<String, Fault> {
        let notes = self.iter_notes(None, fences)?;
        let inbox = self.iter_notes(Some(Area::Inbox), fences)?;
        let stale = self.stale(INBOX_EXPIRY_DAYS)?;
        let stamp = generated
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.clock.today().to_iso());
        Ok(index::render(&stamp, &self.root, &notes, &inbox, &stale))
    }

    /// Write the index. Called after every write, so the copy on disk is never stale.
    pub fn write_index(
        &self,
        generated: Option<&str>,
        fences: &[String],
    ) -> Result<PathBuf, Fault> {
        let path = self.root.join(INDEX_NAME);
        if let Some(parent) = path.parent() {
            self.store.make_dirs(parent)?;
        }
        let text = self.build_index(generated, fences)?;
        self.store.write(&path, &text)?;
        Ok(path)
    }

    /// The frontmatter and the body of a note already on disk.
    fn parts(&self, path: &Path) -> Result<(Frontmatter, String), Fault> {
        let text = self.store.read(path)?;
        let (frontmatter, body) = Frontmatter::parse(&text);
        Ok((frontmatter, body.to_string()))
    }

    /// A name next to one that is taken: `x.md`, then `x-2.md`, and so on. A capture never overwrites a
    /// capture, which is why the Inbox can be filled by a cron job without losing anything.
    fn unique(&self, path: PathBuf) -> Result<PathBuf, Fault> {
        if !self.store.exists(&path) {
            return Ok(path);
        }
        let parent = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let stem = path
            .file_stem()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let suffix = path
            .extension()
            .map(|extension| format!(".{}", extension.to_string_lossy()))
            .unwrap_or_default();
        for index in 2..1000 {
            let candidate = parent.join(format!("{stem}-{index}{suffix}"));
            if !self.store.exists(&candidate) {
                return Ok(candidate);
            }
        }
        Err(Fault::NoFreeName {
            path: path.display().to_string(),
        })
    }
}

/// The topics or projects worth keeping: trimmed, and with the empty ones gone, so a stray comma never
/// becomes a topic nobody can find.
fn cleaned(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

/// The day off the front of a file name, when the note itself says nothing about when it was captured.
fn date_prefix(path: &Path) -> String {
    let stem = crate::note::stem(path);
    let bytes = stem.as_bytes();
    let shaped = bytes.len() > 11
        && bytes[10] == b'-'
        && bytes[..10]
            .iter()
            .enumerate()
            .all(|(index, byte)| match index {
                4 | 7 => *byte == b'-',
                _ => byte.is_ascii_digit(),
            });
    if shaped {
        stem[..10].to_string()
    } else {
        String::new()
    }
}

/// Two paths naming the same note.
///
/// Python resolves, symlinks included, and reads the working directory while it does. The vault
/// compares the two paths lexically instead, so a relative note is read against the vault root rather
/// than against wherever the process happens to be standing.
fn same_path(left: &Path, right: &Path) -> bool {
    normalize(left) == normalize(right)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
