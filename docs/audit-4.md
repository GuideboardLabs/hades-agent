# The fourth audit, claim by claim

A review of 0.1.9 raised six findings about the pack installer. Each one was checked against the source before
anything was changed. All six were real. A seventh turned up while writing the test for the first.

## 1. Concurrent installs could break the digest guarantee (P1, confirmed)

`install` wrote every archive to `<state>/packs/archives/incoming.hadepack` before parsing it. Two installs into
one state therefore shared one path: B could replace the file A had staged, and A would then parse B's archive
while believing it held the bytes it had verified. The internal checksums of B prove B is intact, not that B is
what A approved.

Fixed in three parts:

- the staging name is per installation (`incoming-<pid>-<nanos>-<counter>.hadepack`) and opened with
  `create_new`, so two installs cannot share one;
- after the parser has read the file, its bytes are hashed again from disk and compared with the digest that was
  verified, so a truncated write, a full disk or a name collision is a refusal rather than a silent substitution;
- the cache name is the digest, so the archive that lands is the archive that was checked.

Test: `two_installs_into_one_state_do_not_share_a_staging_file` runs two exports through `add_file` in parallel
against one state and asserts both skills and both cache entries exist. It passed on twelve consecutive runs.

## 2. Found by that test: the catalog write had the same collision (not in the review)

`write_catalog` used `.catalog-<pid>.tmp`. Two installs in one process share a pid, so one thread renamed the
other's file away and the second failed with "could not protect ... No such file or directory". The staging file
was only the visible instance of a pattern. The catalog now writes to a per-write name opened with `create_new`.

This is worth stating plainly: the reviewer found one race; the test written for it found a second one that the
review did not name, in a function the review treated as settled.

## 3. Namespaced ids broke installation (P2, confirmed)

`valid_id` accepts `author/tool`, and the cache path was `format!("{id}-{version}.hadepack")`, so the rename
target contained a directory nobody created. The cache name is now the digest: an id is not a filename, and two
versions of one pack are two archives.

Test: `a_namespaced_pack_id_installs_and_is_cached_under_its_digest`.

## 4. `--digest` was ignored for shelf ids (P1, confirmed)

The CLI forwarded `--digest` only on the local-file branch; `add()` fetched the shelf and trusted the digest in
the index it had just downloaded. An operator who pinned a digest could receive different bytes and be told the
install succeeded.

`add()` now takes the pin. A pin that disagrees with the shelf is refused before anything is fetched, and says
which digest the shelf names now. A pin that agrees still goes through the normal fetch-then-verify path, so the
pinned digest is checked against the bytes, not merely against the index.

Test: `a_pinned_digest_for_a_shelf_pack_is_not_ignored`.

## 5. A local import was read before it was measured (P2, confirmed)

`add_file` called `fs::read`, so a multi-gigabyte file was allocated in full before the archive limit could
reject it. It now stats first, then reads through `.take(MAX_ARCHIVE + 1)`; the extra byte is what distinguishes a
file that grew after the check. Both refusals say nothing was read.

Test: `an_oversized_local_archive_is_refused_before_it_is_read`.

## 6. The tool list had drifted from the tool registry (P2, confirmed)

`KNOWN_TOOLS` was a second copy of the tool registry with 13 entries; the act surface ships 24. A pack requiring
`browser`, `write_file`, `ask` or `spawn` was refused by the runtime that provides them.

The copy is gone. `known_tool` asks `agent::TOOLS` (the list the schema and the ledger already use) plus
`verify`, which is the one requirable name that is not a model-facing tool. A test asserts every shipped tool is
requirable and that an invented one is not, so the two cannot drift apart again without a red run.

## 7. `pack find --refresh` hid a failed refresh (P2, confirmed)

`rows(&state, true).or_else(|_| shipped())` meant an unreachable shelf produced a search of the built-in
catalog with no indication. `pack menu` already named the fallback; `find` now does the same in text output, and
in `--json` it fails with exit 2, because a list has nowhere to put the caveat.

## What the review got right about the shape of the risk

The execution path was the old audit surface; package integrity is the new one, and the installer was younger
than the runtime around it. The digest check was correct and the paths around it were not, which is the failure
mode a digest is supposed to make impossible.

## Still open

- The shelf index is unsigned, so a digest establishes consistency with the index, not publisher identity. The
  operator pin is the independent check, and it now works.
- `pack add` serialises nothing between processes at the level of the whole installation: two installs of
  *different* packs into one state now both succeed, but two installs of the *same* pack run their plans
  concurrently and the catalog is last-writer-wins. The result is consistent (each row describes an installed
  item) but a simultaneous replace of the same identity is not ordered.
- Landlock ABI 8 on this host; `--cage strict` is the mitigation.
