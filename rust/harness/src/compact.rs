//! Atropos: the cut.
//!
//! A session's log is append-only and is never rewritten, so nothing is ever lost. What shortens is the
//! *view*: after a compaction, `Session::view()` starts at the compaction entry and everything before it is
//! still on disk, still greppable, still evidence. That is the contract in one sentence: **a compaction
//! costs context, never evidence.**
//!
//! Who may cut is a naming rule, not a preference. The harness is Atropos for sessions. The curator retires
//! *notes*, which is a different verb over a different store; it must not compact a session, and this
//! module must not retire a note. One authority per kind of thing, or the name stops meaning anything.
//!
//! When: after a walk, if what the run *read* is more than a quarter of the context budget. What it read,
//! not what it wrote: compacting the run that just happened would cut the accepted constraint that the run
//! exists to establish. The summary is written by the harness from the log it already holds, which is why a
//! compacted session can say who wrote it. A model-written summary is a later lever; it is not pretended
//! here.
//!
//! Ordering: the entry is appended (and the store writes it) before the daemon releases the session's
//! lease. A crash after the append leaves the full log plus the summary; a crash before it leaves the full
//! log. There is no window in which the log is shorter than it was.

use session_tree::{Clock, EntryId, Fault, Session, SessionStore};

use crate::context::{Learned, cut_head};

/// Compaction is due when history alone is more than this fraction of the budget.
pub const HISTORY_SHARE: usize = 4;

/// How much of an older summary survives into a new one, so repeated compactions do not grow without bound.
const SUMMARY_CHARS: usize = 400;

/// Whether a compaction would pay for itself.
pub fn due(learned: &Learned, budget: usize) -> bool {
    learned.up_to().is_some() && !learned.is_empty() && learned.tokens() > budget / HISTORY_SHARE
}

/// What the harness writes when it cuts, from what it already holds.
///
/// It keeps the two things a next attempt cannot re-derive cheaply: that there were attempts, and what was
/// already established. The programs are named as being in the log rather than reproduced, which is the
/// whole point of a compaction: the log is the evidence and the view is the prompt.
pub fn summary(learned: &Learned) -> String {
    let mut text = format!(
        "compacted by the harness: {} earlier attempt(s) did not pass; their programs are in the log.",
        learned.tries().len()
    );
    if !learned.constraints().is_empty() {
        text.push_str(" Already established: ");
        let kept: Vec<String> = learned
            .constraints()
            .iter()
            .map(|constraint| constraint.text().to_string())
            .collect();
        text.push_str(&kept.join("; "));
        text.push('.');
    }
    if let Some(older) = learned.summary() {
        text.push_str(" Earlier: ");
        text.push_str(&cut_head(older, SUMMARY_CHARS));
    }
    text
}

/// Cut, if it is due. Returns the entry id of the compaction line, or `None` when nothing was due.
///
/// The session writes the entry as it is appended, so a caller that holds a lease can release it on return:
/// the line is on disk before the lease is gone.
pub fn if_due<S, C>(
    session: &mut Session<S, C>,
    learned: &Learned,
    budget: usize,
) -> Result<Option<EntryId>, Fault>
where
    S: SessionStore + Clone,
    C: Clock + Clone,
{
    if !due(learned, budget) {
        return Ok(None);
    }
    let at = learned
        .up_to()
        .expect("due implies a reading that ended at an entry");
    let id = session.compact(Some(at), &summary(learned))?;
    Ok(Some(id))
}
