# dq performance research

These tools are for offline analysis and kernel diagnostics. The production dq
binary uses fresh filesystem metadata and learns scheduling hints during each
invocation. It reads no research snapshot or persistent index.

## Retained policy

The scanner uses 32 workers by default, eight adaptive work items per pool job,
and up to two jobs per worker. Each new directory samples eight files, completing
leaves with at most 32 files during their first open. Tiny-file continuations in
branching directories measure 32 files; other continuations measure 256.

Scheduling uses half-octave allocation-per-work ranks and a gradual depth penalty
below the first three chart levels. Ranks up to 16 choose an occupied depth
uniformly, then choose work at that depth. A fixed exploration seed makes a given
queue history reproducible; thread completion order can still change that history.

Path hints share observations across directory-name suffixes up to five components.
The longest context with at least eight observed work units wins, with shorter
contexts providing a fallback. Hexadecimal names of at least two characters share
a length bucket. These hints affect order only. Fresh metadata, hard-link
attribution, and the allocation bound determine when the scan may stop.

## Tools

| File | Purpose |
| --- | --- |
| [study-dq.py](study-dq.py) | Capture directory allocation, analyze clustering, and simulate scheduling policies. |
| [capture-dq-trace.py](capture-dq-trace.py) | Capture fresh per-file metadata and optional child-directory metadata. |
| [dq-dir-io-probe.c](dq-dir-io-probe.c) | Compare directory-only native and physical-extent order. File allocation and dq's coverage bound are absent. |
| [clone-io-probe.c](clone-io-probe.c) | Trace only clone/clone3 in an ordinary-user child to compare thread I/O contexts. Linux x86-64 only. |
| [dq-performance-research.json](dq-performance-research.json) | Primary sources, assumptions, experiment outcomes, and rejected variants. |

```sh
python3 -B dev/research/study-dq.py capture /expanse --output /tmp/dq-allocation-snapshot.json
python3 -B dev/research/study-dq.py analyze /tmp/dq-allocation-snapshot.json --output /tmp/dq-allocation-study.json
python3 -B dev/research/study-dq.py simulate /tmp/dq-allocation-snapshot.json --policy fine cohort4 context4 --seed 1 2 --output /tmp/dq-ordering-model.json
python3 -B dev/research/capture-dq-trace.py /expanse --output /tmp/dq-file-trace.jsonl
```

Capture runs have uncontrolled cache state and are not timing benchmarks. The
directory-only model assumes homogeneous direct-file sizes, approximate directory
metadata, unique-file counts, and sequential dispatch. It omits file outliers,
duplicate-link reads, unsettled attribution, and physical I/O. Perfect-information
policies already know unopened allocations. Per-file traces model additional
details but still cannot establish a runtime speedup. Use the
[benchmark harness](../benchmarking.md) for live comparisons.

## Evidence archive

Historical reports preserve their original `/tmp` paths. The local, ignored
`.dq-research-archive/2026-10-13/` directory preserves the available reports, traces,
saved binaries, and source artifacts. Its `manifest.json` maps originals to archive
members, records sizes and SHA-256 hashes, and identifies unavailable references.
The archive also contains a copy of the ledger and research tools. Keep this
directory when cleaning `/tmp`; it is excluded from Git and release artifacts.

Final release verification reports are stored in the same directory. Performance
claims must identify the build hashes, preparation method, all first/repeat results,
and remaining cache limitations. Previously observed modest median improvements
do not establish a three-second first invocation.

See [release preparation results](release-readiness.md) for the final checks and
the complete six-trial comparison after cleanup.
