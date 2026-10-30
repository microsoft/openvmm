#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

"""Generate a run manifest from the downloaded Petri test artifacts."""

import argparse
from datetime import datetime
import json
from pathlib import Path
import re


def generate_manifest(results: Path, run_number: str, creation_time: str) -> dict:
    if not results.is_dir():
        raise NotADirectoryError(results)
    if not re.fullmatch(r"\d+(?:_\d+)?", run_number):
        raise ValueError(f"Invalid run number: {run_number}")
    if datetime.fromisoformat(creation_time.replace("Z", "+00:00")).tzinfo is None:
        raise ValueError("Creation time must include a timezone")

    tests = []
    for log in sorted(results.rglob("petri.jsonl")):
        if not log.is_file():
            continue
        name = log.parent.relative_to(results).as_posix()
        if len(name.split("/")) < 2 or "\\" in name or any(ord(c) < 32 for c in name):
            raise ValueError(f"Invalid test path: {name}")
        tests.append({
            "name": name,
            "status": "passed" if log.with_name("petri.passed").is_file() else "failed",
        })

    return {
        "version": 1,
        "runNumber": run_number,
        "creationTime": creation_time,
        "tests": tests,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path)
    parser.add_argument("--run-number", required=True)
    parser.add_argument("--creation-time", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    manifest = generate_manifest(args.results, args.run_number, args.creation_time)
    args.output.write_text(
        json.dumps(manifest, separators=(",", ":"), ensure_ascii=False) + "\n",
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
