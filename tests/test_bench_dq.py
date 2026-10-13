"""Verify user-confirmed cold-trial ordering without invoking sudo."""

import contextlib
import errno
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("bench_dq", ROOT / "dev/bench-dq.py")
BENCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BENCH)


class BenchmarkTests(unittest.TestCase):
    def test_idle_io_wait_keeps_busy_intervals_and_requires_consecutive_quiet_intervals(self):
        counters = [{"read_blocks": value, "write_blocks": 0, "busy_ms": 0}
                    for value in (0, 5000, 5000, 5000, 10000, 10000, 10000, 10000)]
        with patch.object(BENCH, "filesystem_io", side_effect=counters), \
                patch.object(BENCH.time, "sleep") as sleep, \
                patch.object(BENCH.time, "monotonic", side_effect=[10, 17]):
            result = BENCH.wait_for_idle_io("/unused", timeout_s=7)
        self.assertEqual(sleep.call_count, 7)
        self.assertEqual(result["elapsed_s"], 7)
        self.assertEqual([interval["read_blocks"] for interval in result["intervals"]],
                         [5000, 0, 0, 5000, 0, 0, 0])

    def test_idle_io_wait_refuses_unavailable_counters_and_times_out_without_scanning(self):
        with patch.object(BENCH, "filesystem_io", return_value=None), \
                patch.object(BENCH.time, "sleep") as sleep:
            with self.assertRaisesRegex(RuntimeError, "unavailable"):
                BENCH.wait_for_idle_io("/unused")
            sleep.assert_not_called()
        busy = [{"read_blocks": 0, "write_blocks": 0, "busy_ms": 0},
                {"read_blocks": 0, "write_blocks": 0, "busy_ms": 1000}]
        with patch.object(BENCH, "filesystem_io", side_effect=busy), \
                patch.object(BENCH.time, "sleep"):
            with self.assertRaisesRegex(RuntimeError, "no scan ran"):
                BENCH.wait_for_idle_io("/unused", timeout_s=1)

    def test_isolated_reclaim_moves_only_the_benchmark_into_its_owned_empty_group(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary) / "work.slice"
            parent.mkdir()
            (parent / "cgroup.procs").write_text("")
            (parent / "cgroup.subtree_control").write_text("memory\n")
            group = parent / f"qtools-dq-benchmark-{os.geteuid()}"
            group.mkdir()
            (group / "cgroup.procs").write_text("")
            (group / "memory.reclaim").write_text("")
            with patch.object(BENCH, "current_cgroup", side_effect=[parent / "terminal.scope", group]), \
                    patch.object(BENCH, "reclaim_domain", return_value=parent):
                self.assertEqual(BENCH.isolated_reclaim_group(), group)
            self.assertEqual((group / "cgroup.procs").read_text(), f"{os.getpid()}\n")
            self.assertEqual((parent / "cgroup.procs").read_text(), "")

    def test_isolated_reclaim_refuses_to_move_other_processes_or_enable_controllers(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary) / "work.slice"
            parent.mkdir()
            (parent / "cgroup.procs").write_text("")
            (parent / "cgroup.subtree_control").write_text("memory\n")
            group = parent / f"qtools-dq-benchmark-{os.geteuid()}"
            group.mkdir()
            (group / "cgroup.procs").write_text("98765\n")
            (group / "memory.reclaim").write_text("")
            with patch.object(BENCH, "current_cgroup", return_value=parent / "terminal.scope"), \
                    patch.object(BENCH, "reclaim_domain", return_value=parent):
                with self.assertRaisesRegex(RuntimeError, "occupied"):
                    BENCH.isolated_reclaim_group()
                (parent / "cgroup.subtree_control").write_text("")
                with self.assertRaisesRegex(RuntimeError, "memory controller"):
                    BENCH.isolated_reclaim_group()
            self.assertEqual((group / "cgroup.procs").read_text(), "98765\n")
            self.assertEqual((parent / "cgroup.subtree_control").read_text(), "")

    def test_reclaim_uses_only_our_user_owned_cgroup_and_never_reclaims_anonymous_memory(self):
        cgroup = Path("/sys/fs/cgroup/test.scope")
        before = {"file": 1_000_000, "slab_reclaimable": 100_000, "anon": 2_000_000, "pswpout": 0}
        after = {**before, "file": 10_000, "slab_reclaimable": 1000}
        with patch.object(BENCH, "current_cgroup", return_value=cgroup), \
                patch.object(Path, "stat", return_value=SimpleNamespace(st_uid=1000)), \
                patch.object(BENCH.os, "geteuid", return_value=1000), \
                patch.object(BENCH.os, "sync") as sync, \
                patch.object(BENCH, "memory_stats", side_effect=[before, after, after, after]), \
                patch.object(BENCH.os, "open", return_value=42) as opened, \
                patch.object(BENCH.os, "write", side_effect=OSError(errno.EAGAIN, "under reclaimed")) as write, \
                patch.object(BENCH.os, "close") as close:
            preparation = BENCH.reclaim_caches()
        sync.assert_called_once()
        self.assertTrue(all(call.args[0] == cgroup / "memory.reclaim" for call in opened.call_args_list))
        self.assertTrue(all(call.args[1].endswith(b" swappiness=0\n") for call in write.call_args_list))
        self.assertEqual(write.call_args_list[0].args[1], b"1104096 swappiness=0\n")
        self.assertEqual(close.call_count, 3)
        self.assertEqual(preparation["before"], before)
        self.assertEqual(preparation["after"], after)
        self.assertTrue(all(trial["under_reclaimed"] for trial in preparation["passes"]))

    def test_reclaim_domain_covers_charges_reparented_from_prior_terminal_scopes(self):
        current = Path("/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/work.slice/new.scope")
        stable = current.parent
        with patch.object(BENCH.os, "geteuid", return_value=1000), \
                patch.object(Path, "stat", autospec=True,
                    side_effect=lambda path: SimpleNamespace(st_uid=1000 if path.parent == stable else 0)):
            self.assertEqual(BENCH.reclaim_domain(current), stable)

    def test_reclaim_domain_never_selects_a_root_owned_ancestor(self):
        current = Path("/sys/fs/cgroup/user.slice/work.slice/test.scope")
        with patch.object(BENCH.os, "geteuid", return_value=1000), \
                patch.object(Path, "stat", return_value=SimpleNamespace(st_uid=0)):
            self.assertEqual(BENCH.reclaim_domain(current), current)

    def test_reclaim_rejects_root_and_cgroups_not_owned_by_the_user(self):
        for user, owner in ((1000, 0), (0, 0)):
            with patch.object(BENCH, "current_cgroup", return_value=Path("/sys/fs/cgroup/test")), \
                    patch.object(Path, "stat", return_value=SimpleNamespace(st_uid=owner)), \
                    patch.object(BENCH.os, "geteuid", return_value=user), \
                    patch.object(BENCH.os, "open") as opened:
                with self.assertRaisesRegex(RuntimeError, "owned by the unprivileged"):
                    BENCH.reclaim_caches()
            opened.assert_not_called()

    def test_reclaim_that_leaves_substantial_cache_is_rejected(self):
        retained = {"file": 1024 * 1024 * 1024, "slab_reclaimable": 100_000, "pswpout": 0}
        with patch.object(BENCH, "current_cgroup", return_value=Path("/sys/fs/cgroup/test.scope")), \
                patch.object(Path, "stat", return_value=SimpleNamespace(st_uid=1000)), \
                patch.object(BENCH.os, "geteuid", return_value=1000), \
                patch.object(BENCH.os, "sync"), \
                patch.object(BENCH, "memory_stats", return_value=retained), \
                patch.object(BENCH.os, "open", return_value=42), \
                patch.object(BENCH.os, "write", side_effect=OSError(errno.EAGAIN, "under reclaimed")), \
                patch.object(BENCH.os, "close"):
            with self.assertRaisesRegex(RuntimeError, "incompletely reclaimed"):
                BENCH.reclaim_caches()

    def test_every_reclaimed_trial_prepares_before_scanning_without_external_reset(self):
        events = []
        sample = {"wall_s": 3.0, "files": 20, "directories": 4, "input_blocks": 4000}
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "results.json"
            argv = ["bench-dq.py", temporary, "--binary", sys.executable,
                    "--reclaim-cache", "--runs", "3", "--output", str(output)]
            with patch.object(sys, "argv", argv), \
                    patch.object(BENCH, "reclaim_caches", side_effect=lambda: events.append("reclaim") or {"swappiness": 0}), \
                    patch.object(BENCH, "wait_for_external_reset") as external, \
                    patch.object(BENCH, "run", side_effect=lambda *args: events.append("scan") or sample.copy()), \
                    contextlib.redirect_stdout(io.StringIO()):
                BENCH.main()
            report = json.loads(output.read_text())
        external.assert_not_called()
        self.assertEqual(events, ["reclaim", "scan"] * 3)
        for trial in [report["first"], *report["repeats"]]:
            self.assertEqual(trial["cache_reset_source"], "cgroup-reclaim")
            self.assertEqual(trial["cache_reclaim"]["swappiness"], 0)
        self.assertEqual(report["reclaimed_summary"]["median_s"], 3.0)

    def test_reclaim_failure_prevents_scan(self):
        with tempfile.TemporaryDirectory() as temporary:
            argv = ["bench-dq.py", temporary, "--binary", sys.executable, "--reclaim-cache"]
            with patch.object(sys, "argv", argv), \
                    patch.object(BENCH, "reclaim_caches", side_effect=PermissionError("read-only cgroup")), \
                    patch.object(BENCH, "run") as scan, \
                    contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaises(PermissionError):
                    BENCH.main()
            scan.assert_not_called()

    def test_scan_invokes_only_selected_binary_without_a_shell(self):
        result = subprocess.CompletedProcess([], 0, "",
            "dq[profile]: elapsed=5ms files=10\ndq[debug]: summary-scan=10ms files=20 directories=4\n")
        with tempfile.TemporaryDirectory() as temporary, \
                patch.object(BENCH.subprocess, "run", return_value=result) as command:
            sample = BENCH.run("/chosen/dq", temporary, ["--threads", "8"], {"QTOOLS_DEBUG": "1"})
        self.assertEqual(command.call_args.args, (
            ["/chosen/dq", "--noprogress", "--no-color", "--threads", "8", temporary],
        ))
        self.assertEqual(command.call_args.kwargs["env"], {"QTOOLS_DEBUG": "1"})
        self.assertFalse(command.call_args.kwargs.get("shell", False))
        self.assertEqual(sample["files"], 20)
        self.assertEqual(sample["profile"], ["dq[profile]: elapsed=5ms files=10"])

    def test_io_monitor_observes_one_scan_and_keeps_each_interval(self):
        from unittest.mock import MagicMock
        child = MagicMock()
        child.pid, child.returncode = 42, 0
        child.communicate.side_effect = [subprocess.TimeoutExpired("dq", 1),
                                         (None, "dq[debug]: summary-scan=1100ms files=20 directories=4\n")]
        process = MagicMock()
        process.__enter__.return_value = child
        counters = [{"read_blocks": n, "busy_ms": n} for n in (100, 180, 190)]
        with tempfile.TemporaryDirectory() as temporary, \
                patch.object(BENCH.subprocess, "Popen", return_value=process) as launch, \
                patch.object(BENCH.subprocess, "run") as unmonitored, \
                patch.object(BENCH, "filesystem_io", side_effect=counters), \
                patch.object(BENCH, "worker_waits", return_value={"io_schedule": 32, "futex_wait_queue": 1}):
            sample = BENCH.run("/chosen/dq", temporary, [], {}, sample_io=True)
        launch.assert_called_once()
        unmonitored.assert_not_called()
        self.assertEqual(sample["files"], 20)
        self.assertEqual(sample["filesystem_io"], {"read_blocks": 90, "busy_ms": 90})
        self.assertEqual(sample["io_timeline"][0]["filesystem_io"], {"read_blocks": 80, "busy_ms": 80})
        self.assertEqual(sample["io_timeline"][0]["worker_waits"]["io_schedule"], 32)

    def test_profiling_requires_an_explicit_flag_and_is_recorded_in_the_report(self):
        sample = {"wall_s": 3.0, "files": 20, "directories": 4, "input_blocks": 0}
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "results.json"
            argv = ["bench-dq.py", temporary, "--binary", sys.executable, "--profile", "--output", str(output)]
            with patch.object(sys, "argv", argv), \
                    patch.object(BENCH, "run", return_value=sample) as scan, \
                    contextlib.redirect_stdout(io.StringIO()):
                BENCH.main()
            report = json.loads(output.read_text())
        self.assertEqual(scan.call_args.args[3]["QTOOLS_PROFILE"], "1")
        self.assertTrue(report["profiling"])

    def test_idle_io_changes_only_the_child_priority_without_a_shell(self):
        result = subprocess.CompletedProcess([], 0, "", "dq[debug]: summary-scan=10ms files=20 directories=4\n")
        with tempfile.TemporaryDirectory() as temporary, \
                patch.object(BENCH.shutil, "which", return_value="/usr/sbin/ionice"), \
                patch.object(BENCH.subprocess, "run", return_value=result) as command:
            BENCH.run("/chosen/dq", temporary, [], {}, idle_io=True)
        self.assertEqual(command.call_args.args[0],
            ["/usr/sbin/ionice", "--class", "3", "--", "/chosen/dq", "--noprogress", "--no-color", temporary])
        self.assertFalse(command.call_args.kwargs.get("shell", False))

    def test_missing_ionice_prevents_an_idle_priority_trial(self):
        with patch.object(BENCH.shutil, "which", return_value=None), \
                patch.object(BENCH.subprocess, "run") as command:
            with self.assertRaisesRegex(RuntimeError, "requires ionice"):
                BENCH.run("/chosen/dq", "/tmp", [], {}, idle_io=True)
        command.assert_not_called()

    def test_best_effort_priority_changes_only_the_child_without_a_shell(self):
        result = subprocess.CompletedProcess([], 0, "", "dq[debug]: summary-scan=10ms files=20 directories=4\n")
        for priority in (0, 7):
            with tempfile.TemporaryDirectory() as temporary, \
                    patch.object(BENCH.shutil, "which", return_value="/usr/sbin/ionice"), \
                    patch.object(BENCH.subprocess, "run", return_value=result) as command:
                BENCH.run("/chosen/dq", temporary, [], {}, best_effort_priority=priority)
            self.assertEqual(command.call_args.args[0],
                ["/usr/sbin/ionice", "--class", "2", "--classdata", str(priority), "--",
                 "/chosen/dq", "--noprogress", "--no-color", temporary])
            self.assertFalse(command.call_args.kwargs.get("shell", False))

    def test_io_context_probe_records_mode_and_keeps_the_selected_binary(self):
        for shared in (False, True):
            mode = "shared" if shared else "independent"
            diagnostic = f"dq[io-context-probe]: mode={mode} clone-calls=50 modified={50 if shared else 0}"
            result = subprocess.CompletedProcess([], 0, "",
                f"dq[debug]: summary-scan=10ms files=20 directories=4\n{diagnostic}\n")
            with tempfile.TemporaryDirectory() as temporary, \
                    patch.object(BENCH.subprocess, "run", return_value=result) as command:
                sample = BENCH.run("/chosen/dq", temporary, [], {},
                    io_context_probe="/chosen/probe", shared_io_context=shared)
            self.assertEqual(command.call_args.args[0],
                ["/chosen/probe", f"--{mode}", "/chosen/dq", "--noprogress", "--no-color", temporary])
            self.assertFalse(command.call_args.kwargs.get("shell", False))
            self.assertEqual(sample["io_context_probe"], [diagnostic])

    def test_external_reset_requires_explicit_confirmation(self):
        with patch("builtins.input", return_value="reset-complete"), \
                patch.object(BENCH, "cache_reset_counters", side_effect=[
                    {"drop_pagecache": 1, "drop_slab": 2}, {"drop_pagecache": 2, "drop_slab": 3},
                ]), \
                contextlib.redirect_stdout(io.StringIO()):
            counters = BENCH.wait_for_external_reset()
        self.assertEqual(counters["before"]["drop_pagecache"], 1)
        self.assertEqual(counters["after"]["drop_pagecache"], 2)
        for acknowledgment in ("", "yes"):
            with patch("builtins.input", return_value=acknowledgment), \
                    contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, "External reset was not confirmed"):
                    BENCH.wait_for_external_reset()
        with patch("builtins.input", side_effect=EOFError), \
                contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "External reset was not confirmed"):
                BENCH.wait_for_external_reset()

    def test_confirmation_without_both_kernel_resets_is_rejected(self):
        before = {"drop_pagecache": 4, "drop_slab": 7}
        for after in (before.copy(), {"drop_pagecache": 5, "drop_slab": 7},
                      {"drop_pagecache": 4, "drop_slab": 8}):
            with patch("builtins.input", return_value="reset-complete"), \
                    patch.object(BENCH, "cache_reset_counters", side_effect=[before, after]), \
                    contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, "did not both increase"):
                    BENCH.wait_for_external_reset()

    def test_kernel_counters_are_read_and_missing_counters_are_rejected(self):
        with patch.object(Path, "read_text", return_value="pgpgin 100\ndrop_pagecache 4\ndrop_slab 7\n"):
            self.assertEqual(BENCH.cache_reset_counters(), {"drop_pagecache": 4, "drop_slab": 7})
        with patch.object(Path, "read_text", return_value="drop_pagecache 4\n"):
            with self.assertRaisesRegex(RuntimeError, "counters are unavailable"):
                BENCH.cache_reset_counters()

    def test_watch_mode_starts_only_after_both_counters_advance_without_running_commands(self):
        readings = [
            {"drop_pagecache": 4, "drop_slab": 7},
            {"drop_pagecache": 4, "drop_slab": 7},
            {"drop_pagecache": 5, "drop_slab": 7},
            {"drop_pagecache": 5, "drop_slab": 8},
        ]
        with patch.object(BENCH, "cache_reset_counters", side_effect=readings), \
                patch.object(BENCH.time, "sleep") as sleep, \
                patch.object(BENCH.subprocess, "run") as command, \
                patch("builtins.input") as acknowledgment, \
                contextlib.redirect_stdout(io.StringIO()):
            counters = BENCH.wait_for_external_reset(watch=True)
        self.assertEqual(counters, {"before": readings[0], "after": readings[-1]})
        self.assertEqual(sleep.call_count, 2)
        command.assert_not_called()
        acknowledgment.assert_not_called()

    def test_every_cold_trial_resets_before_scanning_and_includes_first_in_summary(self):
        events = []

        def scan(executable, root, arguments, env, idle_io, probe, shared):
            events.append("scan")
            self.assertNotIn("--precise", arguments)
            self.assertNotIn("QTOOLS_PROFILE", env)
            self.assertIsNone(probe)
            self.assertFalse(shared)
            return {"wall_s": 3.0, "files": 20, "directories": 4, "input_blocks": 0}

        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "results.json"
            argv = ["bench-dq.py", temporary, "--binary", sys.executable,
                    "--cold", "--runs", "3", "--output", str(output)]
            with patch.object(sys, "argv", argv), \
                    patch.dict(os.environ, {"QTOOLS_PROFILE": "1"}), \
                    patch.object(BENCH, "wait_for_external_reset", side_effect=lambda **kwargs: events.append("reset")), \
                    patch.object(BENCH, "run", side_effect=scan), \
                    contextlib.redirect_stdout(io.StringIO()):
                BENCH.main()
            report = json.loads(output.read_text())
        self.assertEqual(events, ["reset", "scan"] * 3)
        self.assertTrue(report["first"]["cache_reset"])
        self.assertEqual(report["first"]["cache_reset_source"], "user-confirmed")
        self.assertEqual(len(report["repeats"]), 2)
        self.assertTrue(all(sample["cache_reset"] for sample in report["repeats"]))
        self.assertEqual(report["cold_summary"]["median_s"], 3.0)
        self.assertNotIn("repeat_summary", report)

    def test_isolated_paired_trials_reclaim_every_time_and_keep_both_first_samples(self):
        walls = iter([10.0, 8.0, 7.0, 9.0, 11.0, 6.0])
        events = []
        scan_arguments = []
        group = Path("/test/work.slice/qtools-dq-benchmark-1000")

        def reclaim(*args):
            events.append(("reclaim", args))
            return {"method": "test-reclaim"}

        def scan(*args, **kwargs):
            events.append(("scan",))
            scan_arguments.append(args[2])
            return {"wall_s": next(walls), "files": 20, "directories": 4, "input_blocks": 100}

        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "paired.json"
            argv = ["bench-dq.py", temporary, "--binary", f"reference={sys.executable}",
                    "--paired-binary", f"candidate={sys.executable}", "--reclaim-cache", "--isolated-reclaim",
                    "--threads", "8", "--paired-threads", "32",
                    "--runs", "3", "--output", str(output)]
            with patch.object(sys, "argv", argv), \
                    patch.object(BENCH, "reclaim_caches", side_effect=reclaim), \
                    patch.object(BENCH, "isolated_reclaim_group", return_value=group), \
                    patch.object(BENCH, "run", side_effect=scan), \
                    contextlib.redirect_stdout(io.StringIO()):
                BENCH.main()
            report = json.loads(output.read_text())
        self.assertEqual(events, [("reclaim", ())] + [("reclaim", (group,)), ("scan",)] * 6)
        self.assertEqual([trial["binary"] for trial in report["trial_order"]],
                         ["reference", "candidate", "candidate", "reference", "reference", "candidate"])
        self.assertEqual(scan_arguments, [["--threads", str(n)] for n in (8, 32, 32, 8, 8, 32)])
        self.assertEqual(report["arguments"], ["--threads", "8"])
        self.assertEqual(report["comparison"]["arguments"], ["--threads", "32"])
        self.assertEqual(report["first"]["wall_s"], 10.0)
        self.assertEqual(report["comparison"]["first"]["wall_s"], 8.0)
        self.assertEqual(report["reclaimed_summary"]["median_s"], 10.0)
        self.assertEqual(report["comparison"]["reclaimed_summary"]["median_s"], 7.0)
        self.assertEqual(len(report["repeats"]), 2)
        self.assertEqual(len(report["comparison"]["repeats"]), 2)

    def test_reset_failure_records_the_reason_without_a_scan_sample(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "results.json"
            argv = ["bench-dq.py", temporary, "--binary", sys.executable,
                    "--cold", "--output", str(output)]
            with patch.object(sys, "argv", argv), \
                    patch.object(BENCH, "wait_for_external_reset", side_effect=RuntimeError("reset denied")), \
                    patch.object(BENCH, "run") as scan, \
                    contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, "reset denied"):
                    BENCH.main()
            scan.assert_not_called()
            report = json.loads(output.read_text())
            self.assertIsNone(report["first"])
            self.assertEqual(report["repeats"], [])
            self.assertEqual(report["failure"], {"trial": 1, "phase": "external-reset", "error": "reset denied"})

    def test_later_reclaim_failure_keeps_the_first_sample_and_failed_attempt(self):
        sample = {"wall_s": 3.0, "files": 20, "directories": 4, "input_blocks": 4000}
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "results.json"
            argv = ["bench-dq.py", temporary, "--binary", sys.executable,
                    "--reclaim-cache", "--runs", "3", "--output", str(output)]
            with patch.object(sys, "argv", argv), \
                    patch.object(BENCH, "reclaim_caches", side_effect=[{"swappiness": 0}, RuntimeError("cache remains")]), \
                    patch.object(BENCH, "run", return_value=sample.copy()) as scan, \
                    contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, "cache remains"):
                    BENCH.main()
            report = json.loads(output.read_text())
        scan.assert_called_once()
        self.assertEqual(report["first"]["wall_s"], 3.0)
        self.assertEqual(report["repeats"], [])
        self.assertEqual(report["failure"], {"trial": 2, "phase": "cache-reclaim", "error": "cache remains"})

    def test_external_mode_waits_before_every_scan_without_invoking_reset(self):
        events = []
        sample = {"wall_s": 3.0, "files": 20, "directories": 4, "input_blocks": 0}
        with tempfile.TemporaryDirectory() as temporary:
            argv = ["bench-dq.py", temporary, "--binary", sys.executable,
                    "--cold", "--external-reset", "--runs", "2"]
            with patch.object(sys, "argv", argv), \
                    patch.object(BENCH.subprocess, "run") as subprocess_run, \
                    patch.object(BENCH, "wait_for_external_reset", side_effect=lambda **kwargs: events.append("reset")), \
                    patch.object(BENCH, "run", side_effect=lambda *args: events.append("scan") or sample.copy()), \
                    contextlib.redirect_stdout(io.StringIO()):
                BENCH.main()
        subprocess_run.assert_not_called()
        self.assertEqual(events, ["reset", "scan"] * 2)

    def test_default_records_one_unprimed_invocation(self):
        sample = {"wall_s": 3.0, "files": 20, "directories": 4, "input_blocks": 0}
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "results.json"
            argv = ["bench-dq.py", temporary, "--binary", sys.executable, "--output", str(output)]
            with patch.object(sys, "argv", argv), \
                    patch.object(BENCH, "wait_for_external_reset") as reset, \
                    patch.object(BENCH, "run", return_value=sample) as scan, \
                    contextlib.redirect_stdout(io.StringIO()):
                BENCH.main()
            report = json.loads(output.read_text())
        reset.assert_not_called()
        scan.assert_called_once()
        self.assertFalse(report["first"]["cache_reset"])
        self.assertEqual(report["repeats"], [])

    def test_privileged_helper_rejects_all_arguments(self):
        result = subprocess.run(
            ["/bin/sh", str(ROOT / "dev/drop-caches.sh"), "--path", "/tmp/anything"],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("takes no arguments", result.stderr)

    @unittest.skipIf(os.geteuid() == 0, "never execute the successful privileged reset in tests")
    def test_privileged_helper_rejects_unprivileged_execution(self):
        result = subprocess.run(
            ["/bin/sh", str(ROOT / "dev/drop-caches.sh")],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("must run as root", result.stderr)


if __name__ == "__main__":
    unittest.main()
