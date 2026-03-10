// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Classifies PR changed files to determine whether all changes fall within
//! approved non-product buckets (e.g. `Guide/**`, `repo_support/**/*.py`).
//!
//! This node works across all Flowey backends:
//!
//! - **GitHub**: a Rust step runs `git diff` against `GITHUB_BASE_REF` and
//!   writes the bool result to `$GITHUB_ENV` under [`GH_ENV_IS_NON_PRODUCT`]
//!   so it is accessible as a job-level output via
//!   [`Pipeline::gh_job_id_of`] + `needs.<job>.outputs.is_non_product`.
//!
//! - **ADO**: an ADO step named [`ADO_STEP_NAME`] runs a bash script that
//!   uses `git diff` against `SYSTEM_PULLREQUEST_TARGETBRANCH`, publishes the
//!   result as an ADO output variable (`is_non_product`), and passes it to the
//!   inline Rust snippet which writes it to the Flowey var.  Downstream jobs
//!   can gate on [`ado_condition`].
//!
//! - **Local**: always writes `false` (conservative; vmm-tests always run).
//!
//! ## Extending non-product buckets
//!
//! All bucket patterns are defined in [`is_non_product_path`] (GitHub/local)
//! and the equivalent bash logic inside the ADO step.  Add new patterns in
//! both places when onboarding a new non-product area.

use flowey::node::prelude::*;
use std::io::Write as _;

// ─── Public constants ────────────────────────────────────────────────────────

/// Name of the GitHub Actions environment variable written to `$GITHUB_ENV`
/// by the Rust classify step.
///
/// Pass this to
/// [`PipelineJob::gh_set_job_output_from_env_var`](flowey_core::pipeline::PipelineJob::gh_set_job_output_from_env_var)
/// to expose the result as a job-level output accessible to dependent jobs:
///
/// ```ignore
/// classify_job.gh_set_job_output_from_env_var(
///     "is_non_product",
///     check_pr_changes::GH_ENV_IS_NON_PRODUCT,
/// )
/// ```
pub const GH_ENV_IS_NON_PRODUCT: &str = "FLOWEY_IS_NON_PRODUCT";

/// Name of the ADO step that publishes the `is_non_product` output variable.
///
/// Used by [`ado_condition`] to build the condition expression
/// `dependencies.<job>.outputs['<step>.is_non_product']`.
pub const ADO_STEP_NAME: &str = "classify_pr_changes";

/// Returns an ADO job `condition:` expression that runs the job only when the
/// classify job determined the PR is NOT a non-product-only change.
///
/// `classify_job_id` is the ADO job ID of the classify job, obtained via
/// [`Pipeline::ado_job_id_of`](flowey_core::pipeline::Pipeline::ado_job_id_of).
pub fn ado_condition(classify_job_id: &str) -> String {
    format!(
        "and(succeeded(), not(canceled()), \
         ne(dependencies.{classify_job_id}.outputs['{ADO_STEP_NAME}.is_non_product'], 'true'))"
    )
}

// ─── Node definition ─────────────────────────────────────────────────────────

