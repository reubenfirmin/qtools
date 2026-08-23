use crate::help::{self, Request, Topic};

pub(super) const EXAMPLES: &str = "\
Examples:
  pq                        what is eating cpu/memory right now
  pq -m -v chrome           memory view, clusters expanded, only chrome
  pq --net                  who is serving / talking on the network
  pq --net -v --listen      every listening socket, one row each
  pq --port 8080            who owns port 8080
  pq --port 8080 --kill     kill whatever is LISTENING on port 8080
  pq --kill gradle          kill the gradle tree (preview + confirm)";

const HELP: &str = "\
pq — query what is using your machine

Usage:
  pq [QUERY] [FILTERS]       Query processes, resources, files, or sockets
  pq --kill PATTERN          Stop matching process trees

Queries:
      --cpu / --memory / --swap
                             Process utilization (CPU is the default)
      --io / --read / --write
                             Process I/O rates
      --net / --unix         Network and Unix sockets
      --deleted / --fds      Open-file usage
      --blocked / --pss / --oom / --limits
                             Process internals

Selection and refinement:
  PATTERN, --user, --pid     Select processes
  --min-cpu, --min-memory    Set utilization floors
  --port, --listen, --tcp…   Refine socket queries
  --interval MS              Refine sampling duration

Reporting:
  -v, --verbose              Expand the selected report
      --tree                 Use a process tree
  -n, --top N                Limit ranked results
      --json / --jsonl       Use machine-readable output

Focused help:
  pq help queries            Everything pq can inspect
  pq help filters            Composable selection and refinement
  pq help reporting          Tables, detail, limits, and JSON
  pq help kill               Process actions

Detailed help:
  pq help process | io | net | files
  pq help blocked | pss | oom | limits | unix

Tip: pq --net --help and pq --kill --help open the matching topic.";

const HELP_PROCESS: &str = "\
pq process utilization queries

Examples:
  pq                         CPU report
  pq --memory                Sort by resident memory
  pq --swap                  Sort by swap
  pq --interval 1000         Sample utilization for one second

Options:
  -c, --cpu                  Sort/chart by CPU (default)
  -m, --memory               Sort/chart by memory
  -s, --swap                 Sort/chart by swap
      --interval MS          CPU sampling interval (default 400)

See also: pq help filters, pq help reporting";

const HELP_QUERIES: &str = "\
pq queries

Process resources:
      --cpu                  CPU utilization (default)
      --memory               Resident memory
      --swap                 Swap usage
      --io / --read / --write
                             Process I/O rates

Files and process internals:
      --deleted / --fds      Deleted open files or descriptor counts
      --blocked              Uninterruptible kernel waits
      --pss                  Proportional memory
      --oom                  Kernel OOM victim preference
      --limits               Finite resource-limit utilization

Sockets:
      --net                  TCP/UDP connections and listeners
      --unix [PATH]          Unix-domain sockets

Detailed pages: pq help process | io | files | blocked | pss | oom | limits | net | unix
Selection composes with every query: pq help filters";

const HELP_FILTERS: &str = "\
pq selection and refinement

Filters compose: every supplied filter must match.

Examples:
  pq chrome                  Identity/command contains “chrome”
  pq --user postgres
  pq --pid 1234
  pq --min-cpu 10
  pq --min-memory 500M
  pq --user rafael --min-memory 2GiB
  pq --net --user postgres --port 5432
  pq --net --tcp --state established
  pq --mount /mnt/data --fds

Process selection:
  PATTERN                    Identity or command substring
      --user USER            Username or numeric UID
      --pid PID              Exact process ID
      --min-cpu PERCENT      Minimum cluster CPU percentage
      --min-memory SIZE      Minimum cluster memory; accepts M, GiB, etc.

Socket refinement:
      --port PORT            Local port
      --remote-port PORT     Peer port
      --listen               Listening sockets only
      --tcp / --udp          Protocol
      --state STATE          listen, established, bound, or other
      --public               Bound to all interfaces (firewall not checked)

Path refinement:
      --mount PATH           Open resources beneath a directory/mount
      --file PATH            One exact held path

Sampling refinement:
      --interval MS          CPU and I/O sampling interval (default 400)

Every supplied selector must match. Process selectors also compose with socket and file queries.";

