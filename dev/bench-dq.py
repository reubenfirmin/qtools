#!/usr/bin/env python3
"""Measure the first dq invocation without priming or discarding any runs."""

import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import shutil
import statistics
import stat
import subprocess
import tempfile
import time


SCAN = re.compile(r"dq\[debug\]: (?:summary-scan|scan)=(\d+)ms ([^\n]+)")


def probe_io_uring():
    """Create and close only our own empty ring; no requests or privilege changes."""
    import ctypes
    import platform
    if os.geteuid() == 0:
        raise RuntimeError("The io_uring diagnostic must run as an ordinary user.")
    if platform.system() != "Linux" or platform.machine() not in ("x86_64", "aarch64"):
        raise RuntimeError("The diagnostic supports Linux x86_64 and aarch64 only.")
    library = ctypes.CDLL(None, use_errno=True)
    library.syscall.restype = ctypes.c_long
    # io_uring_params is a zero-initialized 120-byte Linux UAPI structure.
    parameters = ctypes.create_string_buffer(120)
    descriptor = library.syscall(ctypes.c_long(425), ctypes.c_uint(8), ctypes.byref(parameters))
    error = ctypes.get_errno() if descriptor < 0 else 0
    if descriptor >= 0:
        os.close(descriptor)
    return {"purpose": "untimed-io-uring-availability", "uid": os.geteuid(),
            "available": descriptor >= 0, "errno": error,
            "error": os.strerror(error) if error else None}


def audit_unlinked(root):
    """Read only accessible descriptors; this diagnostic never changes dq's bound."""
    if os.geteuid() == 0:
        raise RuntimeError("The descriptor audit must run as an ordinary user.")
    device = os.stat(root).st_dev
    allocations = {}
    checked = inaccessible = 0
    with os.scandir("/proc") as processes:
        for process in processes:
            if not process.name.isdigit():
                continue
            try:
                with os.scandir(process.path + "/fd") as descriptors:
                    for descriptor in descriptors:
                        try:
                            metadata = descriptor.stat()
                            checked += 1
                            if metadata.st_dev == device and metadata.st_nlink == 0 and stat.S_ISREG(metadata.st_mode):
                                allocations[metadata.st_ino] = max(allocations.get(metadata.st_ino, 0), metadata.st_blocks * 512)
                        except OSError:
                            inaccessible += 1
            except OSError:
                inaccessible += 1
    return {"purpose": "untimed-unlinked-allocation-diagnostic", "checked_descriptors": checked,
            "inaccessible_or_changed": inaccessible, "unique_unlinked_regular_inodes": len(allocations),
            "allocated_bytes": sum(allocations.values()), "uid": os.geteuid(),
            "limitation": "Non-atomic snapshot of accessible descriptors only; never a scanner input or allocation certificate."}


def current_cgroup():
    for line in Path("/proc/self/cgroup").read_text().splitlines():
        if line.startswith("0::/"):
            relative = Path(line[4:])
            if ".." in relative.parts:
                break
            return Path("/sys/fs/cgroup") / relative
    raise RuntimeError("Cannot locate the current cgroup v2 directory.")


def memory_stats(cgroup):
    return {key: int(value) for key, value in
            (line.split() for line in (cgroup / "memory.stat").read_text().splitlines())}


def reclaim_domain(current):
    # Cache charges move to ancestors when a terminal scope exits. Reclaiming
    # only the next scope cannot evict those charges. Prefer the nearest stable
    # containing slice, but only when this ordinary user owns its reclaim file.
    for ancestor in current.parents:
        if not ancestor.name.endswith(".slice"):
            continue
        try:
            owner = (ancestor / "memory.reclaim").stat().st_uid
        except OSError:
            continue
        if owner == os.geteuid() and owner != 0:
            return ancestor
    return current


