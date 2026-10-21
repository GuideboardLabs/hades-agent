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

Deferred, named, and not forgotten. The follow-up task is "fetch: a persistent client behind the same
guarantees", and its acceptance is that the four guarantees above are each a test, plus a benchmark
showing the same 50-fetch shape under 20%.
