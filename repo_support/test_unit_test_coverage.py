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
    def test_collection_requires_one_supported_target_across_runs(self):
        x64 = {"args": ["--target", "x86_64-unknown-linux-gnu"]}
        arm64 = {"args": ["--target", "aarch64-unknown-linux-gnu"]}
        self.assertEqual(coverage.coverage_target([arm64, arm64]), "aarch64-unknown-linux-gnu")
        for runs in ([], [x64, arm64], [{"args": []}], [{"args": ["--target"]}],
                     [{"args": ["--target", "unsupported"]}],
                     [{"args": x64["args"] + arm64["args"]}]):
            with self.subTest(runs=runs), self.assertRaises(ValueError):
                coverage.coverage_target(runs)

    def test_target_specific_caches_and_build_arguments(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "binary"
            binary.write_bytes(b"fixture")
            inventory = {"rust-suites": {"crate": {
                "binary-id": "crate", "binary-path": str(binary), "testcases": {},
            }}}
            for target in sorted(coverage.SUPPORTED_TARGETS):
                output = root / target
                output.mkdir()
                with patch.object(coverage, "capture", side_effect=["", json.dumps(inventory)]) as call, \
                     patch.object(coverage, "export_lines", return_value={}):
                    coverage.collect_run(
                        root, output, {"filter": None, "nextest_config": "fixture.toml"},
                        {"name": "base", "args": ["--target", target], "env": {}},
                        ["cargo"], "llvm-cov", "llvm-profdata", set(),
                        {"files": {}, "tests": [], "runs": [], "issues": []},
                    )
                first = call.call_args_list[0]
                cache = str(root / "target/unit-coverage-build" / target / "base")
                for key in ("CARGO_TARGET_DIR", "CARGO_LLVM_COV_TARGET_DIR",
                            "CARGO_LLVM_COV_BUILD_DIR"):
                    self.assertEqual(first.kwargs["env"][key], cache)
                self.assertEqual(first.args[0][-3:], ["--target", target, "--no-cfg-coverage"])
                self.assertIn(target, call.call_args_list[1].args[0])

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
            "schema_version": 2,
            "source": {"head": "abc", "dirty": True, "snapshot_sha256": "def"},
            "family": "unit",
            "origin": "local",
            "target": "x86_64-unknown-linux-gnu",
            "collection_state": "incomplete",
            "test_outcome": "failed",
            "scope": {"includes_test_source": True},
            "issues": ["missing test profile"],
            "files": {"src/lib.rs": {"10": False, "11": True}},
            "tests": [{
                "id": "test1", "run": "base", "binary_id": "crate", "name": "tests::bad",
                "attempt": 1, "result": "failed", "profile_state": "available",
                "covered": {"src/lib.rs": [11]},
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
            self.assertEqual(result["collection_state"], "incomplete")
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
            report = {"files": {}, "tests": [], "runs": [], "issues": []}

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
                report = {"files": {}, "tests": [], "runs": [], "issues": []}

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

    def test_collection_uses_host_llvm_tools_and_records_the_test_target(self):
        x64 = "x86_64-unknown-linux-gnu"
        arm64 = "aarch64-unknown-linux-gnu"
        directory = self.root / "target/architecture-fixtures"
        directory.mkdir(parents=True)
        config = directory / "nextest.toml"
        config.write_text("")
        sysroot = directory / "sysroot"
        original_capture = coverage.capture
        for index, (host, target) in enumerate((
            (x64, x64), (arm64, arm64), (x64, arm64),
            (None, arm64), ("aarch64-pc-windows-msvc", arm64),
        )):
            with self.subTest(host=host, target=target):
                output = directory / str(index)
                tools = sysroot / "lib/rustlib" / (host or "missing") / "bin"
                tools.mkdir(parents=True, exist_ok=True)
                for name in ("llvm-cov", "llvm-profdata"):
                    (tools / name).touch()
                plan = directory / f"{index}.json"
                coverage.write_json(plan, {
                    "toolchain": None, "filter": None, "nextest_config": str(config),
                    "runs": [{"name": "base", "args": ["--target", target, "-p", "fixture"]}],
                })

                def capture(args, **kwargs):
                    if args[0] == "rustc":
                        if args[1:] == ["-vV"]:
                            return "rustc fixture\n" + (f"host: {host}\n" if host else "")
                        self.assertEqual(args[1:], ["--print", "sysroot"])
                        return str(sysroot)
                    if args[0] == "cargo":
                        self.assertEqual(args[1:], ["llvm-cov", "--version"])
                        return "cargo-llvm-cov fixture"
                    return original_capture(args, **kwargs)

                def collect_run(root, output, plan, run, cargo, llvm_cov,
                                llvm_profdata, source_files, report):
                    report["files"] = {"tracked.rs": {1: True}}
                    report["runs"].append({"exit_code": 0, "expected_tests": 1})
                    report["tests"].append({
                        "id": "test", "run": "base", "binary_id": "fixture", "name": "test",
                        "attempt": 1, "result": "passed", "profile_state": "available",
                        "covered": {"tracked.rs": [1]},
                    })

                with patch.object(coverage, "repository_root", return_value=self.root), \
                     patch.object(coverage, "capture", side_effect=capture), \
                     patch.object(coverage, "collect_run", side_effect=collect_run) as collect, \
                     patch.object(coverage, "collect_source_map"), \
                     patch("sys.stdout", new_callable=io.StringIO) as stdout, \
                     patch("sys.stderr", new_callable=io.StringIO) as stderr:
                    if host in coverage.SUPPORTED_TARGETS:
                        self.assertEqual(coverage.collect(plan, output), 0)
                        self.assertIn("Production lines reached by passing tests:", stdout.getvalue())
                        self.assertNotIn('"schema_version"', stdout.getvalue())
                        self.assertIn(f"Report: {output / 'report.json'}", stderr.getvalue())
                        result = coverage.query(output / "report.json", root=self.root)
                        self.assertEqual(result["target"], target)
                        self.assertEqual(result["host"], host)
                        self.assertEqual(collect.call_args.args[5:7], (
                            tools / "llvm-cov", tools / "llvm-profdata",
                        ))
                    else:
                        with self.assertRaisesRegex(ValueError, "compiler host"):
                            coverage.collect(plan, output)
                        collect.assert_not_called()
                persisted = json.loads((output / "report.json").read_text())
                self.assertEqual(persisted["target"], target)
                self.assertEqual(persisted["collection_state"],
                                 "complete" if host in coverage.SUPPORTED_TARGETS else "incomplete")
                self.assertEqual(coverage.resolve_report(self.root), output / "report.json")

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
        query = coverage.query(path, changed=True, root=self.root)
        result = query["patch"]
        self.assertEqual(result["changed_rust_lines"], 5)
        self.assertEqual(query["coverage"]["execution"]["measured_lines"], 3)
        self.assertEqual(query["coverage"]["execution"]["covered_lines"], 1)
        self.assertEqual(result["unmapped_lines"], 2)
        self.assertEqual(result["unmeasured_files"], 1)
        self.assertEqual(result["non_rust_files"], 1)
        self.assertEqual(result["deleted_files"], 1)
        tracked = next(file for file in result["files"] if file["path"] == "tracked.rs")
        self.assertEqual(tracked["hunks"][0]["tests"][0]["name"], "covers_first")
        self.assertNotIn("lines", tracked)
        self.assertNotIn("sources", tracked["hunks"][0])
        self.assertNotIn("summary", query)
        self.assertNotIn("review_summary", query)
        self.assertNotIn("sources", result)
        self.assertNotIn("coverage", result)
        detail = coverage.query(path, "tracked.rs", 1, 1, root=self.root)
        self.assertEqual(detail["lines"][0]["tests"][0]["name"], "covers_first")
        self.assertEqual(tracked["uncovered_ranges"], [[2, 2], [13, 13]])
        unmeasured = next(file for file in result["files"] if file["path"] != "tracked.rs")
        self.assertEqual(unmeasured["hunks"][0]["tests"], [])
        self.assertEqual(unmeasured["unmapped_ranges"], [[1, 2]])
        bounded = coverage.query(path, changed=True, root=self.root, limit=1)
        self.assertTrue(bounded["patch"]["truncated"])
        self.assertEqual(bounded["coverage"], query["coverage"])
        self.assertEqual(bounded["sources"], query["sources"])

    def test_collection_summary_distinguishes_execution_from_passing_evidence(self):
        (self.root / "tracked.rs").write_text(
            self.source.replace("{ 1 }", "{ 101 }").replace("{ 2 }", "{ 202 }")
            .replace("{ 4 }", "{ 404 }")
        )
        path = self.report()
        report = json.loads(path.read_text())
        report["test_outcome"] = "failed"
        report["files"]["tracked.rs"].update({"2": True, "4": False})
        report["source_map"]["files"]["tracked.rs"]["ranges"] = [
            [1, 3, "production"], [4, 4, "unknown"], [5, 100, "production"],
        ]
        coverage.write_json(path, report)
        with patch("sys.stdout", new_callable=io.StringIO) as stdout:
            coverage.print_collection_summary(coverage.query(path, changed=True, root=self.root))
        text = stdout.getvalue()
        self.assertIn("Changed production lines reached by passing tests: 1/2 (50.0%)", text)
        self.assertIn("Executed production lines without passing-test evidence: 1", text)
        self.assertIn("Unclassified measured lines: 1", text)
        self.assertIn("query --changed", text)
        self.assertNotIn('"schema_version"', text)

    def test_collection_summary_does_not_call_an_empty_patch_covered(self):
        path = self.report(state="incomplete")
        with patch("sys.stdout", new_callable=io.StringIO) as stdout:
            coverage.print_collection_summary(coverage.query(path, changed=True, root=self.root))
        self.assertIn("no measured lines", stdout.getvalue())
        self.assertIn("Issue: missing profile", stdout.getvalue())
        self.assertNotIn("100", stdout.getvalue())

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
        result = coverage.query(path, changed=True, root=self.root)
        self.assertEqual(result["patch"]["changed_rust_lines"], 0)
        self.assertIsNone(result["coverage"]["execution"]["line_percent"])
        (self.root / "deleted.rs").unlink()
        path = self.report("deleted-only")
        result = coverage.query(path, changed=True, root=self.root)
        self.assertEqual(result["patch"]["deleted_files"], 1)
        self.assertIsNone(result["coverage"]["execution"]["line_percent"])

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
        coverage.write_json(index, {"schema_version": 2, "report": str(newest)})
        result = coverage.query(root=self.root)
        self.assertEqual(result["report"], str(newest))
        self.assertEqual(result["collection_state"], "incomplete")
        newest.unlink()
        self.assertTrue(previous.is_file())
        with self.assertRaisesRegex(ValueError, "latest report was removed"):
            coverage.query(root=self.root)

    def test_worker_startup_failure_clears_previous_active_report(self):
        previous = self.report("previous")
        index = self.root / "target/unit-coverage/latest.json"
        config = self.root / "target/nextest.toml"
        config.write_text("")
        for failure in ("output", "config", "snapshot"):
            with self.subTest(failure=failure):
                coverage.write_json(index, {"schema_version": 2, "report": str(previous)})
                output = self.root / "target" / f"failed-{failure}"
                if failure == "output":
                    output.mkdir()
                plan = self.root / "target/plan.json"
                coverage.write_json(plan, {
                    "toolchain": None, "filter": None, "xtask": "xtask",
                    "runs": [{"name": "base", "env": {}, "args": [
                        "--target", "x86_64-unknown-linux-gnu", "-p", "headervec",
                    ]}],
                    "nextest_config": str(config if failure != "config" else config.with_suffix(".missing")),
                })
                with patch.object(coverage, "repository_root", return_value=self.root):
                    if failure == "snapshot":
                        with patch.object(coverage, "source_snapshot", side_effect=ValueError("snapshot failed")), \
                             self.assertRaisesRegex(ValueError, "snapshot failed"):
                            coverage.collect(plan, output)
                    else:
                        with self.assertRaises(OSError):
                            coverage.collect(plan, output)
                self.assertFalse(index.exists())
                with self.assertRaisesRegex(ValueError, "no active coverage report"):
                    coverage.query(root=self.root)
                explicit = coverage.query(previous, root=self.root)
                self.assertEqual(explicit["collection_state"], "complete")
                self.assertEqual(explicit["freshness"]["state"], "current")

    def test_only_the_current_report_and_index_schema_are_accepted(self):
        path = self.report()
        report = json.loads(path.read_text())
        coverage.write_json(path, {**report, "schema_version": 1})
        with self.assertRaisesRegex(ValueError, "unsupported coverage schema"):
            coverage.query(path, root=self.root)
        coverage.write_json(path, report)
        index = self.root / "target/unit-coverage/latest.json"
        coverage.write_json(index, {"schema_version": 1, "report": str(path)})
        with self.assertRaisesRegex(ValueError, "unsupported latest-report index schema"):
            coverage.query(root=self.root)
        coverage.write_json(index, {"schema_version": 2, "report": str(path)})
        self.assertEqual(coverage.query(root=self.root)["collection_state"], "complete")

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
        self.assertEqual(result["sources"]["test_source"]["execution"]["measured_lines"], 0)
        self.assertEqual(result["sources"]["unclassified"]["execution"]["measured_lines"], 0)
        absolute = coverage.query(path, root=self.root)
        self.assertEqual(absolute["view"], "absolute")
        self.assertEqual(set(absolute["files"][0]), {"path", "coverage"})
        self.assertEqual(absolute["sources"]["test_source"]["execution"]["measured_lines"], 1)
        self.assertEqual(absolute["sources"]["unclassified"]["execution"]["measured_lines"], 1)
        patch = result["patch"]["files"][0]
        self.assertEqual(patch["not_covered_by_passing_attempt_ranges"], [[2, 2]])
        self.assertEqual(patch["hunks"][0]["coverage"]["passing_attempts"]["covered_lines"], 1)
        self.assertEqual({test["result"] for test in patch["hunks"][0]["tests"]}, {"passed", "failed"})
        self.assertEqual(result["view"], "changed")
        ranged = coverage.query(path, "tracked.rs", 2, 2, root=self.root)
        self.assertTrue(ranged["lines"][0]["covered"])
        self.assertFalse(ranged["lines"][0]["covered_by_passing_attempt"])
        self.assertEqual(ranged["view"], "file")
        self.assertEqual(ranged["coverage"]["execution"]["measured_lines"], 1)
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
        self.assertEqual(result["sources"]["dependency_production"]["execution"]["measured_lines"], 1)
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

    def test_gaps_rank_production_only_and_distinguish_execution_evidence(self):
        path = self.report()
        report = json.loads(path.read_text())
        report["test_outcome"] = "failed"
        report["files"]["tracked.rs"] = {
            "1": True, "2": False, "3": True, "4": False,
            "5": False, "6": False, "7": False, "9": False,
        }
        report["source_map"]["files"]["tracked.rs"]["ranges"] = [
            [1, 4, "production"], [5, 5, "test"], [6, 6, "auxiliary"],
            [7, 7, "unknown"], [8, 20, "production"],
        ]
        for name in ("z.rs", "a.rs", "dependency.rs", "covered.rs"):
            report["files"][name] = {"1": False, "2": False}
            report["source_map"]["files"][name] = {
                "package": "dependency" if name == "dependency.rs" else "fixture",
                "ranges": [[1, 2, "production"]],
            }
        report["files"]["covered.rs"] = {"1": True}
        report["tests"][0]["covered"]["covered.rs"] = [1]
        report["tests"].append({
            **report["tests"][0], "id": "failed", "name": "failed", "result": "failed",
            "covered": {"tracked.rs": [3]},
        })
        coverage.write_json(path, report)
        result = coverage.query(path, root=self.root, gaps=True)
        gaps = result["gaps"]
        self.assertEqual(result["view"], "gaps")
        self.assertEqual(result["collection_state"], "complete")
        self.assertEqual(result["test_outcome"], "failed")
        self.assertEqual([file["path"] for file in gaps["files"]], ["tracked.rs", "a.rs", "z.rs"])
        self.assertEqual(gaps["lines_without_passing_evidence"], 8)
        self.assertEqual(gaps["unexecuted_lines"], 7)
        self.assertEqual(gaps["executed_without_passing_evidence_lines"], 1)
        self.assertEqual(result["sources"]["unclassified"]["execution"]["measured_lines"], 1)
        ranges = gaps["files"][0]["ranges"]
        self.assertEqual([(r["start"], r["end"]) for r in ranges], [(2, 4), (9, 9)])
        self.assertEqual(ranges[0]["unexecuted_lines"], 2)
        self.assertEqual(ranges[0]["executed_without_passing_evidence_lines"], 1)
        bounded = coverage.query(path, root=self.root, gaps=True, limit=1)["gaps"]
        self.assertTrue(bounded["truncated"])
        self.assertTrue(bounded["files"][0]["truncated"])
        self.assertEqual(bounded["files_with_gaps"], 3)
        self.assertEqual(bounded["lines_without_passing_evidence"], 8)
        self.assertEqual(len(bounded["files"]), 1)
        self.assertEqual(len(bounded["files"][0]["ranges"]), 1)
        for limit, paths in (
            (2, ["tracked.rs"]),
            (3, ["tracked.rs", "a.rs"]),
            (4, ["tracked.rs", "a.rs", "z.rs"]),
        ):
            with self.subTest(limit=limit):
                bounded = coverage.query(path, root=self.root, gaps=True, limit=limit)["gaps"]
                self.assertEqual([file["path"] for file in bounded["files"]], paths)
                self.assertTrue(all(file["ranges"] for file in bounded["files"]))
                self.assertEqual(sum(len(file["ranges"]) for file in bounded["files"]), limit)
                self.assertEqual(bounded["files_with_gaps"], 3)
                self.assertEqual(bounded["lines_without_passing_evidence"], 8)
                self.assertEqual(bounded["truncated"], limit < 4)

    def test_empty_gaps_require_passing_evidence_and_known_source_metadata(self):
        path = self.report()
        report = json.loads(path.read_text())
        report["files"]["tracked.rs"] = {"1": True}
        coverage.write_json(path, report)
        gaps = coverage.query(path, root=self.root, gaps=True)["gaps"]
        self.assertEqual(gaps["files"], [])
        self.assertEqual(gaps["files_with_gaps"], 0)
        self.assertEqual(gaps["lines_without_passing_evidence"], 0)
        self.assertFalse(gaps["truncated"])
        for key in ("source_map", "scope"):
            modified = {**report, key: {}}
            coverage.write_json(path, modified)
            with self.assertRaisesRegex(ValueError, "recollect"):
                coverage.query(path, root=self.root, gaps=True)
        del report["source_map"]
        coverage.write_json(path, report)
        with self.assertRaisesRegex(ValueError, "recollect"):
            coverage.query(path, root=self.root, gaps=True)

    def test_gap_cli_preserves_freshness_and_rejects_conflicting_modes(self):
        path = self.report(state="incomplete")
        for options in ({"changed": True}, {"file": "tracked.rs"}, {"base": self.base}):
            with self.subTest(options=options), self.assertRaises(ValueError):
                coverage.query(path, root=self.root, gaps=True, **options)
        args = ["query", "--report", str(path), "--gaps", "--limit", "1"]
        with patch.object(coverage, "repository_root", return_value=self.root), \
             patch("sys.stdout", new_callable=io.StringIO) as stdout:
            self.assertEqual(coverage.main(args), 0)
            result = json.loads(stdout.getvalue())
        self.assertEqual(result["collection_state"], "incomplete")
        self.assertEqual(result["issues"], ["missing profile"])
        self.assertEqual(result["gaps"]["lines_without_passing_evidence"], 2)
        (self.root / "tracked.rs").write_text("changed source\n")
        with patch.object(coverage, "repository_root", return_value=self.root), \
             patch("sys.stdout", new_callable=io.StringIO) as stdout:
            self.assertEqual(coverage.main(args), 2)
            self.assertEqual(json.loads(stdout.getvalue())["error"], "stale_coverage")
        historical = coverage.query(path, root=self.root, gaps=True, allow_stale=True)
        self.assertEqual(historical["freshness"]["state"], "stale")
        self.assertEqual(historical["gaps"]["lines_without_passing_evidence"], 2)


if __name__ == "__main__":
    unittest.main()
