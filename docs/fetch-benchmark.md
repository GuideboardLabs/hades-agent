# Fetch path: measured, not assumed

The 0.1.0 review asked whether spawning `curl` per fetch is a bottleneck worth replacing with a
persistent HTTP client, and then withdrew the recommendation because no number was on the table. This is
the number.

## What was measured

`scripts/bench-fetch.sh` runs 50 sequential fetches against one local HTTP fixture (an 8 KiB page on
`127.0.0.1`), through the exact invocation `harness/src/fetch.rs` builds — `--proto =http,https`,
`--proto-redir`, `--max-redirs 0`, `--max-filesize 4194304`, headers to a file, body to a file — and then
the same 50 requests over one keep-alive connection in a single process. Three warm fetches precede both
arms. The difference between the arms is the cost of the process, not of the network, which is why the
fixture is loopback.

Two runs on the same machine, in the same session:

| | run 1 | run 2 |
|---|---|---|
| curl per fetch, 50 total | 0.449 s (8.98 ms/fetch) | 0.445 s (8.90 ms/fetch) |
| one connection, 50 total | 0.083 s (1.66 ms/fetch) | 0.087 s (1.74 ms/fetch) |
| process cost | 7.32 ms/fetch | 7.16 ms/fetch |
| share of the current path | 81.5% | 80.4% |
| curl peak RSS | 11 256 KiB | 11 148 KiB |

## The same question on a real page

The loopback fixture has no network, so it measures the process and nothing else. `scripts/bench-fetch-live.sh`
runs both arms against `https://example.com/`, 10 fetches, on this machine:

| | total | per fetch |
|---|---|---|
| curl per fetch | 720 ms | 72.0 ms |
| one process, connections reused | 228 ms | 22.8 ms |

68% of a real fetch is process start plus a fresh TLS handshake. The handshake is the larger half: a
persistent client saves both, a faster process would save only the smaller one.

## Decision

**Over the 20% threshold: open a separate task for a persistent client. Do not bundle it into 0.1.0.**

That task is done. The client is libcurl itself, loaded with `dlopen` in `harness/src/fetch_pool.rs` and
driven through one easy handle for the whole run. Same engine, same flags, same sentences; the process and
the handshake are what goes away. A machine without `libcurl.so.4`, or a run with `HADES_FETCH_POOL=off`,
takes the process path unchanged.

| same machine, 50 loopback fetches | per fetch |
|---|---|
| one libcurl handle | 0.53 ms |
| one `curl` process | 9.16 ms |

| `https://example.com/`, 10 fetches | per fetch |
|---|---|
| one libcurl handle | 19.5 ms |
| one `curl` process | 67.5 ms |

A long-lived `curl` fed by its config stream was tried first and cannot work: curl reads the config to
EOF before running anything, measured at 4008 ms for a block that could have answered at once.

81% of the wall time in a fetch is the process, not the page. That is large enough to matter and large
enough that it should not be discovered again: a research run that fetches twenty sources spends roughly
150 ms in `fork`/`exec`/`curl` startup and 33 ms fetching.

It is also not a one-line change. `fetch.rs` gets things from the process boundary that a client would
have to reproduce deliberately, and each is a guarantee someone would otherwise lose:

- the body cap is enforced by curl (`--max-filesize`, exit 63) *before* the body is transferred;
- the address pin is `--resolve host:port:addr`, which closes the DNS-rebinding window between the check
  and the request;
- redirects are not followed at all (`--max-redirs 0`), so the driver keeps every hop;
- the proxy variables are stripped from the child's environment rather than from the process's;
- every spelling of a failure arrives as curl's own sentence on stderr.

A persistent client therefore needs to be a deliberate piece of work with its own tests for the pin, the
cap, the redirect refusal and the proxy removal — not an optimisation folded into a security release.

## Reproducing

```sh
./scripts/bench-fetch.sh        # 50 fetches, the default
./scripts/bench-fetch.sh 200    # the same shape at a larger count
```

The script starts its own fixture and removes it. It is not part of `cargo test`: the number is a
property of the machine it runs on, and a test that asserts it would fail on a busy one for the wrong
reason.

## Status

Shipped. The four guarantees are tests in `harness/src/fetch.rs`, each run through both paths:

- `the_second_host_of_a_run_is_pinned_to_its_own_address` - the pin is `CURLOPT_CONNECT_TO`, per transfer,
  because `CURLOPT_RESOLVE` is global and would keep pointing a later host at an earlier address;
- `a_body_past_the_cap_is_refused_and_not_read_into_the_process` - `CURLOPT_MAXFILESIZE_LARGE` plus a write
  callback that refuses a streamed body the moment it would pass the cap;
- `a_302_to_a_file_url_is_refused_at_the_hop` and the redirect suite - `CURLOPT_FOLLOWLOCATION` is 0, so the
  `Location` header reaches the driver, which is the only thing that decides;
- `the_process_path_still_answers_when_the_pool_is_off` - the fallback is exercised, not assumed.

Both paths print the same failure sentence, in the same shape, so a message never depends on which one
served the fetch.

## What the two paths must agree on

The pool is an optimisation, so the rules it enforces are the ones the process path already enforced, and
each is covered by a test:

- **No proxy.** The process path removes every spelling of the proxy variables from the child's environment;
  the pool sets `CURLOPT_PROXY` to an empty string after every reset, because libcurl would otherwise read
  the same variables from the runtime's own environment. A proxy would take the connection somewhere other
  than the address that was resolved and checked. `fetch::tests::a_proxy_in_the_environment_does_not_take_a_fetch`
  starts a listener, points `http_proxy` and `HTTP_PROXY` at it, and asserts that nothing arrives.
- **No configuration file.** `curl` is started with `-q` as its first argument. Without it, curl reads
  `~/.curlrc`, which can name a proxy, extra URLs or a relaxed TLS check; the environment was cleared and the
  config file was not. `tests/fetch_config.rs` writes a `.curlrc` that points at a listener, runs the
  subprocess path, and asserts the destination answered and the listener saw nothing.
- **HTTP and HTTPS only, on every libcurl.** `CURLOPT_PROTOCOLS_STR` arrived in 7.85; before it the same
  restriction is the `CURLOPT_PROTOCOLS` bitmask, which exists on every version this build can meet. Ubuntu
  22.04 ships 7.81, where the string option fails and the transfer would have been abandoned for the slow
  path without anyone noticing.
- **No redirects, a body cap that runs while reading, and a pinned destination.**
- **One deadline that includes resolution.** The remaining time is read after DNS, so a slow lookup shortens
  the transfer that follows instead of adding a second budget to it.

`hades doctor` prints the live transport, the version libcurl reported, and which form of the protocol
option is in use.