def isolated_reclaim_group():
    """Move only this benchmark into a persistent, user-owned memory domain."""
    if os.geteuid() == 0:
        raise RuntimeError("Isolated reclaim must run as an ordinary user.")
    parent = reclaim_domain(current_cgroup())
    if not parent.name.endswith(".slice"):
        raise RuntimeError("Isolated reclaim requires a delegated containing slice.")
    for path in (parent, parent / "cgroup.procs"):
        if path.stat().st_uid != os.geteuid():
            raise RuntimeError(f"Benchmark user does not own {path}.")
    if "memory" not in (parent / "cgroup.subtree_control").read_text().split():
        raise RuntimeError("The delegated slice does not already enable the memory controller.")
    group = parent / f"qtools-dq-benchmark-{os.geteuid()}"
    group.mkdir(exist_ok=True)
    for name in ("cgroup.procs", "memory.reclaim"):
        if (group / name).stat().st_uid != os.geteuid():
            raise RuntimeError(f"Benchmark user does not own {group / name}.")
    if (group / "cgroup.procs").read_text().strip():
        raise RuntimeError("The isolated benchmark cgroup is occupied; refusing overlapping trials.")
    (group / "cgroup.procs").write_text(f"{os.getpid()}\n")
    if current_cgroup() != group:
        raise RuntimeError("Benchmark migration did not select the isolated cgroup.")
    return group


def reclaim_caches(cgroup=None):
    """Reclaim file pages and metadata in our delegated cgroup, without root."""
    current = current_cgroup()
    cgroup = reclaim_domain(current) if cgroup is None else cgroup
    reclaim = cgroup / "memory.reclaim"
    if reclaim.stat().st_uid != os.geteuid() or os.geteuid() == 0:
        raise RuntimeError("Cache reclaim requires a cgroup owned by the unprivileged benchmark user.")
    started = time.perf_counter()
    # Finish dirty writes before requesting file-only reclaim. No memory limits
    # are changed and no anonymous-memory pressure is introduced.
    os.sync()
    before = memory_stats(cgroup)
    passes = []
    previous = before
    for _ in range(3):
        requested = previous["file"] + previous["slab_reclaimable"] + 4096
        descriptor = os.open(reclaim, os.O_WRONLY | os.O_CLOEXEC)
        under_reclaimed = False
        try:
            os.write(descriptor, f"{requested} swappiness=0\n".encode())
        except OSError as error:
            if error.errno != errno.EAGAIN:
                raise
            # EAGAIN means the kernel reclaimed less than requested, which is
            # expected when the request includes active or pinned cache pages.
            under_reclaimed = True
        finally:
            os.close(descriptor)
        after = memory_stats(cgroup)
        passes.append({"requested_bytes": requested, "under_reclaimed": under_reclaimed,
                       "before": previous, "after": after})
        previous = after
    if previous.get("pswpout", 0) > before.get("pswpout", 0):
        raise RuntimeError("Swap-out activity occurred during file-only reclaim; refusing to benchmark.")
    remaining = max(0, previous["file"] - previous.get("shmem", 0) - previous.get("file_mapped", 0))
    remaining += previous["slab_reclaimable"]
    if remaining > 64 * 1024 * 1024:
        raise RuntimeError(f"Cache reclaim left {remaining:,} bytes of eligible file/slab cache; "
                           "refusing to benchmark with an incompletely reclaimed cgroup.")
    return {"method": "cgroup-memory-reclaim", "cgroup": str(cgroup), "current_cgroup": str(current), "swappiness": 0,
            "elapsed_s": time.perf_counter() - started,
            "before": before, "after": previous, "passes": passes,
            "remaining_cache_bytes": remaining}


def binary(value):
    label, separator, command = value.partition("=")
    if not separator:
        command, label = value, Path(value).name
    executable = shutil.which(command)
    if not executable:
        raise argparse.ArgumentTypeError(f"executable not found: {command}")
    return label, str(Path(executable).resolve())


def io_stall():
    try:
        line = Path("/proc/pressure/io").read_text().splitlines()[0]
        return int(line.rsplit("total=", 1)[1]) / 1_000_000
    except (OSError, ValueError, IndexError):
        return None


def filesystem_io(root):
    """Device-wide activity, including work from other processes on this filesystem."""
    device = os.stat(root).st_dev
    major, minor = os.major(device), os.minor(device)
    try:
        for line in Path("/proc/diskstats").read_text().splitlines():
            fields = line.split()
            if (int(fields[0]), int(fields[1])) == (major, minor):
                return {"read_blocks": int(fields[5]), "write_blocks": int(fields[9]),
                        "read_ops": int(fields[3]), "write_ops": int(fields[7]),
                        "read_ms": int(fields[6]), "write_ms": int(fields[10]),
                        "busy_ms": int(fields[12]), "weighted_busy_ms": int(fields[13])}
    except (OSError, ValueError, IndexError):
        pass
    return None


