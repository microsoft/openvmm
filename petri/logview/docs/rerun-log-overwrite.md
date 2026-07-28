<!-- Copyright (c) Microsoft Corporation. -->
<!-- Licensed under the MIT License. -->

# Brief: Re-runs overwrite the previous run's Petri logs in the logviewer

## Symptom

When a CI workflow is re-run (e.g. "Re-run failed jobs" on a flaky test),
the logviewer shows only the results of the **latest attempt**. If a test
failed on attempt 1 and passed on attempt 2, the attempt-1 failure logs can
no longer be found. The earlier run's data is effectively lost.

## Root cause

Test results are stored in Azure Blob Storage keyed **only on the GitHub
run ID**, with no notion of the re-run attempt.

- Upload happens in [.github/workflows/upload-petri-results.yml](../../../.github/workflows/upload-petri-results.yml).
  The destination path is derived from `github.event.workflow_run.id`:

  ```sh
  az storage blob upload-batch \
    --destination "$BASE_URL/results" \
    --destination-path "$run_id" \      # <-- run_id only
    --source results
  # ...and the metadata blob:
  az storage blob upload --blob-url "$BASE_URL/results/runs/$run_id" ...
  ```

- **`run_id` (and `run_number`) are stable across re-runs of the same
  workflow run — only `run_attempt` increments.** So every attempt targets
  the exact same `results/<run_id>/...` prefix.

- The logviewer reads back by that same run ID. It lists blobs with
  `prefix=<run_id>` and reads each test folder's `petri.jsonl` /
  `petri.passed` — see [fetchRunDetails](../src/utils/fetch_runs_data.ts)
  and `parseRunDetails`. There is no attempt dimension in the read path
  either, so it can only ever surface one attempt per run ID.

There are actually two overwrite layers reinforcing this, both rooted in
the missing attempt identifier:

1. **GitHub artifacts** — re-running a job replaces its
   `*-vmm-tests-logs` artifact (same name), so the download step already
   sees only the newest attempt's logs for re-run jobs.
2. **Azure blob** — the upload re-targets `results/<run_id>/`, and the
   single metadata blob at `results/runs/<run_id>` (pass/fail counts,
   branch, PR) is rewritten to reflect the latest attempt.

## Fix options

1. **Include `run_attempt` in the blob path (recommended).**
   Store under `results/<run_id>/<run_attempt>/...` (or a
   `<run_id>-<run_attempt>` key) and write one metadata blob per attempt.
   `github.event.workflow_run.run_attempt` is available in the trigger
   payload. The logviewer then lists attempts under a run and shows each
   one, so a fixed flake keeps its original failure logs. This is the only
   option that fully preserves history; it requires a corresponding
   logviewer change to enumerate and display attempts.

2. **Skip upload for non-first attempts.** Guard the job on
   `run_attempt == 1`. Cheap, but does the opposite of what we want — the
   *newer* (passing) result would be dropped, and it still loses data.

3. **Refuse to overwrite existing blobs.** Keeps attempt 1 but silently
   drops attempt 2, and leaves the metadata blob inconsistent. Not
   recommended.

## Recommendation

Adopt option 1: key storage on `run_id` + `run_attempt` and teach the
logviewer to list attempts. This is the only change that keeps every
attempt's logs retrievable, which is exactly the flaky-test debugging case
that motivated this investigation.
