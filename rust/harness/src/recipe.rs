//! A repository's own verification recipe.
//!
//! The shipped profiles assume Cargo. That is right for this tree and wrong for every Python, Go or
//! TypeScript repository, and a check nobody can run is a check that gets overridden by hand — which is
//! exactly the habit the ruler exists to stop. A recipe is one file at the repository root that names the
//! command *and* the files that command reads, because a check whose protected set is guessed protects
//! nothing.
//!
//! The pair is deliberate. A file that changes `verify` without saying what that check reads would let a
//! run swap `cargo test` for `true` and still look governed; refusing it is cheaper than noticing later.

use std::path::Path;

use serde_json::Value;

use crate::profile::{Profile, Protect};

/// The file a repository carries, at its root.
pub const FILE: &str = ".hades-project.json";

/// A parsed recipe: the command that decides the work is done, and what that command reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub verify: String,
    pub protect: Vec<Protect>,
}

impl Recipe {
    /// Load the recipe at `root`, or `None` when the repository does not carry one.
    pub fn load(root: &Path) -> Result<Option<Recipe>, String> {
        let path = root.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Recipe::parse(&text, root).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("could not read {}: {error}", path.display())),
        }
    }

    /// Parse and validate. Every refusal names the field, because this file is written by a person.
    pub fn parse(text: &str, root: &Path) -> Result<Recipe, String> {
        let value: Value =
            serde_json::from_str(text).map_err(|error| format!("{FILE} is not JSON: {error}"))?;
        let version = value
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("{FILE} needs a numeric `version`"))?;
        if version != 1 {
            return Err(format!(
                "{FILE} is version {version}; this build reads version 1"
            ));
        }
        let verify = value
            .get("verify")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .ok_or_else(|| format!("{FILE} needs a non-empty `verify` command"))?
            .to_string();
        let protect = value
            .get("protect")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                format!(
                    "{FILE}: a check without `protect` is refused, because a changed verifier has to name \
                     what it reads"
                )
            })?;
        if protect.is_empty() {
            return Err(format!("{FILE}: `protect` is empty"));
        }
        let mut rules = Vec::new();
        for entry in protect {
            let spec = entry
                .as_str()
                .ok_or_else(|| format!("{FILE}: every `protect` entry is a string"))?;
            let trimmed = spec.trim();
            if trimmed.is_empty() {
                return Err(format!("{FILE}: an empty `protect` entry"));
            }
            if Path::new(trimmed).is_absolute() || trimmed.split('/').any(|part| part == "..") {
                return Err(format!(
                    "{FILE}: `{trimmed}` is not relative to the repository root"
                ));
            }
            rules.push(Protect::parse(trimmed, root)?);
        }
        Ok(Recipe { verify, protect: rules })
    }

    /// Replace the profile's check and its protected set together.
    pub fn apply(&self, profile: &mut Profile) {
        profile.verify = self.verify.clone();
        profile.protect = self.protect.clone();
    }

    /// One line for `doctor`: which recipe, or the built-in.
    pub fn describe(root: &Path) -> String {
        match Recipe::load(root) {
            Ok(Some(recipe)) => format!(
                "recipe: {FILE} ({}; {} protected path(s))",
                recipe.verify,
                recipe.protect.len()
            ),
            Ok(None) => "recipe: built-in cargo".to_string(),
            Err(error) => format!("recipe: refused ({error})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = crate::scratch::root().join(format!("recipe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch root");
        dir
    }

    #[test]
    fn a_recipe_sets_the_check_and_the_protected_paths_together() {
        let root = scratch("load");
        std::fs::write(
            root.join(FILE),
            r#"{"version":1,"verify":"pytest -q","protect":["tests"]}"#,
        )
        .expect("a recipe");
        let recipe = Recipe::load(&root)
            .expect("a readable recipe")
            .expect("a recipe is present");
        assert_eq!(recipe.verify, "pytest -q");
        assert_eq!(recipe.protect.len(), 1);
        assert_eq!(recipe.protect[0].to_string(), "tests");
    }

    #[test]
    fn a_check_without_protection_is_refused() {
        let root = scratch("unprotected");
        let text = r#"{"version":1,"verify":"pytest -q"}"#;
        let error = Recipe::parse(text, &root).expect_err("a changed verifier must name what it protects");
        assert!(error.contains("protect"), "{error}");
    }

    #[test]
    fn a_path_that_leaves_the_root_is_refused() {
        let root = scratch("escape");
        for spec in ["../secrets", "/etc/passwd"] {
            let text = format!(r#"{{"version":1,"verify":"pytest -q","protect":["{spec}"]}}"#);
            let error = Recipe::parse(&text, &root).expect_err(spec);
            assert!(error.contains(spec), "{error}");
        }
    }

    #[test]
    fn a_recipe_replaces_verify_and_protect_as_a_pair() {
        let root = scratch("apply");
        let recipe = Recipe::parse(
            r#"{"version":1,"verify":"go test ./...","protect":["internal"]}"#,
            &root,
        )
        .expect("a recipe");
        let mut profile = crate::profile::named("fix").expect("a shipped profile");
        let before = profile.verify.clone();
        recipe.apply(&mut profile);
        assert_ne!(profile.verify, before);
        assert_eq!(profile.verify, "go test ./...");
        assert_eq!(profile.protect, recipe.protect);
    }

    #[test]
    fn a_repository_without_a_recipe_says_cargo() {
        let root = scratch("absent");
        assert_eq!(Recipe::describe(&root), "recipe: built-in cargo");
    }
}