def wait_for_idle_io(root, timeout_s=120):
    """Wait before timing; never scan, discard a sample, or change other jobs."""
    started = time.monotonic()
    previous = filesystem_io(root)
    if previous is None:
        raise RuntimeError("Filesystem device counters are unavailable; cannot verify idle I/O.")
    intervals = []
    quiet = 0
    for _ in range(timeout_s):
        time.sleep(1)
        current = filesystem_io(root)
        if current is None:
            raise RuntimeError("Filesystem device counters became unavailable while waiting for idle I/O.")
        activity = {key: current[key] - previous[key] for key in ("read_blocks", "write_blocks", "busy_ms")}
        intervals.append(activity)
        idle = all(0 <= activity[key] <= limit for key, limit in
                   (("read_blocks", 2048), ("write_blocks", 2048), ("busy_ms", 100)))
        quiet = quiet + 1 if idle else 0
        if quiet == 3:
            return {"elapsed_s": time.monotonic() - started, "intervals": intervals,
                    "required_quiet_intervals": 3, "interval_s": 1,
                    "max_read_bytes_per_interval": 1048576,
                    "max_write_bytes_per_interval": 1048576, "max_busy_ms_per_interval": 100}
        previous = current
    raise RuntimeError(f"Filesystem device did not become idle within {timeout_s}s; no scan ran.")


def cache_reset_counters():
    """Read evidence of kernel cache resets without performing one."""
    names = {"drop_pagecache", "drop_slab"}
    counters = {}
    for line in Path("/proc/vmstat").read_text().splitlines():
        key, value = line.split()
        if key in names:
            counters[key] = int(value)
    if counters.keys() != names:
        raise RuntimeError("Kernel cache-reset counters are unavailable; refusing to record a cold-cache measurement.")
    return counters


def wait_for_external_reset(watch=False):
    before = cache_reset_counters()
    print("RESET REQUIRED: run sudo /usr/local/sbin/qtools-drop-caches yourself in another terminal. "
          + ("Watching kernel counters; the scan will start automatically after the reset." if watch
             else "Enter reset-complete here after it succeeds."), flush=True)
    if watch:
        while True:
            after = cache_reset_counters()
            if all(after[key] > before[key] for key in before):
                return {"before": before, "after": after}
            time.sleep(0.02)
    try:
        acknowledgment = input()
    except EOFError:
        acknowledgment = ""
    if acknowledgment != "reset-complete":
        raise RuntimeError("External reset was not confirmed; refusing to record a cold-cache measurement.")
    after = cache_reset_counters()
    if any(after[key] <= before[key] for key in before):
        raise RuntimeError("Kernel cache-reset counters did not both increase after the prompt; "
                           "refusing to record a cold-cache measurement.")
    return {"before": before, "after": after}


def filesystem_allocation(root):
    try:
        usage = os.statvfs(root)
    except OSError:
        return None
    return {"capacity_bytes": usage.f_blocks * usage.f_frsize,
            "used_bytes": (usage.f_blocks - usage.f_bfree) * usage.f_frsize,
            "available_bytes": usage.f_bavail * usage.f_frsize}


def scheduler_context(root):
    """Read inherited limits and counters; never change scheduling or limits."""
    context = {"uid": os.geteuid(), "nice": os.getpriority(os.PRIO_PROCESS, 0),
               "cpu_affinity": sorted(os.sched_getaffinity(0)), "cgroups": []}
    try:
        import ctypes
        import platform
        number = {"x86_64": 252, "aarch64": 31}.get(platform.machine())
        if number is not None:
            library = ctypes.CDLL(None, use_errno=True)
            priority = library.syscall(ctypes.c_long(number), ctypes.c_int(1), ctypes.c_int(0))
            context["inherited_ioprio"] = priority if priority >= 0 else {"errno": ctypes.get_errno()}
    except (OSError, AttributeError):
        pass
    try:
        group = current_cgroup()
        while group.is_relative_to(Path("/sys/fs/cgroup")):
            files = {}
            for name in ("cpu.max", "cpu.stat", "cpu.pressure", "io.max", "io.weight", "io.bfq.weight",
                         "io.latency", "io.prio.class", "io.cost.qos", "io.cost.model", "io.stat", "io.pressure",
                         "memory.high", "memory.max", "memory.events", "memory.pressure"):
                try:
                    files[name] = (group / name).read_text().strip()
                except OSError as error:
                    files[name] = {"errno": error.errno}
            context["cgroups"].append({"path": str(group), "files": files})
            group = group.parent
    except (OSError, RuntimeError) as error:
        context["cgroup_error"] = str(error)
    try:
        device = os.stat(root).st_dev
        location = Path(f"/sys/dev/block/{os.major(device)}:{os.minor(device)}").resolve()
        if (location / "partition").exists():
            location = location.parent
        queue = {}
        for name in ("scheduler", "nr_requests", "read_ahead_kb", "rotational",
                     "../device/queue_depth", "iosched/slice_idle", "iosched/low_latency",
                     "iosched/strict_guarantees"):
            try:
                queue[name] = (location / "queue" / name).read_text().strip()
            except OSError as error:
                queue[name] = {"errno": error.errno}
        context["device"] = {"path": str(location), "queue": queue}
    except OSError as error:
        context["device_error"] = str(error)
    return context


