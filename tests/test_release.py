"""Check release versions and reject broken or incomplete published downloads."""

import hashlib
import contextlib
import io
import json
from pathlib import Path
import tempfile
import subprocess
import unittest
from unittest.mock import patch

import release


class ReleaseTests(unittest.TestCase):
    def test_versions_must_advance(self):
        self.assertEqual(release.next_version("0.4.3"), "0.4.4")
        self.assertEqual(release.next_version("0.4.3", "0.5.0"), "0.5.0")
        for version in ("0.4.3", "0.4.2", "v0.4.4", "0.4", "0.04.4", "0.4.4-rc1"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                release.next_version("0.4.3", version)

    def test_bump_changes_only_qtools_and_rejects_mismatched_versions(self):
        with tempfile.TemporaryDirectory() as temporary, patch.object(release, "ROOT", Path(temporary)):
            root = Path(temporary)
            manifest = '[package]\nname = "qtools"\nversion = "0.4.3"\n[dependencies]\nother = "0.4.3"\n'
            lock = 'version = 4\n[[package]]\nname = "other"\nversion = "0.4.3"\n[[package]]\nname = "qtools"\nversion = "0.4.3"\n'
            (root / "Cargo.toml").write_text(manifest)
            (root / "Cargo.lock").write_text(lock)
            release.bump_version("0.4.4")
            self.assertIn('other = "0.4.3"', (root / "Cargo.toml").read_text())
            self.assertIn('name = "other"\nversion = "0.4.3"', (root / "Cargo.lock").read_text())
            self.assertIn('name = "qtools"\nversion = "0.4.4"', (root / "Cargo.lock").read_text())
            (root / "Cargo.lock").write_text(lock)
            before = (root / "Cargo.toml").read_text()
            with self.assertRaises(ValueError):
                release.bump_version("0.4.5")
            self.assertEqual((root / "Cargo.toml").read_text(), before)

    def test_download_verification_rejects_wrong_version_corruption_and_missing_assets(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "version.json").write_text(json.dumps({"version": "0.4.4", "tag": "v0.4.4"}))
            checksums = []
            for name in sorted(release.ASSETS):
                payload = name.encode()
                (root / name).write_bytes(payload)
                checksums.append(f"{hashlib.sha256(payload).hexdigest()}  {name}\n")
            (root / "SHA256SUMS").write_text("".join(checksums))
            self.assertEqual(set(release.verify_downloads(root, "0.4.4")), release.ASSETS)
            with self.assertRaisesRegex(ValueError, "version does not match"):
                release.verify_downloads(root, "0.4.5")
            asset = sorted(release.ASSETS)[0]
            (root / asset).write_bytes(b"corrupt")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                release.verify_downloads(root, "0.4.4")
            for invalid in (checksums[0], "".join(checksums + [checksums[0]]),
                            "".join(checksums).replace(asset, "../outside")):
                with self.subTest(manifest=invalid), self.assertRaises(ValueError):
                    release.checksum_manifest(invalid)

    def test_dirty_checkout_never_bumps_commits_or_pushes(self):
        with patch.object(release, "run", side_effect=["main\n", " M README.md\n"]) as command, \
                patch.object(release, "bump_version") as bump, \
                patch.object(release, "git_remote") as remote:
            with self.assertRaisesRegex(ValueError, "working tree must be clean"):
                release.publish("0.4.4")
            self.assertEqual(command.call_count, 2)
            bump.assert_not_called()
            remote.assert_not_called()

    def test_failed_checks_never_commit_or_push(self):
        commands = []

        def command(*args, **kwargs):
            commands.append(args)
            if args[:3] == ("git", "branch", "--show-current"):
                return "main\n"
            if args[:3] == ("git", "rev-parse", "HEAD"):
                return "commit\n"
            if args[0] == release.sys.executable:
                raise subprocess.CalledProcessError(1, args)
            return ""

        with patch.object(release, "run", side_effect=command), \
                patch.object(release, "bump_version"), \
                patch.object(release, "git_remote", return_value="") as remote:
            with self.assertRaises(subprocess.CalledProcessError):
                release.publish("0.4.4")
            self.assertFalse(any(args[:2] in (("git", "commit"), ("git", "tag"))
                                 and args[2] != "--list" for args in commands))
            self.assertFalse(any(call.args[0] == "push" for call in remote.call_args_list))

    def test_failed_workflow_stops_before_release_verification(self):
        failed = [{"databaseId": 123, "status": "completed", "conclusion": "failure", "url": "run-url"}]
        with patch.object(release, "run", side_effect=[json.dumps(failed), ""]), \
                contextlib.redirect_stdout(io.StringIO()), \
                self.assertRaisesRegex(ValueError, "ci.yml failed"):
            release.wait_workflow("ci.yml", "main", "commit")

    def test_publish_with_git_pruning_preserves_tags_and_updates_tracking_ref(self):
        with tempfile.TemporaryDirectory() as temporary, contextlib.redirect_stdout(io.StringIO()):
            root, remote = Path(temporary) / "checkout", Path(temporary) / "remote.git"
            for location, extra in ((root, []), (remote, ["--bare"])):
                subprocess.run(["git", "init", "--initial-branch=main", *extra, str(location)],
                               check=True, capture_output=True)
            with patch.object(release, "ROOT", root), patch.object(release, "REMOTE", str(remote)):
                command = release.run
                for key, value in (("user.name", "Release test"), ("user.email", "test@example.invalid"),
                                   ("fetch.prune", "true"), ("fetch.pruneTags", "true")):
                    command("git", "config", "--local", key, value)
                (root / "Cargo.toml").write_text('[package]\nname = "qtools"\nversion = "0.4.3"\n')
                (root / "Cargo.lock").write_text('version = 4\n[[package]]\nname = "qtools"\nversion = "0.4.3"\n')
                command("git", "add", "--", "Cargo.toml", "Cargo.lock")
                command("git", "commit", "-m", "Initial", capture=True)
                command("git", "remote", "add", "origin", remote)
                release.git_remote("push", remote, "HEAD:refs/heads/main")
                command("git", "tag", "local-only")

                def checked_command(*args, **kwargs):
                    if args[0] in ("cargo", release.sys.executable):
                        return ""
                    return command(*args, **kwargs)

                with patch.object(release, "run", side_effect=checked_command):
                    commit = release.publish("0.4.4")
                self.assertEqual(command("git", "rev-parse", "refs/remotes/origin/main", capture=True).strip(), commit)
                self.assertEqual(command("git", "rev-parse", "v0.4.4^{commit}", capture=True).strip(), commit)
                self.assertTrue(command("git", "tag", "--list", "local-only", capture=True).strip())
                self.assertFalse(command("git", "status", "--porcelain", capture=True).strip())
                remote_commit = subprocess.check_output(
                    ["git", "--git-dir", str(remote), "rev-parse", "main"], text=True).strip()
                self.assertEqual(remote_commit, commit)


if __name__ == "__main__":
    unittest.main()
