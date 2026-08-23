# qtools - query what's using your machine

`qtools` is a small, growing suite of fast, visual command-line tools for answering "what the hell is
eating my \<resource\>?" on Linux. Each tool is a terse two-letter `_q` ("query") utility, sharing a
common rendering core: donut charts on graphics-capable terminals, colored text bars everywhere else,
JSON for machines. Written in Rust.

* **`dq`** (disk query) - what's using my disk space? A faster, smarter `du`.
* **`pq`** (process query) - what's eating my CPU, memory, or swap? Who's listening on, or talking to,
  the network? A clustered, visual `top` with a friendlier `pkill` and an `lsof -i`/`ss` replacement
  built in.

## Getting started

```
$ ./build.sh && ./install.sh   # build + install dq and pq to ~/.local/bin (make sure it's on PATH)

$ pq                            # what's eating my CPU/memory/swap right now
$ dq                            # what's using disk space in the current directory
```

Help is deliberately layered: `pq --help` and `dq --help` are short orientation pages, with focused
reference under pq's report topics and `dq help views|filters|scan|output`. Contextual forms
such as `pq --net --help`, `pq --kill --help`, and `dq --json --help` open the matching topic directly.

`sudo ./install.sh` installs system-wide (to `/usr/local/bin`) instead of just for your user.

### Cross-distribution AppImage

Release builds are available from [qtools.sh](https://qtools.sh/) as one AppImage containing both
commands. Download the build for your architecture, make it executable, and let it install itself:

```sh
chmod +x Qtools-x86_64.AppImage
./Qtools-x86_64.AppImage --install
pq --help
dq --help
```

This copies the AppImage to `~/.local/bin/qtools.AppImage` and creates the `qtools`, `pq`, and `dq`
symlinks there. It does not require root or pipe downloaded code into a shell. `qtools update` uses
zsync when `appimageupdatetool` is available, or securely downloads and verifies the full AppImage
from the stable repository otherwise. `qtools uninstall` removes the per-user installation.
The binaries check the stable channel in the background at most once a day, after `pq` or `dq` has
finished. When an update is found, the next interactive report ends with a one-line notice.
Use `qtools snooze-update-notification` to hide notices for 30 days; machine-readable and piped
output never includes them.
Maintainer instructions are in [`packaging/RELEASE.md`](packaging/RELEASE.md).

## dq (disk query)

![dq](./dq.png)

Scans typed file and directory facts across a thread pool, stays on the starting filesystem by
default, skips symlinks, and deduplicates hard links by device/inode. Allocated filesystem blocks are
the default accounting mode; `--apparent` reports logical lengths instead. Every view uses additive
groups, rich-terminal donuts or proportional text bars, and the same filter/accounting pipeline.

    dq [dir]                         # directory groups, depth 1
    dq --files --top 30 /var         # largest files recursively
    dq --owners /srv                 # space grouped by file owner
    dq --age /data                   # space grouped by modification age
    dq --apparent /data              # logical rather than allocated size
    dq --exclude cache --exclude '*.tmp' .
    dq --files --older-than 90d --min-size 1G /var
    dq --depth 2 --min-percent 0.5 /
    dq --cross-filesystems /mnt
    dq --errors /var                 # list unreadable paths
    dq --json /var | jq .rows
    dq --jsonl --files /var

Examples:

    dq /tmp
    dq --all --threads 50 /tmp
    dq --allocated --files --older-than 30d --top 20 /var
    dq --json / > sizes.json

## pq (process query)

![pq](./pq.png)

Reads `/proc` (Linux only) and clusters processes by resolved identity, not just executable name: a JVM
running a Gradle daemon groups as `gradle`, not a pile of `java`; Chrome's renderer swarm collapses into
one `chrome (N procs)` (separate profiles/instances are told apart). `-v` expands a cluster to its
member processes.

    pq --cpu / --memory / --swap   # sort/chart metric (default: cpu)
    pq --io / --read / --write      # process I/O rates (combined/read/write sort)
    pq -n N                          # clusters to show (default 15)
    pq -v                             # expand clusters (processes; connections in --net mode)
    pq --interval MS                   # CPU sample interval (default 400ms)
    pq --json                           # machine-readable output
    pq --jsonl                          # one compact JSON snapshot per line
    pq PATTERN                           # filter report to matching clusters
    pq --user USER --min-memory 500M     # conventional filters compose
    pq --pid PID --min-cpu 10             # exact pid and minimum CPU percentage
    pq --net [--listen] [--port N]       # network mode: see below
    pq --kill PATTERN / --port N --kill   # kill mode: see below

Per-cluster memory/swap sums each member's RSS/`VmSwap`, which over-counts shared pages (same caveat as
most process viewers).

### pq --kill: a friendlier pkill

Matches the resolved identity, comm, and full command line (case-insensitive) - so `pq --kill gradle`
finds the JVM Gradle daemon that `pkill gradle` misses. Expands each match to its whole process subtree.
Previews the matching tree and confirms before acting. Escalates: SIGTERM, wait `--grace` seconds
(default 4), then SIGKILL survivors. Never signals pq itself, your shell/its ancestors, pid 1, or
unrelated sibling jobs.

    pq --kill PATTERN [--dry-run] [-y/--yes] [-x/--exact] [--signal SIG] [--grace SECONDS]

### pq --net: who is this box talking to?

