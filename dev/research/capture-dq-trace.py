#!/usr/bin/env python3
"""Capture fresh ordinary-user metadata for offline scheduling research.

This is not a timing benchmark, an index, or an input to dq. Each directory
record preserves its native child/file order and every observed hard link.
"""
import argparse
from collections import deque
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import json
import os
from pathlib import Path
import stat
import time


def filesystem(path):
    value = os.statvfs(path)
    return {"capacity_bytes": value.f_blocks * value.f_frsize,
            "used_bytes": (value.f_blocks - value.f_bfree) * value.f_frsize}


def read_directory(root, relative, device, directory_hints=False):
    path = root if relative == "." else root / relative
    record = {"path": relative, "inode": 0, "allocated": 0,
              "files": [], "children": [], "errors": []}
    if directory_hints:
        record["child_metadata"] = {}
    descriptor = None
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        metadata = os.fstat(descriptor)
        if metadata.st_dev != device:
            record["other_device"] = True
            return record
        record["inode"] = metadata.st_ino
        record["allocated"] = metadata.st_blocks * 512
        with os.scandir(descriptor) as entries:
            for entry in entries:
                try:
                    if entry.is_dir(follow_symlinks=False):
                        record["children"].append(entry.name)
                        if directory_hints:
                            hint = entry.stat(follow_symlinks=False)
                            record["child_metadata"][entry.name] = {
                                "inode": hint.st_ino, "allocated": hint.st_blocks * 512,
                                "size": hint.st_size, "links": hint.st_nlink,
                                "modified": hint.st_mtime_ns, "changed": hint.st_ctime_ns}
                    elif entry.is_file(follow_symlinks=False):
                        metadata = entry.stat(follow_symlinks=False)
                        if metadata.st_dev == device and stat.S_ISREG(metadata.st_mode):
                            record["files"].append([entry.name, metadata.st_ino,
                                                    metadata.st_nlink, metadata.st_blocks * 512])
                except OSError as error:
                    record["errors"].append(f"{entry.name}: {error}")
    except OSError as error:
        record["errors"].append(str(error))
    finally:
        if descriptor is not None:
            os.close(descriptor)
    return record


def capture(root, output, threads, directory_hints=False):
    if os.geteuid() == 0:
        raise RuntimeError("Run the research capture as an ordinary user.")
    root = root.resolve(strict=True)
    device = root.stat().st_dev
    started = time.monotonic()
    header = {"purpose": "dq-offline-file-metadata-trace", "version": 1,
              "root": str(root), "uid": os.geteuid(), "device": device,
              "filesystem_before": filesystem(root), "threads": threads,
              "directory_hints": directory_hints, "started_unix_ns": time.time_ns()}
    directories = files = errors = 0
    last_progress = started
    pending = deque(["."])
    # Refuse to overwrite a previous research capture.
    with output.open("x") as destination, ThreadPoolExecutor(max_workers=threads) as pool:
        destination.write(json.dumps(header) + "\n")
        running = set()
        while pending or running:
            while pending and len(running) < threads * 2:
                running.add(pool.submit(read_directory, root, pending.popleft(), device, directory_hints))
            completed, running = wait(running, return_when=FIRST_COMPLETED)
            for future in completed:
                record = future.result()
                destination.write(json.dumps(record, separators=(",", ":")) + "\n")
                directories += 1
                files += len(record["files"])
                errors += len(record["errors"])
                for name in record["children"]:
                    pending.append(name if record["path"] == "." else record["path"] + "/" + name)
            now = time.monotonic()
            if now - last_progress >= 10:
                print(f"Offline capture: {directories:,} directories, {files:,} files, "
                      f"{errors:,} errors, {now - started:.1f}s", flush=True)
                last_progress = now
        footer = {"complete": True, "directories": directories, "files": files,
                  "errors": errors, "elapsed_s": time.monotonic() - started,
                  "filesystem_after": filesystem(root)}
        destination.write(json.dumps(footer) + "\n")
    print(json.dumps(footer), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--threads", type=int, default=32)
    parser.add_argument("--directory-hints", action="store_true",
                        help="also record fresh child-directory timestamps, sizes and link counts; not a benchmark")
    args = parser.parse_args()
    if not 1 <= args.threads <= 256:
        parser.error("--threads must be between 1 and 256")
    try:
        capture(args.root, args.output, args.threads, args.directory_hints)
    except (OSError, RuntimeError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
