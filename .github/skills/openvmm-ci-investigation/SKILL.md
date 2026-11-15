---
name: openvmm-ci-investigation
description: Investigate CI failures on OpenVMM PRs. Load when a PR has failing CI checks, you need to download and analyze test artifacts, or you need to diagnose build, fmt, clippy, or VMM test failures.
---

# Investigating CI Failures

When a PR has failing CI checks, prefer the investigation script. If it is
unavailable or incompatible with the execution environment, use the read-only
manual fallback below.

The script reads from GitHub and downloads artifacts locally. It does not
modify remote resources. Downloads use the system temporary directory; check
that this destination is suitable for the execution environment.

## Step 1: Run the Script

```bash
# Investigate a specific PR by number
python3 repo_support/investigate_ci.py 2946

# Or by run ID directly
python3 repo_support/investigate_ci.py 23017249697
```

Prefer explicit PR or run identifiers for reproducible investigation. Omitting
the identifier requires the script to resolve the current branch's PR.

The script automatically:
1. Resolves the PR to **every** failed CI run for its head commit. A commit
   can have multiple workflows (e.g. "OpenVMM PR" and "OpenVMM Docs PR"),
   and each failing one is investigated in turn — so don't assume a single
   run covers everything. Only the newest run of each workflow is used, so
   superseded runs are ignored; a `NOTE: <workflow> is still in_progress`
   line means results are incomplete and more failures may appear later.
2. Identifies failed jobs
3. Downloads `*-unit-tests-junit-xml` artifacts and parses JUnit XML for
   unit test failures
4. Downloads `*-vmm-tests-logs` artifacts if they exist
5. Finds tests with `petri.failed` markers and extracts ERROR/WARN lines
6. If no test or JUnit artifacts exist (build/fmt/clippy failure), shows the
   tail of the failed job's log

## Step 2: Analyze the Script Output

Read the script's output to identify:
- Which tests failed (unit tests and/or VMM tests)
- Error messages and root causes
- Whether it's a build/fmt/clippy failure vs. a test failure

When more than one run failed, the output is split by `# RUN n of m` banners.
Check **all** of them — a docs failure and a build failure are reported in
separate runs.

Then use the information to diagnose the issue and suggest fixes.

## Step 3: Diagnose from Logs and Code

**Always try to diagnose the failure from CI logs, test code, and error
messages first.** Most failures can be understood without local reproduction.
Read the relevant test source, trace the error through the code, and form
a hypothesis before considering local repro.

## Step 4: Reproduce Locally (Only If Needed)

If you cannot diagnose the failure from logs alone, **ask the user** whether
they want to attempt local reproduction before proceeding. Do not
automatically start building or running tests on the user's machine.

If the user agrees, check whether the failing platform matches the host
architecture — you can only reproduce tests locally on the same arch. If
it doesn't match, explain this to the user and continue diagnosing from
CI logs and test code.

For approved local reproduction, select the workflow for the failure:

| Failure | Workflow |
|---------|----------|
| Unit test | `cargo nextest run --profile agent -p <package>` with a focused filter |
| VMM test | Load the `vmm-tests` skill and use `cargo xflowey vmm-tests-run` |
| Rustdoc | `cargo doc --no-deps -p <package>` |
| Guide | The Guide build workflow in `flowey/flowey_lib_hvlite/src/build_guide.rs`, including plugin setup, `mdbook test`, and `mdbook build` |
| Build, clippy, or formatting | The failed job's command, scoped to the affected package or rule where possible |

Do **not** use `cargo nextest run -p vmm_tests` directly; it won't have the
required artifacts.

## Reference: Manual Commands

> Use these if the script is unavailable, fails, is incompatible with the
> execution environment, or you need to inspect a specific artifact.

Follow these steps:

### 1. Find the failing run

```bash
# Get the run ID for a PR
gh pr checks <PR_NUMBER> -R microsoft/openvmm
# Or list runs for a specific commit
gh run list -R microsoft/openvmm --commit <SHA>
```

### 2. Identify the failing job

```bash
gh run view <RUN_ID> -R microsoft/openvmm --json jobs \
  -q '[.jobs[] | select(.conclusion == "failure") | {name, databaseId}]'
```

### 3. Download test artifacts

**Unit test results** are stored in JUnit XML artifacts named
`{platform}-unit-tests-junit-xml`. Known platforms include:
- `x64-linux`
- `aarch64-linux`
- `aarch64-linux-musl`

```bash
# Download unit test JUnit XML for a platform
gh run download <RUN_ID> -R microsoft/openvmm \
  -n aarch64-linux-unit-tests-junit-xml -D /tmp/junit-xml
# Parse failures from the XML
python3 -c "
import xml.etree.ElementTree as ET, sys
for f in __import__('pathlib').Path(sys.argv[1]).rglob('*.xml'):
    for tc in ET.parse(f).iter('testcase'):
        fail = tc.find('failure')
        if fail is None:
            fail = tc.find('error')
        if fail is not None:
            print(f'FAIL: {tc.get(\"classname\",\"\")}::{tc.get(\"name\",\"\")}')
            print(f'  {fail.get(\"message\",\"\")[:200]}')
" /tmp/junit-xml
```

