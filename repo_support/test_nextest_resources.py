# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import json
from pathlib import Path
import tempfile
import unittest
import xml.etree.ElementTree as ET

import nextest_resources as resources


JUNIT = """<testsuites timestamp="2026-01-01T00:00:00Z" time="10">
<testsuite>
  <testcase classname="vmm" name="a" timestamp="2026-01-01T00:00:00Z" time="5"/>
  <testcase classname="vmm" name="b" timestamp="2026-01-01T00:00:02Z" time="8"/>
  <testcase name="ignored"><skipped/></testcase>
</testsuite></testsuites>"""


class AnalysisTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.epoch = resources.timestamp("2026-01-01T00:00:00Z")
        self.write_json("metadata.json", {
            "schema_version": 1,
            "started_unix_seconds": self.epoch - 1,
            "logical_cpus": 8,
            "available_parallelism": 6,
            "total_memory_bytes": 16 * resources.GIB,
        })
        self.write_json("outcome.json", {
            "collection_complete": True,
            "collection_error": None,
            "nextest_exit_code": 0,
        })
        self.samples = [
            dict(
                elapsed_seconds=time,
                cpu_percent=cpu,
                used_memory_bytes=used * resources.GIB,
                available_memory_bytes=(16 - used) * resources.GIB,
                used_swap_bytes=0,
                disks=[dict(name="disk", mount_point="/", read_bytes=time * 1048576,
                            written_bytes=0)],
            )
            for time, cpu, used in [(0, None, 2), (1, 100, 2), (3, 25, 4), (6, 50, 8), (11, 75, 4)]
        ]
        self.write_samples()
        (self.directory / "junit.xml").write_text(JUNIT)

    def write_json(self, filename, value):
        (self.directory / filename).write_text(json.dumps(value))

    def write_samples(self):
        (self.directory / "samples.jsonl").write_text(
            "".join(json.dumps(row) + "\n" for row in self.samples)
        )

    def test_overlap_and_time_weighted_cpu_exclude_command_startup(self):
        report = resources.analyze(self.directory)
        summary = report["summary"]
        self.assertAlmostEqual(summary["mean_concurrent_tests"], 1.3)
        self.assertEqual(summary["peak_concurrent_tests"], 2)
        self.assertEqual(summary["seconds_by_concurrent_tests"], {1: 7, 2: 3})
        self.assertEqual(summary["drain_seconds_after_last_test_started"], 8)
        self.assertAlmostEqual(summary["mean_host_cpu_percent"], 57.5)
        self.assertAlmostEqual(summary["mean_busy_logical_cpus"], 4.6)
        self.assertEqual(summary["min_available_memory_gib"], 8)
        self.assertEqual(summary["resource_coverage_percent"], 100)
        self.assertEqual(summary["warnings"], [])

    def test_retries_are_separate_attempts_not_double_counted(self):
        root = ET.fromstring("""<testsuites><testsuite>
          <testcase name="flaky" timestamp="2026-01-01T00:00:05Z" time="2">
            <flakyFailure timestamp="2026-01-01T00:00:00Z" time="1"/>
            <flakyError timestamp="2026-01-01T00:00:02Z" time="1"/>
          </testcase>
          <testcase name="failed" timestamp="2026-01-01T00:00:01Z" time="1">
            <failure/>
            <rerunFailure timestamp="2026-01-01T00:00:03Z" time="1"/>
            <rerunError timestamp="2026-01-01T00:00:05Z" time="1"/>
          </testcase>
        </testsuite></testsuites>""")
        trials = resources.read_trials(root, self.epoch)
        self.assertEqual(len(trials), 6)
        self.assertEqual(sum(trial.result == "retry" for trial in trials), 4)
        self.assertEqual(sum(trial.end - trial.start for trial in trials), 7)
        points = resources.concurrency(trials, 0, 7)
        self.assertEqual(max(running for _, running in points), 2)
        self.assertEqual(points[-1], (7, 0))

    def test_simultaneous_end_and_start_do_not_inflate_peak(self):
        trials = [
            resources.Trial("a", 0, 2, "pass"),
            resources.Trial("b", 2, 4, "pass"),
            resources.Trial("instant", 1, 1, "pass"),
        ]
        self.assertEqual(resources.concurrency(trials, 0, 4), [(0, 1), (2, 1), (4, 0)])

    def test_junit_millisecond_boundaries_do_not_create_phantom_overlap(self):
        root = ET.fromstring("""<testsuites><testsuite>
          <testcase name="first" timestamp="2026-01-01T00:00:00.435Z" time="0.003"/>
          <testcase name="next" timestamp="2026-01-01T00:00:00.438Z" time="0.003"/>
        </testsuite></testsuites>""")
        trials = resources.read_trials(root, self.epoch - 1.234567)
        self.assertEqual(trials[0].end, trials[1].start)
        points = resources.concurrency(trials, trials[0].start, trials[1].end)
        self.assertEqual(max(running for _, running in points), 1)

    def test_missing_cpu_is_unknown_not_idle(self):
        for row in self.samples:
            row["cpu_percent"] = None
        self.write_samples()
        summary = resources.analyze(self.directory)["summary"]
        self.assertIsNone(summary["mean_host_cpu_percent"])
        self.assertIsNone(summary["mean_busy_logical_cpus"])
        self.assertEqual(summary["cpu_coverage_percent"], 0)
        self.assertIn("Valid CPU samples", summary["warnings"][0])

    def test_incomplete_capture_is_marked_and_uncovered_time_not_extrapolated(self):
        (self.directory / "outcome.json").unlink()
        self.samples.pop()
        self.write_samples()
        summary = resources.analyze(self.directory)["summary"]
        self.assertEqual(summary["resource_coverage_percent"], 50)
        self.assertEqual(len(summary["warnings"]), 3)

    def test_unavailable_disk_counters_are_reported_without_throughput(self):
        metadata_path = self.directory / "metadata.json"
        metadata = json.loads(metadata_path.read_text())
        reason = "Windows disk I/O collection is disabled."
        metadata["disk_io_unavailable_reason"] = reason
        self.write_json("metadata.json", metadata)
        for row in self.samples:
            row["disks"] = None
        self.write_samples()
        report = resources.analyze(self.directory)
        self.assertEqual(report["summary"]["warnings"], [reason])
        html = resources.render_html(report)
        self.assertIn(reason, html)
        self.assertNotIn("MiB/s", html)
        self.assertIn("Host CPU, percent", html)
        self.assertEqual(report["summary"]["test_attempts"], 2)

    def test_missing_disk_sample_does_not_bridge_unknown_interval(self):
        self.samples[2]["disks"] = None
        self.write_samples()
        report = resources.analyze(self.directory)
        self.assertIn("Disk I/O counters are unavailable", report["summary"]["warnings"][0])
        disk_chart = resources.render_html(report).split(
            "read_bytes MiB/s (", 1
        )[1].split("</svg>", 1)[0]
        self.assertNotIn("200.00,", disk_chart)
        self.assertIn('<polyline points="500.00,0.00 1000.00,0.00"', disk_chart)

    def test_out_of_order_samples_rejected(self):
        self.samples[2]["elapsed_seconds"] = 1
        self.write_samples()
        with self.assertRaisesRegex(ValueError, "strictly increasing"):
            resources.analyze(self.directory)

    def test_html_escapes_test_names_and_has_no_external_dependencies(self):
        report = resources.analyze(self.directory)
        report["trials"][0].name = "<script>alert(1)</script>"
        html = resources.render_html(report)
        self.assertIn("&lt;script&gt;", html)
        self.assertNotIn("<script>", html)
        self.assertIn("read_bytes MiB/s", html)

    def test_short_run_within_sample_interval_still_plots_cpu(self):
        report = resources.analyze(self.directory)
        report["start"], report["end"] = 1.5, 2.5
        html = resources.render_html(report)
        cpu_chart = html.split("Host CPU, percent (")[1].split("</svg>")[0]
        self.assertIn('<polyline points="0.00,97.50 1000.00,97.50"', cpu_chart)

    def test_disk_throughput_maximum_uses_full_chart_height(self):
        for maximum in (0.5, 2.0):
            with self.subTest(maximum=maximum):
                for row in self.samples:
                    row["disks"][0]["read_bytes"] = int(
                        row["elapsed_seconds"] * maximum * 1024**2
                    )
                self.write_samples()
                html = resources.render_html(resources.analyze(self.directory))
                disk_chart = html.split(
                    f"read_bytes MiB/s (0 - {maximum:.1f})", 1
                )[1].split("</svg>", 1)[0]
                self.assertIn('<polyline points="0.00,0.00 ', disk_chart)

    def test_zero_concurrency_chart_stays_on_baseline(self):
        (self.directory / "junit.xml").write_text(
            '<testsuites timestamp="2026-01-01T00:00:00Z" time="10"/>'
        )
        html = resources.render_html(resources.analyze(self.directory))
        concurrency_chart = html.split(
            "Concurrent test attempts (0 - 0.0)", 1
        )[1].split("</svg>", 1)[0]
        self.assertIn('<polyline points="0.00,130.00 ', concurrency_chart)


if __name__ == "__main__":
    unittest.main()
