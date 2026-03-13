#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""investigate_ci.py - Quick CI failure investigation for OpenVMM PRs.

Usage:
    python repo_support/investigate_ci.py <PR_NUMBER_OR_RUN_ID>

Examples:
    python repo_support/investigate_ci.py 2946          # Investigate PR #2946
    python repo_support/investigate_ci.py 23017249697   # Investigate run directly

This script:
  1. Finds the most recent CI run (or uses the given run ID)
  2. Identifies failed jobs
  3. Downloads unit test JUnit XML artifacts and reports failures
  4. Downloads petri VMM test log artifacts for the run
  5. Finds tests with petri.failed markers
  6. Extracts ERROR/WARN lines from petri.jsonl for quick diagnosis

Requires: gh (GitHub CLI), authenticated to microsoft/openvmm
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path


REPO = "microsoft/openvmm"


def gh(*args: str, check: bool = True) -> str:
    """Run a gh CLI command and return its stdout."""
    result = subprocess.run(
        ["gh", *args],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        if check:
            print(f"ERROR: gh {' '.join(args)}", file=sys.stderr)
            print(result.stderr.strip(), file=sys.stderr)
            sys.exit(1)
        else:
            print(
                f"WARNING: gh {' '.join(args)} failed with exit code {result.returncode}",
                file=sys.stderr,
            )
            if result.stderr.strip():
                print(result.stderr.strip(), file=sys.stderr)
    return result.stdout.strip()


# Workflow names for the main CI pipelines, in priority order.
_CI_WORKFLOW_NAMES = ["OpenVMM PR", "[Optional] OpenVMM Release PR", "OpenVMM Docs PR"]


def _pick_best_run(runs: list[dict]) -> dict | None:
    """Pick the most relevant run from a list, preferring failed CI runs."""
    if not runs:
        return None

    # Conclusions that indicate a non-successful run.
    _failure_conclusions = {"failure", "timed_out", "cancelled", "startup_failure", "action_required"}

    # First pass: prefer a failed run from a known CI workflow.
    for name in _CI_WORKFLOW_NAMES:
        for r in runs:
            if r.get("name") == name and r.get("conclusion") in _failure_conclusions:
                return r

    # Second pass: any run from a known CI workflow.
    for name in _CI_WORKFLOW_NAMES:
        for r in runs:
            if r.get("name") == name:
                return r

    # Fallback: first run.
    return runs[0]


def resolve_run_id(input_val: str) -> str:
    """Resolve a PR number or run ID string to a run ID."""
    try:
        num = int(input_val)
    except ValueError:
        print(
            f"ERROR: '{input_val}' is not a valid PR number or run ID",
            file=sys.stderr,
        )
        sys.exit(1)

    # Try to interpret as a PR number first.
    print(f"==> Trying to resolve '{input_val}' as PR #{num}...")
    pr_proc = subprocess.run(
        [
            "gh",
            "pr",
            "view",
            str(num),
            "-R",
            REPO,
            "--json",
            "headRefOid",
        ],
        capture_output=True,
        text=True,
        check=False,
    )

    if pr_proc.returncode == 0 and pr_proc.stdout.strip():
        try:
            head_sha = json.loads(pr_proc.stdout)["headRefOid"]
        except (json.JSONDecodeError, KeyError):
            head_sha = None

        if head_sha:
            print(f"    PR head SHA: {head_sha}")
            runs_json = gh(
                "run",
                "list",
                "-R",
                REPO,
                "--commit",
                head_sha,
                "--json",
                "databaseId,status,conclusion,name",
            )
            runs = json.loads(runs_json)
            if not runs:
                print(f"ERROR: No CI runs found for PR #{num}", file=sys.stderr)
                sys.exit(1)

            chosen = _pick_best_run(runs)
            assert chosen is not None
            run_id = str(chosen["databaseId"])
            print(f"    Found run: {chosen.get('name', '?')} (ID: {run_id})")
            return run_id

    # PR lookup failed or returned invalid data; treat as run ID.
    print(f"==> Treating '{input_val}' as run ID...")
    return input_val


def get_run_status(run_id: str) -> None:
    """Print the run's status and conclusion."""
    print()
    print("==> Run status:")
    run_json = gh("run", "view", run_id, "-R", REPO, "--json", "status,conclusion,name")
    run = json.loads(run_json)
    conclusion = run.get("conclusion") or "pending"
    print(f"  {run['name']}: {run['status']} ({conclusion})")


def get_failed_jobs(run_id: str) -> list[dict]:
    """Return list of failed job dicts with 'name' and 'databaseId'."""
    print()
    print("==> Checking for failed jobs...")
    jobs_json = gh("run", "view", run_id, "-R", REPO, "--json", "jobs")
    jobs = json.loads(jobs_json)["jobs"]
    non_success = {"failure", "timed_out", "cancelled", "startup_failure", "action_required"}
    failed = [j for j in jobs if j.get("conclusion") in non_success]

    if not failed:
        print("    No failed jobs found.")
    else:
        print("    Failed jobs:")
        for j in failed:
            print(f"      - {j['name']}")

    return failed


def list_artifacts(run_id: str) -> list[str]:
    """List all artifact names for a run."""
    api_json = gh(
        "api", f"repos/{REPO}/actions/runs/{run_id}/artifacts",
        "--paginate", check=False,
    )
    if not api_json:
        return []
    names: list[str] = []
    # --paginate concatenates multiple JSON objects; parse each one.
    decoder = json.JSONDecoder()
    pos = 0
    text = api_json.strip()
    while pos < len(text):
        try:
            obj, end = decoder.raw_decode(text, pos)
            names.extend(a["name"] for a in obj.get("artifacts", []))
            pos = end
        except json.JSONDecodeError as e:
            print(f"WARNING: Failed to parse artifact JSON at position {pos}: {e}", file=sys.stderr)
            print(f"  Context: ...{text[max(0, pos-20):pos+40]}...", file=sys.stderr)
            break
        # skip whitespace between objects
        while pos < len(text) and text[pos] in " \t\n\r":
            pos += 1
    return names


def list_test_log_artifacts(all_artifacts: list[str]) -> list[str]:
    """List available *-vmm-tests-logs artifact names for a run."""
    print()
    print("==> Listing available test log artifacts...")
    log_artifacts = [n for n in all_artifacts if n.endswith("-vmm-tests-logs")]
    if log_artifacts:
        print("    Available artifacts:")
        for name in log_artifacts:
            print(f"      - {name}")
    else:
        print("    No vmm-tests-logs artifacts found.")
        print("    This run may not have produced test artifacts (build failure?)")

    return log_artifacts


def list_junit_artifacts(all_artifacts: list[str]) -> list[str]:
    """List available *-unit-tests-junit-xml artifact names."""
    return [n for n in all_artifacts if n.endswith("-unit-tests-junit-xml")]


def download_artifacts(run_id: str, artifact_names: list[str], workdir: Path) -> None:
    """Download test log artifacts into workdir."""
    print()
    print(f"==> Downloading test log artifacts to {workdir}...")
    for name in artifact_names:
        dest = workdir / name
        if dest.is_dir():
            print(f"    {name} (cached)")
            continue
        print(f"    Downloading {name}...")
        result = subprocess.run(
            ["gh", "run", "download", run_id, "-R", REPO, "-n", name, "-D", str(dest)],
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            print(f"    WARNING: Failed to download {name}")
            if result.stderr.strip():
                for line in result.stderr.strip().splitlines():
                    print(f"      {line}")


def extract_errors_from_jsonl(jsonl_path: Path) -> list[str]:
    """Extract ERROR and WARN lines from a petri.jsonl file."""
    lines = []
    try:
        with open(jsonl_path, encoding="utf-8", errors="replace") as f:
            for raw_line in f:
                raw_line = raw_line.strip()
                if not raw_line:
                    continue
                try:
                    entry = json.loads(raw_line)
                except json.JSONDecodeError:
                    continue
                sev = entry.get("severity", "")
                if sev in ("ERROR", "WARN"):
                    ts = entry.get("timestamp", "?")
                    src = entry.get("source", "?")
                    msg = entry.get("message", "").strip()
                    if len(msg) > 200:
                        msg = msg[:200] + "..."
                    lines.append(f"    [{sev}] [{ts}] {src}: {msg}")
    except OSError as e:
        lines.append(f"    (failed to read petri.jsonl: {e})")
    return lines


def parse_junit_failures(xml_path: Path) -> list[dict]:
    """Parse a JUnit XML file and return a list of failure dicts.

    Each dict has keys: 'suite', 'test', 'message', 'output'.
    """
    failures: list[dict] = []
    try:
        # CI artifacts come from potentially-untrusted PR code.
        # Python's expat-based parser does not resolve external entities and
        # has built-in entity expansion limits, but use defusedxml when
        # available for belt-and-suspenders protection against XML DoS.
        try:
            import defusedxml.ElementTree as SafeET
            tree = SafeET.parse(xml_path)
        except ImportError:
            tree = ET.parse(xml_path)
    except (ET.ParseError, OSError) as e:
        failures.append({"suite": "?", "test": "?", "message": f"(failed to parse JUnit XML: {e})", "output": ""})
        return failures

    for testcase in tree.iter("testcase"):
        failure = testcase.find("failure")
        error = testcase.find("error")
        elem = failure if failure is not None else error
        if elem is None:
            continue
        suite = testcase.get("classname", "")
        name = testcase.get("name", "")
        msg = elem.get("message", "")

        # nextest puts the actual test output in <system-out>/<system-err>
        # rather than in the <failure> element body, so collect all sources.
        output_parts: list[str] = []
        failure_text = (elem.text or "").strip()
        if failure_text:
            output_parts.append(failure_text)
        for tag in ("system-out", "system-err"):
            el = testcase.find(tag)
            if el is not None and el.text and el.text.strip():
                output_parts.append(el.text.strip())
        output = "\n".join(output_parts)

        failures.append({"suite": suite, "test": name, "message": msg, "output": output})

    return failures


def show_junit_failures(run_id: str, junit_artifact_names: list[str], workdir: Path) -> int:
    """Download JUnit XML artifacts and display any failures. Returns failure count."""
    if not junit_artifact_names:
        return 0

    print()
    print("==> Downloading unit test JUnit XML artifacts...")
    total_failures = 0

    for name in junit_artifact_names:
        dest = workdir / name
        if not dest.is_dir():
            print(f"    Downloading {name}...")
            result = subprocess.run(
                ["gh", "run", "download", run_id, "-R", REPO, "-n", name, "-D", str(dest)],
                capture_output=True,
                text=True,
                check=False,
            )
            if result.returncode != 0:
                print(f"    WARNING: Failed to download {name}")
                continue
        else:
            print(f"    {name} (cached)")

        # Find all XML files in the artifact directory.
        xml_files = sorted(dest.rglob("*.xml"))
        if not xml_files:
            print(f"    WARNING: No XML files found in {name}")
            continue

        for xml_file in xml_files:
            failures = parse_junit_failures(xml_file)
            if not failures:
                continue
            total_failures += len(failures)
            for f in failures:
                print()
                print(f"  FAIL: {f['suite']}::{f['test']}")
                if f["message"]:
                    msg = f["message"]
                    if len(msg) > 300:
                        msg = msg[:300] + "..."
                    print(f"    Message: {msg}")
                if f["output"]:
                    # Extract the most useful lines from the test output.
                    # nextest system-out includes the full "running 1 test"
                    # harness output; we want the failure details.
                    output_lines = f["output"].splitlines()
                    # Look for the "failures:" section which has the cause.
                    useful_lines: list[str] = []
                    in_failures = False
                    for line in output_lines:
                        stripped = line.strip()
                        if stripped == "failures:" and not in_failures:
                            in_failures = True
                            continue
                        if in_failures:
                            # Stop at "test result:" or a second "failures:" header
                            if stripped.startswith("test result:") or stripped == "failures:":
                                break
                            if stripped:
                                useful_lines.append(stripped)
                    if useful_lines:
                        for line in useful_lines[:15]:
                            print(f"    {line}")
                        if len(useful_lines) > 15:
                            print(f"    ... ({len(useful_lines) - 15} more lines)")
                    else:
                        # Fallback: show up to 10 lines of raw output.
                        for line in output_lines[:10]:
                            print(f"    {line}")
                        if len(output_lines) > 10:
                            print(f"    ... ({len(output_lines) - 10} more lines)")

    return total_failures


def show_build_failure_log(run_id: str, failed_jobs: list[dict]) -> None:
    """For build failures with no test artifacts, show the CI log tail."""
    print()
    print("==> Checking CI log for errors...")
    if not failed_jobs:
        return
    job = failed_jobs[0]
    job_id = str(job["databaseId"])
    print(f"    Last 50 lines of '{job['name']}':")
    log = gh("run", "view", run_id, "-R", REPO, "--job", job_id, "--log", check=False)
    if log:
        for line in log.splitlines()[-50:]:
            print(f"    {line}")


def find_failed_tests(workdir: Path) -> list[Path]:
    """Find all petri.failed marker files under workdir."""
    return sorted(workdir.rglob("petri.failed"))


def main() -> None:
    if len(sys.argv) == 2 and sys.argv[1] in ("-h", "--help"):
        print(__doc__)
        sys.exit(0)

    if len(sys.argv) != 2:
        print(__doc__)
        sys.exit(1)

    run_id = resolve_run_id(sys.argv[1])
    get_run_status(run_id)
    failed_jobs = get_failed_jobs(run_id)

    if not failed_jobs:
        sys.exit(0)

    all_artifacts = list_artifacts(run_id)
    vmm_artifact_names = list_test_log_artifacts(all_artifacts)
    junit_artifact_names = list_junit_artifacts(all_artifacts)

    if not vmm_artifact_names and not junit_artifact_names:
        show_build_failure_log(run_id, failed_jobs)
        sys.exit(1)

    # Set up work directory
    tmpdir_base = Path(tempfile.gettempdir()) / "openvmm-ci-investigate"
    workdir = tmpdir_base / run_id
    workdir.mkdir(parents=True, exist_ok=True)

    # --- Unit test failures (JUnit XML) ---
    junit_failure_count = 0
    if junit_artifact_names:
        print()
        print("==========================================")
        print("  UNIT TEST FAILURES (JUnit XML)")
        print("==========================================")
        junit_failure_count = show_junit_failures(run_id, junit_artifact_names, workdir)
        if junit_failure_count == 0:
            print("  No unit test failures found in JUnit XML artifacts.")
        else:
            print()
            print(f"  Total unit test failures: {junit_failure_count}")

    # --- VMM test failures (petri) ---
    failed_markers: list[Path] = []
    if vmm_artifact_names:
        download_artifacts(run_id, vmm_artifact_names, workdir)

        print()
        print("==========================================")
        print("  VMM TEST FAILURES (petri)")
        print("==========================================")

        failed_markers = find_failed_tests(workdir)

        if not failed_markers:
            print("  No petri.failed markers found.")
            if junit_failure_count == 0:
                print("  Tests may have passed, or failure occurred before test execution.")
                sys.exit(0)
        else:
            print(f"  Found {len(failed_markers)} failed test(s):")
            print()

    for marker in failed_markers:
        test_dir = marker.parent
        try:
            test_name = marker.read_text(encoding="utf-8", errors="replace").strip()
        except OSError:
            test_name = test_dir.name

        print("  ----------------------------------------")
        print(f"  TEST: {test_name}")
        print(f"  DIR:  {test_dir}")
        print("  ----------------------------------------")

        jsonl_file = test_dir / "petri.jsonl"
        if jsonl_file.is_file():
            print()
            print("  ERROR/WARN entries from petri.jsonl:")
            error_lines = extract_errors_from_jsonl(jsonl_file)
            for line in error_lines:
                print(line)
        else:
            print("  (no petri.jsonl found)")

        print()

    # Summary
    print("==========================================")
    print("  SUMMARY")
    print("==========================================")
    print(f"  Run ID:       {run_id}")
    print(f"  Logview URL:  https://openvmm.dev/test-results/#/runs/{run_id}")
    if junit_failure_count > 0:
        print(f"  Unit test failures: {junit_failure_count}")
    if failed_markers:
        print(f"  VMM test failures:  {len(failed_markers)}")
    if junit_failure_count == 0 and not failed_markers:
        print("  No test failures found.")
    print()
    print(f"  For full logs, examine files in: {workdir}")
    print("  Each test directory may contain:")
    print("    petri.jsonl  - Structured JSON log (primary)")
    print("    petri.log    - Plain text log")
    print("    openhcl.log  - OpenHCL serial console (if test exercised OpenHCL)")
    print("    hyperv.log   - Hyper-V event log (if test uses Hyper-V backend)")
    print("    openvmm.log  - OpenVMM serial console (if test uses OpenVMM backend)")
    print("    guest.log    - Guest OS serial output")
    print("    uefi.log     - UEFI serial output")
    print()
    print(f"  To view in browser: https://openvmm.dev/test-results/#/runs/{run_id}")


if __name__ == "__main__":
    main()
