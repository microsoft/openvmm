# Plan: Replace `vmm-tests-run` Meta-Command with a Proper Flowey Pipeline

## Problem Statement

The `cargo xflowey vmm-tests-run` command is currently implemented as a
**meta-command** — it does not return a `Pipeline` object like other flowey
commands. Instead, it spawns two separate `cargo xflowey` subprocesses
sequentially:

1. `cargo xflowey vmm-tests-discover` → produces a JSON file of required
   artifacts
2. `cargo xflowey vmm-tests` → reads the JSON file, builds dependencies, and
   runs tests

This has several downsides:

- **Double compilation check**: Each `cargo xflowey` invocation triggers a
  `cargo run -p flowey_hvlite`, meaning Cargo must check/resolve the dependency
  graph twice.
- **Subprocess orchestration**: The meta-command manually constructs
  `std::process::Command` objects, forwards CLI flags as strings, and manages
  exit codes — all error-prone plumbing that flowey's pipeline model is designed
  to eliminate.
- **Special-case dispatch**: In `pipelines/mod.rs`, the `VmmTestsRun` variant
  calls `std::process::exit()` instead of returning a `Pipeline`, breaking the
  uniform `IntoPipeline` pattern that every other command follows.
- **File-based data passing**: The two stages communicate through a JSON file on
  disk (`dir/.vmm_tests_artifacts.json`) rather than through flowey's typed
  variable system.

### Future direction

The end goal is to eventually remove `vmm-tests` as a standalone command and
move its packaging and run-remote capabilities into a separate command.
`vmm-tests-run` should be the focused local-execution command. This change is a
prerequisite for that split — it makes `vmm-tests-run` a self-contained pipeline
that doesn't depend on shelling out to `vmm-tests`.

## Why Not Wire Both Nodes into One Pipeline DAG?

The natural question is: can we put `local_discover_vmm_tests_artifacts` and
`local_build_and_run_nextest_vmm_tests` into the same pipeline as two jobs?

**No**, because of a fundamental flowey constraint:

- `local_build_and_run_nextest_vmm_tests` conditionally calls `ctx.reqv()` in
  `process_request()` based on `BuildSelections` values (e.g.,
  `build.openhcl.then(|| ctx.reqv(...))` at line 476). These `ctx.reqv()` calls
  **wire up the DAG** — they happen at flow construction time, not at runtime.
- `local_discover_vmm_tests_artifacts` produces its output inside
  `emit_rust_step()` — that's **runtime**, after the DAG is already built.
- So if both were in the same pipeline, the discovery output wouldn't exist yet
  when the build node needs it to decide which sub-nodes to wire up.

We also investigated:

- **`PipelineJob::with_condition`** — takes a `UseParameter<bool>`, which is a
  pipeline-level parameter set at definition time (like a CI checkbox). It
  cannot be derived from another job's runtime output.
- **Splitting the build node into smaller chunks with per-chunk conditions** —
  same problem: the conditions must be known before the pipeline executes, but
  discovery produces them at runtime.
- **Running two pipelines sequentially** — flowey's architecture is one pipeline
  per invocation. `into_pipeline()` returns a `Pipeline` that the CLI layer
  executes; there's no API to execute a pipeline programmatically within
  `into_pipeline()` and then construct a second one.

## Proposed Solution

Run artifact discovery as a **plain function call** inside `into_pipeline()`,
then construct a single pipeline that feeds the resolved selections to
`local_build_and_run_nextest_vmm_tests`.

This is exactly the pattern `vmm_tests.rs` already uses — it calls
`std::fs::read_to_string()` and `ResolvedArtifactSelections::from_artifact_list_json()`
at pipeline construction time. The only difference is that instead of reading a
pre-existing JSON file, we produce it ourselves by running the two discovery
commands (`cargo nextest list` + `test_binary --list-required-artifacts`).

```
VmmTestsRunCli::into_pipeline()
  │
  ├─ 1. Run discovery inline (at construction time)
  │   ├─ cargo nextest list -p vmm_tests ... --message-format json
  │   ├─ test_binary --list-required-artifacts --tests-from-stdin
  │   └─ ResolvedArtifactSelections::from_artifact_list_json()
  │
  └─ 2. Construct pipeline (same pattern as vmm_tests.rs)
      └─ Single job: local_build_and_run_nextest_vmm_tests with resolved selections
```

The result is a proper flowey pipeline — `VmmTestsRunCli` implements
`IntoPipeline`, returns a `Pipeline`, and flowey executes it normally. No
subprocess spawning, no `std::process::exit()`, no file-based data passing.

Discovery is fast (seconds) compared to the actual builds (minutes), and running
it at construction time actually provides better UX — filter typos and target
mismatches fail immediately instead of after partial pipeline execution.

## Implementation Steps

### 1. Extract discovery logic into a reusable function