def worker_waits(pid):
    """Observe only our child's kernel wait locations, without tracing syscalls."""
    counts = {}
    try:
        for task in (Path("/proc") / str(pid) / "task").iterdir():
            try:
                location = (task / "wchan").read_text().strip()
                counts[location] = counts.get(location, 0) + 1
            except OSError:
                continue
    except OSError as error:
        return {"errno": error.errno}
    return counts


def run(executable, root, arguments, env, idle_io=False, io_context_probe=None, shared_io_context=False,
        best_effort_priority=None, sample_io=False):
    command = [executable, "--noprogress", "--no-color", *arguments, root]
    if idle_io or best_effort_priority is not None:
        ionice = shutil.which("ionice")
        if not ionice:
            raise RuntimeError("Explicit I/O priority requires ionice; refusing to silently use inherited I/O priority.")
        priority = ["--class", "3"] if idle_io else ["--class", "2", "--classdata", str(best_effort_priority)]
        command = [ionice, *priority, "--", *command]
    if io_context_probe:
        command = [io_context_probe, "--shared" if shared_io_context else "--independent", *command]
    context_before = scheduler_context(root)
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    stall_before = io_stall()
    io_before = filesystem_io(root)
    allocation_before = filesystem_allocation(root)
    started = time.perf_counter()
    timeline = []
    if sample_io:
        with subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, env=env) as child:
            previous = io_before
            while True:
                try:
                    _, stderr = child.communicate(timeout=1)
                    result = subprocess.CompletedProcess(command, child.returncode, None, stderr)
                    break
                except subprocess.TimeoutExpired:
                    current = filesystem_io(root)
                    timeline.append({"elapsed_s": time.perf_counter() - started,
                                     "worker_waits": worker_waits(child.pid),
                                     "filesystem_io": None if previous is None or current is None
                                     else {key: current[key] - value for key, value in previous.items()}})
                    previous = current
    else:
        result = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, env=env)
    wall = time.perf_counter() - started
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    stall_after = io_stall()
    io_after = filesystem_io(root)
    allocation_after = filesystem_allocation(root)
    context_after = scheduler_context(root)
    if result.returncode:
        raise RuntimeError(f"{executable} exited {result.returncode}:\n{result.stderr}")
    match = SCAN.search(result.stderr)
    if not match:
        raise RuntimeError(f"{executable} did not report QTOOLS_DEBUG scan counters:\n{result.stderr}")
    counters = {key.replace("-", "_"): int(value)
                for key, value in re.findall(r"([\w-]+)=(\d+)", match[2])}
    return {
        "wall_s": wall, "scan_ms": int(match[1]), **counters,
        "cpu_s": after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime,
        "cpu_user_s": after.ru_utime - before.ru_utime,
        "cpu_system_s": after.ru_stime - before.ru_stime,
        "input_blocks": after.ru_inblock - before.ru_inblock,
        "diagnostics": [line for line in result.stderr.splitlines() if line.startswith("dq[debug]:")],
        "profile": [line for line in result.stderr.splitlines() if line.startswith("dq[profile]:")],
        "io_context_probe": [line for line in result.stderr.splitlines() if line.startswith("dq[io-context-probe]:")],
        "filesystem_io": None if io_before is None or io_after is None
        else {key: io_after[key] - value for key, value in io_before.items()},
        "filesystem_allocation": {"before": allocation_before, "after": allocation_after},
        "scheduler_context": {"before": context_before, "after": context_after},
        "io_timeline": timeline,
        "system_io_stall_s": None if stall_before is None or stall_after is None
        else stall_after - stall_before,
    }


