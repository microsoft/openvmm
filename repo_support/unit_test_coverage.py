#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Local unit coverage worker and JSON reader; collection is configured by Flowey."""

import argparse
import bisect
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time


SCHEMA_VERSION = 2
PROFILE_DIR_ENV = "OPENVMM_UNIT_COVERAGE_PROFILES"
SUPPORTED_TARGETS = {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"}


class StaleCoverageError(ValueError):
    """Coverage belongs to a different source snapshot than the current checkout."""

    def __init__(self, freshness):
        super().__init__("coverage is stale; rerun cargo xflowey unit-test-coverage")
        self.freshness = freshness


def write_json(path, value):
    """Replace a JSON record atomically, including while a test is running."""
    path = Path(path)
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", dir=path.parent, delete=False
    ) as stream:
        temporary = Path(stream.name)
        try:
            json.dump(value, stream, sort_keys=True)
            stream.write("\n")
        except BaseException:
            temporary.unlink()
            raise
    temporary.replace(path)


def capture(args, *, cwd, env=None):
    return subprocess.check_output(args, cwd=cwd, env=env, text=True)


def repository_root():
    return Path(capture(
        ["git", "rev-parse", "--show-toplevel"], cwd=Path.cwd(),
    ).strip()).resolve()


def source_snapshot(root, excluded=()):
    """Identify HEAD plus tracked changes and nonignored untracked content."""
    root = Path(root)
    head = capture(["git", "rev-parse", "HEAD"], cwd=root).strip()
    diff = subprocess.check_output(
        ["git", "--no-pager", "diff", "--no-ext-diff", "--no-textconv", "--binary", "HEAD"],
        cwd=root,
    )
    untracked = subprocess.check_output(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"], cwd=root
    ).split(b"\0")
    digest = hashlib.sha256()
    digest.update(head.encode() + b"\0" + diff)
    dirty = bool(diff)
    for raw in sorted(filter(None, untracked)):
        path = root / os.fsdecode(raw)
        if any(path.is_relative_to(directory) for directory in excluded):
            continue
        dirty = True
        digest.update(b"\0" + raw + b"\0")
        if path.is_symlink():
            digest.update(os.fsencode(os.readlink(path)))
        else:
            with path.open("rb") as stream:
                for block in iter(lambda: stream.read(65536), b""):
                    digest.update(block)
    return {
        "head": head,
        "dirty": dirty,
        "snapshot_sha256": digest.hexdigest(),
        "excluded_paths": sorted({
            path.relative_to(root).as_posix()
            for path in excluded if path.is_relative_to(root)
        }),
    }


def snapshot_exclusions(report, root):
    paths = []
    for name in report["source"].get("excluded_paths", []):
        path = Path(name)
        if path.is_absolute() or ".." in path.parts or not path.parts:
            raise ValueError(f"invalid snapshot exclusion: {name!r}")
        paths.append(root / path)
    return paths


def check_freshness(report, root):
    current = source_snapshot(root, snapshot_exclusions(report, root))
    matches = all(
        current[key] == report["source"][key]
        for key in ("head", "dirty", "snapshot_sha256")
    )
    return {"state": "current" if matches else "stale", "current_source": current}


def resolve_report(root, report_path=None):
    if report_path is not None:
        return Path(report_path).resolve()
    pointer = root / "target/unit-coverage/latest.json"
    if not pointer.is_file():
        raise ValueError(
            "no active coverage report; run cargo xflowey unit-test-coverage "
            "or use --report to inspect an older result"
        )
    latest = json.loads(pointer.read_text(encoding="utf-8"))
    if latest["schema_version"] != SCHEMA_VERSION:
        raise ValueError("unsupported latest-report index schema; collect coverage again")
    path = Path(latest["report"])
    if not path.is_file():
        raise ValueError("the latest report was removed; collect coverage again")
    return path


