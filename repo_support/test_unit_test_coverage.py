#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Tests for coverage provenance, source-line aggregation, and JSON queries."""

import json
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
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            coverage.write_json(path, report)
            result = coverage.query(path, "src/lib.rs", 10, 11)
            self.assertFalse(result["lines"][0]["covered"])
            self.assertEqual(result["lines"][0]["tests"], [])
            self.assertEqual(result["lines"][1]["tests"][0]["result"], "failed")
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


if __name__ == "__main__":
    unittest.main()
