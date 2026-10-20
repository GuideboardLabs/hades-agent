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

/// The most bytes every block may compose into one prompt. A ratchet, not a guess: the built-in system
/// prompt is 1,146 bytes, and two `CAP`-full files could compose 64 KiB. Growing past this is a deliberate
/// decision, not a side effect of one more paragraph.
pub const TOTAL_CAP: usize = 16 * 1024;

/// One named part of a prompt, before it is joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub name: &'static str,
    pub text: String,
}

/// What a reader needs to say which text produced a run, without carrying the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockFact {
    pub name: &'static str,
    pub bytes: usize,
    pub sha256: String,
}

/// The composed prompt: the bytes the model is sent, one digest for those bytes, and one fact per block so
/// a difference between two harnesses can be named rather than diffed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    pub digest: String,
    pub blocks: Vec<BlockFact>,
}

/// Join the non-empty blocks with a blank line and digest the result. An empty block is omitted, and an
/// omitted block is absent from `blocks` rather than present with zero bytes: "guidance: absent" and
/// "guidance: 0 bytes" are different statements about a harness.
pub fn compose(blocks: Vec<Block>) -> Prompt {
    let kept: Vec<Block> = blocks
        .into_iter()
        .filter(|block| !block.text.trim().is_empty())
        .collect();
    let text = kept
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let facts = kept
        .iter()
        .map(|block| BlockFact {
            name: block.name,
            bytes: block.text.len(),
            sha256: mneme::sha256::hex(block.text.as_bytes()),
        })
        .collect();
    Prompt {
        digest: mneme::sha256::hex(text.as_bytes()),
        text,
        blocks: facts,
    }
}

/// The one fact a session records about the prompt that produced it: how many bytes, and the digest of
/// exactly those bytes. Not the text: a log is not a place to copy a harness into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptFact {
    pub bytes: usize,
    pub sha256: String,
}

impl Prompt {
    pub fn bytes(&self) -> usize {
        self.text.len()
    }

    /// The block a person would cut first when the prompt is over its ceiling: the largest one. `None`
    /// when the prompt fits, so a caller cannot warn about a prompt that is fine.
    pub fn over_ceiling(&self) -> Option<&BlockFact> {
        if self.bytes() <= TOTAL_CAP {
            return None;
        }
        self.blocks.iter().max_by_key(|block| block.bytes)
    }

    /// What a session records: the size and digest of the bytes the model was sent.
    pub fn fact(&self) -> PromptFact {
        PromptFact {
            bytes: self.bytes(),
            sha256: self.digest.clone(),
        }
    }
}

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

    /// The blocks this harness composes: the system prompt, then the guidance under a heading that says what
    /// it is. The heading belongs to the block, not to the joiner, so a harness with no guidance does not get
    /// a stray heading.
    pub fn blocks(&self) -> Vec<Block> {
        let guidance = self.guidance.trim();
        // Byte-compatibility with the pre-composer format: a harness with guidance trims the system's
        // trailing space, and one without returns the system prompt exactly as it was read.
        let system = if guidance.is_empty() {
            self.system.clone()
        } else {
            self.system.trim_end().to_string()
        };
        let mut blocks = vec![Block {
            name: "system",
            text: system,
        }];
        if !guidance.is_empty() {
            blocks.push(Block {
                name: "guidance",
                text: format!("# Tool guidance\n\n{guidance}"),
            });
        }
        blocks
    }

    /// The composed prompt, with one digest and one fact per block.
    pub fn prompt(&self) -> Prompt {
        compose(self.blocks())
    }

    /// What the model is actually sent. One string, because a second system message is not portable across
    /// endpoints.
    pub fn effective(&self) -> String {
        self.prompt().text
    }

    /// The one-line source a run prints: the directory, or the built-in prompt.
    pub fn line(&self) -> String {
        match &self.dir {
            Some(dir) => dir.display().to_string(),
            None => "built-in".to_string(),
        }
    }

    /// What a person or a log reads: which parts came from a file, their digests, and how large the result is
    /// against its ceiling.
    pub fn describe(&self, profile: &str) -> String {
        let prompt = self.prompt();
        let mut text = format!("profile: {profile}\n");
        match &self.dir {
            Some(dir) => text.push_str(&format!("dir:     {}\n", dir.display())),
            None => text.push_str("dir:     none (built-in prompt)\n"),
        }
        for block in &prompt.blocks {
            let source = match block.name {
                "system" => match &self.system_path {
                    Some(path) => path.display().to_string(),
                    None => "built-in".to_string(),
                },
                "guidance" => match &self.guidance_path {
                    Some(path) => path.display().to_string(),
                    None => "none".to_string(),
                },
                other => other.to_string(),
            };
            text.push_str(&format!(
                "{:<8} {source} ({} bytes, sha256 {})\n",
                format!("{}:", block.name),
                block.bytes,
                short(&block.sha256)
            ));
        }
        if self.guidance.trim().is_empty() {
            text.push_str("guidance: none (omitted)\n");
        }
        text.push_str(&format!(
            "effective: {} bytes, sha256 {}, ceiling {TOTAL_CAP}\n",
            prompt.bytes(),
            short(&prompt.digest)
        ));
        if let Some(over) = prompt.over_ceiling() {
            text.push_str(&format!(
                "warning: {} bytes over the {TOTAL_CAP} ceiling; cut `{}` first ({} bytes)\n",
                prompt.bytes() - TOTAL_CAP,
                over.name,
                over.bytes
            ));
        }
        text
    }
}