flowey_request! {
    pub struct Request {
        /// Signal that the classification step has completed.
        ///
        /// The classification result is communicated to downstream CI jobs via
        /// backend-native mechanisms: `$GITHUB_ENV` for GitHub Actions (see
        /// [`GH_ENV_IS_NON_PRODUCT`]) and an ADO output variable for ADO
        /// (see [`ADO_STEP_NAME`] and [`ado_condition`]).
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { done } = request;

        match ctx.backend() {
            FlowBackend::Github => {
                ctx.emit_rust_step("classify PR changes", |ctx| {
                    let done = done.claim(ctx);
                    |rt| {
                        classify_github()?;
                        rt.write(done, &());
                        Ok(())
                    }
                });
            }

            FlowBackend::Ado => {
                // The ADO step must have a stable, well-known name so that
                // downstream jobs can reference it as:
                //   dependencies.<job>.outputs['classify_pr_changes.is_non_product']
                //
                // `emit_ado_step_with_inline_script` generates a step that:
                //   1. Runs the bash classify script (sets IS_NON_PRODUCT + ADO output var).
                //   2. Runs the Flowey inline snippet (writes the done signal).
                //
                // NOTE on format! escaping used in the YAML snippet below:
                //   In Rust format strings, `{{` and `}}` produce literal `{` and `}`.
                //   {{{{ ... }}}} → {{ ... }} (used for the {{FLOWEY_INLINE_SCRIPT}} marker)
                //   ${{VAR}} → ${VAR} (Rust escaping → bash variable expansion syntax)
                ctx.emit_ado_step_with_inline_script("classify PR changes", |ctx| {
                    let done = done.claim(ctx);
                    (
                        |_rt| {
                            format!(
                                concat!(
                                    "- bash: |\n",
                                    "    set -euo pipefail\n",
                                    "    TARGET_BRANCH=\"${{SYSTEM_PULLREQUEST_TARGETBRANCH:-}}\"\n",
                                    "    if [[ -z \"$TARGET_BRANCH\" ]]; then\n",
                                    "      echo \"Not a PR run; treating as product change.\"\n",
                                    "      IS_NON_PRODUCT=false\n",
                                    "    else\n",
                                    "      TARGET_BRANCH=\"${{TARGET_BRANCH#refs/heads/}}\"\n",
                                    "      echo \"Comparing against: origin/$TARGET_BRANCH\"\n",
                                    "      CHANGED=$(git diff --name-only \"origin/$TARGET_BRANCH...HEAD\" 2>/dev/null || true)\n",
                                    "      if [[ -z \"$CHANGED\" ]]; then\n",
                                    "        echo \"No changed files found; treating as product change.\"\n",
                                    "        IS_NON_PRODUCT=false\n",
                                    "      else\n",
                                    "        IS_NON_PRODUCT=true\n",
                                    "        while IFS= read -r F; do\n",
                                    "          if [[ \"$F\" == Guide/* ]] || [[ \"$F\" =~ ^repo_support/.*\\.py$ ]]; then\n",
                                    "            : # file is in a non-product bucket\n",
                                    "          else\n",
                                    "            echo \"Product file detected: $F\"\n",
                                    "            IS_NON_PRODUCT=false\n",
                                    "            break\n",
                                    "          fi\n",
                                    "        done <<< \"$CHANGED\"\n",
                                    "      fi\n",
                                    "    fi\n",
                                    "    echo \"is_non_product=$IS_NON_PRODUCT\"\n",
                                    "    echo \"##vso[task.setvariable variable=is_non_product;isOutput=true]$IS_NON_PRODUCT\"\n",
                                    "    {{{{FLOWEY_INLINE_SCRIPT}}}}\n",
                                    "  name: {step_name}\n"
                                ),
                                step_name = ADO_STEP_NAME,
                            )
                        },
                        move |rt| {
                            rt.write(done, &());
                            Ok(())
                        },
                    )
                });
            }

            FlowBackend::Local => {
                // PR classification is not applicable for local runs.
                // vmm-tests always run locally (no job-level skip).
                ctx.emit_rust_step("classify PR changes (local: always product)", |ctx| {
                    let done = done.claim(ctx);
                    |rt| {
                        rt.write(done, &());
                        Ok(())
                    }
                });
            }
        }

        Ok(())
    }
}

// ─── Classification helpers ──────────────────────────────────────────────────

/// Classify PR files on the GitHub backend.
///
/// Uses `GITHUB_BASE_REF` (available in PR events) to find the merge base,
/// then runs `git diff` to list changed files.  Writes the result to
/// `GITHUB_ENV` under [`GH_ENV_IS_NON_PRODUCT`] so it is available as a
/// job-level output.
fn classify_github() -> anyhow::Result<()> {
    use std::process::Command;

    // GITHUB_BASE_REF is set for `pull_request` events (e.g. "main").
    // It is empty for push events, manual dispatches, etc.
    let base_ref = match std::env::var("GITHUB_BASE_REF") {
        Ok(r) if !r.is_empty() => r,
        _ => {
            println!("Not a PR run (GITHUB_BASE_REF not set); treating as product change.");
            return write_github_env(GH_ENV_IS_NON_PRODUCT, "false");
        }
    };

    let target = format!("origin/{base_ref}...HEAD");
    let output = Command::new("git")
        .args(["diff", "--name-only", &target])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("git diff failed ({stderr}); treating as product change.");
        return write_github_env(GH_ENV_IS_NON_PRODUCT, "false");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let changed: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();

    if changed.is_empty() {
        println!("No changed files found; treating as product change.");
        return write_github_env(GH_ENV_IS_NON_PRODUCT, "false");
    }

    println!("Changed files:");
    for f in &changed {
        println!("  {f}");
    }

    let result = changed.iter().all(|f| is_non_product_path(f));
    let result_str = if result { "true" } else { "false" };
    println!("is_non_product={result_str}");

    write_github_env(GH_ENV_IS_NON_PRODUCT, result_str)
}

/// Returns `true` when `path` is entirely within an approved non-product bucket.
///
/// Non-product buckets (extend here AND in the ADO bash script above):
/// - `Guide/**`           — docs tree, validated by the separate docs pipeline
/// - `repo_support/**/*.py` — repo automation scripts, no product impact
fn is_non_product_path(path: &str) -> bool {
    path.starts_with("Guide/")
        || (path.starts_with("repo_support/") && path.ends_with(".py"))
}

/// Appends `NAME=VALUE\n` to the file indicated by `$GITHUB_ENV`.
fn write_github_env(name: &str, value: &str) -> anyhow::Result<()> {
    if let Ok(path) = std::env::var("GITHUB_ENV") {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("failed to open GITHUB_ENV at {path}: {e}"))?;
        writeln!(f, "{name}={value}")
            .map_err(|e| anyhow::anyhow!("failed to write to GITHUB_ENV: {e}"))?;
    }
    Ok(())
}
