//! Claims the README makes that a machine can hold against the tree.
//!
//! The release gate already pins the test count in four places and the version in four places, so those
//! cannot rot quietly. Prose could. `Limits` said "Telegram is the only messaging transport" for five
//! releases after Discord had shipped as a pack, and it stayed green the whole time because nothing read the
//! sentence and nothing compared it with what the repository actually builds.
//!
//! This is that comparison, kept small on purpose: every pack that ships a connector has to be named in
//! `Limits`, and the retired sentence that said otherwise cannot come back. Both are driven from the shelf
//! that is compiled into the binary, so adding a connector pack without updating the limits fails the suite
//! instead of shipping a false statement to someone deciding whether to install this.

use serde_json::Value;

/// The sentence that was false, kept verbatim so it can be caught rather than described.
const RETIRED: &str = "is the only messaging transport";

/// Every problem found, so a caller can print all of them instead of the first.
pub fn audit(readme: &str, shelf: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if readme.contains(RETIRED) {
        problems.push(format!(
            "the README claims a transport `{RETIRED}`; a pack ships a connector, so no transport is the only one"
        ));
    }
    let Some(limits) = section(readme, "## Limits") else {
        problems
            .push("the README has no `## Limits` section, so no limit can be checked".to_string());
        return problems;
    };
    let shelf: Value = match serde_json::from_str(shelf) {
        Ok(shelf) => shelf,
        Err(error) => {
            problems.push(format!("the shelf index does not parse: {error}"));
            return problems;
        }
    };
    // The claim is a list, not a sentence: `- Transports: `telegram` (in the binary), `discord` (pack).`
    // A substring check would be satisfied by "there is no matrix client", which is the opposite of naming
    // what ships, so the line is parsed and the sets are compared.
    let Some(line) = limits.lines().find(|line| {
        line.trim_start()
            .to_lowercase()
            .starts_with("- transports:")
    }) else {
        problems.push(
            "`Limits` has no `- Transports:` line, so what ships cannot be checked against the shelf"
                .to_string(),
        );
        return problems;
    };
    let named: Vec<String> = line
        .split('`')
        .skip(1)
        .step_by(2)
        .map(|token| token.trim().to_lowercase())
        .collect();
    if !named.iter().any(|name| name == "telegram") {
        problems.push(
            "the `- Transports:` line does not name telegram, which is in the binary".to_string(),
        );
    }
    for pack in shelf["packs"].as_array().into_iter().flatten() {
        let ships = pack["items"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|item| item["kind"] == "connector");
        if !ships {
            continue;
        }
        let Some(id) = pack["id"].as_str() else {
            problems.push("a pack in the shelf has no id".to_string());
            continue;
        };
        if !named.iter().any(|name| name == &id.to_lowercase()) {
            problems.push(format!(
                "pack `{id}` ships a connector and the `- Transports:` line does not list it, so the section understates what ships"
            ));
        }
    }
    problems
}

/// One heading's body, up to the next heading of the same level or higher.
fn section<'a>(text: &'a str, heading: &str) -> Option<&'a str> {
    let start = text.find(heading)? + heading.len();
    let rest = &text[start..];
    let end = rest
        .match_indices("\n## ")
        .map(|(offset, _)| offset)
        .next()
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    const README: &str = include_str!("../../../README.md");
    const SHELF: &str = include_str!("../../../docs/packs/INDEX.json");

    #[test]
    fn the_readme_limits_agree_with_the_shelf_that_ships() {
        let problems = audit(README, SHELF);
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn the_retired_sentence_is_caught() {
        let readme = README.replace(
            "- Transports:",
            "- Telegram is the only messaging transport\n- Transports:",
        );
        assert_ne!(readme, README, "the fixture has something to change");
        let problems = audit(&readme, SHELF);
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("no transport is the only one")),
            "{problems:#?}"
        );
    }

    #[test]
    fn a_connector_pack_the_limits_forgot_is_caught() {
        // The shelf is the trigger, not the README: a pack that ships a connector and is never mentioned in
        // `Limits` is the exact rot this exists for, and it should fail before it is released rather than
        // five releases later.
        let shelf = SHELF.replace(
            "\"packs\": [",
            "\"packs\": [\n{\"id\": \"zulip\", \"items\": [{\"kind\": \"connector\", \"name\": \"zulip\"}]},",
        );
        assert_ne!(shelf, SHELF, "the fixture has something to add");
        let problems = audit(README, &shelf);
        assert!(
            problems.iter().any(|problem| problem.contains("zulip")),
            "{problems:#?}"
        );
    }

    #[test]
    fn a_readme_with_no_limits_section_is_a_problem_not_a_pass() {
        let problems = audit("# Hades\n\nNo limits stated.\n", SHELF);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("no `## Limits`"));
    }
}