Reads `/proc/net` directly (no lsof/ss) and attributes sockets to processes via `/proc/<pid>/fd`,
then clusters by the same resolved identity as the main report: Chrome's renderer swarm shows as
one `chrome` row. The overview is a grid, sorted by traffic: connection count, bytes sent and
received (from the same sock_diag counters `ss -i` reads; TCP only, cumulative per connection),
process, the ports it listens on, and the peer ports it talks to (`:443 x28`), with
world-exposed listeners (bound to `0.0.0.0`/`::`) in red. Unattributed rows show their ports too. On graphics terminals the donut
has two rings (outer = established, inner = listeners, same color per cluster, labeled
beneath), every row carries the color swatch of its slice, and a cutoff row counts anything
hidden by `-n`. Targeted queries answer in plain English instead of a donut: `pq --port N` and
`pq --net NAME` draw a card per owning process (user, command line, and each socket with a
reachability arrow: WORLD / local / iface), plus a paste-ready kill hint. A usage footer on the
overview lists the common queries. Listeners bound to all interfaces (`0.0.0.0`/`::`) are world-exposed: they're counted
in the header, their ports show red, and `-v` rows get a `[world]` marker. (Bind scope only; pq
does not interrogate the firewall.) `-v` rows also carry a direction tag: `lsn`, `in`
(accepted on a listened port), or `out` (this process dialed out). Without sudo, other users'
sockets still appear (grouped by user) but can't be tied to a process; run with sudo for full
attribution.

    pq --net                  # lsn/est by cluster, listening ports, sorted by total conns
    pq --net -v               # per-connection rows (direction, proto, state, local, peer)
    pq --net --listen         # only listening sockets: what is serving on this box?
    pq --port 8080            # who owns port 8080 (--port implies --net)
    pq --net --json           # machine-readable output
    pq --net PATTERN          # filter clusters, same matching as the main report
    pq --net --remote-port 443 # sockets connected to peer port 443
    pq --net --tcp / --udp     # filter by protocol
    pq --net --state established # listen, established, bound, or other
    pq --net --public          # listeners bound to all interfaces
    pq --net --services        # annotate ports with names from /etc/services
    pq --net -v --resolve      # reverse-resolve peer IPs (may be slow)

The process filters `--user`, `--pid`, `--min-cpu`, and `--min-memory` can be combined and also
apply in network mode. `--json` is pretty-printed; `--jsonl` emits the same snapshot as one compact
line for append-only logs. Both include `generated_at_unix_ms` and `sample_interval_ms`. `-n` limits
clusters in machine output just as it does in the human report. Use `--no-color` to force the plain
text renderer on an interactive terminal.

### pq --io: who is reading or writing?

Samples `read_bytes` and `write_bytes` from `/proc/<pid>/io` over `--interval`, then aggregates the
rates using pq's normal resolved process identities. `--io` sorts by combined throughput; `--read`
and `--write` select the same report sorted by that direction. Process and socket selectors compose:

    pq --io
    pq --write postgres
    pq --user postgres --io
    pq --port 5432 --io
    pq --remote-port 443 --read
    pq --io --json

These are bytes accounted to process I/O by the kernel. Caching and delayed writeback mean they are
not necessarily equal to instantaneous physical-device throughput. Graphics-capable terminals get
a throughput-share donut by default; other terminals get proportional activity bars, and pipes stay
plain text.

### pq process internals and open resources

Use `pq --tree` for a static, aligned parent/child process table. Filters select the interesting
processes; otherwise-unselected ancestors remain as dim context. Siblings follow the active CPU,
memory, or swap ordering. JSON exposes flat nodes with parent IDs and depths.

    pq --tree
    pq --memory --tree postgres
    pq --tree --json

These views use the same process, socket-owner, user, PID, and path selectors as the other reports.
Every standalone view uses the normal graphics/text/JSON rendering pipeline.

    pq --deleted                 # deleted files still consuming disk space
    pq --deleted -v              # show each held descriptor/path
    pq --fds                     # descriptor counts and classifications
    pq --blocked                 # tasks in uninterruptible D state + wchan
    pq --pss chrome              # proportional/private/shared/anonymous memory
    pq --oom                     # relative kernel OOM-kill preference
    pq --limits                  # finite resource limits nearing exhaustion
    pq --file /var/log/app.log   # who holds, maps, executes, or uses this path
    pq --mount /mnt/data         # what prevents this tree/filesystem being released
    pq --mount /mnt/data --fds   # FD report restricted to those holders
    pq --port 5432 --pss         # PSS restricted to owners of local port 5432

`--deleted` measures the followed open file and deduplicates repeated references to the same
device/inode within an identity. `--file` and `--mount` examine descriptors, cwd, root, executable,
and file-backed mappings. `--pss` reads `smaps_rollup` only when explicitly requested because it is
more expensive and permission-sensitive.

`--oom` uses the kernel's current `oom_score` and `oom_score_adj`; it reports relative victim
preference rather than predicting a kill. `--limits` pairs finite soft limits with live usage where
Linux exposes a meaningful measurement. Machine-wide output shows utilization of 75% or higher;
a process pattern, `--pid`, or `-v` shows all measurable finite limits.

### pq Unix-domain sockets

    pq --unix
    pq --unix /run/docker.sock
    pq --net --unix
    pq --user root --unix

Unix sockets are read from `/proc/net/unix` and attributed through `/proc/PID/fd` socket inodes.
Filesystem and abstract socket names are both retained; sockets that cannot be attributed are shown
as such when the query is otherwise unfiltered.

### pq --port N --kill: free up a port

Kills whatever is LISTENING on local port N (TCP listeners and bound UDP sockets only, so a
client whose ephemeral port happens to be N is never touched). Same preview/confirm flow and
TERM-grace-KILL escalation as `pq --kill`, with the matched socket shown in the preview. If the
listener can't be attributed to a process (someone else's, no sudo), pq refuses rather than
guessing.

    pq --port 8080 --kill [--dry-run] [-y] [--signal SIG] [--grace SECONDS]

In net mode without --port, pq --net --kill PATTERN (or with --listen) kills the trees of processes that own a matching socket and match the pattern.