const HELP_IO: &str = "\
pq process I/O

Examples:
  pq --io                    Sort by combined read/write rate
  pq --read                  Sort by read rate
  pq --write                 Sort by write rate
  pq --io postgres           I/O for matching processes
  pq --user postgres --io    I/O owned by postgres
  pq --port 5432 --io        I/O for processes owning local port 5432
  pq --remote-port 443 --io  I/O for processes connected to peer port 443

Options:
      --io                   Combined process I/O view
      --read                 Process I/O view sorted by reads
      --write                Process I/O view sorted by writes
      --interval MS          Sampling interval (default 400)
  -v, --verbose              Expand clusters to processes

Values come from /proc/PID/io. They describe process I/O requests; caching means they are not
necessarily physical device throughput. All ordinary process and socket filters compose.";

const HELP_FILES: &str = "\
pq files and descriptors

Examples:
  pq --deleted               Deleted files still consuming space
  pq --fds                   Descriptor counts by process
  pq --mount /mnt/data       Why a filesystem is busy
  pq --file /var/log/app.log Who holds a path
  pq --mount /mnt/data --fds FD usage among processes holding that mount

Options:
      --deleted              Rank identities by deleted open-file bytes
      --fds                  Rank identities by open descriptor count
      --mount PATH           Select paths beneath a mount/directory
      --file PATH            Select one exact path
  -v, --verbose              Show descriptors or individual hold reasons

Path selectors examine open descriptors, cwd, root, executable, and mapped files.";

const HELP_BLOCKED: &str = "\
pq blocked processes

Examples:
  pq --blocked
  pq --blocked -v
  pq --user postgres --blocked

Shows tasks in uninterruptible D state and their kernel wait channel from /proc/PID/wchan.";

const HELP_PSS: &str = "\
pq proportional memory

Examples:
  pq --pss chrome
  pq --pid 1234 --pss
  pq --port 5432 --pss

Reads /proc/PID/smaps_rollup and reports PSS plus private/shared/anonymous memory. This is explicit
because it is more expensive and permission-sensitive than the default RSS report.";

const HELP_OOM: &str = "\
pq out-of-memory kill preference

Examples:
  pq --oom                   Rank processes by current kernel OOM score
  pq postgres --oom          Inspect matching processes
  pq --pid 1820 --oom -v     Include adjustment details

The score is the kernel's current relative victim preference, not a probability or guarantee.
Lower scores are less preferred; an adjustment of -1000 protects a process.";

const HELP_LIMITS: &str = "\
pq process resource limits

Examples:
  pq --limits                Show processes at least 75% into a measurable finite limit
  pq postgres --limits       Show all measurable finite limits for matching processes
  pq --pid 1820 --limits -v  Include soft/hard limit details

Measured resources include open files, address space, locked memory, and the real user's
process/thread allowance. Unlimited or non-measurable limits are not assigned fake utilization.";

const HELP_UNIX: &str = "\
pq Unix-domain sockets

Examples:
  pq --unix
  pq --unix /run/docker.sock
  pq --net --unix
  pq --user root --unix

Joins /proc/net/unix socket inodes to owning processes. An optional value filters socket paths.";

const HELP_NET: &str = "\
pq network queries

Examples:
  pq --net                   Network activity by application
  pq --net -v                Every matching connection
  pq --listen                Every listener
  pq --port 8080             Inspect a local port
  pq --remote-port 443       Connections to peer port 443
  pq --net --tcp --state established
  pq --public                Listeners bound to all interfaces

Filters:
      --port PORT            Local port
      --remote-port PORT     Peer port
      --listen               Listening sockets only
      --tcp / --udp          Protocol
      --state STATE          listen, established, bound, or other
      --public               Bound to 0.0.0.0 or :: (firewall not checked)

Presentation:
  -v, --verbose              Per-connection rows
      --services             Annotate ports from /etc/services
      --resolve              Reverse-resolve peers in connection rows; may be slow

Targeting and process filters can be combined. See: pq help filters";

const HELP_KILL: &str = "\
pq safe process termination

Examples:
  pq --kill gradle           Preview, confirm, TERM, then KILL survivors
  pq --kill chrome --dry-run Preview only
  pq --port 8080 --kill      Stop the process listening on local port 8080
  pq --net --udp --kill dns  Stop matching UDP socket owners

