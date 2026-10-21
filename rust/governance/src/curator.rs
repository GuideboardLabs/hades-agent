//! The curator: the only thing that writes durable memory.

use std::path::{Path, PathBuf};

use mneme::{
    Area, Clock, Field, Filing, Frontmatter, Holder, NoteStore, Vault, fenced, relative_posix,
};

use crate::problem::{Problem, ProblemKind};
use crate::refusal::Refusal;
use crate::report::AuditReport;
use crate::timestamp::Timestamp;
use crate::{DURABLE_AREAS, INBOX_CAP, STALE_DAYS};

/// What a caller is told about a note it is asked to file.
///
/// Provenance is required here where the vault makes it optional: the vault's job is to put a note where
/// it is told, and the curator's job is to refuse a note with no source. The summary is required for the
/// same reason, because the index is built from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Curation {
    pub topics: Vec<String>,
    pub projects: Vec<String>,
    /// The research a project stands on. Empty for a research note.
    pub research: Vec<String>,
    /// The project a product came from. Empty except for products.
    pub from_project: Option<String>,
    pub summary: String,
    pub provenance: String,
}

/// The vault's writer of record.
///
/// It holds a vault rather than a root, so every read and write goes through one store and one day, and
/// it takes a timestamp of its own because the moment of curation is the one thing the vault does not
/// know.
pub struct Curator<S: NoteStore, C: Clock, T: Timestamp> {
    vault: Vault<S, C>,
    timestamp: T,
    fences: Vec<String>,
}

impl<S: NoteStore, C: Clock, T: Timestamp> Curator<S, C, T> {
    /// A curator over a vault, fenced with [`crate::FENCE_DEFAULT`].
    pub fn new(vault: Vault<S, C>, timestamp: T) -> Self {
        let fences = crate::FENCE_DEFAULT
            .iter()
            .map(|fence| fence.to_string())
            .collect();
        Curator::with_fences(vault, timestamp, fences)
    }

    pub fn with_fences(vault: Vault<S, C>, timestamp: T, fences: Vec<String>) -> Self {
        Curator {
            vault,
            timestamp,
            fences,
        }
    }

    pub fn vault(&self) -> &Vault<S, C> {
        &self.vault
    }

    pub fn timestamp(&self) -> &T {
        &self.timestamp
    }

    pub fn fences(&self) -> &[String] {
        &self.fences
    }

    // ---------------- fences ----------------

    pub fn is_fenced(&self, path: &Path) -> bool {
        fenced(path, &self.fences)
    }

    fn refuse_fenced(&self, path: &Path) -> Result<(), Refusal> {
        if self.is_fenced(path) {
            return Err(Refusal::Fenced {
                path: path.display().to_string(),
            });
        }
        Ok(())
    }

    // ---------------- writing ----------------

    /// Into the Inbox. The only way in.
    pub fn capture(
        &self,
        text: &str,
        title: Option<&str>,
        provenance: Option<&str>,
    ) -> Result<PathBuf, Refusal> {
        Ok(self.vault.capture(text, title, provenance)?)
    }

    /// Move a note out of the Inbox, refusing anything that would become an orphan or a dead end.
    ///
    /// The order of the refusals is the order of the rules: the destination first, because a fenced area
    /// is out of bounds whatever the note holds; then the topic, the summary and the provenance, which
    /// are what make a note durable rather than merely present.
    pub fn file(&self, note: &Path, area: Area, curation: &Curation) -> Result<PathBuf, Refusal> {
        let filing = self.prepared(area, curation)?;

        let permit = self.vault.permit(Holder::Curator);
        let filed = self.vault.file(&permit, note, area, &filing)?;
        self.stamp(&filed, permit.holder().name())?;
        self.vault.write_index(None, &self.fences)?;
        Ok(filed)
    }

    /// Write a new durable note, rather than moving one that already exists.
    ///
    /// Clotho's path: a run's outcome was never an Inbox note, so `file` cannot be used without
    /// inventing a staging file. The checks are the same function as `file`'s, which is the point:
    /// there is one definition of what makes a note durable, and two ways to reach it.
    pub fn record(
        &self,
        area: Area,
        title: &str,
        slug: &str,
        body: &str,
        curation: &Curation,
    ) -> Result<PathBuf, Refusal> {
        let filing = self.prepared(area, curation)?;
        let topic = filing.topics.first().cloned();
        let path = self.vault.area_path(area).join(format!("{slug}.md"));
        let permit = self.vault.permit(Holder::Curator);
        let frontmatter = Frontmatter {
            title: Some(title.to_string()),
            summary: Some(curation.summary.trim().to_string()),
            status: Some("durable".to_string()),
            topic,
            topics: filing.topics.clone(),
            projects: filing.projects.clone(),
            research: curation.research.clone(),
            from_project: curation.from_project.clone(),
            source_url: filing.source_url.clone(),
            provenance: filing.provenance.clone(),
            ..Frontmatter::default()
        };
        self.vault.write_note(&path, &frontmatter, body)?;
        self.stamp(&path, permit.holder().name())?;
        // The path this returns is the one a person opens. A database store only has a row until the
        // note is exported, so the export is part of filing, not a later chore someone has to know about.
        self.vault.export(&path)?;
        self.vault.write_index(None, &self.fences)?;
        Ok(path)
    }

