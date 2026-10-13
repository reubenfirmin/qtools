import importlib.util
import os
from pathlib import Path
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location("capture_dq_trace", Path(__file__).parents[1] / "dev/research/capture-dq-trace.py")
CAPTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CAPTURE)


class CaptureDirectoryHintsTests(unittest.TestCase):
    def test_hints_are_fresh_metadata_and_symlinks_are_not_descended(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            child = root / "child"
            child.mkdir()
            (root / "file").write_bytes(b"content")
            (root / "alias").symlink_to(child, target_is_directory=True)
            ordinary = CAPTURE.read_directory(root, ".", root.stat().st_dev)
            hinted = CAPTURE.read_directory(root, ".", root.stat().st_dev, True)
            self.assertNotIn("child_metadata", ordinary)
            self.assertEqual(hinted["children"], ["child"])
            self.assertEqual(hinted["files"], ordinary["files"])
            self.assertEqual(hinted["errors"], [])
            metadata = os.stat(child, follow_symlinks=False)
            self.assertEqual(hinted["child_metadata"]["child"], {
                "inode": metadata.st_ino, "allocated": metadata.st_blocks * 512,
                "size": metadata.st_size, "links": metadata.st_nlink,
                "modified": metadata.st_mtime_ns, "changed": metadata.st_ctime_ns})