**File**: `flowey/flowey_lib_hvlite/src/_jobs/local_discover_vmm_tests_artifacts.rs`

Extract the core logic from the `emit_rust_step` closure into a standalone
public function that returns the JSON string directly — no file I/O:

```rust
/// Run artifact discovery directly (not as a flowey step).
///
/// Runs `cargo nextest list` and `--list-required-artifacts` to determine
/// what artifacts the matching tests need. Returns the raw JSON string.
pub fn discover_artifacts_sync(
    repo_root: &Path,
    target: &str,
    filter: &str,
    release: bool,
) -> anyhow::Result<String> { ... }
```

This function uses `std::process::Command` directly rather than flowey's
`shell_cmd!` macro (which requires a flowey runtime context). It captures stdout
from both commands and returns the artifacts JSON as a `String`. No intermediate
file is written — the caller passes the string straight to
`ResolvedArtifactSelections::from_artifact_list_json()`.

**Implementation notes:**

- Use `.current_dir(repo_root)` on both `Command` instances — the current node
  does this implicitly via `rt.sh.change_dir()`.
- The `--list-required-artifacts --tests-from-stdin` call requires piping stdin.
  With `std::process::Command`, this means `.stdin(Stdio::piped())`, spawning
  the child, writing to its stdin handle, then collecting output. This is more
  involved than the `shell_cmd!(...).stdin(data).output()` API.
- Check that `cargo nextest` is available before running. If it's not found,
  return a clear error: `"cargo-nextest not found — run 'cargo xflowey
  restore-packages' first"`. The current meta-command relies on flowey nodes
  to install prerequisites, but `discover_artifacts_sync()` runs before any
  nodes execute.
- The helper functions `parse_nextest_output()` and `parse_artifacts_output()`
  can be reused as-is — they're pure functions with no flowey dependencies.

**Delegation pattern for the existing flowey node:**

The existing `Node` (used by standalone `vmm-tests-discover`) delegates to the
extracted function inside its `emit_rust_step` closure:

```rust
ctx.emit_rust_step("build vmm_tests and discover artifacts", |ctx| {
    done.claim(ctx);
    build_essential.claim(ctx);
    // ... claim deps ...
    let openvmm_repo_path = openvmm_repo_path.claim(ctx);
    let artifacts_json_out = artifacts_json_out.map(|v| v.claim(ctx));
    move |rt| {
        let openvmm_repo_path = rt.read(openvmm_repo_path);
        let json = discover_artifacts_sync(
            &openvmm_repo_path, &target_str, &filter, release,
        )?;
        // Handle --output file and WriteVar as before
        if let Some(output_path) = output {
            std::fs::write(&output_path, &json)?;
        } else {
            println!("{}", json);
        }
        if let Some(var) = artifacts_json_out {
            rt.write(var, &json);
        }
        Ok(())
    }
});
```

### 2. Extract shared pipeline construction helpers

**File**: `flowey/flowey_hvlite/src/pipelines/vmm_tests.rs` (or a new shared
module)

Before writing the new `into_pipeline()`, extract the following shared logic
from `vmm_tests.rs` so both `vmm_tests.rs` and `vmm_tests_run.rs` can reuse it:

- **Target resolution**: `VmmTestTargetCli` → `CommonTriple` → `target_os` /
  `target_architecture` / `recipe_arch` (vmm_tests.rs lines 124-151).
- **WSL path validation**: The check that `--dir` is a Windows path when
  targeting Windows from WSL (vmm_tests.rs lines 206-216).
- **Pipeline job construction**: The `dep_on` chain for `cfg_versions`,
  `cfg_hvlite_reposource`, `cfg_common`, `cfg_versions::LocalKernel`, and
  `local_build_and_run_nextest_vmm_tests::Params` (vmm_tests.rs lines 261-315).

This is ~200 lines of near-duplicate code. Starting with shared helpers prevents
bugs from divergence and makes the eventual deprecation of standalone `vmm-tests`
cleaner.

### 3. Make `VmmTestsRunCli` implement `IntoPipeline`

**File**: `flowey/flowey_hvlite/src/pipelines/vmm_tests_run.rs`

Replace the current `run()` method with an `IntoPipeline` implementation:

```rust
impl IntoPipeline for VmmTestsRunCli {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            anyhow::bail!("vmm-tests-run is for local use only")
        }

        // 1. Resolve target (shared helper)
        let (target, target_os, target_architecture, recipe_arch) =
            resolve_target(self.target, backend_hint)?;

        // 2. Run discovery inline — returns JSON string, no file written
        let repo_root = crate::repo_root();
        let target_str = target.as_triple().to_string();
        let artifacts_json = discover_artifacts_sync(
            &repo_root, &target_str, &self.filter, self.release,
        ).context("during artifact discovery")?;

        // 3. Resolve to build selections
        let resolved = ResolvedArtifactSelections::from_artifact_list_json(
            &artifacts_json, target_architecture, target_os,
        ).context("failed to parse discovered artifacts")?;
        if !resolved.unknown.is_empty() {
            anyhow::bail!(
                "Unknown artifacts (mapping needs updating):\n  {}",
                resolved.unknown.join("\n  ")
            );
        }

        // 4. Build selections (same as vmm_tests.rs lines 219-234)
        let selections = VmmTestSelections::Custom {
            filter: self.filter,
            artifacts: resolved.downloads.into_iter().collect(),
            build: resolved.build.clone(),
            deps: /* resolve from target_os + resolved.build */,
            needs_release_igvm: resolved.needs_release_igvm,
        };

        // 5. Validate WSL path (shared helper)
        validate_wsl_path(&self.dir, target_os)?;

        // 6. Construct pipeline (shared helper)
        build_vmm_tests_pipeline(
            backend_hint, target, selections,
            self.dir, self.verbose, self.install_missing_deps,
            self.unstable_whp, self.release, self.build_only,
            self.copy_extras, self.custom_kernel_modules,
            self.custom_kernel, self.skip_vhd_prompt, recipe_arch,
        )
    }
}
```

### 4. Update the dispatch in `pipelines/mod.rs`

**File**: `flowey/flowey_hvlite/src/pipelines/mod.rs`

Replace the special-case handling:

```rust
// Before:
OpenvmmPipelines::VmmTestsRun(cmd) => {
    let result = cmd.run(pipeline_hint);
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => { log::error!("{:?}", e); std::process::exit(1); }
    }
}

// After:
OpenvmmPipelines::VmmTestsRun(cmd) => cmd.into_pipeline(pipeline_hint),
```

### 5. Remove the `run()` method

**File**: `flowey/flowey_hvlite/src/pipelines/vmm_tests_run.rs`

Delete `VmmTestsRunCli::run()` — all logic now lives in `into_pipeline()`.

### 5. Consider deprecating the standalone commands (future work)

With `vmm-tests-run` as a self-contained pipeline, the standalone
`vmm-tests-discover` and `vmm-tests` (with `--artifacts-file`) commands become
less necessary for the typical local workflow. Eventually:

- `vmm-tests-discover` could remain as a utility for advanced users who want to
  inspect what artifacts a filter needs.
- `vmm-tests` packaging and run-remote functionality should move to a separate
  command. The local-execution path is fully handled by `vmm-tests-run`.

This is out of scope for this change but is the intended future direction.

## Files to Modify

| File | Change |
|------|--------|
| `flowey/flowey_lib_hvlite/src/_jobs/local_discover_vmm_tests_artifacts.rs` | Extract `discover_artifacts_sync()` function; have the flowey node delegate to it |
| `flowey/flowey_hvlite/src/pipelines/vmm_tests_run.rs` | Rewrite: implement `IntoPipeline` instead of `run()` |
| `flowey/flowey_hvlite/src/pipelines/mod.rs` | Remove special-case dispatch for `VmmTestsRun` |
| `flowey/flowey_hvlite/src/pipelines/vmm_tests.rs` | Optional: extract shared pipeline construction into helper |

## Risks and Considerations

- **Prerequisite: cargo-nextest must be installed**: `discover_artifacts_sync()`
  runs before any flowey nodes execute, so it can't rely on flowey's dependency
  installation. The function should check for `cargo nextest` and produce a clear
  error directing the user to `cargo xflowey restore-packages`.

- **Discovery compiles vmm_tests**: `cargo nextest list` compiles the
  `vmm_tests` binary. On a clean build this takes minutes, not seconds. On
  incremental builds it's fast. This is not a regression from the current
  behavior (the meta-command also compiles via the discover subprocess), but the
  first run after checkout will be slow.

- **Discovery errors at construction time**: If the filter is invalid or nextest
  fails, the error happens before the pipeline starts. This is actually better
  UX — failures are immediate with clear context rather than buried in pipeline
  step output. Wrap errors with `.context("during artifact discovery")` for
  clarity.

- **Shared code with vmm-tests-discover**: The extracted function is used by
  both `vmm-tests-run` (direct call) and `vmm-tests-discover` (flowey node).
  Using a single shared function keeps them in sync.

- **No change to the build-and-run node**: `local_build_and_run_nextest_vmm_tests`
  is used as-is. The `VmmTestSelections::Custom` construction follows the same
  pattern already proven in `vmm_tests.rs`.

## Testing

- Run `cargo xflowey vmm-tests-run --filter "test(some_test)" --dir /tmp/out`
  and verify it produces the same results as before.
- Run `cargo xflowey vmm-tests-discover` to verify the standalone command still
  works.
- Verify that `--build-only`, `--release`, `--target`, and other flags all work
  correctly.
- Verify error cases: invalid filter, unknown artifacts, unsupported target.
