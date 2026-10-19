//! The harness as a file tree, not a constant.
//!
//! A model's behaviour is set by the text around it as much as by its weights. Keeping that text in a Rust
//! constant means a change needs a rebuild and no two profiles can carry different instructions, which is
//! exactly what a weak local model and a cloud model need. This module loads the text from
//! `$HADES_HOME/harness/<profile>/` when it is there and falls back to the built-in prompt when it is not.
//!
//! Two files, and only two:
//!
//! - `system.md` replaces the built-in system prompt. An empty file is refused: a harness with no
//!   instructions is not a smaller harness, it is a broken one.
//! - `tool_guidance.md` is appended after the system prompt under its own heading. That is the place for
//!   what the tool descriptions cannot say: when to use which tool, what a refusal means, how to recover.
//!
//! The floor, the policy, the approval broker and the check are **not** files here. A harness that can edit
//! its own guardrails is a governance hole, and this module must never grow a `guardrails.md`.

use std::path::{Path, PathBuf};

/// A prompt file larger than this is not an instruction, it is a payload.
pub const CAP: usize = 32 * 1024;

/// The harness a run will use, and where each part came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    pub dir: Option<PathBuf>,
    pub system: String,
    pub system_path: Option<PathBuf>,
    pub guidance: String,
    pub guidance_path: Option<PathBuf>,
}

impl Files {
    /// Built-in text only, for a caller with no harness directory.
    pub fn builtin() -> Files {
        Files {
            dir: None,
            system: crate::model::SYSTEM.to_string(),
            system_path: None,
            guidance: String::new(),
            guidance_path: None,
        }
    }

    /// Load the harness for `profile`: an explicit directory, else `$HADES_HOME/harness/<profile>`, else the
    /// built-in prompt. An explicit directory that does not exist is an error, because a caller who named a
    /// path meant it, while a missing default is simply the built-in harness.
    pub fn load(explicit: Option<&str>, profile: &str) -> Result<Files, String> {
        let dir = match explicit {
            Some(dir) => {
                let path = PathBuf::from(dir);
                if !path.is_dir() {
                    return Err(format!("`--harness {dir}` is not a directory"));
                }
                Some(path)
            }
            None => default_dir(profile),
        };
        let Some(dir) = dir else {
            return Ok(Files::builtin());
        };

        let mut files = Files::builtin();
        files.dir = Some(dir.clone());
        if let Some((path, text)) = read(&dir.join("system.md"))? {
            files.system = text;
            files.system_path = Some(path);
        }
        if let Some((path, text)) = read(&dir.join("tool_guidance.md"))? {
            files.guidance = text;
            files.guidance_path = Some(path);
        }
        Ok(files)
    }

    /// What the model is actually sent: the system prompt, then the guidance under a heading that says what
    /// it is. One string, because a second system message is not portable across endpoints.
    pub fn effective(&self) -> String {
        if self.guidance.trim().is_empty() {
            return self.system.clone();
        }
        format!(
            "{}\n\n# Tool guidance\n\n{}",
            self.system.trim_end(),
            self.guidance.trim()
        )
    }

    /// The one-line source a run prints: the directory, or the built-in prompt.
    pub fn line(&self) -> String {
        match &self.dir {
            Some(dir) => dir.display().to_string(),
            None => "built-in".to_string(),
        }
    }

    /// What a person or a log reads: which parts came from a file, and how large the result is.
    pub fn describe(&self, profile: &str) -> String {
        let mut text = format!("profile: {profile}\n");
        match &self.dir {
            Some(dir) => text.push_str(&format!("dir:     {}\n", dir.display())),
            None => text.push_str("dir:     none (built-in prompt)\n"),
        }
        text.push_str(&match &self.system_path {
            Some(path) => format!(
                "system:  {} ({} bytes)\n",
                path.display(),
                self.system.len()
            ),
            None => "system:  built-in\n".to_string(),
        });
        text.push_str(&match &self.guidance_path {
            Some(path) => format!(
                "guidance: {} ({} bytes)\n",
                path.display(),
                self.guidance.len()
            ),
            None => "guidance: none\n".to_string(),
        });
        text.push_str(&format!("effective: {} bytes\n", self.effective().len()));
        text
    }
}

/// `$HADES_HOME/harness/<profile>` when the directory exists, else `~/.hades/harness/<profile>`.
fn default_dir(profile: &str) -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HADES_HOME") {
        let dir = PathBuf::from(home).join("harness").join(profile);
        if dir.is_dir() {
            return Some(dir);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let dir = Path::new(&home)
        .join(".hades")
        .join("harness")
        .join(profile);
    dir.is_dir().then_some(dir)
}

/// One optional file: absent is fine, empty is not, and anything over the cap is refused by name.
fn read(path: &Path) -> Result<Option<(PathBuf, String)>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    if text.trim().is_empty() {
        return Err(format!(
            "{} is empty; a harness file with no text is a broken harness, not a smaller one",
            path.display()
        ));
    }
    if text.len() > CAP {
        return Err(format!(
            "{} is {} bytes, over the 32 KiB a harness file may carry",
            path.display(),
            text.len()
        ));
    }
    Ok(Some((path.to_path_buf(), text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/scratch")
            .join(format!("hades-harness-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    #[test]
    fn a_missing_directory_is_the_built_in_prompt() {
        let dir = scratch("builtin");
        let files = Files::load(None, "no-such-profile").expect("built-in");
        assert_eq!(files.system, crate::model::SYSTEM);
        assert!(files.dir.is_none_or(|path| !path.starts_with(&dir)));
        assert!(files.guidance.is_empty());
    }

    #[test]
    fn system_md_replaces_and_guidance_is_appended() {
        let dir = scratch("files");
        std::fs::write(dir.join("system.md"), "You are terse.").expect("system");
        std::fs::write(dir.join("tool_guidance.md"), "Read before writing.").expect("guidance");
        let files = Files::load(Some(dir.to_str().unwrap()), "test").expect("loaded");
        assert_eq!(files.system, "You are terse.");
        let effective = files.effective();
        assert!(effective.starts_with("You are terse."), "{effective}");
        assert!(effective.contains("# Tool guidance"), "{effective}");
        assert!(effective.ends_with("Read before writing."), "{effective}");
    }

    #[test]
    fn guidance_alone_keeps_the_built_in_system_prompt() {
        let dir = scratch("guidance-only");
        std::fs::write(dir.join("tool_guidance.md"), "Prefer patch.").expect("guidance");
        let files = Files::load(Some(dir.to_str().unwrap()), "test").expect("loaded");
        assert_eq!(files.system, crate::model::SYSTEM);
        assert!(files.effective().contains("Prefer patch."));
    }

    #[test]
    fn an_empty_file_is_refused_by_name() {
        let dir = scratch("empty");
        std::fs::write(dir.join("system.md"), "\n  \n").expect("empty file");
        let said = Files::load(Some(dir.to_str().unwrap()), "test").expect_err("refused");
        assert!(said.contains("is empty"), "{said}");
    }

    #[test]
    fn a_missing_explicit_directory_is_an_error() {
        let said = Files::load(Some("/no/such/harness"), "test").expect_err("refused");
        assert!(said.contains("not a directory"), "{said}");
    }

    #[test]
    fn an_oversized_file_is_refused_by_name() {
        let dir = scratch("big");
        std::fs::write(dir.join("system.md"), "x".repeat(CAP + 1)).expect("big file");
        let said = Files::load(Some(dir.to_str().unwrap()), "test").expect_err("refused");
        assert!(said.contains("32 KiB"), "{said}");
    }
}
