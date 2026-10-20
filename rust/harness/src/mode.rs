//! What a surface may do.
//!
//! Two values, because there are two questions. A research job reads and answers; it has no way to
//! change anything, because the tools that change things are not in its schema at all. `act` is a job a
//! person asked for on purpose, and the hard floor still stands in front of it.
//!
//! The mode belongs to the job and to the `World` the loop runs against. It is not a sentence in the task
//! text: a model told "do not write files" still has `write_file` in its hands, and the source behind
//! this rule measured exactly that failure.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Read, search, fetch and answer. No tool that changes anything.
    #[default]
    Read,
    /// Read, investigate and ask, then answer with a plan. No tool that changes anything.
    Plan,
    /// Everything, with the hard floor in front of it.
    Act,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Read => "read",
            Mode::Plan => "plan",
            Mode::Act => "act",
        }
    }

    pub fn parse(text: &str) -> Option<Mode> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "read" => Mode::Read,
            "plan" => Mode::Plan,
            "act" => Mode::Act,
            _ => return None,
        })
    }

    pub fn acts(self) -> bool {
        self == Mode::Act
    }

    pub fn plans(self) -> bool {
        self == Mode::Plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_safe_one() {
        assert_eq!(Mode::default(), Mode::Read);
        assert!(!Mode::default().acts());
    }

    #[test]
    fn a_mode_is_named_and_parsed_consistently() {
        for mode in [Mode::Read, Mode::Act] {
            assert_eq!(Mode::parse(mode.name()), Some(mode));
        }
        assert_eq!(Mode::parse(" ACT "), Some(Mode::Act));
        assert_eq!(Mode::parse("write"), None);
    }
}
