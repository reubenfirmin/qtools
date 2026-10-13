"""Validate the directory-only diagnostic's namespace conservation."""
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


class DirectoryIoProbeTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("cc") and os.geteuid() != 0, "ordinary user and C compiler required")
    def test_physical_order_preserves_entries_across_multiple_windows(self):
        source = Path(__file__).parents[1] / "dev/research/dq-dir-io-probe.c"
        with tempfile.TemporaryDirectory(prefix="dq-dir-probe-test-") as temporary:
            root = Path(temporary) / "tree"
            root.mkdir()
            for index in range(37):
                leaf = root / (f"child-{index}-" + "x" * 150)
                leaf.mkdir()
                (leaf / "nested").mkdir()
                (leaf / "file").write_bytes(b"one")
                os.link(leaf / "file", leaf / "alias")
                (leaf / "cycle").symlink_to(root, target_is_directory=True)
            results = []
            for physical in (0, 1):
                binary = Path(temporary) / f"probe-{physical}"
                subprocess.run(["cc", "-O2", "-pthread", "-Wall", "-Wextra", "-Werror",
                                "-DWINDOW=4", f"-DPHYSICAL_ORDER={physical}", str(source), "-o", str(binary)],
                               check=True, capture_output=True, text=True)
                result = subprocess.run([str(binary), "--threads", "3", str(root)],
                                        check=True, capture_output=True, text=True)
                values = dict((key, int(value)) for key, value in re.findall(r"([a-z]+)=(\d+)", result.stderr))
                self.assertIn("purpose=directory-only-diagnostic", result.stderr)
                self.assertEqual(values["directories"], 75)
                self.assertEqual(values["files"], 74)
                self.assertEqual(values["entries"], 185)
                self.assertEqual(values["errors"], 0)
                results.append(values)
            self.assertEqual(results[0]["checksum"], results[1]["checksum"])


if __name__ == "__main__":
    unittest.main()
