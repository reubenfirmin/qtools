#!/usr/bin/env python3
"""Study allocation clustering from a full dq report; never a benchmark or index.

The perfect-information schedules know each directory's allocation before opening
it. Their work counts expose potential savings, not a usable online policy, a
time prediction, or a replacement for dq's allocation certificate.
"""

import argparse
import hashlib
import heapq
import random
import re
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time


ROOT_LABEL = "[root files and metadata]"


def filesystem_space(root):
    stats = os.statvfs(root)
    size = stats.f_frsize or stats.f_bsize
    return {"capacity_bytes": stats.f_blocks * size,
            "used_bytes": (stats.f_blocks - stats.f_bfree) * size}


def capture(args):
    executable = shutil.which(args.binary)
    if not executable:
        raise RuntimeError(f"Executable not found: {args.binary}")
    root = Path(args.root).resolve()
    command = [str(Path(executable).resolve()), "--jsonl", "--precise", "--all",
               "--zeroes", "--depth", "1000000", "--errors", "--noprogress", str(root)]
    env = os.environ.copy()
    env["QTOOLS_NO_CACHE"] = "1"
    env["QTOOLS_DEBUG"] = "1"
    env.pop("QTOOLS_PROFILE", None)
    with open(executable, "rb") as binary:
        digest = hashlib.file_digest(binary, "sha256").hexdigest()
    before = filesystem_space(root)
    started = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix="dq-study-state-") as state:
        env["XDG_STATE_HOME"] = state
        stamp = Path(state) / "qtools" / "last-update-check"
        stamp.parent.mkdir()
        stamp.write_text(str(int(time.time())))
        with args.output.open("w") as output:
            result = subprocess.run(command, env=env, stdout=output,
                                    stderr=subprocess.PIPE, text=True)
    receipt = {"purpose": "offline-allocation-study", "cache_state": "uncontrolled-diagnostic",
               "command": command, "returncode": result.returncode,
               "wall_s": time.perf_counter() - started,
               "filesystem_before": before, "filesystem_after": filesystem_space(root),
               "binary_sha256": digest,
               "diagnostics": result.stderr.splitlines()}
    args.output.with_suffix(args.output.suffix + ".receipt.json").write_text(
        json.dumps(receipt, indent=2) + "\n")
    if result.returncode:
        raise RuntimeError(f"Full diagnostic scan exited {result.returncode}:\n{result.stderr}")
    print(f"Saved full directory allocations to {args.output} ({receipt['wall_s']:.3f}s diagnostic).")


def directory_rows(report):
    metadata = report["metadata"]
    if metadata["view"] != "directories" or metadata["accounting"] != "allocated":
        raise ValueError("Study requires an allocated directory-summary report.")
    if report["hidden_rows"] or metadata.get("pruned_directories", 0):
        raise ValueError("Study requires every directory; capture with --all --zeroes --precise.")
    root = Path(metadata["root"])
    rows = {}
    for row in report["rows"]:
        path = root if row["label"] == ROOT_LABEL else Path(row["label"])
        relative = path.relative_to(root).as_posix()
        if relative in rows:
            raise ValueError(f"Duplicate directory: {relative}")
        rows[relative] = {"bytes": row["bytes"], "files": row["files"]}
    if "." not in rows:
        raise ValueError("Study requires root metadata; capture with --zeroes.")
    for relative in rows:
        if relative != "." and Path(relative).parent.as_posix() not in rows:
            raise ValueError("Missing parent: report must contain direct totals for every directory.")
    if len(rows) != metadata["directories"]:
        raise ValueError("Directory rows do not match the full scan's directory count.")
    if sum(row["bytes"] for row in rows.values()) != report["total_bytes"]:
        raise ValueError("Directory allocations do not sum to the report total.")
    return rows


def perfect_information_schedule(rows, target, key):
    """Open each chosen directory's ancestors and measure all their direct files.

    All sizes are visible to the chooser, including unopened directories. File
    counts omit duplicate hard links; the real scanner must also resolve their
    attribution. The schedule intentionally reports these optimistic limitations.
    """
    measured = files = 0
    opened = set()
    for relative in sorted(rows, key=lambda path: (key(rows[path]), path), reverse=True):
        if measured >= target:
            break
        ancestors = []
        current = relative
        while current not in opened:
            ancestors.append(current)
            if current == ".":
                break
            current = Path(current).parent.as_posix()
        for current in reversed(ancestors):
            opened.add(current)
            measured += rows[current]["bytes"]
            files += rows[current]["files"]
    return {"directories": len(opened), "unique_files": files,
            "metadata_work_proxy": files + 2 * len(opened),
            "measured_bytes": measured, "target_met": measured >= target}


