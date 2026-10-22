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

## Done in 0.1.1

| Item | What changed | Test |
|---|---|---|
| A vision endpoint that is not this machine | `vision: {url, model, key_env}` in the config; `HADES_VISION_URL` and `HADES_VISION_MODEL_NAME` for one process; the local launcher stays as the fallback | `vision::tests::a_configured_endpoint_is_used_instead_of_a_local_server`, `config::tests::vision_is_optional_and_a_bad_url_is_refused` |
| Browser work per state | The sessions map holds one `Arc<Mutex<Held>>` per state; the outer lock is taken to find or create an entry and released before the action | live browser suite; `browser::tests::a_browser_on_the_path` |
| A check that is not Cargo | `.hades-project.json` was honoured by `run`, `turn` and `submit` and ignored by `context` and `tune`, so a Python tree was prepared for `cargo test`. All five read it now | `cli_oracle::a_project_recipe_reaches_the_context_and_tune_checks` |
| Cross-environment suite | `scripts/cross-env.sh`: a Python project, a Node project, a browser named by `HADES_CHROME`, a vision endpoint in the config | the script itself, seven checks |
| Browser discovery | Distribution layouts and a `PATH` search; `hades doctor` names the binary | `browser::tests::a_browser_on_the_path`, `browser::tests::common_browser_locations_are_known` |

### 5. Not doing

No async HTTP framework, no per-domain egress proxy, no automatic replay of uncertain side effects, no
hardcoded verifier model, no claim that a verified citation proves a conclusion. The audit agrees; the
reasons are already in the README and `HADES.md`.
