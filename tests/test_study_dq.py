import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location("study_dq", Path(__file__).parents[1] / "dev/research/study-dq.py")
STUDY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(STUDY)


class AllocationStudyTests(unittest.TestCase):
    def report(self):
        return {"metadata": {"root": "/data", "view": "directories", "accounting": "allocated", "directories": 4},
                "hidden_rows": 0, "total_bytes": 126,
                "rows": [{"label": STUDY.ROOT_LABEL, "bytes": 4, "files": 0},
                         {"label": "/data/empty", "bytes": 2, "files": 0},
                         {"label": "/data/empty/large", "bytes": 100, "files": 1},
                         {"label": "/data/empty/small", "bytes": 20, "files": 10}]}

    def test_perfect_information_still_pays_for_empty_ancestors(self):
        rows = STUDY.directory_rows(self.report())
        result = STUDY.perfect_information_schedule(rows, 100, lambda row: row["bytes"])
        self.assertEqual((result["directories"], result["unique_files"], result["measured_bytes"]), (3, 1, 106))
        self.assertTrue(result["target_met"])

    def test_shared_ancestors_are_counted_once_and_impossible_target_remains_unmet(self):
        rows = STUDY.directory_rows(self.report())
        result = STUDY.perfect_information_schedule(rows, 200, lambda row: row["bytes"])
        self.assertEqual((result["directories"], result["unique_files"], result["measured_bytes"]), (4, 11, 126))
        self.assertFalse(result["target_met"])

    def test_coarse_or_filtered_reports_are_rejected(self):
        for mutate in [lambda report: report.update(hidden_rows=1),
                       lambda report: report["metadata"].update(pruned_directories=1),
                       lambda report: report["rows"].pop(1),
                       lambda report: report["metadata"].update(directories=5),
                       lambda report: report.update(total_bytes=999)]:
            with self.subTest(mutate=mutate):
                report = self.report()
                mutate(report)
                with self.assertRaises(ValueError):
                    STUDY.directory_rows(report)

    def test_online_model_conserves_all_allocation_and_cannot_reach_an_impossible_target(self):
        rows = {".": {"bytes": 4096, "files": 0},
                "container": {"bytes": 65536, "files": 0},
                "container/leaf": {"bytes": 1048576, "files": 1024}}
        total = sum(row["bytes"] for row in rows.values())
        for policy in ("fine", "cohort", "novelty", "local4", "broad", "broad4", "cohort4",
                       "context", "context4", "context-max", "depth1", "depth2", "depth4",
                       "explore2", "explore4", "explore8", "explore16", "explore32", "explore64",
                       "branch2", "branch4", "branch8", "branch16", "branch32",
                       "fair2", "fair4", "fair8", "fair16", "fair32",
                       "diryield4", "diryield16", "diryield64",
                       "recent128", "recent1024", "recent4096", "recent16384", "signed1", "signed2", "signed4", "rand0", "rand16", "rand16-lifo", "rand16-ranked", "rand16-parent", "rand16-branch", "rand16-shallow", "rand16-deep", "rand16-population", "rand16-yield", "rand16-leaf128", "rand16-leaf512", "rand16-leafall"):
            with self.subTest(policy=policy):
                result = STUDY.simulate_policy(rows, total + 1, policy, 1)
                self.assertFalse(result["target_met"])
                self.assertEqual(result["measured_bytes"], total)
                self.assertEqual((result["directories"], result["unique_files"], result["queued_work"]), (3, 1024, 0))

    def test_hidden_large_allocation_is_discovered_only_after_opening_its_ancestors(self):
        rows = {".": {"bytes": 0, "files": 0},
                "container": {"bytes": 0, "files": 0},
                "container/leaf": {"bytes": 1048576, "files": 1}}
        result = STUDY.simulate_policy(rows, 1048576, "cohort", 1)
        self.assertEqual(result["directories"], 3)
        self.assertEqual(result["unique_files"], 1)

    def trace_rows(self, links=2):
        records = [{"purpose": "dq-offline-file-metadata-trace", "version": 1},
                   {"path": ".", "inode": 1, "allocated": 4096,
                    "files": [["shared", 10, links, 32768]], "children": ["child"]},
                   {"path": "child", "inode": 2, "allocated": 4096,
                    "files": [["alias", 10, links, 32768], ["sparse", 11, 1, 0],
                              ["large", 12, 1, 262144]], "children": []},
                   {"complete": True, "directories": 2, "files": 4}]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "trace.jsonl"
            path.write_text("".join(json.dumps(record) + "\n" for record in records))
            rows, _, _ = STUDY.file_trace_rows(path)
        return rows

    def test_file_trace_counts_each_physical_allocation_once_and_preserves_sparse_links(self):
        physical = 8192 + 32768 + 262144
        for policy in ("fine", "depth1", "explore4", "fair4", "signed1", "recent128", "linkjoin", "suffix8", "suffix64", "rand0", "rand16", "rand16-lifo", "rand16-ranked", "rand16-parent", "rand16-branch", "rand16-shallow", "rand16-deep", "rand16-population", "rand16-yield", "rand16-taildepth", "rand16-nochildcost", "rand16-cheapleaf512", "rand16-linkjoin", "rand16-tailrandom", "rand16-tailstride", "rand16-leaf128", "rand16-leaf512", "rand16-leafall", "rand16-certyield", "rand16-unique", "rand16-recent128", "rand16-descendants"):
            with self.subTest(policy=policy):
                result = STUDY.simulate_policy(self.trace_rows(), physical + 1, policy, 1)
                self.assertEqual(result["measured_bytes"], physical)
                self.assertEqual(result["pending_link_bytes"], 0)
                self.assertEqual((result["directories"], result["unique_files"]), (2, 4))
                self.assertFalse(result["target_met"])

    def test_suffix_learning_orders_unread_names_without_crediting_predictions(self):
        rows = self.trace_rows()
        rows["."]["entries"] = [[f"seed{i}.bin", 100 + i, 1, 65536] for i in range(8)]
        rows["."]["files"] = 8
        rows["child"]["entries"] = [[f"file{i}.js", 200 + i, 1, 0] for i in range(1001)]
        rows["child"]["entries"][5] = ["hidden.bin", 205, 1, 268435456]
        rows["child"]["files"] = 1001
        total = 8192 + 8 * 65536 + 268435456
        learned = STUDY.simulate_policy(rows, total, "suffix8", 1)
        native = STUDY.simulate_policy(rows, total, "depth1", 1)
        self.assertTrue(learned["target_met"])
        self.assertEqual(learned["measured_bytes"], total)
        self.assertEqual(learned["unique_files"], 17)
        self.assertGreater(native["unique_files"], learned["unique_files"])
        complete = STUDY.simulate_policy(rows, total + 1, "suffix8", 1)
        self.assertFalse(complete["target_met"])
        self.assertEqual(complete["unique_files"], 1009)
        self.assertEqual(complete["measured_bytes"], total)

    def test_structural_predictions_preserve_accounting_and_hidden_descendants(self):
        rows = self.trace_rows()
        physical = 8192 + 32768 + 262144
        for policy in ("rand16-shape", "rand16-shape-max", "rand16-shape-mean", "rand16-shape-mean-max",
                       "rand16-novelty", "rand16-count64", "rand16-count1024", "rand16-rarity",
                       "rand16-mix2", "rand16-mix4", "rand16-mix8", "rand16-mix16",
                       "rand16-refresh256", "rand16-refresh1024", "rand16-refresh16384",
                       "rand16-tailcost4", "rand16-tailcost16", "rand16-tailcost64",
                       "rand16-ancmax2", "rand16-ancmax4",
                       "rand16-features", "rand16-features-max", "rand16-features-specific", "rand16-gitpacks",
                       "rand16-peak", "rand16-peakblend", "rand16-sizebiased",
                       "rand16-inode12", "rand16-inode15", "rand16-inode18", "rand16-inode15-max",
                       "rand16-tailboost16", "rand16-tailtotal", "rand16-positiveevidence", "rand16-nonzeroancestor",
                       "rand16-path3", "rand16-path5", "rand16-path3-max", "rand16-path5-direct",
                       "rand16-path5-unpenalized", "rand16-path5-max",
                       "rand16-path5-family", "rand16-path5-stem", "rand16-path5-family-stem",
                       "rand16-path5-certyield", "rand16-path5-unique", "rand16-path5-recent128",
                       "rand16-path7", "rand16-path9", "rand16-path5-support1",
                       "rand16-path5-support32", "rand16-path5-support128",
                       "rand16-path5-skip", "rand16-path5-nodepeak",
                       "rand16-path5-chain", "rand16-path5-branchpenalty",
                       "rand8-path5", "rand24-path5", "rand16-path5-cacheprior",
                       "rand16-path5-linkjoin", "rand16-path5-nochildcost",
                       "rand16-path5-dircost8", "rand16-path5-dircost32",
                       "rand16-path5-suffix8", "rand16-path5-suffixpilot8",
                       "rand16-path5-leaf128", "rand16-path5-leaf512"):
            with self.subTest(policy=policy):
                result = STUDY.simulate_policy(rows, physical + 1, policy, 1)
                self.assertEqual(result["measured_bytes"], physical)
                self.assertEqual(result["pending_link_bytes"], 0)
                self.assertEqual((result["directories"], result["unique_files"]), (2, 4))
                self.assertFalse(result["target_met"])
                hidden = STUDY.simulate_policy({".": {"bytes": 0, "files": 0},
                                               "container": {"bytes": 0, "files": 0},
                                               "container/leaf": {"bytes": 1048576, "files": 1}},
                                              1048576, policy, 1)
                self.assertEqual(hidden["directories"], 3)
                self.assertEqual(hidden["unique_files"], 1)

    def test_file_trace_unseen_links_stay_in_the_uncertainty_budget(self):
        physical = 8192 + 32768 + 262144
        result = STUDY.simulate_policy(self.trace_rows(links=3), physical, "depth1", 1)
        self.assertEqual(result["measured_bytes"], physical)
        self.assertEqual(result["pending_link_bytes"], 32768)
        self.assertFalse(result["target_met"])

    def test_directory_probes_are_charged_without_crediting_predictions(self):
        rows = self.trace_rows()
        rows["."]["started_unix_ns"] = 86_400_000_000_000
        rows["."]["child_metadata"] = {"child": {"inode": 2, "size": 8192, "links": 2,
                                                     "modified": 0, "allocated": 8192}}
        physical = 8192 + 32768 + 262144
        for policy in ("rand16-probe-density", "rand16-probe-agemax", "rand16-probe-agereplace",
                       "rand16-probe-structure", "rand16-probe-newer"):
            with self.subTest(policy=policy):
                result = STUDY.simulate_policy(rows, physical + 1, policy, 1)
                self.assertFalse(result["target_met"])
                self.assertEqual(result["measured_bytes"], physical)
                self.assertEqual(result["pending_link_bytes"], 0)
                self.assertEqual((result["directories"], result["unique_files"]), (2, 4))
                self.assertEqual(result["directory_metadata_probes"], 1)
                self.assertEqual(result["metadata_work_with_probes"], result["metadata_query_work_proxy"] + 1)

    def test_optimistic_link_diagnostic_distinguishes_coverage_from_attribution(self):
        result = STUDY.simulate_policy(self.trace_rows(links=3), 4096 + 32768,
                                      "rand16-optimisticlinks", 1)
        self.assertTrue(result["target_met"])
        self.assertFalse(result["certified_attribution_target_met"])
        self.assertEqual(result["directories"], 1)
        self.assertEqual(result["pending_link_bytes"], 32768)
        self.assertIn("diagnostic only", result["attribution_assumption"])

    def test_file_trace_rejects_incomplete_capture_and_missing_children(self):
        for remove in ("footer", "child"):
            with self.subTest(remove=remove), tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "trace.jsonl"
                records = [{"purpose": "dq-offline-file-metadata-trace", "version": 1},
                           {"path": ".", "inode": 1, "allocated": 4096,
                            "files": [], "children": ["absent"]}]
                if remove == "child":
                    records.append({"complete": True, "directories": 1, "files": 0})
                path.write_text("".join(json.dumps(record) + "\n" for record in records))
                with self.assertRaises(ValueError):
                    STUDY.file_trace_rows(path)


if __name__ == "__main__":
    unittest.main()
