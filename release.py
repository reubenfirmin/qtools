#!/usr/bin/env python3
"""Publish a qtools release and verify the GitHub and stable downloads."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request


ROOT = Path(__file__).resolve().parent
REPO = "reubenfirmin/qtools"
REMOTE = f"https://github.com/{REPO}.git"
STABLE = "https://repo.qtools.sh"
ASSETS = {
    f"Qtools-{arch}.AppImage{suffix}"
    for arch in ("x86_64", "aarch64")
    for suffix in ("", ".zsync")
}


def run(*args, capture=False, env=None):
    if not capture:
        print(f"+ {shlex.join(map(str, args))}", flush=True)
    return subprocess.run(
        list(map(str, args)), cwd=ROOT, check=True, text=True,
        stdout=subprocess.PIPE if capture else None, env=env,
    ).stdout


def git_remote(*args, capture=False):
    # Use gh's HTTPS credentials without changing the checkout's remote or SSH config.
    return run("git", "-c", "credential.helper=", "-c",
               "credential.helper=!gh auth git-credential", *args, capture=capture)


def version_tuple(version):
    if not re.fullmatch(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", version):
        raise ValueError(f"expected a version such as 0.4.4, got {version!r}")
    return tuple(map(int, version.split(".")))


def next_version(current, requested=None):
    major, minor, patch = version_tuple(current)
    version = requested or f"{major}.{minor}.{patch + 1}"
    if version_tuple(version) <= (major, minor, patch):
        raise ValueError(f"release version must be newer than {current}")
    return version


def manifest_version(text):
    match = re.search(r'^version = "([^"]+)"$', text, re.MULTILINE)
    if not match:
        raise ValueError("cannot find the version in Cargo.toml")
    version_tuple(match[1])
    return match[1]


def bump_version(version):
    manifest = ROOT / "Cargo.toml"
    lock = ROOT / "Cargo.lock"
    manifest_text, lock_text = manifest.read_text(), lock.read_text()
    current = manifest_version(manifest_text)
    manifest_old = f'version = "{current}"'
    lock_old = f'name = "qtools"\nversion = "{current}"'
    if manifest_text.count(manifest_old) != 1 or lock_text.count(lock_old) != 1:
        raise ValueError("cannot locate the qtools version unambiguously")
    manifest.write_text(manifest_text.replace(manifest_old, f'version = "{version}"', 1))
    lock.write_text(lock_text.replace(lock_old, f'name = "qtools"\nversion = "{version}"', 1))


def publish(version):
    if run("git", "branch", "--show-current", capture=True).strip() != "main":
        raise ValueError("release from main")
    if run("git", "status", "--porcelain", capture=True).strip():
        raise ValueError("commit your changes before releasing; the working tree must be clean")
    git_remote("fetch", "--no-prune", REMOTE, "refs/heads/main")
    run("git", "merge-base", "--is-ancestor", "FETCH_HEAD", "HEAD")
    tag = f"v{version}"
    if run("git", "tag", "--list", tag, capture=True).strip() or git_remote(
            "ls-remote", "--tags", REMOTE, f"refs/tags/{tag}", capture=True).strip():
        raise ValueError(f"{tag} already exists; use --resume {tag} to finish verification")
    original_head = run("git", "rev-parse", "HEAD", capture=True).strip()
    bump_version(version)
    run(sys.executable, "-B", "-m", "unittest", "discover", "-s", "tests", "-p", "test_release.py")
    run("cargo", "test", "--locked", "--all-targets")
    run("cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings")
    run("cargo", "build", "--locked", "--release")
    run("git", "diff", "--check")
    if run("git", "rev-parse", "HEAD", capture=True).strip() != original_head:
        raise ValueError("HEAD changed during checks; inspect the checkout before releasing")
    changes = run("git", "status", "--porcelain", capture=True).splitlines()
    if sorted(changes) != [" M Cargo.lock", " M Cargo.toml"]:
        raise ValueError("files changed during checks; inspect the checkout before releasing")
    run("git", "add", "--", "Cargo.toml", "Cargo.lock")
    run("git", "commit", "-m", f"Release qtools {version}")
    commit = run("git", "rev-parse", "HEAD", capture=True).strip()
    run("git", "tag", "-a", tag, "-m", f"qtools {version}")
    print(f"If interrupted after pushing, continue with: ./release.py --resume {tag}", flush=True)
    git_remote("push", "--atomic", REMOTE, "HEAD:refs/heads/main", f"refs/tags/{tag}")
    git_remote("fetch", "--no-prune", REMOTE, "refs/heads/main:refs/remotes/origin/main")
    return commit


def wait_workflow(workflow, branch, commit):
    deadline = time.monotonic() + 1800
    previous = None
    while time.monotonic() < deadline:
        runs = json.loads(run("gh", "run", "list", "--repo", REPO, "--workflow", workflow,
                              "--branch", branch, "--commit", commit, "--event", "push",
                              "--json", "databaseId,status,conclusion,url", capture=True))
        if runs:
            latest = runs[0]
            state = (latest["databaseId"], latest["status"], latest["conclusion"])
            if state != previous:
                print(f"{workflow}: {latest['status']} {latest['conclusion']} {latest['url']}", flush=True)
                previous = state
            if latest["status"] == "completed":
                if latest["conclusion"] != "success":
                    run("gh", "run", "view", str(latest["databaseId"]), "--repo", REPO, "--log-failed")
                    raise ValueError(f"{workflow} failed: {latest['url']}")
                return
        time.sleep(10)
    raise ValueError(f"timed out waiting for {workflow}; inspect GitHub Actions and use --resume")


def checksum_manifest(text):
    checksums = {}
    for line in text.splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  (Qtools-(?:x86_64|aarch64)\.AppImage(?:\.zsync)?)", line)
        if not match or match[2] in checksums:
            raise ValueError("unexpected or duplicate entry in SHA256SUMS")
        checksums[match[2]] = match[1]
    if set(checksums) != ASSETS:
        raise ValueError("SHA256SUMS must contain both AppImages and both zsync files")
    return checksums


def verify_downloads(directory, version):
    if json.loads((directory / "version.json").read_text()) != {"version": version, "tag": f"v{version}"}:
        raise ValueError(f"{directory.name}: published version does not match {version}")
    checksums = checksum_manifest((directory / "SHA256SUMS").read_text())
    for name, expected in checksums.items():
        digest = hashlib.sha256()
        with (directory / name).open("rb") as artifact:
            for block in iter(lambda: artifact.read(1024 * 1024), b""):
                digest.update(block)
        if digest.hexdigest() != expected:
            raise ValueError(f"{directory.name}: checksum mismatch for {name}")
    return checksums


def download_stable(directory):
    for name in ("version.json", "SHA256SUMS", *sorted(ASSETS)):
        request = urllib.request.Request(f"{STABLE}/{name}", headers={"User-Agent": "qtools-release"})
        with urllib.request.urlopen(request, timeout=30) as response:
            with (directory / name).open("wb") as destination:
                shutil.copyfileobj(response, destination)


def verify_release(version, commit):
    tag = f"v{version}"
    with tempfile.TemporaryDirectory(prefix=f"qtools-release-{version}-") as temporary:
        work = Path(temporary)
        github, stable = work / "github", work / "stable"
        github.mkdir()
        stable.mkdir()
        run("gh", "release", "download", tag, "--repo", REPO, "--dir", github)
        expected = verify_downloads(github, version)
        deadline = time.monotonic() + 300
        while True:
            try:
                download_stable(stable)
                if verify_downloads(stable, version) != expected:
                    raise ValueError("stable checksums differ from GitHub")
                break
            except (OSError, ValueError) as error:
                if time.monotonic() >= deadline:
                    raise ValueError(f"stable channel verification failed: {error}") from error
                print(f"Waiting for stable channel: {error}", flush=True)
                time.sleep(10)
        environment = dict(os.environ, XDG_CACHE_HOME=str(work / "cache"))
        for name in sorted(ASSETS):
            if name.endswith(".AppImage"):
                run("gh", "attestation", "verify", github / name, "--repo", REPO,
                    "--signer-workflow", f"{REPO}/.github/workflows/release.yml",
                    "--source-ref", f"refs/tags/{tag}", "--source-digest", commit, env=environment)
    print("Verified GitHub and stable downloads, checksums, and both build attestations.", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", nargs="?", help="new version; defaults to the next patch version")
    parser.add_argument("--notes-file", type=Path, help="release notes; otherwise GitHub generates them")
    parser.add_argument("--resume", metavar="TAG", help="wait for and verify an already-pushed release")
    args = parser.parse_args()
    if args.resume and args.version:
        parser.error("use either a new version or --resume")
    if args.notes_file:
        args.notes_file = args.notes_file.resolve(strict=True)
        if not args.notes_file.is_file():
            parser.error("--notes-file must name a file")
    for tool in ("git", "gh", "cargo"):
        if not shutil.which(tool):
            raise ValueError(f"install {tool} before releasing")
    run("gh", "auth", "status")
    if args.resume:
        version = args.resume.removeprefix("v")
        version_tuple(version)
        git_remote("fetch", "--no-prune", REMOTE, f"refs/tags/v{version}")
        commit = run("git", "rev-parse", "FETCH_HEAD^{commit}", capture=True).strip()
    else:
        current = manifest_version((ROOT / "Cargo.toml").read_text())
        version = next_version(current, args.version)
        commit = publish(version)
    wait_workflow("ci.yml", "main", commit)
    wait_workflow("release.yml", f"v{version}", commit)
    if args.notes_file:
        run("gh", "release", "edit", f"v{version}", "--repo", REPO, "--notes-file", args.notes_file)
    verify_release(version, commit)
    print(f"Published qtools {version}: https://github.com/{REPO}/releases/tag/v{version}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Release stopped: {error}\nLocal edits, commits, and tags are kept for inspection.", file=sys.stderr)
        sys.exit(1)
