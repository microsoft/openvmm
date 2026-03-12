---
applyTo: "repo_support/**,.github/**,ci-flowey/**,vmm_tests/**"
---

# Investigating CI Failures

When a PR has failing CI checks, use this workflow to diagnose the root cause.
A helper script is available at `repo_support/investigate_ci.py`.

## Quick Start

```bash
# Investigate a PR by number
python repo_support/investigate_ci.py 2946

# Or by run ID directly
python repo_support/investigate_ci.py 23017249697
```

## Manual Investigation Steps

If the script isn't available or you need more control, follow these steps:

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

### 3. Download test log artifacts

VMM test results are stored in artifacts named `{platform}-vmm-tests-logs`.
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

The `petri.failed` file contains the test name.

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

Each test directory contains:
- `petri.jsonl` — Structured JSON Lines log **(primary file for investigation)**
- `petri.log` — Plain text version of the test log
- `petri.passed` or `petri.failed` — Pass/fail marker
- `openhcl.log` — OpenHCL serial console output
- `hyperv.log` — Hyper-V event log
- `guest.log` — Guest OS serial output
- Sometimes: `screenshot_*.png`, `dumpfile.dmp`

## Viewing Results in Browser

Test results are uploaded to Azure Blob Storage and viewable at:
`https://openvmm.dev/test-results/#/runs/<RUN_ID>`

## Common Failure Patterns

- **TripleFault**: VM hit a fatal error during boot. Check `petri.jsonl` for
  Hyper-V Worker/Chipset errors. Often infrastructure-related, not caused by
  the PR's code changes.
- **Timeout**: Test exceeded its time limit. Check if the VM booted at all.
- **Guest assertion failure**: Guest-side test failed. Check `guest.log`.
- **Build failure**: No test artifacts will exist. Check the job log directly
  with `gh run view --job <JOB_ID> --log`.

## Important API Note

The `gh run view --json artifacts` flag does **not** exist. To list artifacts
for a run, use the GitHub API directly:

```bash
gh api repos/microsoft/openvmm/actions/runs/<RUN_ID>/artifacts
```
