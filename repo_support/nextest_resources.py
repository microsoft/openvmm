#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Correlate a CI *-resources artifact's host samples with nextest JUnit timing.

Uses only the Python 3 standard library. Prints a JSON summary and optionally
writes a self-contained HTML timeline: nextest_resources.py DIR --html report.html
"""

from __future__ import annotations

import argparse
from collections import defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from html import escape
import json
import math
from pathlib import Path
import xml.etree.ElementTree as ET


GIB = 1024**3
RETRIES = {"flakyFailure", "flakyError", "rerunFailure", "rerunError"}


@dataclass
class Trial:
    name: str
    start: float
    end: float
    result: str


def number(value: object) -> float:
    result = float(value)
    if not math.isfinite(result):
        raise ValueError(f"non-finite measurement: {value}")
    return result


def timestamp_micros(value: str) -> int:
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError(f"JUnit timestamp has no timezone: {value}")
    delta = parsed - datetime(1970, 1, 1, tzinfo=timezone.utc)
    return (delta.days * 86400 + delta.seconds) * 1000000 + delta.microseconds


def timestamp(value: str) -> float:
    return timestamp_micros(value) / 1000000


def read_trials(root: ET.Element, epoch: float) -> list[Trial]:
    trials = []
    for case in root.iter("testcase"):
        if case.find("skipped") is not None:
            continue
        name = f"{case.get('classname', '')}::{case.attrib['name']}"
        result = "pass"
        if case.find("failure") is not None or case.find("error") is not None:
            result = "fail"
        attempts = [(case, result)]
        attempts.extend((child, "retry") for child in case if child.tag in RETRIES)
        for attempt, outcome in attempts:
            start = timestamp_micros(attempt.attrib["timestamp"]) - round(epoch * 1000000)
            duration = number(attempt.attrib["time"])
            if duration < 0:
                raise ValueError(f"negative test duration: {name}")
            # Keep matching boundaries identical: subtracting floating-point Unix
            # timestamps can otherwise invent tiny overlaps and inflate the peak.
            end = start + round(duration * 1000000)
            trials.append(Trial(name, start / 1000000, end / 1000000, outcome))
    return sorted(trials, key=lambda trial: (trial.start, trial.name))


def concurrency(trials: list[Trial], start: float, end: float) -> list[tuple[float, int]]:
    changes = defaultdict(int)
    changes[start] = 0
    changes[end] = 0
    for trial in trials:
        left, right = max(start, trial.start), min(end, trial.end)
        if left < right:
            changes[left] += 1
            changes[right] -= 1
    running = 0
    points = []
    for time, change in sorted(changes.items()):
        running += change
        points.append((time, running))
    return points


def analyze(directory: Path) -> dict:
    metadata = json.loads((directory / "metadata.json").read_text())
    if metadata["schema_version"] != 1:
        raise ValueError("unsupported resource artifact schema")
    samples = [
        json.loads(line)
        for line in (directory / "samples.jsonl").read_text().splitlines()
    ]
    if len(samples) < 2:
        raise ValueError("at least two resource samples are required")
    times = [number(row["elapsed_seconds"]) for row in samples]
    if any(right <= left for left, right in zip(times, times[1:])):
        raise ValueError("resource sample times must be strictly increasing")

    root = ET.parse(directory / "junit.xml").getroot()
    epoch = number(metadata["started_unix_seconds"])
    start_micros = timestamp_micros(root.attrib["timestamp"]) - round(epoch * 1000000)
    start = start_micros / 1000000
    duration = number(root.attrib["time"])
    if duration <= 0:
        raise ValueError("JUnit run duration must be positive")
    end = (start_micros + round(duration * 1000000)) / 1000000
    trials = read_trials(root, epoch)
    points = concurrency(trials, start, end)
    histogram = defaultdict(float)
    for (left, running), (right, _) in zip(points, points[1:]):
        histogram[running] += right - left

    cpu_integral = cpu_seconds = covered_seconds = 0.0
    memory_rows = []
    for previous, row in zip(samples, samples[1:]):
        left = max(start, number(previous["elapsed_seconds"]))
        right = min(end, number(row["elapsed_seconds"]))
        if right <= left:
            continue
        interval = right - left
        covered_seconds += interval
        memory_rows.extend([previous, row])
        if row["cpu_percent"] is not None:
            cpu_integral += number(row["cpu_percent"]) * interval
            cpu_seconds += interval
    if not memory_rows:
        raise ValueError("resource samples do not overlap the JUnit test run")
    outcome_file = directory / "outcome.json"
    outcome = json.loads(outcome_file.read_text()) if outcome_file.exists() else None
    warnings = []
    if outcome is None or not outcome["collection_complete"]:
        warnings.append("Resource collection incomplete; inspect outcome.json and the job log.")
    if covered_seconds / duration < 0.99:
        warnings.append("Resource samples cover less than 99% of the JUnit run.")
    if cpu_seconds / duration < 0.99:
        warnings.append("Valid CPU samples cover less than 99% of the JUnit run.")
    if any(trial.start < start - 0.01 or trial.end > end + 0.01 for trial in trials):
        warnings.append("Some test attempts extend outside the JUnit run window.")
    last_start = max((trial.start for trial in trials), default=start)
    average_cpu = cpu_integral / cpu_seconds if cpu_seconds else None
    summary = {
        "test_run_seconds": duration,
        "nextest_command_seconds": times[-1],
        "test_attempts": len(trials),
        "retry_attempts": sum(trial.result == "retry" for trial in trials),
        "mean_concurrent_tests": sum(n * seconds for n, seconds in histogram.items()) / duration,
        "peak_concurrent_tests": max(histogram, default=0),
        "seconds_by_concurrent_tests": dict(sorted(histogram.items())),
        "drain_seconds_after_last_test_started": max(0, end - last_start) if trials else 0,
        "mean_host_cpu_percent": average_cpu,
        "mean_busy_logical_cpus": (
            average_cpu * metadata["logical_cpus"] / 100 if average_cpu is not None else None
        ),
        "cpu_coverage_percent": 100 * cpu_seconds / duration,
        "resource_coverage_percent": 100 * covered_seconds / duration,
        "peak_used_memory_gib": max(row["used_memory_bytes"] for row in memory_rows) / GIB,
        "min_available_memory_gib": min(row["available_memory_bytes"] for row in memory_rows) / GIB,
        "peak_used_swap_gib": max(row["used_swap_bytes"] for row in memory_rows) / GIB,
        "logical_cpus": metadata["logical_cpus"],
        "available_parallelism": metadata["available_parallelism"],
        "nextest_exit_code": outcome["nextest_exit_code"] if outcome else None,
        "warnings": warnings,
    }
    return dict(
        metadata=metadata, samples=samples, trials=trials, concurrency=points,
        start=start, end=end, summary=summary,
    )


def render_html(report: dict) -> str:
    start, end = report["start"], report["end"]
    duration = end - start
    width, height = 1000, 130

    def x(time: float) -> float:
        return width * (time - start) / duration

    def interval_values(values: list[tuple[float, float | None]]) -> list[tuple[float, float | None]]:
        points = []
        for (left, _), (right, value) in zip(values, values[1:]):
            if left < end and right > start:
                points.extend([(max(start, left), value), (min(end, right), value)])
        return points

    def chart(title: str, values: list[tuple[float, float | None]], maximum: float) -> str:
        # Split at missing observations instead of drawing across unknown CPU data.
        paths = []
        path = []
        scale_maximum = maximum if maximum > 0 else 1
        for time, value in values:
            if value is None:
                if path:
                    paths.append(path)
                    path = []
                continue
            if start <= time <= end:
                path.append(f"{x(time):.2f},{height - height * value / scale_maximum:.2f}")
        if path:
            paths.append(path)
        lines = "".join(
            f'<polyline points="{" ".join(path)}" fill="none" stroke="#2864b4" stroke-width="2"/>'
            for path in paths
        )
        return (
            f"<h2>{escape(title)} (0 - {maximum:.1f})</h2>"
            f'<svg viewBox="0 0 {width} {height}" role="img" aria-label="{escape(title)}">'
            f'<rect width="{width}" height="{height}" fill="#f1f3f5"/>{lines}</svg>'
        )

    samples = report["samples"]
    cpu = [(row["elapsed_seconds"], row["cpu_percent"]) for row in samples]
    memory = [(row["elapsed_seconds"], row["used_memory_bytes"] / GIB) for row in samples]
    parallel = []
    previous = 0
    for time, running in report["concurrency"]:
        parallel.extend([(time, previous), (time, running)])
        previous = running
    charts = chart("Concurrent test attempts", parallel, report["summary"]["peak_concurrent_tests"])
    charts += chart("Host CPU, percent", interval_values(cpu), 100)
    charts += chart(
        "Host used memory, GiB", interval_values(memory),
        report["metadata"]["total_memory_bytes"] / GIB,
    )

    disk_rates = defaultdict(list)
    for previous, row in zip(samples, samples[1:]):
        elapsed = row["elapsed_seconds"] - previous["elapsed_seconds"]
        before = {(disk["name"], disk["mount_point"]): disk for disk in previous["disks"]}
        for disk in row["disks"]:
            key = (disk["name"], disk["mount_point"])
            if key not in before:
                continue
            for counter in ["read_bytes", "written_bytes"]:
                delta = disk[counter] - before[key][counter]
                rate = delta / elapsed / 1024**2 if delta >= 0 else None
                rates = disk_rates[f"{key[0]} ({key[1]}) {counter} MiB/s"]
                if not rates:
                    rates.append((previous["elapsed_seconds"], None))
                rates.append((row["elapsed_seconds"], rate))
    for name, rates in sorted(disk_rates.items()):
        maximum = max((rate for _, rate in rates if rate is not None), default=0)
        if maximum:
            charts += chart(name, interval_values(rates), maximum)

    bars = []
    for trial in report["trials"]:
        left, right = max(start, trial.start), min(end, trial.end)
        if right <= left:
            continue
        color = {"pass": "#35844b", "fail": "#b73232", "retry": "#ae7418"}[trial.result]
        label = f"{trial.name} [{trial.result}] {trial.end - trial.start:.3f}s"
        bars.append(
            f'<tr><td>{escape(label)}</td><td><svg viewBox="0 0 {width} 18">'
            f'<rect x="{x(left):.2f}" width="{max(0.5, x(right) - x(left)):.2f}" '
            f'height="18" fill="{color}"><title>{escape(label)}</title></rect>'
            "</svg></td></tr>"
        )
    summary = escape(json.dumps(report["summary"], indent=2))
    metadata = escape(json.dumps(report["metadata"], indent=2))
    return (
        '<!doctype html><html lang="en"><meta charset="utf-8">'
        "<title>Nextest resource usage</title>"
        "<style>body{font:14px system-ui;margin:2em}svg{width:100%}"
        "td{padding:2px;vertical-align:top}td:first-child{max-width:40em;overflow-wrap:anywhere}"
        "td:last-child{width:60%}table{width:100%}pre{white-space:pre-wrap}</style>"
        "<h1>Nextest resource usage</h1>"
        "<p>All plots share an x-axis: seconds since test-run start, "
        f"0 at left to {duration:.1f} at right. Hover a test bar for its duration.</p>"
        "<p>CPU and memory are host-wide, not per-test attribution. Hypervisor guest "
        "work may not be visible in OS CPU counters. Low CPU alone does not establish "
        "scheduler inefficiency. Disk counters can alias across mount points; do not sum them.</p>"
        f"<h2>Summary</h2><pre>{summary}</pre>{charts}"
        f"<h2>Test attempts, in start order</h2><table>{''.join(bars)}</table>"
        f"<details><summary>Run metadata</summary><pre>{metadata}</pre></details></html>"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path, help="extracted *-resources artifact")
    parser.add_argument("--html", type=Path, help="write a self-contained HTML timeline")
    args = parser.parse_args()
    try:
        report = analyze(args.directory)
        if args.html:
            args.html.write_text(render_html(report), encoding="utf-8")
        print(json.dumps(report["summary"], indent=2, allow_nan=False))
    except (OSError, ValueError, KeyError, TypeError, ET.ParseError) as err:
        parser.exit(1, f"error: {err}\n")


if __name__ == "__main__":
    main()