def summary(samples):
    times = [sample["wall_s"] for sample in samples]
    median = statistics.median(times)
    return {
        "median_s": median, "min_s": min(times), "max_s": max(times),
        "spread_percent": (max(times) - min(times)) / median * 100,
        "files_range": [min(sample["files"] for sample in samples),
                        max(sample["files"] for sample in samples)],
        "directories_range": [min(sample["directories"] for sample in samples),
                              max(sample["directories"] for sample in samples)],
        "input_blocks": sum(sample["input_blocks"] for sample in samples),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=lambda value: str(Path(value).resolve()))
    parser.add_argument("--binary", type=binary, metavar="[LABEL=]PATH",
                        help="build to measure; default: target/release/dq")
    parser.add_argument("--paired-binary", type=binary, metavar="[LABEL=]PATH",
                        help="with --reclaim-cache, interleave this build with --binary, reversing order every round")
    parser.add_argument("--runs", type=int, default=1,
                        help="total invocations; repeats are reported separately (default: 1)")
    parser.add_argument("--cold", action="store_true",
                        help="wait for a user-performed cache reset before every trial; never invokes sudo")
    parser.add_argument("--reclaim-cache", action="store_true",
                        help="reclaim file and metadata caches in our user-owned cgroup before every trial; no sudo")
    parser.add_argument("--isolated-reclaim", action="store_true",
                        help="with --reclaim-cache, bootstrap once, then reclaim only a dedicated user-owned benchmark cgroup")
    parser.add_argument("--wait-for-idle-io", action="store_true",
                        help="before each trial, wait up to 120s for three quiet device intervals; no scans or discarded samples")
    parser.add_argument("--sample-io", action="store_true",
                        help="diagnostic: record device counters and our child's wait locations once per second; adds monitoring overhead")
    parser.add_argument("--external-reset", action="store_true",
                        help="compatibility alias; --cold always requires a user-performed reset")
    parser.add_argument("--watch-reset", action="store_true",
                        help="with --cold, detect your reset using kernel counters and start immediately")
    parser.add_argument("--threads", type=int, help="worker count")
    parser.add_argument("--paired-threads", type=int,
                        help="worker count for --paired-binary; defaults to --threads")
    parser.add_argument("--profile", action="store_true",
                        help="record scan progress and accounting timers; adds profiling overhead")
    parser.add_argument("--audit-unlinked", action="store_true",
                        help="untimed diagnostic: sum accessible unlinked open-file allocation before cache preparation")
    parser.add_argument("--probe-io-uring", action="store_true",
                        help="diagnostic only: create and close an empty unprivileged ring, then exit")
    parser.add_argument("--idle-io", action="store_true",
                        help="diagnostic: launch dq with unprivileged idle I/O priority (ionice class 3)")
    parser.add_argument("--best-effort-priority", type=int, choices=range(8),
                        help="diagnostic: set only dq's unprivileged best-effort I/O priority (0 through 7)")
    parser.add_argument("--io-context-probe", type=Path,
                        help="diagnostic: clone-only tracer executable; defaults to independent thread I/O contexts")
    parser.add_argument("--shared-io-context", action="store_true",
                        help="with --io-context-probe, add CLONE_IO to thread creation")
    parser.add_argument("--output", type=Path, help="save every sample and the binary hash as JSON")
    args = parser.parse_args()
    if args.probe_io_uring:
        result = probe_io_uring()
        print(json.dumps(result, indent=2))
        if args.output:
            args.output.write_text(json.dumps(result, indent=2) + "\n")
        return
    try:
        label, executable = args.binary or binary(str(Path(__file__).resolve().parents[1] / "target/release/dq"))
    except argparse.ArgumentTypeError as error:
        parser.error(str(error))
    if args.runs < 1 or any(value is not None and value < 1
                            for value in (args.threads, args.paired_threads)):
        parser.error("runs and threads must be positive")
    if args.paired_threads is not None and not args.paired_binary:
        parser.error("--paired-threads requires --paired-binary")
    if args.external_reset and not args.cold:
        parser.error("--external-reset requires --cold")
    if args.watch_reset and not args.cold:
        parser.error("--watch-reset requires --cold")
    if args.reclaim_cache and args.cold:
        parser.error("choose --reclaim-cache or --cold")
    if args.isolated_reclaim and not args.reclaim_cache:
        parser.error("--isolated-reclaim requires --reclaim-cache")
    if args.paired_binary and not args.reclaim_cache:
        parser.error("--paired-binary requires --reclaim-cache")
    if args.shared_io_context and not args.io_context_probe:
        parser.error("--shared-io-context requires --io-context-probe")
    if args.idle_io and args.best_effort_priority is not None:
        parser.error("choose --idle-io or --best-effort-priority")
    probe = str(args.io_context_probe.resolve()) if args.io_context_probe else None
    if probe and (not Path(probe).is_file() or not os.access(probe, os.X_OK)):
        parser.error(f"not an executable probe: {probe}")
    if not Path(args.root).is_dir():
        parser.error(f"not a directory: {args.root}")
    arguments = [] if args.threads is None else ["--threads", str(args.threads)]
    report = {
        "root": args.root, "arguments": arguments, "profiling": args.profile,
        "wait_for_idle_io": args.wait_for_idle_io,
        "sample_io": args.sample_io,
        "io_class": "idle" if args.idle_io else "best-effort" if args.best_effort_priority is not None else "inherited",
        "io_priority": args.best_effort_priority,
        "io_context_probe": None if probe is None else {"path": probe,
            "sha256": hashlib.sha256(Path(probe).read_bytes()).hexdigest(), "shared": args.shared_io_context},
        "cache_preparation": "nearest user-owned containing slice (or current cgroup) file/slab reclaim with swappiness=0 before every trial" if args.reclaim_cache
        else "user-performed sync and Linux drop_caches=3 before every trial; kernel counters verified" if args.cold
        else "none; initial kernel cache state is uncontrolled",
        "binary": {"label": label, "path": executable,
                   "sha256": hashlib.sha256(Path(executable).read_bytes()).hexdigest()},
        "first": None, "repeats": [],
    }
    if args.paired_binary:
        paired_label, paired_executable = args.paired_binary
        report["comparison"] = {"binary": {"label": paired_label, "path": paired_executable,
            "sha256": hashlib.sha256(Path(paired_executable).read_bytes()).hexdigest()},
            "arguments": arguments if args.paired_threads is None
            else ["--threads", str(args.paired_threads)],
            "first": None, "repeats": []}
        report["trial_order"] = []
    if args.audit_unlinked:
        report["allocation_audit"] = audit_unlinked(args.root)
        print(json.dumps(report["allocation_audit"]), flush=True)
    with tempfile.TemporaryDirectory(prefix="qtools-bench-") as state:
        # Suppress background update checks and historical scan indexes in old builds.
        update_state = Path(state) / "qtools"
        update_state.mkdir()
        (update_state / "last-update-check").write_text(f"{int(time.time())}\n")
        env = {**os.environ, "QTOOLS_DEBUG": "1", "QTOOLS_NO_CACHE": "1", "XDG_STATE_HOME": state}
        env.pop("QTOOLS_PROFILE", None)
        if args.profile:
            env["QTOOLS_PROFILE"] = "1"
        env.pop("QTOOLS_INTERNAL_UPDATE_CHECK", None)
        print("Measuring cache-reclaimed trials; automatic unprivileged reclaim before each invocation." if args.reclaim_cache
              else "Measuring cold-cache trials; waiting for your reset before each invocation." if args.cold
              else "Measuring the first invocation; no metadata priming or warmups.", flush=True)
        isolated = None
        if args.isolated_reclaim:
            # Earlier scans and builds may have cache charges outside the new
            # domain. Clear those once before moving the benchmark; no scan is
            # performed or discarded during this bootstrap.
            print("Bootstrapping isolated cache ownership with containing-slice reclaim.", flush=True)
            try:
                report["cache_bootstrap"] = reclaim_caches()
                isolated = isolated_reclaim_group()
            except (OSError, RuntimeError) as error:
                report["failure"] = {"phase": "isolated-cache-bootstrap", "error": str(error)}
                if args.output:
                    args.output.write_text(json.dumps(report, indent=2) + "\n")
                raise
            report["isolated_cgroup"] = str(isolated)
            report["cache_preparation"] = "containing-slice bootstrap, then dedicated user-owned benchmark cgroup reclaim before every invocation"
            print(f"Reclaiming only {isolated} between trials.", flush=True)
        sequence = []
        for round_index in range(args.runs):
            targets = [report, report["comparison"]] if args.paired_binary else [report]
            if args.paired_binary and round_index % 2:
                targets.reverse()
            sequence.extend((round_index, target) for target in targets)
        for index, (round_index, target) in enumerate(sequence):
            selected = target["binary"]
            phase = "cache-reclaim" if args.reclaim_cache else "external-reset" if args.cold else "scan"
            try:
                preparation = (reclaim_caches(isolated) if isolated else reclaim_caches()) if args.reclaim_cache else None
                reset_counters = wait_for_external_reset(watch=args.watch_reset) if args.cold else None
                phase = "wait-for-idle-io"
                idle_preparation = wait_for_idle_io(args.root) if args.wait_for_idle_io else None
                phase = "scan"
                priority_options = {} if args.best_effort_priority is None else {"best_effort_priority": args.best_effort_priority}
                if args.sample_io:
                    priority_options["sample_io"] = True
                sample = run(selected["path"], args.root, target["arguments"], env, args.idle_io, probe, args.shared_io_context,
                             **priority_options)
            except (OSError, RuntimeError) as error:
                # Keep the failed attempt alongside any earlier measurements.
                # A refused preparation must never become a warm scan sample.
                report["failure"] = {"trial": index + 1, "phase": phase, "error": str(error)}
                if args.paired_binary:
                    report["failure"]["binary"] = selected["label"]
                if args.output:
                    args.output.write_text(json.dumps(report, indent=2) + "\n")
                raise
            sample["cache_reset"] = args.cold or args.reclaim_cache
            sample["cache_reset_source"] = "cgroup-reclaim" if args.reclaim_cache else (
                ("kernel-counter-watch" if args.watch_reset else "user-confirmed") if args.cold else None)
            sample["cache_reset_counters"] = reset_counters
            sample["cache_reclaim"] = preparation
            sample["io_idle_preparation"] = idle_preparation
            if target["first"] is None:
                target["first"] = sample
                phase = "FIRST"
            else:
                target["repeats"].append(sample)
                phase = f"repeat {index}"
            if args.cold:
                phase = f"cold trial {index + 1}"
            elif args.reclaim_cache:
                phase = f"reclaimed trial {index + 1}"
            if args.paired_binary:
                report["trial_order"].append({"trial": index + 1, "round": round_index + 1, "binary": selected["label"]})
            print(f"{phase}: {selected['label']} {sample['wall_s']:.3f}s "
                  f"files={sample['files']:,} dirs={sample['directories']:,} "
                  f"pruned={sample.get('pruned', 0):,} input-blocks={sample['input_blocks']}", flush=True)
            if args.output:
                args.output.write_text(json.dumps(report, indent=2) + "\n")
    if args.reclaim_cache:
        stats = summary([report["first"], *report["repeats"]])
        report["reclaimed_summary"] = stats
        print(f"Reclaimed trials: median {stats['median_s']:.3f}s, "
              f"range {stats['min_s']:.3f}–{stats['max_s']:.3f}s, "
              f"spread {stats['spread_percent']:.1f}%")
        if args.paired_binary:
            comparison = report["comparison"]
            comparison["reclaimed_summary"] = summary([comparison["first"], *comparison["repeats"]])
            stats = comparison["reclaimed_summary"]
            print(f"Comparison {comparison['binary']['label']}: median {stats['median_s']:.3f}s, "
                  f"range {stats['min_s']:.3f}–{stats['max_s']:.3f}s, "
                  f"spread {stats['spread_percent']:.1f}%")
    elif args.cold:
        stats = summary([report["first"], *report["repeats"]])
        report["cold_summary"] = stats
        print(f"Cold trials: median {stats['median_s']:.3f}s, "
              f"range {stats['min_s']:.3f}–{stats['max_s']:.3f}s, "
              f"spread {stats['spread_percent']:.1f}%")
    elif report["repeats"]:
        stats = summary(report["repeats"])
        report["repeat_summary"] = stats
        print(f"Repeats only: median {stats['median_s']:.3f}s, "
              f"range {stats['min_s']:.3f}–{stats['max_s']:.3f}s, "
              f"spread {stats['spread_percent']:.1f}%")
    if args.output:
        args.output.write_text(json.dumps(report, indent=2) + "\n")
        print(f"Saved {args.output}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError) as error:
        raise SystemExit(str(error))
