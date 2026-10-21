//! The ask bar's state: which question is showing, which option is selected, and whether the person is
//! typing their own answer instead.
//!
//! Pure state, no rendering and no I/O, so the navigation rules can be tested without a terminal. The
//! important one is the navigation rule: moving down past the last option selects `Other` and puts the
//! caret in the input, which is the automatic decline of everything recommended.

use crate::approval::Question;
use crate::question::Ask;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Options,
    Input,
}

#[derive(Debug, Clone, Default)]
pub struct AskState {
    pub questions: Vec<Question>,
    pub active: usize,
    /// Index into the options; `options.len()` is the `Other` row.
    pub option: usize,
    pub focus: Option<Focus>,
    pub collapsed: bool,
}

impl AskState {
    pub fn is_open(&self) -> bool {
        !self.questions.is_empty()
    }

    /// Replace the pending set, keeping the active question if it is still there.
    pub fn set(&mut self, questions: Vec<Question>) {
        let current = self.current().map(|question| question.request.id);
        let questions: Vec<Question> = questions
            .into_iter()
            .filter(|question| question.answer.is_none())
            .collect();
        if questions.is_empty() {
            self.questions.clear();
            self.active = 0;
            self.option = 0;
            self.focus = None;
            self.collapsed = false;
            return;
        }
        let kept = current.and_then(|id| {
            questions
                .iter()
                .position(|question| question.request.id == id)
        });
        self.questions = questions;
        self.active = kept.unwrap_or_else(|| self.active.min(self.questions.len() - 1));
        if kept.is_some() {
            // The same question is still up: a poll must not undo the person's selection.
            let rows = self.ask().map(|ask| ask.options.len()).unwrap_or(0);
            self.option = self.option.min(rows);
            self.focus.get_or_insert(Focus::Options);
        } else {
            self.option = self.recommended().unwrap_or(0);
            self.focus = Some(Focus::Options);
            self.collapsed = false;
        }
    }

    pub fn current(&self) -> Option<&Question> {
        self.questions.get(self.active)
    }

    /// The question as the bar renders it. A legacy record without options gets the built-in permission
    /// choices, so an old question is still answerable.
    pub fn ask(&self) -> Option<Ask> {
        let question = self.current()?;
        Some(
            question.request.ask.clone().unwrap_or_else(|| {
                Ask::permission(&question.request.action, &question.request.reason)
            }),
        )
    }

    pub fn recommended(&self) -> Option<usize> {
        self.ask()?
            .options
            .iter()
            .position(|choice| choice.recommended)
    }

    pub fn down(&mut self) {
        let Some(ask) = self.ask() else {
            return;
        };
        let last = ask.options.len();
        if self.option < last {
            self.option += 1;
        }
        if self.option == last {
            self.focus = Some(Focus::Input);
        } else {
            self.focus = Some(Focus::Options);
        }
    }

    pub fn up(&mut self) {
        let Some(ask) = self.ask() else {
            return;
        };
        let last = ask.options.len();
        if self.focus == Some(Focus::Input) {
            self.focus = Some(Focus::Options);
            self.option = last;
        } else if self.option > 0 {
            self.option -= 1;
        }
    }

    /// Move to the input without changing the selection: a printable key does this.
    pub fn type_here(&mut self) {
        self.focus = Some(Focus::Input);
        if let Some(ask) = self.ask() {
            self.option = ask.options.len();
        }
    }

    pub fn is_other(&self) -> bool {
        self.ask()
            .is_some_and(|ask| self.option >= ask.options.len())
    }

    pub fn selected_key(&self) -> Option<String> {
        let ask = self.ask()?;
        if self.option < ask.options.len() {
            Some(ask.options[self.option].key.clone())
        } else {
            Some("other".into())
        }
    }

    pub fn cycle(&mut self) {
        if self.questions.len() > 1 {
            self.active = (self.active + 1) % self.questions.len();
            self.option = self.recommended().unwrap_or(0);
            self.focus = Some(Focus::Options);
            self.collapsed = false;
        }
    }

    pub fn toggle_collapsed(&mut self) {
        if self.is_open() {
            self.collapsed = !self.collapsed;
        }
    }

    /// How many terminal rows the bar needs. Collapsed is one strip; open is the reason line, the options
    /// and the `Other` row.
    pub fn rows(&self) -> u16 {
        if !self.is_open() {
            return 0;
        }
        if self.collapsed {
            return 1;
        }
        self.ask()
            .map(|ask| ask.options.len() as u16 + 2)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{ApprovalRequest, PendingId};
    use crate::question::{Ask, Choice, Effect};

    fn question(id: u64, ask: Ask) -> Question {
        Question {
            request: ApprovalRequest {
                id: PendingId(id),
                session: "chat".into(),
                action: "shell".into(),
                reason: "the floor".into(),
                at: "now".into(),
                ask: Some(ask),
            },
            answer: None,
        }
    }

    fn fork() -> Ask {
        Ask::choice(
            "Which store?",
            "both fit",
            false,
            vec![
                Choice::new("sqlite", "SQLite", "one file", Effect::Free).recommended(),
                Choice::new("postgres", "Postgres", "a server", Effect::Free),
            ],
        )
    }

    #[test]
    fn the_recommended_option_is_selected_on_arrival() {
        let mut state = AskState::default();
        state.set(vec![question(0, fork())]);
        assert_eq!(state.option, 0);
        assert_eq!(state.focus, Some(Focus::Options));
        assert_eq!(state.rows(), 4, "reason, two options, other");
    }

    #[test]
    fn down_past_the_last_option_selects_other_and_focuses_the_input() {
        let mut state = AskState::default();
        state.set(vec![question(0, fork())]);
        state.down();
        assert_eq!(state.option, 1);
        assert_eq!(state.focus, Some(Focus::Options));
        state.down();
        assert_eq!(state.option, 2, "the other row");
        assert_eq!(state.focus, Some(Focus::Input));
        assert!(state.is_other());
        assert_eq!(state.selected_key().as_deref(), Some("other"));
    }

    #[test]
    fn typing_jumps_to_the_input_without_losing_the_other_row() {
        let mut state = AskState::default();
        state.set(vec![question(0, fork())]);
        state.type_here();
        assert_eq!(state.focus, Some(Focus::Input));
        assert!(state.is_other());
    }

    #[test]
    fn a_new_question_is_selected_again_and_the_old_selection_does_not_leak() {
        let mut state = AskState::default();
        state.set(vec![question(0, fork())]);
        state.down();
        state.set(vec![question(1, fork())]);
        assert_eq!(state.active, 0);
        assert_eq!(
            state.option, 0,
            "a new question starts on its recommendation"
        );
    }

    #[test]
    fn keeping_the_same_question_keeps_the_selection() {
        let mut state = AskState::default();
        state.set(vec![question(0, fork())]);
        state.down();
        state.set(vec![question(0, fork())]);
        assert_eq!(state.option, 1);
    }

    #[test]
    fn answering_removes_the_bar() {
        let mut state = AskState::default();
        state.set(vec![question(0, fork())]);
        state.set(vec![]);
        assert!(!state.is_open());
        assert_eq!(state.rows(), 0);
        assert_eq!(state.focus, None);
    }

    #[test]
    fn a_legacy_question_still_has_options() {
        let mut state = AskState::default();
        state.set(vec![Question {
            request: ApprovalRequest {
                id: PendingId(4),
                session: "chat".into(),
                action: "shell".into(),
                reason: "policy asked".into(),
                at: "now".into(),
                ask: None,
            },
            answer: None,
        }]);
        assert_eq!(state.ask().expect("built in").options.len(), 3);
    }
}