/// The first eight characters of a digest: enough to tell two harnesses apart by eye, short enough to read.
fn short(digest: &str) -> &str {
    &digest[..digest.len().min(8)]
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
        let dir =
            crate::scratch::root().join(format!("hades-harness-{name}-{}", std::process::id()));
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

    #[test]
    fn compose_omits_an_empty_block_and_keeps_order() {
        let prompt = compose(vec![
            Block {
                name: "a",
                text: "one".to_string(),
            },
            Block {
                name: "b",
                text: "   ".to_string(),
            },
            Block {
                name: "c",
                text: "two".to_string(),
            },
        ]);
        assert_eq!(prompt.text, "one\n\ntwo");
        let names: Vec<&str> = prompt.blocks.iter().map(|block| block.name).collect();
        assert_eq!(names, vec!["a", "c"]);
        assert_eq!(prompt.digest, mneme::sha256::hex(b"one\n\ntwo"));
        assert_eq!(prompt.digest.len(), 64);
    }

    #[test]
    fn compose_is_byte_stable() {
        let blocks = || {
            vec![
                Block {
                    name: "system",
                    text: "S".to_string(),
                },
                Block {
                    name: "guidance",
                    text: "G".to_string(),
                },
            ]
        };
        let first = compose(blocks());
        let second = compose(blocks());
        assert_eq!(first.text, second.text);
        assert_eq!(first.digest, second.digest);
        assert_eq!(first.fact(), second.fact());
    }

    #[test]
    fn the_built_in_prompt_fits_under_the_ceiling() {
        let prompt = Files::builtin().prompt();
        assert!(
            prompt.bytes() <= TOTAL_CAP,
            "the built-in prompt is {} bytes, over the {TOTAL_CAP} ceiling",
            prompt.bytes()
        );
        assert!(prompt.over_ceiling().is_none());
    }

    #[test]
    fn describe_names_the_ceiling_and_the_block_to_cut() {
        let dir = scratch("over-ceiling");
        std::fs::write(dir.join("system.md"), "s".repeat(CAP)).expect("system");
        std::fs::write(dir.join("tool_guidance.md"), "g".repeat(CAP)).expect("guidance");
        let files = Files::load(Some(dir.to_str().unwrap()), "test").expect("loaded");
        let text = files.describe("test");
        assert!(text.contains(&format!("ceiling {TOTAL_CAP}")), "{text}");
        assert!(text.contains("warning:"), "{text}");
        assert!(text.contains("cut `guidance`"), "{text}");
        assert!(text.contains("sha256 "), "{text}");
    }

    #[test]
    fn describe_says_guidance_is_absent_rather_than_empty() {
        let text = Files::builtin().describe("library");
        assert!(text.contains("guidance: none (omitted)"), "{text}");
        assert!(text.contains("effective: "), "{text}");
    }
}
