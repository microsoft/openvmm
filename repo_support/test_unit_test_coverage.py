#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for coverage provenance, source-line aggregation, and JSON queries."""

import json
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import unit_test_coverage as coverage


class CoverageTests(unittest.TestCase):
    def test_environment_is_parsed_without_evaluation(self):
        parsed = coverage.parse_environment(
            "RUSTFLAGS='-C instrument-coverage'\n"
            "QUOTED='a'\\''b'\n"
            "ENCODED='-C\x1finstrument-coverage'\n"
            "NOT_EXECUTED='$(exit 99)'\n"
        )
        self.assertEqual(parsed["QUOTED"], "a'b")
        self.assertEqual(parsed["ENCODED"], "-C\x1finstrument-coverage")
        self.assertEqual(parsed["NOT_EXECUTED"], "$(exit 99)")
        with self.assertRaises(ValueError):
            coverage.parse_environment("RUSTFLAGS=a b")

    def test_union_retains_misses_and_excludes_generated_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            source = root / "src/lib.rs"
            first = coverage.parse_lcov(
                f"SF:{source}\nDA:10,0\nDA:11,2\nend_of_record\n"
                f"SF:{root / 'target/generated.rs'}\nDA:1,1\nend_of_record\n",
                root, {"src/lib.rs"},
            )
            second = coverage.parse_lcov(
                f"SF:{source}\nDA:10,4\nDA:12,0\nend_of_record\n",
                root, {"src/lib.rs"},
            )
            coverage.merge_lines(first, second)
            self.assertEqual(first, {"src/lib.rs": {10: True, 11: True, 12: False}})
            self.assertEqual(coverage.summarize(first)["covered_lines"], 2)

    def test_query_keeps_zero_unmeasured_and_failed_test_evidence_distinct(self):
        report = {
            "schema_version": 1,
            "source": {"head": "abc", "dirty": True, "snapshot_sha256": "def"},
            "family": "unit",
            "origin": "local",
            "target": "x86_64-unknown-linux-gnu",
            "state": "incomplete",
            "all_tests_passed": False,
            "scope": {"includes_test_source": True},
            "issues": ["missing test profile"],
            "summary": {"measured_lines": 2, "covered_lines": 1, "line_percent": 50},
            "files": {"src/lib.rs": {"10": False, "11": True}},
            "tests": [{
                "id": "test1", "run": "base", "binary_id": "crate", "name": "tests::bad",
                "attempt": 1, "result": "failed", "covered": {"src/lib.rs": [11]},
            }],
        }
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(coverage, "check_freshness", return_value={"state": "current"}):
            path = Path(directory) / "report.json"
            coverage.write_json(path, report)
            result = coverage.query(path, "src/lib.rs", 10, 11)
            self.assertFalse(result["lines"][0]["covered"])
            self.assertEqual(result["lines"][0]["tests"], [])
            self.assertEqual(result["lines"][1]["tests"][0]["result"], "failed")
            self.assertFalse(result["lines"][1]["covered_by_passing_attempt"])
            self.assertEqual(result["coverage"]["passing_attempts"]["covered_lines"], 0)
            self.assertEqual(result["collection_state"], "unknown")
            self.assertEqual(result["test_outcome"], "failed")
            self.assertEqual(coverage.query(path, "windows.rs")["measurement"], "unmeasured")
            self.assertIsNone(coverage.summarize({})["line_percent"])
            self.assertTrue(coverage.query(path, "src/lib.rs", limit=1)["truncated"])
            with self.assertRaises(ValueError):
                coverage.query(path, start=1)
            with self.assertRaises(ValueError):
                coverage.query(path, limit=0)

    def test_runner_separates_test_attempts_and_listing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = {
                coverage.PROFILE_DIR_ENV: directory,
                "NEXTEST_BINARY_ID": "crate::binary",
                "NEXTEST_TEST_NAME": "test/with::special'name",
                "NEXTEST_RUN_ID": "run",
                "NEXTEST_ATTEMPT": "1",
                "NEXTEST_ATTEMPT_ID": "run$test$1",
            }
            child = [
                sys.executable, "-c",
                "import os,pathlib,sys; "
                "pathlib.Path(os.environ['LLVM_PROFILE_FILE'].replace('%m','1')"
                ".replace('%p',str(os.getpid()))).write_bytes(b'profile'); sys.exit(7)",
            ]
            with patch.dict(os.environ, env):
                self.assertEqual(coverage.nextest_runner(child), 7)
                os.environ["NEXTEST_ATTEMPT"] = "2"
                os.environ["NEXTEST_ATTEMPT_ID"] = "run$test$2"
                self.assertEqual(coverage.nextest_runner(child), 7)
            records = [json.loads(p.read_text()) for p in root.glob("*/attempt.json")]
            self.assertEqual(len(records), 2)
            self.assertEqual({r["attempt"] for r in records}, {1, 2})
            self.assertTrue(all(r["result"] == "failed" for r in records))
            self.assertTrue(all(r["name"] == env["NEXTEST_TEST_NAME"] for r in records))
            self.assertEqual(len(list(root.glob("*/*.profraw"))), 2)
            with patch.dict(os.environ, {coverage.PROFILE_DIR_ENV: directory}, clear=True):
                self.assertEqual(coverage.nextest_runner([sys.executable, "-c", "pass"]), 0)
            self.assertEqual(len(list(root.glob("*/attempt.json"))), 2)

    def test_snapshot_distinguishes_tracked_and_untracked_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", directory], check=True)
            source = root / "lib.rs"
            source.write_text("original\n")
            subprocess.run(["git", "add", "lib.rs"], cwd=root, check=True)
            subprocess.run(
                ["git", "-c", "user.name=Coverage Test", "-c",
                 "user.email=coverage@example.invalid", "commit", "-qm", "fixture"],
                cwd=root, check=True,
                env={**os.environ, "GIT_CONFIG_NOSYSTEM": "1"},
            )
            clean = coverage.source_snapshot(root)
            self.assertFalse(clean["dirty"])
            source.write_text("changed\n")
            changed = coverage.source_snapshot(root)
            self.assertTrue(changed["dirty"])
            self.assertEqual(clean["head"], changed["head"])
            self.assertNotEqual(clean["snapshot_sha256"], changed["snapshot_sha256"])
            (root / "untracked.rs").write_text("new\n")
            self.assertNotEqual(
                changed["snapshot_sha256"], coverage.source_snapshot(root)["snapshot_sha256"],
            )

    def test_inventory_excludes_ignored_and_filtered_tests(self):
        inventory = {"rust-suites": {"crate": {
            "binary-id": "crate",
            "testcases": {
                "selected": {"ignored": False, "filter-match": {"status": "matches"}},
                "ignored": {"ignored": True, "filter-match": {"status": "matches"}},
                "filtered": {"ignored": False, "filter-match": {"status": "mismatch"}},
            },
        }}}
        self.assertEqual(coverage.selected_tests(inventory), {("crate", "selected")})

    def test_missing_profiles_are_not_treated_as_complete_coverage(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "output"
            output.mkdir()
            binary = root / "test-binary"
            binary.write_bytes(b"fixture")
            inventory = {"rust-suites": {"crate": {
                "binary-id": "crate", "binary-path": str(binary),
                "testcases": {
                    "test": {"ignored": False, "filter-match": {"status": "matches"}},
                },
            }}}
            report = {"files": {}, "tests": [], "runs": [], "issues": [],
                      "all_tests_passed": None}

            def finish_without_profile(*args, **kwargs):
                attempt = output / "runs/base/profiles/attempt"
                attempt.mkdir()
                coverage.write_json(attempt / "attempt.json", {
                    "binary_id": "crate", "name": "test", "result": "passed",
                })
                return 0

            with patch.object(coverage, "capture", side_effect=["", json.dumps(inventory)]), \
                 patch.object(coverage, "export_lines", return_value={"lib.rs": {1: False}}), \
                 patch.object(coverage.subprocess, "call", side_effect=finish_without_profile):
                coverage.collect_run(
                    root, output, {"filter": None, "nextest_config": "fixture.toml"},
                    {"name": "base", "args": ["--target", "x86_64-unknown-linux-gnu"], "env": {}},
                    ["cargo"], "llvm-cov", "llvm-profdata", {"lib.rs"}, report,
                )
            self.assertEqual(report["files"], {"lib.rs": {1: False}})
            self.assertEqual(report["tests"][0]["profile_state"], "missing")
            self.assertIn("missing profile", report["issues"][0])

    def test_failed_tests_with_profiles_are_not_collection_errors(self):
        for exit_code in (100, 101):
            with self.subTest(exit_code=exit_code), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                output = root / "output"
                output.mkdir()
                binary = root / "binary"
                binary.write_bytes(b"fixture")
                inventory = {"rust-suites": {"crate": {
                    "binary-id": "crate", "binary-path": str(binary),
                    "testcases": {
                        "test": {"ignored": False, "filter-match": {"status": "matches"}},
                    },
                }}}
                report = {"files": {}, "tests": [], "runs": [], "issues": [],
                          "all_tests_passed": None}

                def finish_with_profile(*args, **kwargs):
                    attempt = output / "runs/base/profiles/attempt"
                    attempt.mkdir()
                    coverage.write_json(attempt / "attempt.json", {
                        "binary_id": "crate", "name": "test", "result": "failed",
                    })
                    (attempt / "fixture.profraw").write_bytes(b"fixture")
                    return exit_code

                with patch.object(coverage, "capture", side_effect=["", json.dumps(inventory)]), \
                     patch.object(coverage, "export_lines", side_effect=[
                         {"lib.rs": {1: False}}, {"lib.rs": {1: True}},
                     ]), \
                     patch.object(coverage.subprocess, "call", side_effect=finish_with_profile), \
                     patch.object(coverage.subprocess, "run"):
                    coverage.collect_run(
                        root, output, {"filter": None, "nextest_config": "fixture.toml"},
                        {"name": "base", "args": ["--target", "x86_64-unknown-linux-gnu"], "env": {}},
                        ["cargo"], "llvm-cov", "llvm-profdata", {"lib.rs"}, report,
                    )
                self.assertEqual(report["tests"][0]["profile_state"], "available")
                self.assertEqual(report["files"], {"lib.rs": {1: True}})
                self.assertEqual(coverage.coverage_index(report), {})
                self.assertEqual(bool(report["issues"]), exit_code != 100)


class GitQueryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.git("init", "-q")
        (self.root / ".gitignore").write_text("/target/\n")
        self.source = "".join(
            f"fn value_{number}() -> u32 {{ {number} }}\n" for number in range(1, 13)
        )
        (self.root / "tracked.rs").write_text(self.source)
        (self.root / "deleted.rs").write_text("fn deleted() {}\n")
        self.commit()
        self.base = self.git("rev-parse", "HEAD").strip()

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-c", "user.name=Coverage Test", "-c",
             "user.email=coverage@example.invalid", "-c", "core.hooksPath=/dev/null", *args],
            cwd=self.root, text=True,
        )

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")

    def report(self, name="run", *, state="complete"):
        output = self.root / "target/unit-coverage" / name
        output.mkdir(parents=True)
        report = {
            "schema_version": 2,
            "source": coverage.source_snapshot(self.root, [self.root / "target/unit-coverage"]),
            "family": "unit", "origin": "local", "target": "x86_64-unknown-linux-gnu",
            "collection_state": state,
            "test_outcome": "passed" if state == "complete" else "incomplete",
            "all_tests_passed": True if state == "complete" else None,
            "scope": {"runs": [{"args": ["-p", "fixture"]}]},
            "issues": [] if state == "complete" else ["missing profile"],
            "files": {"tracked.rs": {"1": True, "2": False, "13": False}},
            "tests": [{
                "id": "test1", "run": "base", "binary_id": "crate", "name": "covers_first",
                "attempt": 1, "result": "passed", "covered": {"tracked.rs": [1]},
                "profile_state": "available",
            }],
            "source_map": {"files": {"tracked.rs": {
                "package": "fixture", "ranges": [[1, 100, "production"]],
            }}, "issues": []},
            "comparison": coverage.resolve_comparison(self.root, self.base),
        }
        coverage.write_json(output / "report.json", report)
        return output / "report.json"

    def test_patch_combines_committed_staged_unstaged_and_untracked_changes(self):
        source = self.root / "tracked.rs"
        source.write_text(self.source.replace("{ 1 }", "{ 101 }"))
        self.commit()
        source.write_text(source.read_text() + "fn added() {}\n")
        self.git("add", "tracked.rs")
        source.write_text(source.read_text().replace("{ 2 }", "{ 202 }"))
        (self.root / 'new "quoted" café.rs').write_text("fn extra() {\n}\n")
        (self.root / "notes.md").write_text("not executable Rust\n")
        (self.root / "deleted.rs").unlink()
        path = self.report()
        result = coverage.query(path, changed=True, root=self.root)["patch"]
        self.assertEqual(result["changed_rust_lines"], 5)
        self.assertEqual(result["summary"]["measured_lines"], 3)
        self.assertEqual(result["summary"]["covered_lines"], 1)
        self.assertEqual(result["unmapped_lines"], 2)
        self.assertEqual(result["unmeasured_files"], 1)
        self.assertEqual(result["non_rust_files"], 1)
        self.assertEqual(result["deleted_files"], 1)
        tracked = next(file for file in result["files"] if file["path"] == "tracked.rs")
        self.assertEqual([line["line"] for line in tracked["lines"]], [1, 2, 13])
        self.assertEqual(tracked["lines"][0]["tests"][0]["name"], "covers_first")
        self.assertEqual(tracked["uncovered_ranges"], [[2, 2], [13, 13]])
        unmeasured = next(file for file in result["files"] if file["path"] != "tracked.rs")
        self.assertEqual(unmeasured["lines"], [])
        self.assertEqual(unmeasured["unmapped_ranges"], [[1, 2]])
        bounded = coverage.query(path, changed=True, root=self.root, limit=1)["patch"]
        self.assertTrue(bounded["truncated"])
        self.assertEqual(bounded["summary"], result["summary"])

    def test_rename_maps_only_edited_new_side_lines(self):
        destination = self.root / "renamed é space.rs"
        (self.root / "tracked.rs").rename(destination)
        destination.write_text(self.source.replace("{ 4 }", "{ 404 }"))
        self.git("add", "-A")
        comparison = coverage.resolve_comparison(self.root, self.base)
        changed, _, _ = coverage.changed_source_lines(self.root, comparison)
        self.assertEqual(changed, {"renamed é space.rs": {4}})

    def test_empty_patch_and_deletion_only_patch_are_not_100_percent(self):
        path = self.report()
        result = coverage.query(path, changed=True, root=self.root)["patch"]
        self.assertEqual(result["changed_rust_lines"], 0)
        self.assertIsNone(result["summary"]["line_percent"])
        (self.root / "deleted.rs").unlink()
        path = self.report("deleted-only")
        result = coverage.query(path, changed=True, root=self.root)["patch"]
        self.assertEqual(result["deleted_files"], 1)
        self.assertIsNone(result["summary"]["line_percent"])

    def test_stale_queries_fail_and_historical_override_is_explicit(self):
        (self.root / "untracked.rs").write_text("fn before() {}\n")
        path = self.report()
        (self.root / "untracked.rs").write_text("fn after() {}\n")
        with self.assertRaises(coverage.StaleCoverageError):
            coverage.query(path, root=self.root)
        historical = coverage.query(path, root=self.root, allow_stale=True)
        self.assertEqual(historical["freshness"]["state"], "stale")
        with self.assertRaises(ValueError):
            coverage.query(path, root=self.root, allow_stale=True, changed=True)
        with patch.object(coverage, "repository_root", return_value=self.root), \
             patch("sys.stdout", new_callable=io.StringIO) as stdout:
            self.assertEqual(coverage.main(["query", "--report", str(path)]), 2)
            self.assertEqual(json.loads(stdout.getvalue())["error"], "stale_coverage")

    def test_latest_points_to_newest_attempt_even_when_it_is_incomplete(self):
        previous = self.report("previous")
        newest = self.report("newest", state="incomplete")
        index = self.root / "target/unit-coverage/latest.json"
        coverage.write_json(index, {"schema_version": 1, "report": str(newest)})
        result = coverage.query(root=self.root)
        self.assertEqual(result["report"], str(newest))
        self.assertEqual(result["collection_state"], "incomplete")
        newest.unlink()
        self.assertTrue(previous.is_file())
        with self.assertRaisesRegex(ValueError, "latest report was removed"):
            coverage.query(root=self.root)

    def test_default_base_uses_configured_origin_head_not_a_guessed_branch(self):
        with self.assertRaisesRegex(ValueError, "supply --base"):
            coverage.resolve_comparison(self.root, None)
        self.git("update-ref", "refs/remotes/origin/trunk", self.base)
        self.git("symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk")
        self.assertEqual(
            coverage.resolve_comparison(self.root, None)["resolved"], self.base,
        )

    def test_source_changes_during_patch_query_are_rejected(self):
        path = self.report()
        with patch.object(
            coverage, "check_freshness",
            side_effect=[{"state": "current"}, {"state": "stale"}],
        ):
            with self.assertRaises(coverage.StaleCoverageError):
                coverage.query(path, changed=True, root=self.root)

    def test_passing_evidence_and_source_views_do_not_hide_failed_only_hits(self):
        source = self.root / "tracked.rs"
        source.write_text(self.source.replace("{ 1 }", "{ 101 }").replace("{ 2 }", "{ 202 }"))
        path = self.report()
        report = json.loads(path.read_text())
        report["test_outcome"] = "failed"
        report["all_tests_passed"] = False
        report["files"]["tracked.rs"]["2"] = True
        report["tests"].append({
            "id": "test2", "run": "base", "binary_id": "crate", "name": "fails_after_second",
            "attempt": 1, "result": "failed", "profile_state": "available",
            "covered": {"tracked.rs": [2]},
        })
        # A known test-only line and an unknown span must not inflate production coverage.
        report["files"]["tracked.rs"]["3"] = True
        report["files"]["tracked.rs"]["4"] = False
        report["source_map"]["files"]["tracked.rs"]["ranges"] = [
            [1, 2, "production"], [3, 3, "test"], [4, 4, "unknown"],
            [5, 100, "production"],
        ]
        report["source_map"]["files"]["tracked.rs"]["package"] = "fixture"
        coverage.write_json(path, report)
        result = coverage.query(path, changed=True, root=self.root)
        self.assertEqual(result["collection_state"], "complete")
        self.assertEqual(result["test_outcome"], "failed")
        production = result["sources"]["selected_production"]
        self.assertEqual(production["execution"]["covered_lines"], 2)
        self.assertEqual(production["passing_attempts"]["covered_lines"], 1)
        self.assertEqual(result["sources"]["test_source"]["execution"]["measured_lines"], 1)
        self.assertEqual(result["sources"]["unclassified"]["execution"]["measured_lines"], 1)
        patch = result["patch"]["files"][0]
        self.assertEqual(patch["not_covered_by_passing_attempt_ranges"], [[2, 2]])
        self.assertTrue(patch["lines"][1]["covered"])
        self.assertFalse(patch["lines"][1]["covered_by_passing_attempt"])
        self.assertEqual(patch["hunks"][0]["coverage"]["passing_attempts"]["covered_lines"], 1)
        self.assertEqual({test["result"] for test in patch["hunks"][0]["tests"]}, {"passed", "failed"})
        self.assertEqual(result["review_summary"]["scope"], "changed")
        self.assertEqual(result["review_summary"]["production_lines_without_passing_evidence"], 1)
        ranged = coverage.query(path, "tracked.rs", 2, 2, root=self.root)
        self.assertEqual(ranged["review_summary"]["scope"], "file_range")
        self.assertEqual(ranged["summary"]["measured_lines"], 1)
        self.assertEqual(ranged["sources"]["test_source"]["execution"]["measured_lines"], 0)

    def test_workspace_dependency_views_and_bounded_hunks(self):
        source = self.root / "tracked.rs"
        source.write_text(self.source.replace("{ 1 }", "{ 101 }").replace("{ 3 }", "{ 303 }"))
        path = self.report()
        report = json.loads(path.read_text())
        report["source_map"]["files"]["tracked.rs"]["package"] = "dependency"
        coverage.write_json(path, report)
        result = coverage.query(path, changed=True, root=self.root, limit=1)
        self.assertEqual(result["sources"]["selected_production"]["execution"]["measured_lines"], 0)
        self.assertEqual(result["sources"]["dependency_production"]["execution"]["measured_lines"], 3)
        self.assertEqual(len(result["patch"]["files"][0]["hunks"]), 1)
        self.assertTrue(result["patch"]["truncated"])

    def test_test_status_is_not_collection_status(self):
        report = {
            "collection_state": "complete",
            "scope": {"runs": [{}]},
            "runs": [{"exit_code": 100, "expected_tests": 1}],
        }
        self.assertEqual(coverage.test_outcome(report), "failed")
        report["runs"][0]["exit_code"] = 0
        report["collection_state"] = "incomplete"
        self.assertEqual(coverage.test_outcome(report), "passed")
        report["runs"][0]["exit_code"] = 101
        self.assertEqual(coverage.test_outcome(report), "incomplete")
        report["runs"][0].update(exit_code=0, expected_tests=0)
        self.assertEqual(coverage.test_outcome(report), "not_run")


if __name__ == "__main__":
    unittest.main()
