#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Local unit coverage worker and JSON reader; collection is configured by Flowey."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time


SCHEMA_VERSION = 1
PROFILE_DIR_ENV = "OPENVMM_UNIT_COVERAGE_PROFILES"


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
    return {"head": head, "dirty": dirty, "snapshot_sha256": digest.hexdigest()}


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


def collect_run(root, output, plan, run, cargo, llvm_cov, llvm_profdata, source_files, report):
    started = time.monotonic()
    run_dir = output / "runs" / run["name"]
    run_dir.mkdir(parents=True)
    profiles = run_dir / "profiles"
    profiles.mkdir()
    cache = root / "target" / "unit-coverage-build" / run["name"]
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
    target = build_args[build_args.index("--target") + 1]
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
        report["all_tests_passed"] = False
        report["issues"].append(f"{run['name']}: nextest exited with code {result}")
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


def collect(plan_path, output):
    started = time.monotonic()
    root = Path(capture(["git", "rev-parse", "--show-toplevel"], cwd=Path.cwd()).strip()).resolve()
    output = Path(output).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.mkdir()  # Never reuse execution counts from an earlier invocation.
    plan = json.loads(Path(plan_path).read_text(encoding="utf-8"))
    shutil.copy2(plan["nextest_config"], output / "nextest.toml")
    plan["nextest_config"] = str(output / "nextest.toml")
    write_json(output / "plan.json", plan)
    cargo = ["cargo"] + ([f"+{plan['toolchain']}"] if plan["toolchain"] else [])
    rustc = ["rustc"] + ([f"+{plan['toolchain']}"] if plan["toolchain"] else [])
    excluded = [output, root / "target" / "unit-coverage-build"]
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
        "target": "x86_64-unknown-linux-gnu",
        "build_profile": "dev",
        "nextest_profile": "agent",
        "state": "collecting",
        "all_tests_passed": None,
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
    try:
        report["rustc"] = capture(rustc + ["-vV"], cwd=root).strip()
        report["cargo_llvm_cov"] = capture(cargo + ["llvm-cov", "--version"], cwd=root).strip()
        sysroot = Path(capture(rustc + ["--print", "sysroot"], cwd=root).strip())
        llvm_dir = sysroot / "lib/rustlib/x86_64-unknown-linux-gnu/bin"
        llvm_cov, llvm_profdata = llvm_dir / "llvm-cov", llvm_dir / "llvm-profdata"
        if not llvm_cov.is_file() or not llvm_profdata.is_file():
            raise ValueError("missing LLVM tools; run: rustup component add llvm-tools-preview")
        for run in plan["runs"]:
            collect_run(
                root, output, plan, run, cargo, llvm_cov, llvm_profdata, source_files, report,
            )
            write_json(output / "report.json", report)
        if not report["tests"]:
            report["issues"].append("no tests selected; coverage is unavailable")
        else:
            report["all_tests_passed"] = all(run["exit_code"] == 0 for run in report["runs"])
        if source_snapshot(root, excluded) != snapshot:
            report["issues"].append("source changed during collection; coverage snapshot is invalid")
        report["state"] = "incomplete" if report["issues"] else "complete"
    except Exception as error:
        report["state"] = "incomplete"
        report["issues"].append(str(error))
        raise
    finally:
        report["duration_seconds"] = time.monotonic() - started
        report["summary"] = summarize(report["files"])
        write_json(output / "report.json", report)
        print(json.dumps({
            "report": str(output / "report.json"),
            "state": report["state"],
            "summary": report["summary"],
        }), file=sys.stderr)
    return 0 if report["state"] == "complete" and report["all_tests_passed"] else 1


def summarize(files):
    total = sum(len(lines) for lines in files.values())
    covered = sum(sum(lines.values()) for lines in files.values())
    return {
        "measured_lines": total,
        "covered_lines": covered,
        "line_percent": covered * 100 / total if total else None,
    }


def query(report_path, file=None, start=None, end=None, limit=100):
    report = json.loads(Path(report_path).read_text(encoding="utf-8"))
    if report["schema_version"] != SCHEMA_VERSION:
        raise ValueError(f"unsupported coverage schema: {report['schema_version']}")
    if start is not None and (file is None or start <= 0):
        raise ValueError("--start requires --file and a positive line number")
    if end is not None and (start is None or end < start):
        raise ValueError("--end requires --start and must not precede it")
    if not 1 <= limit <= 1000:
        raise ValueError("--limit must be between 1 and 1000")
    result = {
        key: report[key] for key in
        ("schema_version", "source", "family", "origin", "target", "state",
         "all_tests_passed", "scope", "issues", "summary")
    }
    if file is None:
        names = sorted(report["files"])
        result["files"] = [
            {"path": name, **summarize({name: report["files"][name]})}
            for name in names[:limit]
        ]
        result["truncated"] = len(names) > limit
    else:
        name = Path(file).as_posix()
        lines = report["files"].get(name)
        result["file"] = name
        result["measurement"] = "measured" if lines is not None else "unmeasured"
        selected = sorted(
            (int(number), hit) for number, hit in (lines or {}).items()
            if (start is None or int(number) >= start)
            and (end is None or int(number) <= end)
        )
        result["lines"] = []
        for number, hit in selected[:limit]:
            tests = [
                {key: test[key] for key in
                 ("id", "run", "binary_id", "name", "attempt", "result")}
                for test in report["tests"] if number in test["covered"].get(name, [])
            ]
            result["lines"].append({
                "line": number,
                "covered": hit,
                "tests": tests[:limit],
                "tests_truncated": len(tests) > limit,
            })
        result["truncated"] = len(selected) > limit
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
    reader.add_argument("--report", type=Path, required=True)
    reader.add_argument("--file", help="repository-relative source path")
    reader.add_argument("--start", type=int)
    reader.add_argument("--end", type=int)
    reader.add_argument("--limit", type=int, default=100)
    args = parser.parse_args(argv)
    try:
        if args.command == "collect":
            return collect(args.plan, args.output)
        if args.command == "nextest-runner":
            return nextest_runner(args.args)
        print(json.dumps(query(args.report, args.file, args.start, args.end, args.limit)))
        return 0
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"unit coverage: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