def resolve_comparison(root, base):
    if base is None:
        result = subprocess.run(
            ["git", "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
            cwd=root, text=True, capture_output=True,
        )
        if result.returncode:
            raise ValueError("no PR base recorded or origin/HEAD configured; supply --base <revision>")
        base = result.stdout.strip()
    resolved = capture(
        ["git", "rev-parse", "--verify", "--end-of-options", f"{base}^{{commit}}"], cwd=root,
    ).strip()
    merge_base = capture(["git", "merge-base", "HEAD", resolved], cwd=root).strip()
    return {"requested": base, "resolved": resolved, "merge_base": merge_base}


def changed_source_lines(root, comparison, excluded=()):
    """Return new-side Rust lines, including staged, unstaged, and untracked changes."""
    merge_base = comparison["merge_base"]
    git_diff = [
        "git", "--no-pager", "diff", "--no-ext-diff", "--no-textconv",
        "--no-color", "--find-renames",
    ]
    records = subprocess.check_output(
        git_diff + ["--name-status", "-z", merge_base, "--"], cwd=root,
    ).split(b"\0")
    changed = {}
    other_files = 0
    deleted_files = 0
    position = 0
    while position < len(records) and records[position]:
        status = records[position].decode("ascii")
        position += 1
        names = [os.fsdecode(records[position])]
        position += 1
        if status[0] in ("R", "C"):
            names.append(os.fsdecode(records[position]))
            position += 1
        name = names[-1]
        if status[0] == "D":
            deleted_files += 1
            continue
        if Path(name).suffix != ".rs":
            other_files += 1
            continue
        text = capture(
            git_diff + ["--unified=0", "--inter-hunk-context=0", merge_base, "--", *names],
            cwd=root,
        )
        lines = set()
        for match in re.finditer(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@", text, re.MULTILINE):
            start = int(match[1])
            count = int(match[2]) if match[2] is not None else 1
            lines.update(range(start, start + count))
        changed[name] = lines
    untracked = subprocess.check_output(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"], cwd=root,
    ).split(b"\0")
    for raw in filter(None, untracked):
        name = os.fsdecode(raw)
        path = root / name
        if any(path.is_relative_to(directory) for directory in excluded):
            continue
        if path.suffix != ".rs":
            other_files += 1
            continue
        # A symlink's Git diff describes its target name, not Rust source lines.
        if path.is_symlink():
            changed[name] = set()
            continue
        data = path.read_bytes()
        count = data.count(b"\n") + int(bool(data) and not data.endswith(b"\n"))
        changed[name] = set(range(1, count + 1))
    return changed, other_files, deleted_files


def parse_environment(text):
    """Read cargo-llvm-cov assignments as data, never evaluate shell output."""
    env = {}
    for line in text.splitlines():
        key, separator, value = line.partition("=")
        if not separator or not key or not all(
            c.isascii() and (c.isalnum() or c == "_") for c in key
        ):
            raise ValueError(f"invalid cargo-llvm-cov environment assignment: {line!r}")
        values = shlex.split(value)
        if len(values) > 1:
            raise ValueError(f"invalid cargo-llvm-cov environment value for {key}")
        env[key] = values[0] if values else ""
    return env


def nextest_runner(command):
    """Assign profiles to one nextest attempt before starting any test threads."""
    if not command:
        raise ValueError("the coverage runner requires a test command")
    directory = Path(os.environ[PROFILE_DIR_ENV])
    env = os.environ.copy()
    name = env.get("NEXTEST_TEST_NAME")
    if name is None:
        env["LLVM_PROFILE_FILE"] = str(directory / "listing" / "%m-%p.profraw")
        return subprocess.call(command, env=env)

    identity = {
        "binary_id": env["NEXTEST_BINARY_ID"],
        "name": name,
        "attempt": int(env["NEXTEST_ATTEMPT"]),
        "attempt_id": env["NEXTEST_ATTEMPT_ID"],
        "nextest_run_id": env["NEXTEST_RUN_ID"],
    }
    test_id = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    attempt_dir = directory / test_id
    attempt_dir.mkdir()
    record = {
        **identity,
        "id": test_id,
        "result": "running",
        "exit_code": None,
    }
    write_json(attempt_dir / "attempt.json", record)
    env["LLVM_PROFILE_FILE"] = str(attempt_dir / "%m-%p.profraw")
    result = subprocess.call(command, env=env)
    record.update(result="passed" if result == 0 else "failed", exit_code=result)
    write_json(attempt_dir / "attempt.json", record)
    return result if result >= 0 else 1


def parse_lcov(text, root, source_files):
    """Extract source line hits, retaining zero counts and excluding generated files."""
    files = {}
    current = None
    for line in text.splitlines():
        if line.startswith("SF:"):
            path = Path(line[3:])
            if not path.is_absolute():
                path = root / path
            resolved = path.resolve()
            relative = resolved.relative_to(root) if resolved.is_relative_to(root) else None
            name = relative.as_posix() if relative is not None else None
            current = files.setdefault(name, {}) if name in source_files else None
        elif line.startswith("DA:") and current is not None:
            fields = line[3:].split(",")
            number, count = int(fields[0]), int(fields[1])
            if number <= 0 or count < 0:
                raise ValueError(f"invalid LCOV line count: {line}")
            current[number] = current.get(number, False) or count > 0
        elif line == "end_of_record":
            current = None
    return files


def merge_lines(destination, source):
    for name, lines in source.items():
        target = destination.setdefault(name, {})
        for line, covered in lines.items():
            target[line] = target.get(line, False) or covered


def export_lines(llvm_cov, binaries, profile, root, source_files, output):
    args = [str(llvm_cov), "export", "--format=lcov"]
    args += [f"--instr-profile={profile}"] if profile else ["--empty-profile"]
    for binary in binaries:
        args += ["--object", str(binary)]
    with output.open("w", encoding="utf-8") as stream:
        subprocess.run(args, cwd=root, stdout=stream, check=True)
    return parse_lcov(output.read_text(encoding="utf-8"), root, source_files)


def selected_tests(inventory):
    return {
        (suite["binary-id"], name)
        for suite in inventory["rust-suites"].values()
        for name, test in suite["testcases"].items()
        if not test["ignored"] and test["filter-match"]["status"] == "matches"
    }


def coverage_target(runs):
    targets = set()
    for run in runs:
        args = run["args"]
        if args.count("--target") != 1 or args[-1] == "--target":
            raise ValueError("coverage runs require exactly one explicit --target")
        targets.add(args[args.index("--target") + 1])
    if len(targets) != 1 or not targets <= SUPPORTED_TARGETS:
        raise ValueError("coverage requires one Linux GNU x64 or ARM64 target across all runs")
    return targets.pop()


def collect_run(root, output, plan, run, cargo, llvm_cov, llvm_profdata, source_files, report):
    started = time.monotonic()
    run_dir = output / "runs" / run["name"]
    run_dir.mkdir(parents=True)
    profiles = run_dir / "profiles"
    profiles.mkdir()
    target = coverage_target([run])
    cache = root / "target" / "unit-coverage-build" / target / run["name"]
    env = {
        **os.environ,
        **run["env"],
        "CARGO_TARGET_DIR": str(cache),
        "CARGO_LLVM_COV_TARGET_DIR": str(cache),
        "CARGO_LLVM_COV_BUILD_DIR": str(cache),
        # Fail explicitly rather than installing tools as a side effect of collection.
        "CARGO_LLVM_COV_SETUP": "no",
    }
    build_args = run["args"]
    env.update(parse_environment(capture(
        cargo + ["llvm-cov", "show-env", "--target", target, "--no-cfg-coverage"],
        cwd=root, env=env,
    )))
    env[PROFILE_DIR_ENV] = str(profiles)
    # Keep profiles produced by listing or build-time execution out of test evidence.
    env["LLVM_PROFILE_FILE"] = str(profiles / "listing" / "%m-%p.profraw")
    args = build_args + ["--config-file", plan["nextest_config"]]
    if plan["filter"]:
        args += ["-E", plan["filter"]]
    inventory = json.loads(capture(
        cargo + ["nextest", "list", "--profile", "agent", "--message-format", "json"] + args,
        cwd=root, env=env,
    ))
    write_json(run_dir / "inventory.json", inventory)
    expected = selected_tests(inventory)
    run_record = {
        "name": run["name"],
        "build_args": build_args,
        "expected_tests": len(expected),
        "exit_code": None,
        "build_and_list_seconds": time.monotonic() - started,
    }
    report["runs"].append(run_record)
    binaries_dir = run_dir / "binaries"
    binaries_dir.mkdir()
    binaries = {}
    for suite in inventory["rust-suites"].values():
        binary_id = suite["binary-id"]
        destination = binaries_dir / hashlib.sha256(binary_id.encode()).hexdigest()
        shutil.copy2(suite["binary-path"], destination)
        binaries[binary_id] = destination
    if not binaries:
        raise ValueError(f"{run['name']}: no test binaries were built")
    merge_lines(report["files"], export_lines(
        llvm_cov, binaries.values(), None, root, source_files, run_dir / "baseline.lcov",
    ))
    if not expected:
        run_record["exit_code"] = 0
        return
    print(f"Collecting {len(expected)} tests ({run['name']})", file=sys.stderr)
    test_started = time.monotonic()
    result = subprocess.call(
        cargo + ["nextest", "run", "--profile", "agent", "--no-fail-fast"] + args,
        cwd=root, env=env,
    )
    run_record["exit_code"] = result
    run_record["test_seconds"] = time.monotonic() - test_started
    if result != 0:
        if result == 100:
            print(f"{run['name']}: tests failed; collecting their profiles", file=sys.stderr)
        else:
            report["issues"].append(f"{run['name']}: nextest execution failed with code {result}")
    observed = set()
    for attempt in sorted(profiles.glob("*/attempt.json")):
        record = json.loads(attempt.read_text(encoding="utf-8"))
        record["run"] = run["name"]
        record["covered"] = {}
        report["tests"].append(record)
        key = (record["binary_id"], record["name"])
        if key not in expected:
            raise ValueError(f"unexpected test identity in coverage: {key}")
        if record["result"] != "running":
            observed.add(key)
        raw = sorted(attempt.parent.glob("*.profraw"))
        if not raw:
            record["profile_state"] = "missing"
            report["issues"].append(f"{run['name']}: missing profile for {key}")
            continue
        if record["result"] == "running":
            report["issues"].append(f"{run['name']}: incomplete attempt for {key}")
        profdata = attempt.parent / "coverage.profdata"
        subprocess.run(
            [str(llvm_profdata), "merge", "-sparse", *map(str, raw), "-o", str(profdata)],
            check=True,
        )
        lines = export_lines(
            llvm_cov, [binaries[record["binary_id"]]], profdata,
            root, source_files, attempt.parent / "coverage.lcov",
        )
        record["profile_state"] = "available"
        record["covered"] = {
            name: sorted(line for line, hit in hits.items() if hit)
            for name, hits in lines.items() if any(hits.values())
        }
        merge_lines(report["files"], lines)
    if observed != expected:
        report["issues"].append(
            f"{run['name']}: {len(expected - observed)} selected tests lack a completed attempt"
        )


def test_outcome(report):
    runs = report["runs"]
    codes = [run["exit_code"] for run in runs]
    if 100 in codes:
        return "failed"
    if any(code not in (0, None) for code in codes):
        return "incomplete"
    if len(runs) != len(report["scope"]["runs"]) or None in codes:
        return "pending" if report["collection_state"] == "collecting" else "incomplete"
    return "passed" if any(run["expected_tests"] for run in runs) else "not_run"


def collect_source_map(root, output, plan, cargo, report):
    metadata = json.loads(capture(
        cargo + ["metadata", "--locked", "--no-deps", "--format-version", "1"], cwd=root,
    ))
    members = set(metadata["workspace_members"])
    request = output / "source-map-request.json"
    response = output / "source-map.json"
    write_json(request, {
        "files": sorted(report["files"]),
        "packages": [p for p in metadata["packages"] if p["id"] in members],
    })
    subprocess.run(
        [plan["xtask"], "--custom-root", str(root), "coverage-sources",
         "--input", str(request), "--output", str(response)],
        cwd=root, check=True,
    )
    report["source_map"] = json.loads(response.read_text(encoding="utf-8"))


def collect(plan_path, output):
    started = time.monotonic()
    root = repository_root()
    latest = root / "target/unit-coverage/latest.json"
    latest.unlink(missing_ok=True)
    output = Path(output).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.mkdir()  # Never reuse execution counts from an earlier invocation.
    plan = json.loads(Path(plan_path).read_text(encoding="utf-8"))
    shutil.copy2(plan["nextest_config"], output / "nextest.toml")
    plan["nextest_config"] = str(output / "nextest.toml")
    write_json(output / "plan.json", plan)
    cargo = ["cargo"] + ([f"+{plan['toolchain']}"] if plan["toolchain"] else [])
    rustc = ["rustc"] + ([f"+{plan['toolchain']}"] if plan["toolchain"] else [])
    excluded = [
        output, root / "target/unit-coverage-build", root / "target/unit-coverage",
    ]
    snapshot = source_snapshot(root, excluded)
    source_files = {
        name for name in capture(
            ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=root,
        ).split("\0") if name
    }
    report = {
        "schema_version": SCHEMA_VERSION,
        "family": "unit",
        "origin": "local",
        "source": snapshot,
        "target": None,
        "host": None,
        "build_profile": "dev",
        "nextest_profile": "agent",
        "collection_state": "collecting",
        "test_outcome": "pending",
        "scope": {
            "filter": plan["filter"],
            "runs": plan["runs"],
            "includes_test_source": True,
            "doctests": False,
            "separate_child_binaries": False,
        },
        "runs": [],
        "tests": [],
        "files": {},
        "issues": [],
    }
    write_json(output / "report.json", report)
    latest.parent.mkdir(parents=True, exist_ok=True)
    # Point to the newest attempt even if it fails; never fall back to old success.
    write_json(latest, {"schema_version": SCHEMA_VERSION, "report": str(output / "report.json")})
    try:
        report["target"] = coverage_target(plan["runs"])
        if plan.get("base") is not None:
            report["comparison"] = resolve_comparison(root, plan["base"])
        report["rustc"] = capture(rustc + ["-vV"], cwd=root).strip()
        host = re.search(r"^host: (\S+)$", report["rustc"], re.MULTILINE)
        if host is None or host[1] not in SUPPORTED_TARGETS:
            raise ValueError("coverage requires a Linux GNU x64 or ARM64 compiler host")
        report["host"] = host[1]
        report["cargo_llvm_cov"] = capture(cargo + ["llvm-cov", "--version"], cwd=root).strip()
        sysroot = Path(capture(rustc + ["--print", "sysroot"], cwd=root).strip())
        # LLVM tools execute on the compiler host, which may differ from the test target.
        llvm_dir = sysroot / "lib/rustlib" / report["host"] / "bin"
        llvm_cov, llvm_profdata = llvm_dir / "llvm-cov", llvm_dir / "llvm-profdata"
        if not llvm_cov.is_file() or not llvm_profdata.is_file():
            raise ValueError("missing LLVM tools; run: rustup component add llvm-tools-preview")
        for run in plan["runs"]:
            collect_run(
                root, output, plan, run, cargo, llvm_cov, llvm_profdata, source_files, report,
            )
            write_json(output / "report.json", report)
        if not report["tests"]:
            report["issues"].append(
                "no test-attempt records; coverage is unavailable"
                if any(run["expected_tests"] for run in report["runs"])
                else "no tests selected; coverage is unavailable"
            )
        collect_source_map(root, output, plan, cargo, report)
        if source_snapshot(root, excluded) != snapshot:
            report["issues"].append("source changed during collection; coverage snapshot is invalid")
        report["collection_state"] = "incomplete" if report["issues"] else "complete"
    except Exception as error:
        report["collection_state"] = "incomplete"
        report["issues"].append(str(error))
        raise
    finally:
        report["duration_seconds"] = time.monotonic() - started
        report["test_outcome"] = test_outcome(report)
        report["summary"] = summarize(report["files"])
        write_json(output / "report.json", report)
        print(
            f"Coverage: {report['collection_state']}; tests: {report['test_outcome']}",
            file=sys.stderr,
        )
        print(f"Report: {output / 'report.json'}", file=sys.stderr)
    print_collection_summary(query(
        output / "report.json", changed=plan.get("base") is not None, root=root,
    ))
    return 0 if report["collection_state"] == "complete" and report["test_outcome"] == "passed" else 1


def print_collection_summary(result):
    for issue in result["issues"][:3]:
        print(f"Issue: {issue}")
    if len(result["issues"]) > 3:
        print(f"... and {len(result['issues']) - 3} more issues (see report)")
    production = result["sources"]["selected_production"]
    passing = production["passing_attempts"]
    changed = result["view"] == "changed"
    label = "Changed production" if changed else "Production"
    if passing["measured_lines"]:
        detail = (
            f"{passing['covered_lines']}/{passing['measured_lines']} "
            f"({passing['line_percent']:.1f}%)"
        )
    else:
        detail = "no measured lines"
    print(f"{label} lines reached by passing tests: {detail}")
    without_passing = production["execution"]["covered_lines"] - passing["covered_lines"]
    if without_passing:
        print(f"Executed production lines without passing-test evidence: {without_passing}")
    unknown = result["sources"]["unclassified"]["execution"]["measured_lines"]
    if unknown:
        print(f"Unclassified measured lines: {unknown}")
    if changed:
        patch = result["patch"]
        if patch["unmapped_lines"] or patch["unmeasured_files"]:
            print(
                f"Unmapped changed lines: {patch['unmapped_lines']}; "
                f"unmeasured changed files: {patch['unmeasured_files']}"
            )
    command = "python3 repo_support/unit_test_coverage.py query"
    print(f"JSON details: {command}" + (" --changed" if changed else ""))


def summarize(files):
    total = sum(len(lines) for lines in files.values())
    covered = sum(sum(lines.values()) for lines in files.values())
    return {
        "measured_lines": total,
        "covered_lines": covered,
        "line_percent": covered * 100 / total if total else None,
    }


def coverage_index(report):
    passed = {}
    for test in report["tests"]:
        if test["result"] == "passed" and test.get("profile_state") == "available":
            for name, lines in test["covered"].items():
                passed.setdefault(name, set()).update(lines)
    return passed


def selected_packages(report):
    return {
        args[index + 1]
        for run in report["scope"].get("runs", [])
        for args in [run["args"]]
        for index, value in enumerate(args[:-1]) if value == "-p"
    }


def source_category(report, name, number):
    metadata = report.get("source_map", {}).get("files", {}).get(name)
    if metadata is None:
        return "unclassified"
    ranges = metadata["ranges"]
    index = bisect.bisect_right(ranges, [number + 1]) - 1
    if index < 0 or number > ranges[index][1]:
        return "unclassified"
    role = ranges[index][2]
    if role == "production":
        return (
            "selected_production" if metadata["package"] in selected_packages(report)
            else "dependency_production"
        )
    return {"test": "test_source", "auxiliary": "auxiliary"}.get(role, "unclassified")


def evidence_summary(files, passed):
    return {
        "execution": summarize(files),
        "passing_attempts": summarize({
            name: {number: int(number) in passed.get(name, set()) for number in lines}
            for name, lines in files.items()
        }),
    }


def source_summaries(report, files, passed):
    categories = {
        name: {} for name in
        ("selected_production", "dependency_production", "test_source", "auxiliary", "unclassified")
    }
    for name, lines in files.items():
        for number, hit in lines.items():
            category = source_category(report, name, int(number))
            categories[category].setdefault(name, {})[number] = hit
    return {category: evidence_summary(files, passed) for category, files in categories.items()}


def line_details(report, name, number, hit, limit):
    tests = [
        {key: test[key] for key in
         ("id", "run", "binary_id", "name", "attempt", "result")}
        for test in report["tests"] if number in test["covered"].get(name, [])
    ]
    tests.sort(key=lambda t: (t["result"] != "passed", t["name"], t["run"], t["attempt"]))
    passing = any(
        t["result"] == "passed" and t.get("profile_state") == "available"
        and number in t["covered"].get(name, []) for t in report["tests"]
    )
    return {
        "line": number, "covered": hit, "tests": tests[:limit],
        "tests_truncated": len(tests) > limit,
        "covered_by_passing_attempt": passing,
        "source_category": source_category(report, name, number),
    }


def line_ranges(numbers):
    ranges = []
    for number in sorted(numbers):
        if ranges and number == ranges[-1][1] + 1:
            ranges[-1][1] = number
        else:
            ranges.append([number, number])
    return ranges


def gap_counts(lines):
    executed = sum(lines.values())
    return {
        "lines_without_passing_evidence": len(lines),
        "unexecuted_lines": len(lines) - executed,
        "executed_without_passing_evidence_lines": executed,
    }


def gaps_query(report, passed, limit):
    candidates = []
    totals = gap_counts({})
    for name, hits in report["files"].items():
        production = {
            int(number): hit for number, hit in hits.items()
            if source_category(report, name, int(number)) == "selected_production"
        }
        missing = {
            number: hit for number, hit in production.items()
            if number not in passed.get(name, set())
        }
        if missing:
            candidates.append((name, production, missing))
            for key, value in gap_counts(missing).items():
                totals[key] += value
    candidates.sort(key=lambda item: (-len(item[2]), item[0]))
    files = []
    remaining = limit
    for name, production, missing in candidates[:limit]:
        if remaining == 0:
            break
        ranges = line_ranges(missing)
        selected = ranges[:remaining]
        remaining -= len(selected)
        files.append({
            "path": name,
            "coverage": evidence_summary({name: production}, passed),
            **gap_counts(missing),
            "ranges": [
                {"start": start, "end": end, **gap_counts({
                    number: missing[number] for number in range(start, end + 1)
                })}
                for start, end in selected
            ],
            "truncated": len(selected) < len(ranges),
        })
    return {
        "source_category": "selected_production",
        "evidence": "passing_attempts",
        "order": "lines_without_passing_evidence_descending_then_path",
        "files_with_gaps": len(candidates),
        **totals,
        "files": files,
        "truncated": len(files) < len(candidates) or any(file["truncated"] for file in files),
    }


def hunk_summary(report, name, start, end, hits, passed, limit):
    measured = {number: hit for number, hit in hits.items() if start <= number <= end}
    references = {
        test["id"]: {
            key: test[key] for key in ("id", "run", "binary_id", "name", "attempt", "result")
        }
        for test in report["tests"]
        if any(start <= line <= end for line in test["covered"].get(name, []))
    }
    tests = sorted(references.values(), key=lambda t: (t["result"] != "passed", t["name"]))
    return {
        "start": start, "end": end,
        "unmapped_lines": end - start + 1 - len(measured),
        "coverage": evidence_summary({name: measured}, passed),
        "tests": tests[:limit], "tests_truncated": len(tests) > limit,
    }


def changed_query(report, root, comparison, limit, passed):
    changed, other_files, deleted_files = changed_source_lines(
        root, comparison, snapshot_exclusions(report, root),
    )
    files = []
    measured = {}
    unmapped = 0
    unmeasured_files = 0
    remaining_hunks = limit
    for name, numbers in sorted(changed.items()):
        coverage = {int(number): hit for number, hit in report["files"].get(name, {}).items()}
        hits = {number: coverage[number] for number in numbers if number in coverage}
        measured[name] = hits
        unmapped += len(numbers - coverage.keys())
        unmeasured_files += int(bool(numbers) and name not in report["files"])
        if len(files) >= limit:
            continue
        unmapped_ranges = line_ranges(numbers - coverage.keys())
        uncovered_ranges = line_ranges(number for number, hit in hits.items() if not hit)
        not_passing = line_ranges(number for number in hits if number not in passed.get(name, set()))
        hunks = line_ranges(numbers)
        selected_hunks = hunks[:remaining_hunks]
        remaining_hunks -= len(selected_hunks)
        files.append({
            "path": name,
            "measurement": (
                "no-line-changes" if not numbers else
                "measured" if name in report["files"] else "unmeasured"
            ),
            "changed_lines": len(numbers),
            "unmapped_lines": len(numbers - coverage.keys()),
            "unmapped_ranges": unmapped_ranges[:limit],
            "uncovered_ranges": uncovered_ranges[:limit],
            "not_covered_by_passing_attempt_ranges": not_passing[:limit],
            "coverage": evidence_summary({name: hits}, passed),
            "hunks": [
                hunk_summary(report, name, start, end, hits, passed, min(limit, 5))
                for start, end in selected_hunks
            ],
            "truncated": (
                len(unmapped_ranges) > limit or len(uncovered_ranges) > limit
                or len(not_passing) > limit or len(selected_hunks) < len(hunks)
            ),
        })
    return measured, {
        "comparison": comparison,
        "diff": "merge-base-to-worktree-including-untracked",
        "changed_rust_lines": sum(len(lines) for lines in changed.values()),
        "unmapped_lines": unmapped,
        "unmeasured_files": unmeasured_files,
        "non_rust_files": other_files,
        "deleted_files": deleted_files,
        "files": files,
        "truncated": len(files) < len(changed) or any(file["truncated"] for file in files),
    }


def query(report_path=None, file=None, start=None, end=None, limit=100, *,
          changed=False, base=None, root=None, allow_stale=False, gaps=False):
    root = Path(root).resolve() if root is not None else repository_root()
    report_path = resolve_report(root, report_path)
    report = json.loads(Path(report_path).read_text(encoding="utf-8"))
    if report["schema_version"] != SCHEMA_VERSION:
        raise ValueError(
            f"unsupported coverage schema: {report['schema_version']}; collect coverage again"
        )
    if start is not None and (file is None or start <= 0):
        raise ValueError("--start requires --file and a positive line number")
    if end is not None and (start is None or end < start):
        raise ValueError("--end requires --start and must not precede it")
    if not 1 <= limit <= 1000:
        raise ValueError("--limit must be between 1 and 1000")
    if base is not None and not changed:
        raise ValueError("--base requires --changed")
    if changed and (file is not None or start is not None or allow_stale):
        raise ValueError("--changed cannot be combined with --file, line ranges, or --allow-stale")
    if gaps and (changed or file is not None or start is not None or end is not None):
        raise ValueError("--gaps cannot be combined with --changed, --file, or line ranges")
    freshness = check_freshness(report, root)
    if freshness["state"] != "current" and not allow_stale:
        raise StaleCoverageError(freshness)
    if gaps and ("files" not in report.get("source_map", {}) or not selected_packages(report)):
        raise ValueError(
            "gap discovery requires source classification and selected-package metadata; "
            "recollect with the current tool"
        )
    result = {
        key: report[key] for key in
        ("schema_version", "source", "family", "origin", "target",
         "scope", "issues", "collection_state", "test_outcome")
    }
    result["host"] = report.get("host")
    passed = coverage_index(report)
    result["source_classification"] = "available" if "source_map" in report else "unavailable"
    result["classification_issues"] = report.get("source_map", {}).get(
        "issues", ["source classification unavailable; recollect with the current tool"],
    )[:limit]
    result["classification_issues_truncated"] = (
        len(report.get("source_map", {}).get("issues", [])) > limit
    )
    result["report"] = str(report_path)
    result["freshness"] = freshness
    files = report["files"]
    if changed:
        result["view"] = "changed"
        comparison = (
            report["comparison"] if base is None and "comparison" in report
            else resolve_comparison(root, base)
        )
        files, result["patch"] = changed_query(report, root, comparison, limit, passed)
    elif gaps:
        result["view"] = "gaps"
        result["gaps"] = gaps_query(report, passed, limit)
    elif file is None:
        result["view"] = "absolute"
        names = sorted(report["files"])
        result["files"] = [
            {"path": name, "coverage": evidence_summary({name: report["files"][name]}, passed)}
            for name in names[:limit]
        ]
        result["truncated"] = len(names) > limit
    else:
        result["view"] = "file"
        name = Path(file).as_posix()
        lines = report["files"].get(name)
        result["file"] = name
        result["measurement"] = "measured" if lines is not None else "unmeasured"
        selected = sorted(
            (int(number), hit) for number, hit in (lines or {}).items()
            if (start is None or int(number) >= start)
            and (end is None or int(number) <= end)
        )
        result["lines"] = [
            line_details(report, name, number, hit, limit)
            for number, hit in selected[:limit]
        ]
        files = {name: dict(selected)}
        result["truncated"] = len(selected) > limit
    result["coverage"] = evidence_summary(files, passed)
    result["sources"] = source_summaries(report, files, passed)
    if changed:
        freshness = check_freshness(report, root)
        if freshness["state"] != "current":
            raise StaleCoverageError(freshness)
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    worker = commands.add_parser("collect", help="internal Flowey collection worker")
    worker.add_argument("--plan", type=Path, required=True)
    worker.add_argument("--output", type=Path, required=True)
    wrapper = commands.add_parser("nextest-runner", help=argparse.SUPPRESS)
    wrapper.add_argument("args", nargs=argparse.REMAINDER)
    reader = commands.add_parser("query", help="query a local coverage report as JSON")
    reader.add_argument("--report", type=Path, help="report path (defaults to the latest local attempt)")
    reader.add_argument("--file", help="repository-relative source path")
    reader.add_argument("--start", type=int)
    reader.add_argument("--end", type=int)
    reader.add_argument("--limit", type=int, default=100)
    reader.add_argument("--changed", action="store_true", help="report changed Rust lines and covering tests")
    reader.add_argument("--gaps", action="store_true",
                        help="find selected production ranges without passing-test evidence")
    reader.add_argument("--base", help="base revision (defaults to the recorded base or origin/HEAD)")
    reader.add_argument("--allow-stale", action="store_true", help="inspect historical data without claiming freshness")
    args = parser.parse_args(argv)
    try:
        if args.command == "collect":
            return collect(args.plan, args.output)
        if args.command == "nextest-runner":
            return nextest_runner(args.args)
        print(json.dumps(query(
            args.report, args.file, args.start, args.end, args.limit,
            changed=args.changed, base=args.base, allow_stale=args.allow_stale,
            gaps=args.gaps,
        )))
        return 0
    except StaleCoverageError as error:
        print(json.dumps({"error": "stale_coverage", "message": str(error),
                          "freshness": error.freshness}))
        return 2
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"unit coverage: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
