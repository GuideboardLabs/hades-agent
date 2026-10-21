---
name: keep-memory-useful
description: Use when deciding what to remember, or when a fact changes. Store durable decisions with provenance, supersede instead of duplicating, and keep the inbox small.
version: 1
---

# Keep memory useful

Memory is a tool with a budget. A store that keeps everything answers nothing quickly.

## When to use

- You learned something that will matter in a later session.
- A stored fact is now wrong.
- You are about to save a second note about the same thing.

## What deserves a note

- A decision and the reason behind it.
- A finding that took real work to establish.
- A constraint that will shape future work.
- A correction to something already stored.

## What does not

- Temporary status, a plan you will finish today, or a step in a task.
- A restatement of the conversation.
- Raw logs, transcripts or scraped pages. Keep those where they belong and link to them.
- A fact you can look up again in one command.

## Steps

1. **Search before writing.** If a note exists, correct it instead of adding a second one.
2. **Write one claim per note.** A note that contains four ideas cannot be superseded cleanly.
3. **Put the provenance in the note.** Where it came from, when, and what it applies to.
4. **Supersede, do not delete.** Name the old note, keep it as history, and let the new one win.
5. **Review the inbox.** A capture that has not been developed in two weeks is clutter. Promote, merge or archive it.
6. **Re-read what you saved.** If it does not make sense without the conversation, it is not durable.

## Script

Check that a note is retrievable and sourced:

```bash
python3 "$(hades skills path keep-memory-useful)/scripts/note_check.py" note.md
```

It requires a summary, title, ISO date and a source field, and refuses a likely credential. It cannot tell
whether the claim is true or whether one note holds two ideas.

## Rules

- A note without a summary is a note nobody will find.
- A duplicate is worse than a gap: it splits recall between two half-truths.
- Never store a secret, a credential or personal data.
- Keep the total small enough that a search returns a page, not a pile.
