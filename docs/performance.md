# Performance: what the two transports cost

The static musl build cannot load libcurl, so every fetch runs through the `curl` executable. That is a real
difference and it is measured here rather than described.

## Method

`hades pack add review-a-diff --yes` into a fresh home, three times per binary, on the same machine, against the
same shelf over the same network. Each install performs two fetches: the index and the archive (~2 KB and ~4 KB),
then the filesystem work.

| binary | transport | runs (ms) | median |
|---|---|---|---|
| `hades-portable` | in-process libcurl | 315, 193, 231 | 231 |
| `hades-musl` | `curl` subprocess | 256, 256, 255 | 256 |

**+25 ms per install, about 11%**, for two small fetches. The difference is process startup, not throughput: the
spawn and the TLS handshake are both inside the measurement, which is why it does not scale with body size. On a
large body the two paths converge, because the time is the network.

## What this means

- A person installing one pack will not notice. A job making hundreds of small fetches will.
- Correctness is identical: the same budget, the same proxy clearing, the same `CONNECT_TO` pinning, the same
  body cap. The subprocess path is the one that has been exercised all along on machines without
  `libcurl.so.4`.
- `hades doctor` prints which transport is live before anyone benchmarks anything, so a surprising number has a
  visible cause.

## Not measured here

- Large bodies, many concurrent fetches, and slow links. The subprocess path spawns one process per fetch, so
  concurrency is bounded by process count rather than by libcurl's pool; that is the case where the difference
  would grow, and it is worth measuring before anyone runs a crawl on a static build.
- ARM64. No runtime here.
