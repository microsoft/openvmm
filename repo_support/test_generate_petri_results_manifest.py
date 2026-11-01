# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from generate_petri_results_manifest import generate_manifest


class ManifestTests(unittest.TestCase):
    def test_artifact_paths_and_statuses_without_reading_logs(self):
        with tempfile.TemporaryDirectory() as directory:
            results = Path(directory)
            for name in [
                "x64/job/nested/passed/petri.jsonl",
                "x64/job/nested/passed/petri.passed",
                "aarch64/failed/petri.jsonl",
                "aarch64/failed/petri.failed_unstable",
                "aarch64/failed/attachment.log",
                "x64/orphan/petri.passed",
                "x64/spaces & \u03bb/petri.jsonl",
            ]:
                path = results / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"not log JSON")
            manifest = generate_manifest(results, "123_2", "2026-10-30T16:00:00Z")
            self.assertEqual(manifest, {
                "version": 1,
                "runNumber": "123_2",
                "creationTime": "2026-10-30T16:00:00Z",
                "tests": [
                    {"name": "aarch64/failed", "status": "failed"},
                    {"name": "x64/job/nested/passed", "status": "passed"},
                    {"name": "x64/spaces & \u03bb", "status": "failed"},
                ],
            })

    def test_empty_results_and_invalid_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            results = Path(directory)
            self.assertEqual(
                generate_manifest(results, "123", "2026-10-30T16:00:00+00:00")["tests"],
                [],
            )
            with self.assertRaises(NotADirectoryError):
                generate_manifest(results / "missing", "123_1", "2026-10-30T16:00:00Z")
            for run, timestamp in [
                ("../123_1", "2026-10-30T16:00:00Z"),
                ("123_1", "not a timestamp"),
                ("123_1", "2026-10-30T16:00:00"),
            ]:
                with self.subTest(run=run, timestamp=timestamp):
                    with self.assertRaises(ValueError):
                        generate_manifest(results, run, timestamp)

    def test_rejects_paths_without_architecture_and_test(self):
        with tempfile.TemporaryDirectory() as directory:
            results = Path(directory)
            (results / "petri.jsonl").touch()
            with self.assertRaisesRegex(ValueError, "Invalid test path"):
                generate_manifest(results, "123_1", "2026-10-30T16:00:00Z")

    def test_cli_writes_the_versioned_json_body(self):
        with tempfile.TemporaryDirectory() as directory:
            results = Path(directory) / "results"
            test = results / "x64" / "test & \u03bb"
            test.mkdir(parents=True)
            (test / "petri.jsonl").touch()
            output = Path(directory) / "manifest.json"
            subprocess.run([
                sys.executable,
                str(Path(__file__).with_name("generate_petri_results_manifest.py")),
                str(results),
                "--run-number", "123_1",
                "--creation-time", "2026-10-30T16:00:00Z",
                "--output", str(output),
            ], check=True)
            self.assertEqual(
                json.loads(output.read_text(encoding="utf-8")),
                generate_manifest(results, "123_1", "2026-10-30T16:00:00Z"),
            )


if __name__ == "__main__":
    unittest.main()