def analyze(args):
    report = json.loads(args.input.read_text())
    receipt = json.loads(args.input.with_suffix(args.input.suffix + ".receipt.json").read_text())
    rows = directory_rows(report)
    capacity = receipt["filesystem_after"]["capacity_bytes"]
    used = max(receipt["filesystem_before"]["used_bytes"], receipt["filesystem_after"]["used_bytes"])
    budget = int(capacity * args.max_error / 100 * 0.99)
    target = max(0, used - budget)
    children = dict.fromkeys(rows, 0)
    for relative in rows:
        if relative != ".":
            children[Path(relative).parent.as_posix()] += 1
    subtree = {path: [row["bytes"], row["files"], 1] for path, row in rows.items()}
    for path in sorted(rows, key=lambda path: len(Path(path).parts), reverse=True):
        if path == ".":
            continue
        parent = subtree[Path(path).parent.as_posix()]
        for index, value in enumerate(subtree[path]):
            parent[index] += value
    by_direct_size = []
    for low, high in [(0, 4096), (4096, 65536), (65536, 1048576),
                      (1048576, 16777216), (16777216, 268435456),
                      (268435456, 4294967296), (4294967296, None)]:
        members = [row for row in rows.values()
                   if (row["bytes"] >= low if low == 0 else row["bytes"] > low)
                   and (high is None or row["bytes"] <= high)]
        by_direct_size.append({"lower_bytes": low, "upper_bytes": high,
                               "directories": len(members),
                               "bytes": sum(row["bytes"] for row in members),
                               "unique_files": sum(row["files"] for row in members)})
    empty_local = [path for path, row in rows.items() if row["files"] == 0]
    hidden_mass = [path for path in empty_local if subtree[path][0] >= 1024**3]
    result = {"purpose": "offline-allocation-study", "source": str(args.input),
              "limitations": ["Uncontrolled diagnostic cache state; no wall-time prediction.",
                              "Chooser sees allocations of unopened directories.",
                              "Unique-file counts use canonical hard-link attribution and omit duplicate-link reads.",
                              "Pending hard-link attribution, file sampling, and I/O locality are not modeled.",
                              "Snapshot may change during its scan; unreadable entries remain missing."],
              "scan_metadata": report["metadata"], "capacity_bytes": capacity,
              "filesystem_used_bytes": used, "allocation_budget_bytes": budget,
              "target_measured_bytes": target, "snapshot_bytes": report["total_bytes"],
              "directory_direct_allocation_distribution": by_direct_size,
              "perfect_information_schedules": {
                  "largest_direct_allocation": perfect_information_schedule(rows, target, lambda row: row["bytes"]),
                  "direct_allocation_per_work": perfect_information_schedule(
                      rows, target, lambda row: row["bytes"] / (row["files"] + 2))},
              "directories_without_direct_unique_files": len(empty_local),
              "directories_without_direct_unique_files_containing_gib_subtrees": len(hidden_mass),
              "largest_subtrees_without_direct_unique_files": [
                  {"path": path, "bytes": subtree[path][0], "unique_files": subtree[path][1],
                   "directories": subtree[path][2], "immediate_children": children[path]}
                  for path in sorted(hidden_mass, key=lambda path: subtree[path][0], reverse=True)[:24]]}
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"target_measured_bytes": target,
                      "perfect_information_schedules": result["perfect_information_schedules"],
                      "directories_without_direct_unique_files": len(empty_local),
                      "hidden_gib_subtrees": len(hidden_mass)}, indent=2))
    print(f"Saved allocation study to {args.output}.")


def file_trace_rows(path):
    """Load a complete research trace without consulting the live filesystem."""
    rows = {}
    footer = None
    with path.open() as source:
        header = json.loads(next(source))
        if header.get("purpose") != "dq-offline-file-metadata-trace" or header.get("version") != 1:
            raise ValueError("Not a supported file metadata trace.")
        for line in source:
            record = json.loads(line)
            if footer is not None:
                raise ValueError("Records follow the trace completion footer.")
            if "complete" in record:
                footer = record
                continue
            relative = record["path"]
            if relative in rows:
                raise ValueError("Duplicate directory in the trace.")
            entries = record["files"]
            rows[relative] = {"bytes": record["allocated"] + sum(entry[3] for entry in entries),
                              "files": len(entries), "entries": entries,
                              "metadata": (record["inode"], record["allocated"]),
                              "child_order": record["children"],
                              "child_metadata": record.get("child_metadata", {}),
                              "started_unix_ns": header.get("started_unix_ns", 0)}
    if footer is None or footer.get("complete") is not True:
        raise ValueError("Incomplete metadata capture; no scheduling result is valid.")
    if len(rows) != footer["directories"] or sum(row["files"] for row in rows.values()) != footer["files"]:
        raise ValueError("Trace counts do not match its completion footer.")
    if "." not in rows:
        raise ValueError("The trace has no root directory.")
    for relative, row in rows.items():
        if relative != "." and Path(relative).parent.as_posix() not in rows:
            raise ValueError("The trace is missing a parent directory.")
        for child in row["child_order"]:
            child_path = child if relative == "." else relative + "/" + child
            if child_path not in rows:
                raise ValueError("A listed child has no directory record.")
    return rows, header, footer


