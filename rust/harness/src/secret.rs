//! The shapes a credential takes, and one function that removes them from text.
//!
//! Two places need this: `distill` refuses a runbook that contains a secret-shaped token, and the
//! activity ledger writes a line about every tool call, including the command. The list lived inside the
//! gate, so the ledger had no list at all and a pasted `ghp_...` token in a shell command sat in
//! `activity.jsonl` verbatim. One list, because two lists drift and the drift is a token in a file.

/// The prefixes a leaked credential starts with. Short on purpose: a longer list would be a parser and
/// this is a shape check, so a false positive costs one redacted word, never a kept secret.
pub(crate) const SHAPES: [&str; 6] = ["sk-", "ghp_", "xoxb-", "xoxp-", "AKIA", "-----BEGIN"];

/// What a redacted token is replaced with, so a reader can see the shape was there.
pub(crate) const MARK: &str = "«redacted»";

/// The fewest characters after a prefix before it counts as a token rather than a word.
///
/// Prose mentions `sk-` and `AKIA` as words; a token has a run of characters after the prefix. Eight is
/// below every format's shortest secret and above every sentence that names one.
const LEAST_TOKEN: usize = 8;

/// Replace every secret-shaped token in `text` with [`MARK`], leaving everything else byte-identical.
///
/// A prefix only counts when a token follows it. A PEM header is the one shape whose continuation is
/// words rather than token characters, so it is redacted to the end of its line: the header is the part
/// that says what the block is, and the block itself is worthless to a reader without it.
pub(crate) fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for shape in SHAPES {
        let mut from = 0;
        while let Some(offset) = out[from..].find(shape) {
            let start = from + offset;
            let after = start + shape.len();
            let end = if shape == "-----BEGIN" {
                out[after..]
                    .find('\n')
                    .map(|newline| after + newline)
                    .unwrap_or(out.len())
            } else {
                let run = out[after..]
                    .chars()
                    .take_while(|character| {
                        character.is_ascii_alphanumeric()
                            || matches!(character, '_' | '-' | '+' | '/' | '=')
                    })
                    .count();
                if run < LEAST_TOKEN {
                    from = after;
                    continue;
                }
                after + run
            };
            out.replace_range(start..end, MARK);
            from = start + MARK.len();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_redacted_and_a_word_that_names_a_shape_is_not() {
        assert_eq!(
            redact("git push https://ghp_abcdef1234567890@github.com/x/y"),
            "git push https://«redacted»@github.com/x/y"
        );
        assert_eq!(redact("export AKIAIOSFODNN7EXAMPLE"), "export «redacted»");
        assert_eq!(
            redact("-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n"),
            "«redacted»\nabc\n"
        );
        // Prose about the shapes is not a secret: nothing follows the prefix.
        assert_eq!(
            redact("the sk- prefix and AKIA are shapes"),
            "the sk- prefix and AKIA are shapes"
        );
        assert_eq!(redact("cargo test -p mneme"), "cargo test -p mneme");
    }
}