Options:
      --kill [PATTERN]       Select matching process trees/socket owners
      --dry-run              Preview without signaling
  -y, --yes                  Skip confirmation
  -x, --exact                Exact resolved-identity match
      --signal SIGNAL        Initial signal (default TERM)
      --grace SECONDS        Wait before SIGKILL (default 4)
  -9, --sigkill              SIGKILL immediately

pq excludes itself, PID 1, and the invoking shell/ancestor chain.";

const HELP_REPORTING: &str = "\
pq reporting

Examples:
  pq -v chrome               Expand clusters into member processes
  pq --tree                  Render a static parent/child table
  pq -n 30                   Show up to 30 ranked groups
  pq --json                  Pretty JSON snapshot
  pq --jsonl                 One compact JSON object and newline
  pq --net --json | jq .clusters
  pq --no-color | less

Report shape:
  -v, --verbose              Expand clusters to processes, descriptors, or connections
      --tree                 Static parent/child process table
  -n, --top N                Ranked groups to show (default 15)
      --services             Annotate network ports from /etc/services
      --resolve              Reverse-resolve peers in detailed network rows

Output encoding:
      --json                 Pretty machine-readable JSON
      --jsonl                Compact one-object-per-line JSON
      --no-color             Disable ANSI color and terminal graphics

JSON includes generated_at_unix_ms and sample_interval_ms.";

static HELP_TOPICS: &[Topic] = &[
    Topic { name: "queries", aliases: &["query", "modes"], text: HELP_QUERIES },
    Topic { name: "process", aliases: &["proc", "metrics"], text: HELP_PROCESS },
    Topic { name: "io", aliases: &["read", "write"], text: HELP_IO },
    Topic { name: "files", aliases: &["fds", "deleted", "mount", "file"], text: HELP_FILES },
    Topic { name: "blocked", aliases: &["wait", "stuck"], text: HELP_BLOCKED },
    Topic { name: "pss", aliases: &["smaps"], text: HELP_PSS },
    Topic { name: "oom", aliases: &["oom-score"], text: HELP_OOM },
    Topic { name: "limits", aliases: &["ulimit", "rlimit"], text: HELP_LIMITS },
    Topic { name: "unix", aliases: &["unix-sockets"], text: HELP_UNIX },
    Topic { name: "net", aliases: &["network", "ports"], text: HELP_NET },
    Topic { name: "filters", aliases: &["filter"], text: HELP_FILTERS },
    Topic { name: "kill", aliases: &["signal"], text: HELP_KILL },
    Topic { name: "reporting", aliases: &["report", "output", "json", "jsonl"], text: HELP_REPORTING },
];

static HELP_CONTEXTS: &[(&[&str], &str)] = &[
    (&["--kill", "--dry-run", "--signal", "--grace", "--sigkill", "-9"], "kill"),
    (&["--io", "--read", "--write"], "io"),
    (&["--deleted", "--fds", "--mount", "--file"], "files"),
    (&["--blocked"], "blocked"),
    (&["--pss"], "pss"),
    (&["--oom"], "oom"),
    (&["--limits"], "limits"),
    (&["--unix"], "unix"),
    (&["--net", "--port", "--listen", "--remote-port", "--tcp", "--udp", "--state", "--public"], "net"),
    (&["--user", "--pid", "--min-cpu", "--min-memory"], "filters"),
    (&["--json", "--jsonl", "--no-color", "--tree", "--verbose", "-v", "--top", "-n", "--services", "--resolve"], "reporting"),
    (&["--cpu", "-c", "--memory", "-m", "--swap", "-s", "--interval"], "process"),
];

pub(crate) fn show_and_exit() {
    let args: Vec<String> = std::env::args().collect();
    match help::route(&args, HELP, HELP_TOPICS, HELP_CONTEXTS) {
        Request::None => {}
        Request::Show(text) => { println!("{text}"); std::process::exit(0); }
        Request::Unknown(topic) => {
            eprintln!("pq: unknown help topic '{topic}'\navailable topics: {}", help::topic_names(HELP_TOPICS));
            std::process::exit(2);
        }
    }
}
