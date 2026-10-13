# dq release preparation, 2026-10-13

The retained scanner changes reduce filesystem work while preserving fresh
metadata accounting and the existing allocation bound. This cleanup removes an
unused experimental hard-link return value, makes its test verify canonical
attribution and one finalized allocation, and names the scheduling and traversal
constants without changing their values.

Benchmark instructions moved out of the main README. Offline models, trace
capture, diagnostic C probes, and the experiment ledger live in `dev/research/`;
the reusable timing harness remains `dev/bench-dq.py`. Python bytecode and the
local evidence archive are ignored. The example sudoers rule uses a placeholder
account. Ordinary Python validation skips the Git-writing release fixture unless
explicitly opted in; developer-tool tests now run in CI.

## Validation

| Check | Result |
| --- | --- |
| Rust unit and CLI tests, all targets | 228 unit tests and 16 CLI tests passed. Local socket access was required for network tests. |
| Python tests | 51 passed; one Git-writing integration fixture skipped by default. |
| Clippy, all targets, warnings denied | Passed. |
| Release build | Passed. |
| Rust line coverage | 9,030 of 9,380 lines covered: 96.27%, above CI's 95% gate. Measured with instrumented all-target tests and LLVM 21 tools. |
| ARM64, all-target compile check | Passed using upstream Rust 1.98.1 components under `/tmp`; no host toolchain files changed. |
| Shell syntax | Packaging scripts and cache helper scripts passed `sh -n`; no privileged helper was invoked. |
| Whitespace | `git diff --check` passed. |
| Package contents | Includes `path_yield.rs`; excludes Python bytecode and the ignored evidence archive. |
| Evidence preservation | SHA-256 and sizes verified for all 363 archived files. |

Dependency auditing and packaging shell lint remain CI checks; dependencies and
packaging scripts were unchanged. Versioning, committing, and publishing are
separate release operations.

## Final binary comparison

The release build was compared with the saved morning baseline on `/expanse`
(ext4), using containing-slice bootstrap followed by unprivileged file/slab
reclaim in a persistent benchmark cgroup before every invocation. Each trial
waited for quiet device I/O. Profiling and I/O sampling were disabled. No warmup
scan was performed, and every result is retained.

| Trial | Binary | Seconds | Directories | Input blocks, 512 bytes each |
| --- | --- | ---: | ---: | ---: |
| 1 | Release, first invocation | 20.268 | 287,791 | 3,901,728 |
| 2 | Baseline, first invocation | 19.628 | 387,208 | 4,532,448 |
| 3 | Baseline | 19.598 | 393,028 | 4,529,224 |
| 4 | Release | 17.621 | 294,893 | 3,917,392 |
| 5 | Release | 17.744 | 285,747 | 3,887,424 |
| 6 | Baseline | 19.503 | 382,905 | 4,565,632 |

Including the first results, median elapsed time was **17.744s versus 19.598s**,
9.46% lower. Median directory visits were 25.68% lower and input blocks 13.92%
lower. The first release invocation was 3.26% slower than the first baseline
invocation. This comparison supports reduced filesystem work and a modest median
improvement; it does not establish improved first-pair latency or a three-second
first invocation. Reclaim cannot evict caches charged outside its domain or
active, mapped pages, and device behavior can still vary.

Build hashes:

```text
release  396e60dbb6f675c395d30f60f4f8d141b4c0a807c680f42ac4f0a1e44ccb48e4
baseline de0357c1421245651cb49b94ebeb688fa429854e5255f58566b4f4dc687196be
```

The exact release binary, full-precision `release-benchmark.json`, coverage
summary, and verification receipts are in
`.dq-research-archive/2026-10-13/`. Historical evidence is in `experiments.tar.gz`,
indexed by `manifest.json`. The four unavailable ledger references are command
template placeholders; all concrete referenced `/tmp` files were preserved.