    /// The rules a durable note has to pass, in one place, returning the filing they produce.
    fn prepared(&self, area: Area, curation: &Curation) -> Result<Filing, Refusal> {
        if !DURABLE_AREAS.contains(&area) {
            return Err(Refusal::Unfiled {
                detail: format!("{area} is not a durable area; expected one of {DURABLE_AREAS:?}"),
            });
        }
        self.refuse_fenced(&self.vault.area_path(area))?;

        let topics = cleaned(&curation.topics);
        if topics.is_empty() {
            return Err(Refusal::Unfiled {
                detail: "a durable note needs at least one topic".to_string(),
            });
        }
        if curation.summary.trim().is_empty() {
            return Err(Refusal::Unfiled {
                detail: "a durable note needs a summary, because the index is built from it"
                    .to_string(),
            });
        }
        if curation.provenance.trim().is_empty() {
            return Err(Refusal::MissingProvenance {
                detail: "a durable note must say where it came from".to_string(),
            });
        }

        // Where it came from keeps its shape: a link is a source, and anything else is a provenance.
        let mut filing = Filing {
            topics,
            projects: cleaned(&curation.projects),
            from_project: curation.from_project.clone(),
            summary: Some(curation.summary.clone()),
            ..Filing::default()
        };
        if looks_like_a_url(&curation.provenance) {
            filing.source_url = Some(curation.provenance.clone());
        } else {
            filing.provenance = Some(curation.provenance.clone());
        }
        Ok(filing)
    }

    /// Record who wrote it, and when. The audit reads both back.
    ///
    /// The Python passes the provenance here and never uses it; the argument is the holder instead,
    /// because that is what the stamp is actually for and the permit is where it comes from.
    fn stamp(&self, path: &Path, holder: &str) -> Result<(), Refusal> {
        let mut note = self.vault.read_note(path)?;
        note.frontmatter
            .extra
            .insert("curator".to_string(), Field::Scalar(holder.to_string()));
        note.frontmatter.extra.insert(
            "curated_at".to_string(),
            Field::Scalar(self.timestamp.now()),
        );
        self.vault.write_note(path, &note.frontmatter, &note.body)?;
        Ok(())
    }

    // ---------------- audit ----------------

    /// Report the state of the vault. A clean report is the only acceptable reassurance.
    pub fn audit(&self) -> Result<AuditReport, Refusal> {
        let mut report = AuditReport::new();
        for area in Area::ALL {
            let count = self
                .vault
                .iter_notes(Some(area), &[])?
                .iter()
                .filter(|note| !note.is_marker())
                .count();
            report.counts.insert(area.key().to_string(), count);
        }

        for area in [Area::Research, Area::Projects, Area::Products] {
            for note in self.vault.iter_notes(Some(area), &self.fences)? {
                if note.is_marker() {
                    continue;
                }
                let path = relative_posix(&note.path, self.vault.root());
                if note.summary().is_empty() {
                    report.problems.push(Problem::new(
                        ProblemKind::MissingSummary,
                        path.clone(),
                        "no summary",
                    ));
                }
                if note.topics().is_empty() {
                    report.problems.push(Problem::new(
                        ProblemKind::MissingTopic,
                        path.clone(),
                        "no topic",
                    ));
                }
                let source = note.frontmatter.source_url.as_deref().unwrap_or("");
                let provenance = note.frontmatter.provenance.as_deref().unwrap_or("");
                if source.is_empty() && provenance.is_empty() {
                    report.problems.push(Problem::new(
                        ProblemKind::MissingProvenance,
                        path.clone(),
                        "no source_url or provenance",
                    ));
                }
                if area == Area::Projects {
                    for (reference, outcome) in self
                        .vault
                        .resolve_research_all(&note.frontmatter.research)?
                    {
                        if let Some(reason) = outcome.reason(&reference) {
                            report.problems.push(Problem::new(
                                ProblemKind::DanglingResearch,
                                path.clone(),
                                reason,
                            ));
                        }
                    }
                }
                if area == Area::Products
                    && let Some(project) = note.frontmatter.from_project.as_deref()
                    && let Some(reason) = self.vault.resolve_project(project)?.reason(project)
                {
                    report.problems.push(Problem::new(
                        ProblemKind::DanglingProject,
                        path.clone(),
                        reason,
                    ));
                }
            }
        }

        // The whole tree, at every depth, and a path check rather than a read: a fenced note is reported
        // for being there, never for what it says.
        for path in self.vault.store().walk(self.vault.root())? {
            let relative = relative_posix(&path, self.vault.root());
            if self.is_fenced(Path::new(&relative)) {
                // A record, not a defect: see `AuditReport::fenced`.
                report.fenced.push(Problem::new(
                    ProblemKind::FencedPath,
                    relative,
                    "a path check, not a read",
                ));
            }
        }

        let inbox: Vec<_> = self
            .vault
            .iter_notes(Some(Area::Inbox), &[])?
            .into_iter()
            .filter(|note| !note.is_marker())
            .collect();
        report.counts.insert("stale".to_string(), 0);
        if inbox.len() > INBOX_CAP {
            report.problems.push(Problem::new(
                ProblemKind::InboxOverCap,
                Area::Inbox.folder(),
                format!("{} candidates, cap is {INBOX_CAP}", inbox.len()),
            ));
        }

        let overdue = self.vault.stale(STALE_DAYS)?;
        report.counts.insert("stale".to_string(), overdue.len());
        if !overdue.is_empty() {
            let mut names: Vec<String> = overdue
                .iter()
                .map(|note| {
                    note.path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            names.sort();
            report.problems.push(Problem::new(
                ProblemKind::StaleInbox,
                Area::Inbox.folder(),
                format!(
                    "{} older than {STALE_DAYS} days: {}",
                    overdue.len(),
                    names
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<String>>()
                        .join(", ")
                ),
            ));
        }
        Ok(report)
    }
}

/// The topics or projects worth keeping: trimmed, and with the empty ones gone.
fn cleaned(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

/// A provenance that is a link is a source, and everything else is prose about where it came from.
fn looks_like_a_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}
