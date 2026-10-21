//! The vault itself: the operations, and the value that holds a store, a clock and a root.

use std::collections::BTreeSet;
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
use crate::refs::{self, ResearchRef};
use crate::seams::{Clock, NoteStore};

/// What is known about a note when it is filed. Empty means the vault leaves that key as it found it,
/// except for the topics, which are the whole reason the move is allowed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filing {
    pub topics: Vec<String>,
    pub projects: Vec<String>,
    pub from_project: Option<String>,
    pub summary: Option<String>,
    pub source_url: Option<String>,
    pub provenance: Option<String>,
}

/// A vault: six areas under one root, over an injected store and clock.
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

    /// Create the six areas, the taxonomy, the seeded markers and an initial index. Idempotent, so
    /// running it twice is not an error, and a vault that already has a file keeps it.
    ///
    /// The markers are marking posts, not memory: `status: marker` is exempt from the structural
    /// rules, so an empty clone is navigable without pretending it knows anything.
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
        self.write_taxonomy()?;
        self.seed_markers()?;
        self.write_index(None, &[])?;
        Ok(self.root.clone())
    }

    /// The fixed 26 categories, written at the root so a fresh agent can read the doors before it
    /// files anything. Regenerated on every init; it is a map, not a note.
    pub fn write_taxonomy(&self) -> Result<PathBuf, Fault> {
        let path = self.root.join("TAXONOMY.md");
        self.store.write(&path, &crate::topic::taxonomy())?;
        Ok(path)
    }

    /// One marker per area. A marker is a note with `status: marker`; it is deliberately exempt from
    /// the topic/category rules because it is signage, not knowledge.
    fn seed_markers(&self) -> Result<(), Fault> {
        for area in Area::ALL {
            let path = self.area_path(area).join("START HERE.md");
            if self.store.exists(&path) {
                continue;
            }
            let frontmatter = Frontmatter {
                title: Some(format!("{}: start here", area.key())),
                summary: Some(area.purpose().to_string()),
                status: Some("marker".to_string()),
                provenance: Some("seeded".to_string()),
                ..Frontmatter::default()
            };
            let body = format!(
                "# {}\n\n{}\n\nThis note is a marking post. Replace it with real work, or leave it; \
                 either way it is not a claim about the world.\n",
                area.key(),
                area.purpose()
            );
            let written = self.write_note(&path, &frontmatter, &body)?;
            self.store.export(&written)?;
        }
        Ok(())
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
        // The tree is the export, and the Inbox is the queue a person reads. A row with no file
        // would leave `INDEX.md` counting candidates that are nowhere to be opened - the drift
        // SQLite introduced when it became the record, because `write_note` reaches the store and
        // not the folder. So the export is part of the capture, as it is part of a filing.
        self.store.export(&written)?;
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
        frontmatter.topics = topics.clone();
        if frontmatter.topic.is_none() && area.needs_topic() {
            frontmatter.topic = topics.first().cloned();
        }
        if area == Area::Products && frontmatter.from_project.is_none() {
            frontmatter.from_project = filing.from_project.clone();
        }
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
        // Filing is a write, so the tree gets it now, exactly as `record` does: a note a curator
        // hands back is a file a person can open.
        self.store.export(&written)?;
        if !same_path(&written, &path) {
            self.store.remove(&path)?;
            // The old markdown is an export too. Without this the Inbox keeps the copy the record
            // just moved away, and the folder contradicts the index.
            self.store.remove_export(&path)?;
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
        // The structural rules live here as well as in the SQLite store: a file-backed vault has to
        // refuse the same note, or the rules would only exist on one of the two paths.
        let located = crate::sqlite::note_path(&self.root, path);
        let frontmatter = match &located {
            Some((area, slug)) => crate::sqlite::normalize(path, *area, slug, frontmatter.clone())?,
            None => frontmatter.clone(),
        };
        // A project names the research it stands on, and naming a note that does not exist is a
        // declaration with nothing behind it. Import checks the same rule against the source tree.
        if let Some((Area::Projects, _)) = &located
            && frontmatter.status.as_deref() == Some("durable")
        {
            let resolved = self.resolve_research_all(&frontmatter.research)?;
            if let Some((reference, outcome)) = resolved
                .iter()
                .find(|(_, outcome)| !matches!(outcome, ResearchRef::Found(_)))
            {
                return Err(Fault::InvalidNote {
                    path: path.display().to_string(),
                    detail: outcome.reason(reference).unwrap_or_else(|| {
                        format!("`{reference}` does not name an existing research note")
                    }),
                });
            }
        }
        // A product names the project it came from. A name that resolves to nothing would make the
        // release history a claim rather than a link, so it is refused here as well as in the audit.
        if let Some((Area::Products, _)) = &located
            && let Some(project) = frontmatter
                .from_project
                .as_deref()
                .filter(|project| !project.trim().is_empty())
            && !matches!(self.resolve_project(project)?, ResearchRef::Found(_))
        {
            return Err(Fault::InvalidNote {
                path: path.display().to_string(),
                detail: format!("`{project}` does not name an existing project"),
            });
        }
        let text = format!("{}\n{}\n", frontmatter.render(), body.trim());
        self.store.write(path, &text)?;
        Ok(path.to_path_buf())
    }

    /// The canonical slugs below one area, without `.md` and without a trailing `/index`.
    fn slugs_in(&self, area: Area) -> Result<BTreeSet<String>, Fault> {
        let mut slugs = BTreeSet::new();
        for path in self.store.walk(&self.area_path(area))? {
            if let Some((found, slug)) = crate::sqlite::note_path(&self.root, &path)
                && found == area
            {
                let slug = slug.strip_suffix("/index").unwrap_or(&slug);
                slugs.insert(slug.to_string());
            }
        }
        Ok(slugs)
    }

    /// Resolve one research reference. See [`crate::refs`] for the accepted forms.
    pub fn resolve_research(&self, reference: &str) -> Result<ResearchRef, Fault> {
        let slugs = self.slugs_in(Area::Research)?;
        Ok(refs::resolve(&slugs, reference, Area::Research.folder()))
    }

    /// Resolve every reference in one walk, keeping the input order and the outcome per reference.
    pub fn resolve_research_all(
        &self,
        references: &[String],
    ) -> Result<Vec<(String, ResearchRef)>, Fault> {
        let slugs = self.slugs_in(Area::Research)?;
        Ok(references
            .iter()
            .map(|reference| {
                (
                    reference.clone(),
                    refs::resolve(&slugs, reference, Area::Research.folder()),
                )
            })
            .collect())
    }

    /// Resolve a project reference to the project's slug below `30 Projects`.
    pub fn resolve_project(&self, reference: &str) -> Result<ResearchRef, Fault> {
        let slugs = self.slugs_in(Area::Projects)?;
        Ok(refs::resolve(&slugs, reference, Area::Projects.folder()))
    }

    /// Whether a topic note exists. Accepts a full topic slug, `.../index.md`, or a path.
    pub fn topic_exists(&self, topic: &str) -> Result<bool, Fault> {
        let topic = refs::clean(topic, Area::Topics.folder());
        let folder = self.area_path(Area::Topics);
        Ok(self.store.exists(&folder.join(&topic).join("index.md"))
            || self.store.exists(&folder.join(format!("{topic}.md"))))
    }

    /// The path of an existing project, preferring the `<name>/index.md` convention.
    fn project_path(&self, project: &str) -> PathBuf {
        let folder = self.area_path(Area::Projects);
        let index = folder.join(project).join("index.md");
        if self.store.exists(&index) {
            index
        } else {
            folder.join(format!("{project}.md"))
        }
    }

    /// Write, export and re-index one creation. Every creation verb ends here so the tree and the
    /// record cannot disagree about a note that was just made.
    fn created(&self, path: PathBuf, frontmatter: &Frontmatter) -> Result<PathBuf, Fault> {
        let path = self.write_note(&path, frontmatter, "")?;
        self.store.export(&path)?;
        self.write_index(None, &[])?;
        Ok(path)
    }

    fn clash(&self, path: &Path) -> Fault {
        Fault::InvalidNote {
            path: path.display().to_string(),
            detail: "a note already exists at this path".to_string(),
        }
    }

    /// Create a topic under one of the 26 categories, optionally beneath an existing topic.
    pub fn create_topic(
        &self,
        category: &str,
        name: &str,
        parent: Option<&str>,
        summary: &str,
        provenance: &str,
    ) -> Result<PathBuf, Fault> {
        let Some(category) = crate::topic::Category::parse(category) else {
            return Err(Fault::InvalidNote {
                path: format!("{}", self.area_path(Area::Topics).display()),
                detail: format!(
                    "`{category}` is not one of the 26 categories; read TAXONOMY.md for the doors"
                ),
            });
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(Fault::InvalidNote {
                path: self.area_path(Area::Topics).display().to_string(),
                detail: "a topic needs a name".to_string(),
            });
        }
        let slug = slugify(name, 12);
        let topic = match parent {
            Some(parent) => {
                let parent = refs::clean(parent, Area::Topics.folder());
                if !parent.starts_with(&format!("{}/", category.key())) {
                    return Err(Fault::InvalidNote {
                        path: self.area_path(Area::Topics).display().to_string(),
                        detail: format!(
                            "parent `{parent}` is not under `{}`; a subtopic stays in its category",
                            category.key()
                        ),
                    });
                }
                if !self.topic_exists(&parent)? {
                    return Err(Fault::InvalidNote {
                        path: self.area_path(Area::Topics).display().to_string(),
                        detail: format!("parent topic `{parent}` does not exist"),
                    });
                }
                format!("{parent}/{slug}")
            }
            None => format!("{}/{}", category.key(), slug),
        };
        let path = self.area_path(Area::Topics).join(&topic).join("index.md");
        if self.store.exists(&path) {
            return Err(self.clash(&path));
        }
        let frontmatter = Frontmatter {
            title: Some(name.to_string()),
            summary: Some(summary.trim().to_string()),
            status: Some("durable".to_string()),
            category: Some(category.key().to_string()),
            topic: Some(topic.clone()),
            parent: parent.map(|parent| refs::clean(parent, Area::Topics.folder())),
            topics: vec![topic],
            provenance: Some(provenance.to_string()),
            ..Frontmatter::default()
        };
        self.created(path, &frontmatter)
    }

    /// Create a research note under an existing topic.
    pub fn create_research(
        &self,
        topic: &str,
        title: &str,
        slug: Option<&str>,
        summary: &str,
        provenance: &str,
        source_url: Option<&str>,
    ) -> Result<PathBuf, Fault> {
        let topic = refs::clean(topic, Area::Topics.folder());
        if !self.topic_exists(&topic)? {
            return Err(Fault::InvalidNote {
                path: self.area_path(Area::Research).display().to_string(),
                detail: format!(
                    "topic `{topic}` does not exist; create it first with `hades topic`"
                ),
            });
        }
        let slug = slug
            .map(str::to_string)
            .filter(|slug| !slug.trim().is_empty())
            .unwrap_or_else(|| slugify(title, 12));
        if slug.is_empty() {
            return Err(Fault::InvalidNote {
                path: self.area_path(Area::Research).display().to_string(),
                detail: "a research note needs a title or a slug".to_string(),
            });
        }
        let path = self
            .area_path(Area::Research)
            .join(&topic)
            .join(format!("{slug}.md"));
        if self.store.exists(&path) {
            return Err(self.clash(&path));
        }
        let frontmatter = Frontmatter {
            title: Some(title.trim().to_string()),
            summary: Some(summary.trim().to_string()),
            status: Some("durable".to_string()),
            topic: Some(topic.clone()),
            topics: vec![topic],
            source_url: source_url.map(str::to_string),
            provenance: Some(provenance.to_string()),
            ..Frontmatter::default()
        };
        self.created(path, &frontmatter)
    }

    /// Create a project hub that stands on at least one existing research note.
    pub fn create_project(
        &self,
        name: &str,
        topic: &str,
        research: &[String],
        summary: &str,
        provenance: &str,
    ) -> Result<PathBuf, Fault> {
        let topic = refs::clean(topic, Area::Topics.folder());
        if !self.topic_exists(&topic)? {
            return Err(Fault::InvalidNote {
                path: self.area_path(Area::Projects).display().to_string(),
                detail: format!(
                    "topic `{topic}` does not exist; create it first with `hades topic`"
                ),
            });
        }
        let resolved = self.resolve_research_all(research)?;
        let mut canonical = Vec::new();
        for (reference, outcome) in &resolved {
            match outcome {
                ResearchRef::Found(slug) => canonical.push(slug.clone()),
                _ => {
                    return Err(Fault::InvalidNote {
                        path: self.area_path(Area::Projects).display().to_string(),
                        detail: outcome.reason(reference).unwrap_or_else(|| {
                            format!("`{reference}` does not name an existing research note")
                        }),
                    });
                }
            }
        }
        if canonical.is_empty() {
            return Err(Fault::InvalidNote {
                path: self.area_path(Area::Projects).display().to_string(),
                detail: "a project names at least one research note".to_string(),
            });
        }
        let slug = slugify(name, 12);
        let path = self.area_path(Area::Projects).join(&slug).join("index.md");
        if self.store.exists(&path) {
            return Err(self.clash(&path));
        }
        let frontmatter = Frontmatter {
            title: Some(name.trim().to_string()),
            summary: Some(summary.trim().to_string()),
            status: Some("durable".to_string()),
            topic: Some(topic.clone()),
            topics: vec![topic],
            projects: vec![slug],
            research: canonical,
            provenance: Some(provenance.to_string()),
            ..Frontmatter::default()
        };
        self.created(path, &frontmatter)
    }

    /// Freeze a project as a product. The topic is inherited from the project when not given.
    pub fn create_product(
        &self,
        name: &str,
        from_project: &str,
        topic: Option<&str>,
        status: &str,
        summary: &str,
        provenance: &str,
    ) -> Result<PathBuf, Fault> {
        let project = match self.resolve_project(from_project)? {
            ResearchRef::Found(project) => project,
            ResearchRef::Missing => {
                return Err(Fault::InvalidNote {
                    path: self.area_path(Area::Products).display().to_string(),
                    detail: format!("`{from_project}` does not name an existing project"),
                });
            }
            ResearchRef::Ambiguous(candidates) => {
                return Err(Fault::InvalidNote {
                    path: self.area_path(Area::Products).display().to_string(),
                    detail: format!(
                        "`{from_project}` matches {} projects: {}",
                        candidates.len(),
                        candidates.join(", ")
                    ),
                });
            }
        };
        let project_note = self.read_note(&self.project_path(&project))?;
        let topic = match topic {
            Some(topic) => refs::clean(topic, Area::Topics.folder()),
            None => {
                let topic = project_note.topic().to_string();
                if topic.is_empty() {
                    return Err(Fault::InvalidNote {
                        path: self.area_path(Area::Products).display().to_string(),
                        detail: format!("project `{project}` has no topic to inherit"),
                    });
                }
                topic
            }
        };
        if !self.topic_exists(&topic)? {
            return Err(Fault::InvalidNote {
                path: self.area_path(Area::Products).display().to_string(),
                detail: format!("topic `{topic}` does not exist"),
            });
        }
        let slug = slugify(name, 12);
        let path = self.area_path(Area::Products).join(&slug).join("index.md");
        if self.store.exists(&path) {
            return Err(self.clash(&path));
        }
        let frontmatter = Frontmatter {
            title: Some(name.trim().to_string()),
            summary: Some(summary.trim().to_string()),
            status: Some(status.trim().to_string()),
            topic: Some(topic.clone()),
            topics: vec![topic],
            projects: vec![project.clone()],
            from_project: Some(project),
            provenance: Some(provenance.to_string()),
            ..Frontmatter::default()
        };
        self.created(path, &frontmatter)
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
            for path in self.store.walk(&folder)? {
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
            // Recall is knowledge: research, projects and products. The Inbox is a queue, the Outbox is
            // a channel nothing reads yet, and a topic is a door, not a finding.
            let area = crate::sqlite::note_path(&self.root, &note.path).map(|(area, _)| area);
            if !matches!(area, Some(Area::Research | Area::Projects | Area::Products)) {
                continue;
            }
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