**VMM test results** are stored in artifacts named `{platform}-vmm-tests-logs`.
The known platforms are:
- `x64-windows-intel`
- `x64-windows-intel-tdx`
- `x64-windows-amd`
- `x64-windows-amd-snp`
- `x64-linux`
- `aarch64-windows`

```bash
# Download a specific platform's test logs
gh run download <RUN_ID> -R microsoft/openvmm \
  -n x64-windows-amd-snp-vmm-tests-logs -D /tmp/test-logs
```

### 4. Find failed tests

Each test gets its own directory inside the artifact. Look for `petri.failed`
marker files (passing tests have `petri.passed` instead):

```bash
find /tmp/test-logs -name "petri.failed"
```

The `petri.failed` file contains the error petri recorded for the test. The
test's real name is in `petri.test`, which petri writes before the test body
runs — so a directory with `petri.test` but *no* result marker is a test that
was killed or crashed before it could report.

### 5. Extract errors from petri.jsonl

The `petri.jsonl` file in each test directory is the primary structured log.
Each line is a JSON object with fields: `timestamp`, `source`, `severity`,
`message`. Filter for `ERROR` and `WARN` severity for a quick diagnosis:

```bash
python3 -c "
import json, sys
for line in open(sys.argv[1]):
    try:
        e = json.loads(line.strip())
        if e.get('severity') in ('ERROR', 'WARN'):
            print(f'[{e[\"severity\"]}] {e.get(\"source\",\"?\")}: {e.get(\"message\",\"\").strip()[:200]}')
    except: pass
" /tmp/test-logs/<test-dir>/petri.jsonl
```

## Artifact Contents

### Unit test JUnit XML

Artifacts named `{platform}-unit-tests-junit-xml` contain JUnit XML files
with `<testcase>` elements. Failed tests have `<failure>` or `<error>`
children with `message` attributes describing the failure. These are the
primary artifacts for diagnosing unit test / cargo-nextest failures.

### VMM test logs (petri)

Each test directory contains:
- `petri.jsonl` — Structured JSON Lines log **(primary file for investigation)**
- `petri.log` — Plain text version of the test log
- `petri.test` — The test's name, written before the test body runs
- `petri.passed` or `petri.failed` — Pass/fail marker; a failure marker holds the error
- `openhcl.log` — OpenHCL serial console output, if the test exercised OpenHCL
- `hyperv.log` — Hyper-V event log, if the test exercises the Hyper-V backend
- `openvmm.log` — OpenVMM serial console output, if the test exercises the OpenVMM backend
- `guest.log`, `uefi.log` — Guest OS serial output
- Sometimes: `screenshot_*.png` — periodic screenshots of the guest
- Sometimes: `dumpfile.dmp`

## Viewing Results in Browser

Test results are uploaded to Azure Blob Storage and viewable at:
`https://openvmm.dev/test-results/#/runs/<RUN_ID>_<RUN_ATTEMPT>`

Logs are uploaded per re-run attempt, so the run key includes the attempt
number (`1` for the first attempt). `investigate_ci.py` prints the correct
URL in its summary.

## Common Failure Patterns

- **Unit test failure**: A `unit tests` job failed. The script downloads
  JUnit XML artifacts and shows the failing test names and messages. Common
  causes: new test code that relies on OS capabilities not available in CI
  (e.g. TAP devices, elevated permissions).
- **Formatting / house-rules**: The `quick check [fmt, clippy x64-linux]` job
  failed. No test artifacts will exist. Check the job log for the specific rule
  that failed. For approved reproduction, run `cargo xtask fmt --fix` locally.
- **Guide failure**: The `build mdbook guide` job in "OpenVMM Docs PR" failed.
  Check for broken Guide markdown, links, or code examples and
  `[ERROR] (mdbook::...)` diagnostics. Use the Guide workflow above for approved
  reproduction, not `cargo doc`. No test artifacts will exist.
- **Rustdoc failure**: The `build and check docs` job in "OpenVMM Docs PR" failed.
  Rustdoc warnings are denied in CI. Use package-scoped `cargo doc` for approved
  reproduction. No test artifacts will exist.
- **TripleFault**: VM hit a fatal error during boot. Check `petri.jsonl` for
  Hyper-V Worker/Chipset errors. Often infrastructure-related, not caused by
  the PR's code changes.
- **Timeout**: Test exceeded its time limit. Check if the VM booted at all.
- **Guest assertion failure**: Guest-side test failed. Check `guest.log`.
- **Build failure**: No test artifacts will exist. Check the job log directly
  with `gh run view <RUN_ID> --job <JOB_ID> --log`.

## Important API Note

The `gh run view --json artifacts` flag does **not** exist. To list artifacts
for a run, use the GitHub API directly:

```bash
gh api repos/microsoft/openvmm/actions/runs/<RUN_ID>/artifacts
```
