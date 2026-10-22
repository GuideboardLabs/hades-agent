# Response to the third audit

An external review of 0.1.0 reported five defects and seven residual issues. Every claim was checked against
the source before anything changed. Where the review is wrong, the evidence says so; where it is right, the
fix and its test are named. This file is the record: what was fixed, what was not reproduced, and what is
scheduled with an acceptance test rather than a promise.

## Checked and confirmed

1. **`/proc/<parent>/environ` is readable to a caged child.** `cage.rs` grants `/proc` in both modes
   (`COMPAT_ROOTS` and `STRICT_ROOTS`), and `command.rs` clears the child's environment without touching the
   parent's proc entry. A child that is a direct child of the runtime can read the runtime's startup
   environment where the kernel allows the ptrace read.
2. **A background child can hold a deadline open.** `run_inner` stopped checking the clock once the shell
   exited, then called `join()` on two blocking pipe readers. `sh -c 'sleep 300 &'` with a 2 s deadline hung
   for 300 s.
3. **The fetch pool serialized every transfer.** One easy handle behind a `Mutex` held across
   `curl_easy_perform`, so four research workers queued. This was introduced by the 0.1.0 optimisation.
4. **The vision launch had three defects.** `GGML_VK_DISABLE_HOST_VISIBLE_VMEM` is not a llama.cpp variable
   (it is `..._VIDMEM`); `systemd-run`'s stderr went to `/dev/null`, so a failed launch was silent; and the
   poll ran the full four minutes before reporting.
5. **`resolve_shot` followed a symlink at the final name.** The parent was canonicalized, the final
   component was not, and `fs::write` follows links.
6. **Browser discovery missed distribution layouts.** Only `.cloakbrowser`, three `/usr/bin` names and
   `/snap/bin/chromium` were checked.

## Checked and not confirmed

- **"Model HTTPS response handling is unbounded."** It is bounded: `http.rs` caps the body at
  `MAX_BODY = 8 MiB` while reading (`http.rs:45`, the loop at `http.rs:186`), and says so in the error. The
  cap is generous, not absent. No change made; the number is now stated in `HADES.md`.
- **"The global sessions mutex serializes browser work for different states."** True as a description, but
  the lock is held for the lookup and the action. Fixing it is a concurrency change worth doing on its own
  evidence (see below), not folded into a security pass.

## Fixed in this pass

| Fix | Where | Test |
|---|---|---|
| Process sealed against `/proc` introspection at startup, `HADES_ALLOW_PTRACE=1` to opt out | `cage::seal_environment`, `main.rs` | `cage_oracle::a_caged_child_cannot_read_the_parents_environment` |
| Deadline covers the output streams; the group is killed and output is not claimed | `command.rs` | `command::tests::a_background_child_cannot_hold_the_deadline_open` |
| Pool of easy handles, transferred outside any lock | `fetch_pool.rs` | `fetch::tests::fetches_in_different_threads_do_not_queue` |
| Correct Vulkan variable, stderr captured, early failure from `systemd-run` and `is-failed` | `vision.rs` | build and manual launch; unit test for the name is in the plan |
| Symlink refused at the final name, `O_NOFOLLOW` write, symlinked subdirectory refused | `browser.rs` | `browser::tests::a_screenshot_cannot_be_written_through_a_symlink` |
| Distribution browser paths and a PATH search | `browser.rs` | `browser::tests::common_browser_locations_are_known` |

The first two are the audit's top two items; the third is the regression its optimisation introduced.

## Next, in order

### 1. A vision endpoint that is not this machine

Today the image tool always starts a local Qwen3-VL GGUF through `systemd-run`. On a container or WSL host
without a user systemd manager it cannot work at all, and on a host without the model files it refuses.

Design:

- `config.json` gains `vision`: `{ "endpoint": "<name from endpoints>", "model": "<id>" }`, or a
  `"preset"` naming an entry in the model preset table. Absent means the local path, unchanged.
- `describe` builds the same OpenAI-shaped body it builds now and posts it through `crate::http::post` to
  the configured endpoint, with the key resolved by variable name through the existing resolver.
- The local launcher stays as the fallback and keeps its new early failure.
- `HADES_VISION_ENDPOINT` / `HADES_VISION_MODEL_NAME` override the config for one process.

Acceptance: a stub endpoint that records the request proves the configured path is used and that no
`systemd-run` is attempted; the local path keeps its current test; `hades doctor` prints which of the two
is configured.

### 2. Browser sessions: lock per state, not per process

`SESSIONS` is a `Mutex<BTreeMap<PathBuf, Held>>` held across `Session::act`, so two states that could work
in parallel queue. Design: the map holds `Arc<Mutex<Held>>` per key; the outer lock is taken only to find or
create the entry, then dropped before the action. A session that fails is removed by key after the action,
under the same rule as today.

Acceptance: two states start two browsers and a click in each completes in about the time of one, with a
test that fails if the outer lock is held during `act`.

### 3. A check any profile can be pointed at

`fix` and `sweep` ship `cargo test --quiet`. That is the right default for this repository and the wrong
assumption for a Python or Node project. Design: profile defaults gain an optional `check` string, read from
`config.json` (`profiles.fix.check`), validated as a program that runs; the shipped default stays. The
`--protect` list follows the same idea: a profile can name its own manifests.

Acceptance: a profile with `check: "pytest -q"` runs that, `hades profiles` shows it, and the protected-file
set names no Cargo path when the check is not Cargo.

### 4. A small cross-environment suite

Four fixtures, run by `scripts/cross-env.sh`, each one a claim that is currently untested:

- a Python project (no `Cargo.toml`) with the `fix` profile pointed at `pytest`;
- a Node project with `npm test`;
- a browser at a non-standard path, found through PATH, with a live screenshot;
- a remote OpenAI-compatible stub for the main model and for vision.

WSL/container behaviour is covered by the last two: neither depends on systemd or on a model file.

### 5. Not doing

No async HTTP framework, no per-domain egress proxy, no automatic replay of uncertain side effects, no
hardcoded verifier model, no claim that a verified citation proves a conclusion. The audit agrees; the
reasons are already in the README and `HADES.md`.
