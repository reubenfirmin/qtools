# dq benchmarking

Measure the first invocation and keep every repeat and outlier. Run only one timed
scan at a time, with builds, tests, trace capture, and archive work finished before
cache preparation starts.

Run the Rust and developer-tool checks before building the measured binary:

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
python3 -B -m unittest discover -s tests -p 'test_*.py'
git diff --check
cargo build --locked --release
```

The release test that creates disposable Git commits, tags, and local pushes is
opt-in through `QTOOLS_RELEASE_GIT_INTEGRATION=1`. Ordinary validation skips it;
agents must not enable it without explicit authorization for those actions.
CI also requires at least 95% Rust line coverage and checks the aarch64 target.

## Automatic comparisons

Copy both builds to immutable paths, then compare them with fresh cache preparation
before every invocation:

```sh
python3 -B dev/bench-dq.py /expanse --binary before=/tmp/dq-before \
    --paired-binary after=/tmp/dq-after --reclaim-cache --isolated-reclaim \
    --wait-for-idle-io --runs 3 --output /tmp/dq-comparison.json
```

This measures six invocations in before/after, after/before, before/after order.
Each build's first result is retained; no scan primes the cache or serves as a
discarded warmup. Without thread options each binary uses its own default. Set
`--threads N` for both, or `--paired-threads N` for the comparison binary.

The harness records binary hashes, arguments, elapsed time, CPU time, scanned
files and directories, process input blocks, device I/O, and allocation-bound
diagnostics. Text output and disabled progress exclude terminal graphics. It
removes `QTOOLS_PROFILE` unless `--profile` is explicitly requested and suppresses
the background update check. Historical dq indexes are disabled for older builds.

### Cache preparation

`--reclaim-cache` uses the ordinary user's delegated cgroup v2
[`memory.reclaim`](https://docs.kernel.org/admin-guide/cgroup-v2.html#memory-interface-files).
It selects the nearest containing slice whose reclaim file the user owns, falling
back to the current cgroup. It flushes dirty writes and requests file/slab reclaim
three times with `swappiness=0`. It changes no memory limits or controller settings
and allocates no memory to create pressure. Preparation fails if swap-out occurs
or more than 64 MiB of eligible file/slab cache remains.

`--isolated-reclaim` first reclaims the containing slice once, then moves only the
harness into a persistent, empty, user-owned benchmark cgroup. Its parent must
already enable the memory controller. Subsequent trials reclaim that group only;
keeping the group preserves its cache charges across harness exits. An occupied
group is rejected. Bootstrap and preparation time are recorded separately from
timed scans. A sandbox may require write access to the delegated cgroup files.

Results are labelled **reclaimed**, distinct from a global `drop_caches` reset.
Reclaim cannot evict pages charged outside the selected domain or active, mapped
pages. Reclaiming a containing slice also affects other applications in that slice.
Compare physical reads alongside wall time to detect unequal cache conditions.

`--wait-for-idle-io` requires three consecutive one-second intervals with at most
1 MiB read, 1 MiB written, and 100 ms of device busy time per interval. It refuses
to scan if counters are unavailable or the device stays busy for 120 seconds.
Other activity can resume during a scan. Every preparation interval is retained.
Failures retain earlier measurements and record the failed trial, phase, and reason.

Without reclaim or a global reset, this command measures an uncontrolled initial
cache state; later invocations may demonstrate warming:

```sh
python3 -B dev/bench-dq.py /expanse --runs 3 --output /tmp/dq-uncontrolled.json
```

## Diagnostics

Use these options separately from ordinary timing comparisons, and apply the same
options to both binaries when comparing diagnostic runs:

| Option | Information or changed behavior |
| --- | --- |
| `--profile` | Allocation progress, largest contributing batch, timing of accounting and scheduling, and eight file-allocation size buckets. Adds timing overhead. Hard-linked inodes enter the histogram once; directory metadata is separate. |
| `--sample-io` | Device counters and the child's worker waits once per second. Adds monitoring overhead; waits do not identify a particular kernel scheduling decision. |
| `--idle-io` | Runs dq with `ionice --class 3`; changes the measured I/O policy. |
| `--best-effort-priority 0..7` | Runs dq with `ionice --class 2` and the chosen priority; incompatible with `--idle-io`. |
| `--audit-unlinked` | Untimed estimate from accessible open descriptors; never changes dq's allocation bound. |
| `--probe-io-uring` | Creates and closes an empty unprivileged ring, records availability, then exits. |
| `--io-context-probe PATH` | Uses the archived clone tracer; `--shared-io-context` adds CLONE_IO to traced thread creation. |

## User-operated global resets

`--cold` requires a new reset before every trial. The harness never invokes a
privileged command. The user runs the installed helper in another terminal and
enters `reset-complete` after it succeeds. Both kernel reset counters must increase;
missing confirmation or unchanged counters refuse the trial. `--watch-reset`
detects the counter changes automatically. `--external-reset` is a compatibility
alias; `--cold` always requires a user-performed reset.

Review [drop-caches.sh](drop-caches.sh), then install it yourself:

```sh
sudo install -o root -g root -m 0555 dev/drop-caches.sh /usr/local/sbin/qtools-drop-caches
sudo visudo -f /etc/sudoers.d/qtools-drop-caches
```

Copy the rule from [drop-caches.sudoers](drop-caches.sudoers), replacing
`BENCHMARK_USER` with the account operating the helper. The empty argument string
allows no arguments. The installed helper accepts no paths, commands, or
environment overrides; it calls sync and writes the fixed value `3` to
`/proc/sys/vm/drop_caches`. Its installed copy and parent directory must be writable
only by root. [reset-dq-caches.sh](reset-dq-caches.sh) is a user-operated shortcut;
agents must never invoke it. Device/controller caches are unaffected.

```sh
python3 -B dev/bench-dq.py /expanse --binary before=/tmp/dq-before \
    --cold --watch-reset --runs 3 --output /tmp/dq-before.json
python3 -B dev/bench-dq.py /expanse --binary after=/tmp/dq-after \
    --cold --watch-reset --runs 3 --output /tmp/dq-after.json
```

## Interpreting results

Worker completion order changes the adaptive frontier. Compare work performed,
physical reads, and wall time; retain first results and throughput outliers. The
stopping bound uses fresh measured allocation. Ordering hints and offline models
never supply credited bytes. See [the research archive](research/README.md) for
the retained policy, rejected experiments, and preserved evidence.
