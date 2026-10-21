---
name: research-with-sources
description: Use when a claim needs evidence. Define the question, search, open the primary source, quote it, date it, and keep fact separate from inference.
version: 1
---

# Research with sources

The output of research is not a longer answer. It is a smaller set of claims you can defend.

## When to use

- A factual claim matters to a decision.
- A number, date, version or quote will be repeated.
- Two sources disagree.

## Steps

1. **Write the question.** One sentence, answerable. "Which release removed the flag?" is better than "tell me about the flag".
2. **Search for the primary source.** Documentation, the repository, the paper, the filing, the changelog. A blog post about a paper is a lead, not evidence.
3. **Open it.** Do not answer from a search snippet or a URL slug.
4. **Record the exact claim.** Quote the sentence, or copy the number, with the date and the version it applies to.
5. **Check the date.** A 2023 answer to a 2026 question is a historical claim.
6. **Look for the counter-case.** Search for the failure report, the issue, the correction.
7. **Separate fact from inference.** "The release notes say X" and "therefore Y" are two lines.
8. **Cite where a reader can check.** Link, title, date. If a source is private, say so.

## Choose the tool

- `fetch` for a plain page whose HTML has the text.
- `browser` with `observe` for a page that needs JavaScript, a login, or an interaction. The observation
  lists the interactive elements; act once, then read the observation that comes back.
- `wait` for the condition you need, not a fixed sleep. If a ref is stale, read the current page and pick
  the new ref.
- `screenshot` only when the text is not enough, then look at it with `image`.

## Rules

- Never invent a source, a quotation or a number. If you cannot verify it, say what you could not verify.
- A search result is not a source. Open it.
- Prefer the current documentation over remembered behavior. APIs change.
- When sources disagree, state both and say which is newer or more authoritative.
- Keep raw material out of the final note. Summarize the claim, keep the link, and put the transcript in an archive.

## Script

Check that every cited link resolves before repeating its claim:

```bash
python3 "$(hades skills path research-with-sources)/scripts/source_check.py" https://example.com/paper
```

It reports status, final URL, content type, title and publication date when the page exposes one. It does
not read the claim for you.

## Pitfalls

- A confident secondary summary that has drifted from the primary text.
- A version number copied from a package name that includes a different build.
- A benchmark number without its hardware, context length or quantization.