def simulate_policy(rows, target, policy, seed):
    """Reveal each directory only when opened; assume homogeneous direct files.

    Directory snapshots assume homogeneous direct files. A file trace instead
    preserves actual allocations, native entry order and hard-link observations.
    Neither model predicts wall time or the storage device's request schedule.
    """
    paths = sorted(rows, key=lambda path: (path.count("/"), path))
    paths.remove("."); paths.insert(0, ".")
    ids = {path: index for index, path in enumerate(paths)}
    traced = "entries" in rows["."]
    suffix_match = re.search(r"suffix(?:pilot)?([0-9]+)", policy)
    uses_suffix = traced and suffix_match is not None
    suffix_support = int(suffix_match[1]) if suffix_match else 0
    parents = [-1] + [ids[path.rsplit("/", 1)[0] if "/" in path else "."] for path in paths[1:]]
    depths = [0] + [path.count("/") + 1 for path in paths[1:]]
    children = [[] for _ in paths]
    names = []
    for index, path in enumerate(paths):
        name = path.rsplit("/", 1)[-1]
        if len(name) >= 2 and all(char in "0123456789abcdefABCDEF" for char in name):
            name = "[hex:" + str(len(name).bit_length()) + "]"
        if "-family" in policy:
            # Generic name families; no dataset-specific names or allocation.
            name = re.sub(r"[0-9a-fA-F]{8,}", "[digest]", name)
            name = re.sub(r"[0-9]+", "[number]", name)
        if "-stem" in policy:
            stem = re.split(r"[-_.]", name.lstrip("."), maxsplit=1)[0]
            name = stem or name
        names.append(name)
        if index:
            children[parents[index]].append(index)
    if policy.startswith("context"):
        names = [("." if index == 0 else names[parents[index]], name) for index, name in enumerate(names)]
    rng = random.Random(seed)
    file_rng = random.Random(seed ^ 0xD051A7)
    if traced:
        for node, path in enumerate(paths):
            children[node] = [ids[name if path == "." else path + "/" + name]
                              for name in rows[path]["child_order"]]
    else:
        for siblings in children:
            rng.shuffle(siblings)
    branch_costs = [0.0] * len(paths)
    compressed_depths = [0] * len(paths)
    report_branches = [0] * len(paths)
    for node in range(1, len(paths)):
        parent = parents[node]
        branch_costs[node] = branch_costs[parent] + math.log2(max(1, len(children[parent])))
        # The parent listing is already visible when this child is queued.
        # Empty unary containers need no choice between competing descendants.
        if depths[node] <= 3:
            compressed_depths[node] = depths[node]
        else:
            branching = len(children[parent]) > 1
            if policy.endswith("-chain"):
                branching |= rows[paths[parent]]["files"] > 0
            compressed_depths[node] = compressed_depths[parent] + int(branching)
        report_branches[node] = node if depths[node] <= 3 else report_branches[parent]
    observed_bytes = [0.0] * len(paths)
    observed_cost = [0] * len(paths)
    observed_peaks = [0.0] * len(paths)
    observed_squares = [0.0] * len(paths)
    batch_peak = batch_squares = 0.0
    direct_observed_bytes = [0.0] * len(paths)
    direct_observed_cost = [0] * len(paths)
    observed_directories = [0] * len(paths)
    cohorts = {}
    cohort_peaks = {}
    buckets = [[] for _ in range(512)]
    frontier = {}
    shallow_frontier = []
    branch_frontiers = {}
    branch_visits = {}
    fair_frontier = []
    next_work = 0
    pops = 0
    occupied = 0
    queued = 0
    measured = 0.0
    files = directories = 0
    checkpoints = []
    late_allocations = {}
    pending_entries = {}
    pending_suffixes = {}
    suffix_observations = {}
    seen_inodes = set()
    link_observations = {}
    pending_link_bytes = 0
    namespace_inodes = {}
    settled_namespace_links = set()
    metadata_queries = 0
    random_depth_frontiers = {}
    random_depth_threshold = int(policy[4:].split("-")[0]) if policy.startswith("rand") else -1
    depth_yields = {}
    structural_classes = [None] * len(paths)
    structural_yields = {}
    name_visits = {}
    exploration_depths = {}
    mix_interval = int(policy.split("-mix", 1)[1]) if "-mix" in policy else 0
    refresh_interval = int(policy.split("-refresh", 1)[1]) if "-refresh" in policy else 4096
    optimistic_links = policy.endswith("-optimisticlinks")
    feature_keys = []
    feature_yields = {}
    path_keys = []
    path_yields = {}
    path_peaks = {}
    cache_ancestors = [False] * len(paths)
    path_order = int(policy.split("-path", 1)[1].split("-", 1)[0]) if "-path" in policy else 0
    path_support = int(policy.split("-support", 1)[1]) if "-support" in policy else 8
    directory_cost = int(policy.split("-dircost", 1)[1]) if "-dircost" in policy else 2
    if path_order:
        components = []
        for node, name in enumerate(names):
            context = (components[parents[node]] + (name,)) if node else (name,)
            components.append(context[-path_order:])
            cache_ancestors[node] = (cache_ancestors[parents[node]] if node else False) or name in ("cache", ".cache", ".next")
            keys = []
            for length in range(1, min(path_order, len(context)) + 1):
                suffix = context[-length:]
                if policy.endswith("-skip"):
                    # Only directory names from opened parents are features.
                    # A wildcard lets layouts share evidence across variable
                    # package/project names without exposing child metadata.
                    keys.extend(suffix[:gap] + (None,) + suffix[gap + 1:]
                                for gap in range(1, length - 1))
                keys.append(suffix)
            path_keys.append(keys)
    inode_yields = {}
    inode_shift = int(policy.split("-inode", 1)[1].split("-", 1)[0]) if "-inode" in policy else 0
    inode_groups = [rows[path]["metadata"][0] >> inode_shift for path in paths] if traced and inode_shift else []
    directory_probes = 0
    probe_yields = {}
    probe_keys = [None] * len(paths)
    probe_hints = [None] * len(paths)
    if "-probe-" in policy:
        for node in range(1, len(paths)):
            name = paths[node].rsplit("/", 1)[-1]
            hint = rows[paths[parents[node]]].get("child_metadata", {}).get(name)
            probe_hints[node] = hint
            if hint is not None:
                if policy.endswith("-structure"):
                    probe_keys[node] = (max(1, hint["size"]).bit_length(), max(1, hint["links"]).bit_length())
                else:
                    age_days = max(0, rows["."].get("started_unix_ns", 0) - hint["modified"]) // 86_400_000_000_000
                    probe_keys[node] = int(age_days + 1).bit_length()
    if "-features" in policy:
        for node, name in enumerate(names):
            parent_name = names[parents[node]] if node else "."
            keys = [("name", name), ("parent", parent_name), ("pair", parent_name, name)]
            feature_keys.append(keys)

    def parent_structure(node, mean):
        # Only the already opened parent's listing and measured pilot are used.
        key = (rows[paths[node]]["files"].bit_length(), len(children[node]).bit_length())
        if "shape-mean" in policy:
            key += (max(1, int(mean)).bit_length() // 4,)
        return key

    def add_random_depth(work_id, work, rank):
        if rank <= random_depth_threshold:
            if policy.endswith(("-ranked", "-parent", "-branch")):
                key = (rank if policy.endswith("-ranked") else parents[work[0]]
                       if policy.endswith("-parent") else report_branches[work[0]])
                random_depth_frontiers.setdefault(depths[work[0]], {}).setdefault(key, []).append(work_id)
            else:
                random_depth_frontiers.setdefault(depths[work[0]], []).append(work_id)

    def suffix_key(name):
        suffix = name.rsplit(".", 1)[-1] if "." in name else ""
        return suffix.lower() if 0 < len(suffix) <= 16 and suffix.isascii() else ""

    def suffix_prediction(node, fallback):
        best_key, best_mean = None, -1
        for key, indices in pending_suffixes[node].items():
            if not indices:
                continue
            allocation, count = suffix_observations.get(key, (0, 0))
            mean = allocation / count if count >= suffix_support else fallback
            if mean > best_mean:
                best_key, best_mean = key, mean
        return best_key, best_mean

    def settle_namespace_link(inode):
        nonlocal pending_link_bytes, metadata_queries
        if inode in settled_namespace_links or inode not in link_observations:
            return
        _, links, allocated = link_observations[inode]
        count = namespace_inodes.get(inode, 0)
        if count > links:
            raise ValueError("Fresh directory names exceed the reported hard-link count.")
        if count == links:
            # Pessimistically charge a fresh canonical-path validation read.
            # Runtime would also verify its inode, allocation and link count.
            metadata_queries += 1
            settled_namespace_links.add(inode)
            pending_link_bytes -= allocated

    def observe_namespace(node):
        if policy != "linkjoin" and not policy.endswith("-linkjoin"):
            return
        for entry in rows[paths[node]]["entries"]:
            # Only the inode identity is exposed by the fresh directory read.
            inode = entry[1]
            namespace_inodes[inode] = namespace_inodes.get(inode, 0) + 1
            settle_namespace_link(inode)

    def measure_entries(node, selected):
        nonlocal measured, pending_link_bytes, metadata_queries, batch_peak, batch_squares
        measured_before = measured
        resolved_before = measured - pending_link_bytes
        allocation = 0
        for index in selected:
            _, inode, links, allocated = rows[paths[node]]["entries"][index]
            if (policy == "linkjoin" or policy.endswith("-linkjoin")) and inode in link_observations:
                allocation += link_observations[inode][2]
                continue
            metadata_queries += 1
            allocation += allocated
            batch_peak = max(batch_peak, allocated)
            batch_squares += allocated * allocated
            if uses_suffix:
                key = suffix_key(rows[paths[node]]["entries"][index][0])
                total, count = suffix_observations.get(key, (0, 0))
                suffix_observations[key] = total + allocated, count + 1
            if inode not in seen_inodes:
                seen_inodes.add(inode)
                measured += allocated
                if links > 1:
                    pending_link_bytes += allocated
            if links > 1:
                observed, previous_links, previous_allocated = link_observations.get(inode, (0, links, allocated))
                if (previous_links, previous_allocated) != (links, allocated):
                    raise ValueError("Hard-link allocation or link count changed in the capture.")
                observed += 1
                if observed > links:
                    raise ValueError("The trace exposes more aliases than reported hard links.")
                link_observations[inode] = observed, links, allocated
                if policy == "linkjoin" or policy.endswith("-linkjoin"):
                    settle_namespace_link(inode)
                elif observed == links:
                    pending_link_bytes -= allocated
        if policy.endswith("-certyield"):
            return measured - pending_link_bytes - resolved_before
        if policy.endswith("-unique"):
            return measured - measured_before
        return allocation

    def yield_rank(average):
        value = max(1, int(average))
        exponent = value.bit_length() - 1
        half = int(exponent > 0 and bool(value & (1 << (exponent - 1))))
        return min(63, exponent * 2 + half)

    def priority(work):
        node, tail, mean = work
        supported_path = False
        if tail:
            if uses_suffix:
                _, mean = suffix_prediction(node, mean)
            if "-tailcost" in policy:
                mean /= int(policy.split("-tailcost", 1)[1])
            if "-tailboost" in policy:
                mean *= int(policy.split("-tailboost", 1)[1])
            if policy.endswith("-tailtotal"):
                mean *= tail
            rank = yield_rank(mean)
            if policy.endswith("-taildepth"):
                rank = max(0, rank - min(63, max(0, depths[node] - 3)))
            return rank + (64 if policy.startswith("signed") else 0)
        depth = depths[node]
        if not depth:
            return 511
        average = 1024 * 1024
        if policy.startswith(("cohort", "context")):
            cohort_bytes, cohort_cost = cohorts.get(names[node], (0.0, 0))
            if cohort_cost >= 8:
                average = cohort_bytes / cohort_cost
                if policy == "context-max":
                    average = cohort_peaks.get(names[node], average)
        else:
            ancestor = parents[node]
            ancestor_means = []
            ancestor_limit = int(policy.split("-ancmax", 1)[1]) if "-ancmax" in policy else 1
            while ancestor > 0:
                cost = observed_cost[ancestor]
                allocation = observed_bytes[ancestor]
                if policy.endswith("-descendants"):
                    cost -= direct_observed_cost[ancestor]
                    allocation -= direct_observed_bytes[ancestor]
                supported = cost >= 8
                if policy.endswith("-positiveevidence"):
                    supported |= allocation > 0 and cost > 0
                elif policy.endswith("-nonzeroancestor"):
                    supported &= allocation > 0
                if supported and (not policy.startswith("broad") or depths[ancestor] <= 3):
                    average = allocation / cost
                    if policy.endswith("-peak"):
                        average = observed_peaks[ancestor]
                    elif policy.endswith("-peakblend"):
                        average = math.sqrt(average * observed_peaks[ancestor])
                    elif policy.endswith("-sizebiased"):
                        average = observed_squares[ancestor] / max(1, allocation)
                    if policy.startswith("diryield"):
                        average = observed_bytes[ancestor] / max(1, observed_directories[ancestor] * int(policy[8:]))
                    ancestor_means.append(average)
                    if len(ancestor_means) >= ancestor_limit:
                        break
                ancestor = parents[ancestor]
            if ancestor_means:
                average = max(ancestor_means)
            if (policy == "novelty" or policy.endswith("-novelty")) and names[node] not in cohorts:
                average = max(average, 1024 * 1024)
        if "-count" in policy:
            prior = int(policy.split("-count", 1)[1]) * 1024
            average = max(average, prior / math.sqrt(1 + name_visits.get(names[node], 0)))
        if policy.endswith("-rarity"):
            average /= (1 + name_visits.get(names[node], 0)) ** 0.25
        if "-shape" in policy:
            total, cost = structural_yields.get(structural_classes[node], (0.0, 0))
            if cost >= 8:
                prediction = total / cost
                average = max(average, prediction) if policy.endswith("-max") else prediction
        if "-features" in policy:
            predictions = []
            for key in feature_keys[node]:
                total, cost = feature_yields.get(key, (0.0, 0))
                if cost >= 8:
                    predictions.append(total / cost)
            if predictions:
                if policy.endswith("-max"):
                    average = max(average, *predictions)
                elif policy.endswith("-specific"):
                    average = predictions[-1]
                else:
                    average = sum(predictions) / len(predictions)
        if path_order:
            # Names are known from the opened parent. Back off to a shorter
            # path context until current-invocation observations support it.
            for key in reversed(path_keys[node]):
                total, cost = path_yields.get(key, (0.0, 0))
                if cost >= path_support:
                    prediction = path_peaks.get(key, total / cost) if policy.endswith("-nodepeak") else total / cost
                    average = max(average, prediction) if policy.endswith("-max") else prediction
                    supported_path = True
                    break
            if policy.endswith("-cacheprior") and cache_ancestors[node]:
                # A standard cache name is a generic exploration hint only.
                # No estimated allocation enters the stopping certificate.
                average = max(average, 1024 * 1024)
        if inode_groups:
            # A fresh getdents record exposes a child's inode when its parent
            # opens. The trace stores the same identity in the child's row;
            # no unopened allocation or other child metadata is consulted.
            total, cost = inode_yields.get(inode_groups[node], (0.0, 0))
            if cost >= 8:
                prediction = total / cost
                average = max(average, prediction) if policy.endswith("-max") else prediction
        if probe_hints[node] is not None:
            hint = probe_hints[node]
            if policy.endswith("-density"):
                if hint["links"] == 2:
                    average *= min(128, max(1, hint["size"] / 4096))
            elif not policy.endswith("-newer"):
                total, cost = probe_yields.get(probe_keys[node], (0.0, 0))
                if cost >= 8:
                    prediction = total / cost
                    average = max(average, prediction) if policy.endswith("-agemax") else prediction
        if policy in ("local4", "broad4", "cohort4", "context4"):
            average /= 4
        rank = yield_rank(average)
        if policy.endswith("-probe-newer") and probe_hints[node] is not None:
            rank = max(0, 48 - 2 * probe_keys[node])
        if policy.endswith("-gitpacks"):
            # Git's documented layout stores consolidated objects in packs.
            # Names only guide traversal; all allocation still needs metadata.
            name = names[node]
            parent_name = names[parents[node]] if node else "."
            if name == ".git" or (parent_name == ".git" and name == "objects") or (parent_name == "objects" and name == "pack"):
                rank = 63
                return (4 - depth) * 64 + rank if depth <= 3 else rank
        if policy.startswith("signed"):
            rank = max(1, 64 + rank - min(63, max(0, depth - 3) * int(policy[6:])))
            return (4 - depth) * 128 + rank if depth <= 3 else rank
        if policy in ("depth1", "depth2", "depth4", "linkjoin") or policy.startswith(("explore", "branch", "fair", "diryield", "recent", "suffix", "rand")):
            penalty = int(policy[-1]) if policy.startswith("depth") else 1
            if not (supported_path and policy.endswith("-unpenalized")):
                penalty_depth = compressed_depths[node] if policy.endswith(("-chain", "-branchpenalty")) else depth
                rank = max(0, rank - max(0, penalty_depth - 3) * penalty)
        return (4 - depth) * 64 + rank if depth <= 3 else rank

    def push(work):
        nonlocal occupied, queued, next_work
        rank = priority(work)
        work_id = next_work
        next_work += 1
        frontier[work_id] = work
        if mix_interval and not work[1]:
            exploration_depths.setdefault(depths[work[0]], []).append(work_id)
        buckets[rank].append(work_id)
        add_random_depth(work_id, work, rank)
        if not work[1] and policy.startswith(("explore", "branch")):
            node = work[0]
            exploration_cost = (branch_costs[node] + math.log2(depths[node] + 1)
                                if policy.startswith("branch") else depths[node])
            heapq.heappush(shallow_frontier, (exploration_cost, work_id))
        if not work[1] and policy.startswith("fair"):
            branch = report_branches[work[0]]
            pending = branch_frontiers.setdefault(branch, [])
            if not pending:
                heapq.heappush(fair_frontier, (branch_visits.get(branch, 0), branch))
            pending.append(work_id)
        occupied |= 1 << rank
        queued += 1

    def observe(node, allocation, cost, directory=False):
        depth = depths[node]
        total, count = depth_yields.get(depth, (0.0, 0))
        depth_yields[depth] = total + allocation, count + cost
        direct_observed_bytes[node] += allocation
        direct_observed_cost[node] += cost
        if probe_keys[node] is not None:
            key = probe_keys[node]
            total, count = probe_yields.get(key, (0.0, 0))
            probe_yields[key] = total + allocation, count + cost
        if inode_groups:
            key = inode_groups[node]
            total, count = inode_yields.get(key, (0.0, 0))
            inode_yields[key] = total + allocation, count + cost
        if "-features" in policy:
            # Reward is credited only to the operation's own directory.
            # Nested ancestor observations would count the same work repeatedly.
            for key in feature_keys[node]:
                total, count = feature_yields.get(key, (0.0, 0))
                feature_yields[key] = total + allocation, count + cost
        if "-shape" in policy and structural_classes[node] is not None:
            key = structural_classes[node]
            total, count = structural_yields.get(key, (0.0, 0))
            structural_yields[key] = total + allocation, count + cost
        path_observation_node = node
        while node >= 0:
            if path_order and (not policy.endswith("-direct") or node == path_observation_node):
                for key in path_keys[node]:
                    total, count = path_yields.get(key, (0.0, 0))
                    total += allocation
                    count += cost
                    if "-recent" in policy:
                        window = int(policy.split("recent", 1)[1])
                        if count > window:
                            total *= window / count
                            count = window
                    path_yields[key] = total, count
            observed_bytes[node] += allocation
            observed_cost[node] += cost
            if path_order and policy.endswith("-nodepeak") and observed_cost[node] >= path_support:
                peak = observed_bytes[node] / observed_cost[node]
                for key in path_keys[node]:
                    path_peaks[key] = max(path_peaks.get(key, 0), peak)
            if "-peak" in policy or policy.endswith("-sizebiased"):
                peak = batch_peak if traced else allocation / max(1, cost)
                squares = batch_squares if traced else allocation * peak
                observed_peaks[node] = max(observed_peaks[node], peak)
                observed_squares[node] += squares
            observed_directories[node] += int(directory)
            if policy.startswith("recent") or "-recent" in policy:
                window = int(policy.split("recent", 1)[1])
                if observed_cost[node] > window:
                    observed_bytes[node] *= window / observed_cost[node]
                    observed_cost[node] = window
            if node:
                name = names[node]
                cohort_bytes, cohort_cost = cohorts.get(name, (0.0, 0))
                cohorts[name] = (cohort_bytes + allocation, cohort_cost + cost)
                cohort_peaks[name] = max(cohort_peaks.get(name, 0.0), allocation / cost)
            node = parents[node]

    def rerank():
        nonlocal buckets, occupied
        refreshed = [[] for _ in range(512)]
        random_depth_frontiers.clear()
        mask = 0
        for rank in range(511, -1, -1):
            for work_id in reversed(buckets[rank]):
                work = frontier.get(work_id)
                if work is None:
                    continue
                new_rank = priority(work)
                refreshed[new_rank].append(work_id)
                add_random_depth(work_id, work, new_rank)
                mask |= 1 << new_rank
        for bucket in refreshed:
            bucket.reverse()
        buckets, occupied = refreshed, mask

    push((0, 0, 0.0))
    next_checkpoint = 0.5
    while queued and measured - (0 if optimistic_links else pending_link_bytes) < target:
        batch_peak = batch_squares = 0.0
        late = measured >= target * 0.9
        pops += 1
        chosen = None
        if mix_interval and pops % mix_interval == 0:
            while exploration_depths and chosen is None:
                depth = rng.choice(list(exploration_depths))
                pending = exploration_depths[depth]
                index = rng.randrange(len(pending))
                work_id = pending[index]
                pending[index] = pending[-1]
                pending.pop()
                if not pending:
                    del exploration_depths[depth]
                chosen = frontier.pop(work_id, None)
        interval = (int(policy[7:]) if policy.startswith("explore")
                    else int(policy[6:]) if policy.startswith("branch")
                    else int(policy[4:]) if policy.startswith("fair") else 0)
        if chosen is None and interval and pops % interval == 0 and policy.startswith("fair"):
            while fair_frontier:
                visits, branch = heapq.heappop(fair_frontier)
                if visits != branch_visits.get(branch, 0):
                    continue
                pending = branch_frontiers[branch]
                while pending:
                    chosen = frontier.pop(pending.pop(), None)
                    if chosen is not None:
                        break
                if chosen is not None:
                    break
        elif chosen is None and interval and pops % interval == 0:
            while shallow_frontier:
                _, work_id = heapq.heappop(shallow_frontier)
                chosen = frontier.pop(work_id, None)
                if chosen is not None:
                    break
        while chosen is None:
            rank = occupied.bit_length() - 1
            if rank <= random_depth_threshold:
                while random_depth_frontiers:
                    available = list(random_depth_frontiers)
                    if policy.endswith(("-shallow", "-deep", "-population", "-yield")):
                        weights = []
                        for candidate in available:
                            if policy.endswith("-shallow"):
                                weight = 1 / max(1, candidate - 3)
                            elif policy.endswith("-deep"):
                                weight = max(1, candidate - 3)
                            elif policy.endswith("-population"):
                                weight = len(random_depth_frontiers[candidate])
                            else:
                                total, count = depth_yields.get(candidate, (0.0, 0))
                                weight = 1 + min(16, math.sqrt(total / max(1, count) / 4096))
                            weights.append(weight)
                        depth = rng.choices(available, weights=weights, k=1)[0]
                    else:
                        depth = rng.choice(available)
                    group = random_depth_frontiers[depth]
                    group_key = (max(group) if policy.endswith("-ranked") else rng.choice(list(group))
                                 if policy.endswith(("-parent", "-branch")) else None)
                    pending = group[group_key] if group_key is not None else group
                    index = len(pending) - 1 if policy.endswith("-lifo") else rng.randrange(len(pending))
                    work_id = pending[index]
                    pending[index] = pending[-1]
                    pending.pop()
                    if not pending:
                        if group_key is not None:
                            del group[group_key]
                        if group_key is None or not group:
                            del random_depth_frontiers[depth]
                    chosen = frontier.pop(work_id, None)
                    if chosen is not None:
                        break
                if chosen is not None:
                    break
            work_id = buckets[rank].pop()
            if not buckets[rank]:
                occupied &= ~(1 << rank)
            chosen = frontier.pop(work_id, None)
        node, tail, mean = chosen
        queued -= 1
        if not tail and policy.startswith("fair"):
            branch = report_branches[node]
            branch_visits[branch] = branch_visits.get(branch, 0) + 1
            if branch_frontiers.get(branch):
                heapq.heappush(fair_frontier, (branch_visits[branch], branch))
        if tail:
            key = None
            if uses_suffix:
                key, predicted = suffix_prediction(node, mean)
                chunk = 32 if predicted <= 4096 and children[node] else 256
                count = min(tail, chunk, len(pending_suffixes[node][key]))
            else:
                chunk = 32 if mean <= 4096 and children[node] else 256
                count = min(tail, chunk)
            if traced:
                source = pending_suffixes[node][key] if key is not None else pending_entries[node]
                if policy.endswith("-tailstride"):
                    positions = {0 if count == 1 else index * (len(source) - 1) // (count - 1)
                                 for index in range(count)}
                    selected = [entry for index, entry in enumerate(source) if index in positions]
                    source[:] = [entry for index, entry in enumerate(source) if index not in positions]
                else:
                    selected = [source.pop() for _ in range(count)]
                allocation = measure_entries(node, selected)
                mean = allocation / count
            else:
                allocation = mean * count
                measured += allocation
            files += count
            credited = allocation
            observe(node, allocation, count + 1)
            if tail > count:
                push((node, tail - count, mean))
        else:
            row = rows[paths[node]]
            if "-probe-" in policy:
                directory_probes += len(children[node])
            if traced:
                observe_namespace(node)
                leaf_limit = policy.split("-leaf", 1)[1] if "-leaf" in policy else "32"
                leaf_limit = row["files"] if leaf_limit == "all" else int(leaf_limit)
                limit = leaf_limit if not children[node] and row["files"] <= leaf_limit else 8
                count = min(limit, row["files"])
                selected = {0 if count == 1 else index * (row["files"] - 1) // (count - 1)
                            for index in range(count)}
                if "-suffixpilot" in policy and count < row["files"]:
                    predicted = []
                    for index, entry in enumerate(row["entries"]):
                        total, observed = suffix_observations.get(suffix_key(entry[0]), (0.0, 0))
                        if observed >= suffix_support:
                            predicted.append((total / observed, index))
                    if predicted:
                        # Use only already measured type statistics. Preserve
                        # half the pilot for native-order exploration.
                        predicted.sort(reverse=True)
                        selected = {index for _, index in predicted[:count // 2]}
                        remaining = count - len(selected)
                        selected.update(0 if remaining == 1 else index * (row["files"] - 1) // (remaining - 1)
                                        for index in range(remaining))
                        for index in range(row["files"]):
                            if len(selected) >= count:
                                break
                            selected.add(index)
                pending_entries[node] = [index for index in range(row["files"]) if index not in selected]
                if policy.endswith("-tailrandom"):
                    file_rng.shuffle(pending_entries[node])
                if uses_suffix:
                    groups = pending_suffixes[node] = {}
                    for index in pending_entries[node]:
                        groups.setdefault(suffix_key(row["entries"][index][0]), []).append(index)
                inode, metadata = row["metadata"]
                if inode not in seen_inodes:
                    seen_inodes.add(inode)
                    measured += metadata
                allocation = measure_entries(node, sorted(selected))
                mean = allocation / count if count else 0.0
                if policy.endswith("-cheapleaf512") and not children[node] and row["files"] <= 512 and mean <= 4096:
                    remaining = pending_entries[node]
                    allocation += measure_entries(node, remaining)
                    count += len(remaining)
                    pending_entries[node] = []
                    mean = allocation / count if count else 0.0
            else:
                leaf_limit = policy.split("-leaf", 1)[1] if "-leaf" in policy else "32"
                leaf_limit = row["files"] if leaf_limit == "all" else int(leaf_limit)
                count = min(leaf_limit if not children[node] and row["files"] <= leaf_limit else 8, row["files"])
                metadata = min(4096, row["bytes"]) if row["files"] else row["bytes"]
                mean = (row["bytes"] - metadata) / row["files"] if row["files"] else 0.0
                allocation = mean * count
                measured += metadata + allocation
            files += count; directories += 1
            credited = metadata + allocation
            name_visits[names[node]] = name_visits.get(names[node], 0) + 1
            observe(node, allocation, count + directory_cost + (0 if policy.endswith("-nochildcost") else len(children[node])), directory=True)
            for child in children[node]:
                if "-shape" in policy:
                    structural_classes[child] = parent_structure(node, mean)
                push((child, 0, 0.0))
            if row["files"] > count:
                push((node, row["files"] - count, mean))
            if directories % refresh_interval == 0:
                rerank()
        if late:
            path = paths[node]
            late_allocations[path] = late_allocations.get(path, 0.0) + credited
        if target and measured / target >= next_checkpoint:
            checkpoints.append({"fraction_of_target": next_checkpoint, "directories": directories, "unique_files": files})
            next_checkpoint += 0.1
    return {"policy": policy, "seed": seed, "directories": directories,
            "unique_files": files, "metadata_work_proxy": files + 2 * directories,
            "metadata_queries": metadata_queries if traced else files,
            "metadata_query_work_proxy": (metadata_queries if traced else files) + 2 * directories,
            "directory_metadata_probes": directory_probes,
            "metadata_work_with_probes": (metadata_queries if traced else files) + 2 * directories + directory_probes,
            "measured_bytes": round(measured), "pending_link_bytes": pending_link_bytes,
            "target_met": measured - (0 if optimistic_links else pending_link_bytes) >= target,
            "certified_attribution_target_met": measured - pending_link_bytes >= target,
            "attribution_assumption": "perfect information removes unresolved attribution; diagnostic only" if optimistic_links else "all reported links observed",
            "file_count_kind": "native entries including repeated hard links" if traced else "canonical unique files",
            "queued_work": queued, "checkpoints": checkpoints,
            "largest_allocations_after_ninety_percent": [{"path": path, "bytes": round(allocation)}
                for path, allocation in sorted(late_allocations.items(), key=lambda item: item[1], reverse=True)[:20]]}


def simulate(args):
    if args.file_trace:
        rows, header, footer = file_trace_rows(args.input)
        receipt = {"filesystem_before": header["filesystem_before"], "filesystem_after": footer["filesystem_after"]}
    else:
        report = json.loads(args.input.read_text())
        receipt = json.loads(args.input.with_suffix(args.input.suffix + ".receipt.json").read_text())
        rows = directory_rows(report)
    capacity = receipt["filesystem_after"]["capacity_bytes"]
    used = max(receipt["filesystem_before"]["used_bytes"], receipt["filesystem_after"]["used_bytes"])
    target = max(0, used - int(capacity * args.max_error / 100 * 0.99))
    result = {"purpose": "offline-online-ordering-model", "source": str(args.input),
              "target_measured_bytes": target,
              "study_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "snapshot_sha256": hashlib.sha256(args.input.read_bytes()).hexdigest(),
              "limitations": ["No wall-time or physical-I/O model; no live performance claim.",
                              "Every direct file in a directory has the same assumed allocation; real outliers are absent.",
                              "Directory metadata is assumed to be up to 4 KiB when it has direct files, otherwise its entire direct allocation.",
                              "Canonical unique-file counts omit duplicate hard-link reads and delayed attribution.",
                              "Single coordinator; no worker batching, in-flight work or branch pruning.",
                              "Seeded sibling order replaces filesystem entry order.",
                              "Cohort statistics correlate nested observations and are scheduling heuristics."],
              "trials": []}
    if args.file_trace:
        result["purpose"] = "offline-file-trace-ordering-model"
        result["capture_errors"] = footer["errors"]
        result["limitations"] = ["No wall-time or physical-I/O model; no live performance claim.",
                                 "A sequential coordinator omits worker batching, in-flight reads and branch pruning.",
                                 "Allocation observations come from a completed capture, not an atomic filesystem snapshot.",
                                 "Native entry order, individual file allocation and reported hard-link counts are retained.",
                                 "Unknown files are revealed only when selected; predictions never credit estimated bytes."]
    for seed in args.seed:
        for policy in args.policy:
            if "suffix" in policy and not args.file_trace:
                raise ValueError("Suffix learning requires a per-file trace.")
            trial = simulate_policy(rows, target, policy, seed)
            result["trials"].append(trial)
            print(json.dumps({key: trial[key] for key in ("policy", "seed", "directories", "unique_files", "metadata_work_proxy", "target_met")}), flush=True)
            args.output.write_text(json.dumps(result, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    capture_parser = commands.add_parser("capture", help="Record direct allocations for every directory.")
    capture_parser.add_argument("root")
    capture_parser.add_argument("--binary", default="target/release/dq")
    capture_parser.add_argument("--output", required=True, type=Path)
    analyze_parser = commands.add_parser("analyze", help="Study clustering using perfect information.")
    analyze_parser.add_argument("input", type=Path)
    analyze_parser.add_argument("--max-error", type=float, default=3.0)
    analyze_parser.add_argument("--output", required=True, type=Path)
    simulation_parser = commands.add_parser("simulate", help="Compare online ordering under stated snapshot assumptions.")
    simulation_parser.add_argument("input", type=Path)
    simulation_parser.add_argument("--max-error", type=float, default=3.0)
    policy_option = simulation_parser.add_argument("--policy", nargs="+", choices=["fine", "cohort", "novelty", "local4", "broad", "broad4", "cohort4", "context", "context4", "context-max", "depth1", "depth2", "depth4", "explore2", "explore4", "explore8", "explore16", "explore32", "explore64", "branch2", "branch4", "branch8", "branch16", "branch32", "fair2", "fair4", "fair8", "fair16", "fair32", "diryield4", "diryield16", "diryield64", "recent128", "recent1024", "recent4096", "recent16384", "signed1", "signed2", "signed4", "linkjoin", "suffix8", "suffix64", "rand0", "rand16", "rand24", "rand32", "rand16-lifo", "rand16-ranked", "rand16-parent", "rand16-branch", "rand16-shallow", "rand16-deep", "rand16-population", "rand16-yield", "rand16-taildepth", "rand16-nochildcost", "rand16-cheapleaf512", "rand16-linkjoin", "rand16-tailrandom", "rand16-tailstride", "rand16-leaf128", "rand16-leaf512", "rand16-leafall", "rand16-certyield", "rand16-unique", "rand16-recent128", "rand16-descendants", "rand16-shape", "rand16-shape-max", "rand16-shape-mean", "rand16-shape-mean-max", "rand16-novelty", "rand16-count64", "rand16-count1024", "rand16-rarity", "rand16-mix2", "rand16-mix4", "rand16-mix8", "rand16-mix16", "rand16-refresh256", "rand16-refresh1024", "rand16-refresh16384", "rand16-tailcost4", "rand16-tailcost16", "rand16-tailcost64"], default=["fine", "cohort", "novelty"])
    simulation_parser.add_argument("--seed", nargs="+", type=int, default=[1])
    policy_option.choices.extend(["rand16-ancmax2", "rand16-ancmax4", "rand16-optimisticlinks",
                                 "rand16-features", "rand16-features-max", "rand16-features-specific",
                                 "rand16-gitpacks", "rand16-peak", "rand16-peakblend", "rand16-sizebiased",
                                 "rand16-inode12", "rand16-inode15", "rand16-inode18", "rand16-inode15-max",
                                 "rand16-tailboost16", "rand16-tailtotal", "rand16-positiveevidence", "rand16-nonzeroancestor",
                                 "rand16-probe-density", "rand16-probe-agemax", "rand16-probe-agereplace",
                                 "rand16-probe-structure", "rand16-probe-newer",
                                 "rand16-path3", "rand16-path5", "rand16-path3-max", "rand16-path5-direct",
                                 "rand16-path5-unpenalized", "rand16-path5-max",
                                 "rand16-path5-family", "rand16-path5-stem", "rand16-path5-family-stem",
                                 "rand16-path5-certyield", "rand16-path5-unique", "rand16-path5-recent128"])
    policy_option.choices.extend(["rand16-path7", "rand16-path9", "rand16-path5-support1",
                                  "rand16-path5-support32", "rand16-path5-support128"])
    policy_option.choices.extend(["rand16-path5-skip", "rand16-path5-nodepeak"])
    policy_option.choices.extend(["rand16-path5-chain", "rand16-path5-branchpenalty"])
    policy_option.choices.extend(["rand8-path5", "rand24-path5", "rand16-path5-cacheprior"])
    policy_option.choices.extend(["rand16-path5-linkjoin", "rand16-path5-optimisticlinks"])
    policy_option.choices.extend(["rand16-path5-nochildcost", "rand16-path5-dircost8", "rand16-path5-dircost32"])
    policy_option.choices.extend(["rand16-path5-suffix8", "rand16-path5-suffixpilot8",
                                  "rand16-path5-leaf128", "rand16-path5-leaf512"])
    simulation_parser.add_argument("--file-trace", action="store_true", help="input is a complete per-file metadata trace")
    simulation_parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.command != "capture" and not 0 <= args.max_error <= 100:
        parser.error("--max-error must be between 0 and 100")
    if args.command != "capture" and args.input.resolve() == args.output.resolve():
        parser.error("analysis output must differ from the captured report")
    try:
        {"capture": capture, "analyze": analyze, "simulate": simulate}[args.command](args)
    except (OSError, ValueError, RuntimeError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
