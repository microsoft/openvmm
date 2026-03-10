// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Classifies PR changed files to determine whether all changes fall within
//! approved non-product buckets.
//!
//! Non-product bucket patterns are defined in
//! [`non_product_config.toml`](../non_product_config.toml) alongside this
//! source file.  To add or remove a bucket, edit that file — all backends
//! (GitHub, ADO, local) read from the same parsed config.
//!
//! ## How classification works
//!
//! A PR is **non-product** only when every changed file matches at least one
//! bucket.  Any unmatched file → product change (conservative default).
//!
//! ## Cross-job communication
//!
//! | Backend | Classification method | Cross-job result |
//! |---------|----------------------|-----------------|
//! | GitHub  | `git diff origin/$GITHUB_BASE_REF...HEAD` | Written to `$GITHUB_ENV` as [`GH_ENV_IS_NON_PRODUCT`]; exposed as a job output |
//! | ADO     | `git diff origin/$SYSTEM_PULLREQUEST_TARGETBRANCH...HEAD` | Published as `##vso[task.setvariable;isOutput=true]`; step name [`ADO_STEP_NAME`] |
//! | Local   | Always `false` (conservative) | N/A |

use flowey::node::prelude::*;
use std::io::Write as _;

// ─── Bucket config ───────────────────────────────────────────────────────────

/// Embedded TOML config defining the non-product bucket patterns.
///
/// Edit `non_product_config.toml` in this directory to add or remove buckets.
const CONFIG_TOML: &str = include_str!("non_product_config.toml");

/// A single non-product path bucket parsed from `non_product_config.toml`.
#[derive(Debug, serde::Deserialize)]
pub struct Bucket {
    /// Path must start with this string.
    pub prefix: String,
    /// If present, path must also end with this string (e.g. `".py"`).
    pub suffix: Option<String>,
    /// Human-readable rationale shown in classification output.
    pub description: String,
}

/// Top-level structure of `non_product_config.toml`.
#[derive(Debug, serde::Deserialize)]
struct Config {
    bucket: Vec<Bucket>,
}

/// Parse and return the non-product bucket list from the embedded config.
///
/// Panics on malformed TOML — the config is embedded at compile time so a
/// parse failure is a programmer error, not a runtime condition.
pub fn load_buckets() -> Vec<Bucket> {
    toml_edit::de::from_str::<Config>(CONFIG_TOML)
        .unwrap_or_else(|e| panic!("non_product_config.toml is malformed: {e}"))
        .bucket
}

/// Returns `true` when `path` matches at least one non-product bucket.
pub fn is_non_product_path(path: &str, buckets: &[Bucket]) -> bool {
    buckets.iter().any(|b| {
        path.starts_with(b.prefix.as_str())
            && b.suffix
                .as_deref()
                .map_or(true, |s| path.ends_with(s))
    })
}

// ─── Public constants ────────────────────────────────────────────────────────

/// Name of the `$GITHUB_ENV` variable written by the classify Rust step.
///
/// Pass this to [`PipelineJob::gh_set_job_output_from_env_var`] to expose it
/// as a job-level output readable by dependent jobs.
///
/// [`PipelineJob::gh_set_job_output_from_env_var`]:
///     flowey_core::pipeline::PipelineJob::gh_set_job_output_from_env_var
pub const GH_ENV_IS_NON_PRODUCT: &str = "FLOWEY_IS_NON_PRODUCT";

/// ADO step `name:` used when publishing the `is_non_product` output variable.
///
/// Downstream jobs reference the result as
/// `dependencies.<JOB>.outputs['classify_pr_changes.is_non_product']`.
pub const ADO_STEP_NAME: &str = "classify_pr_changes";

/// Returns an ADO `condition:` expression that gates a job on the PR NOT being
/// a non-product-only change.
///
/// `classify_job_id` is the ADO job ID obtained via
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
        /// The result is communicated to downstream jobs via backend-native
        /// mechanisms: `$GITHUB_ENV` (GitHub) and an ADO output variable (ADO).
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
                        let buckets = load_buckets();
                        let result = classify("GITHUB_BASE_REF", false, &buckets)?;
                        write_github_env(GH_ENV_IS_NON_PRODUCT, if result { "true" } else { "false" })?;
                        rt.write(done, &());
                        Ok(())
                    }
                });
            }

            FlowBackend::Ado => {
                // `emit_ado_step_with_inline_script` lets us set the step's
                // `name:` field, which is required for ADO cross-job output
                // variable references:
                //   dependencies.<JOB>.outputs['classify_pr_changes.is_non_product']
                //
                // The bash wrapper is intentionally minimal — just
                // {{FLOWEY_INLINE_SCRIPT}}.  All classification logic lives in
                // the Rust closure below, using the same `classify` helper as
                // the GitHub backend.
                //
                // NOTE on format! escaping:
                //   {{{{ ... }}}} → {{ ... }} (the {{FLOWEY_INLINE_SCRIPT}} marker)
                ctx.emit_ado_step_with_inline_script("classify PR changes", |ctx| {
                    let done = done.claim(ctx);
                    (
                        |_rt| {
                            format!(
                                "- bash: |\n    {{{{FLOWEY_INLINE_SCRIPT}}}}\n  name: {step_name}\n",
                                step_name = ADO_STEP_NAME,
                            )
                        },
                        move |rt| {
                            let buckets = load_buckets();
                            // ADO supplies the target branch with a `refs/heads/` prefix.
                            let result = classify(
                                "SYSTEM_PULLREQUEST_TARGETBRANCH",
                                true,
                                &buckets,
                            )?;
                            let result_str = if result { "true" } else { "false" };
                            println!("is_non_product={result_str}");
                            println!(
                                "##vso[task.setvariable variable=is_non_product;isOutput=true]{result_str}"
                            );
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

/// Get the list of PR-changed files and classify them against `buckets`.
///
/// `base_ref_env` is the environment variable that holds the target branch
/// name (e.g. `GITHUB_BASE_REF` or `SYSTEM_PULLREQUEST_TARGETBRANCH`).
///
/// `strip_refs_heads` strips a leading `refs/heads/` prefix from the value,
/// which ADO includes but GitHub does not.
///
/// Returns `true` when every changed file falls within a non-product bucket.
fn classify(base_ref_env: &str, strip_refs_heads: bool, buckets: &[Bucket]) -> anyhow::Result<bool> {
    use std::process::Command;

    let base_ref = match std::env::var(base_ref_env) {
        Ok(r) if !r.is_empty() => {
            if strip_refs_heads {
                r.strip_prefix("refs/heads/").map(str::to_string).unwrap_or(r)
            } else {
                r
            }
        }
        _ => {
            println!("Not a PR run ({base_ref_env} not set); treating as product change.");
            return Ok(false);
        }
    };

    let target = format!("origin/{base_ref}...HEAD");
    let output = Command::new("git")
        .args(["diff", "--name-only", &target])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("git diff failed ({stderr}); treating as product change.");
        return Ok(false);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let changed: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();

    if changed.is_empty() {
        println!("No changed files; treating as product change.");
        return Ok(false);
    }

    println!("Changed files:");
    for f in &changed {
        let matched = is_non_product_path(f, buckets);
        println!("  {} {f}", if matched { "○" } else { "●" });
    }

    let result = changed.iter().all(|f| is_non_product_path(f, buckets));
    println!("is_non_product={}", if result { "true" } else { "false" });
    Ok(result)
}

/// Appends `NAME=VALUE\n` to the file pointed to by `$GITHUB_ENV`.
fn write_github_env(name: &str, value: &str) -> anyhow::Result<()> {
    if let Ok(path) = std::env::var("GITHUB_ENV") {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("failed to open GITHUB_ENV at {path}: {e}"))?;
        writeln!(f, "{name}={value}")
            .map_err(|e| anyhow::anyhow!("failed to write GITHUB_ENV: {e}"))?;
    }
    Ok(())
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_buckets() -> Vec<Bucket> {
        load_buckets()
    }

    #[test]
    fn test_config_parses_successfully() {
        // Should not panic.
        let buckets = test_buckets();
        assert!(!buckets.is_empty(), "expected at least one bucket in config");
    }

    #[test]
    fn test_guide_root_file_is_non_product() {
        let b = test_buckets();
        assert!(is_non_product_path("Guide/src/intro.md", &b));
    }

    #[test]
    fn test_guide_nested_file_is_non_product() {
        let b = test_buckets();
        assert!(is_non_product_path("Guide/src/dev_guide/flowey/pipelines.md", &b));
    }

    #[test]
    fn test_repo_support_py_is_non_product() {
        let b = test_buckets();
        assert!(is_non_product_path("repo_support/relabel_backported.py", &b));
    }

    #[test]
    fn test_repo_support_non_py_is_product() {
        let b = test_buckets();
        // A non-.py file under repo_support/ is still a product change.
        assert!(!is_non_product_path("repo_support/README.md", &b));
    }

    #[test]
    fn test_product_code_is_product() {
        let b = test_buckets();
        assert!(!is_non_product_path("vmm_tests/src/tests/foo.rs", &b));
    }

    #[test]
    fn test_flowey_code_is_product() {
        let b = test_buckets();
        assert!(!is_non_product_path(
            "flowey/flowey_lib_hvlite/src/check_pr_changes.rs",
            &b
        ));
    }

    #[test]
    fn test_github_workflow_yaml_is_product() {
        let b = test_buckets();
        // Pipeline YAML changes should trigger full tests.
        assert!(!is_non_product_path(".github/workflows/openvmm-pr.yaml", &b));
    }

    #[test]
    fn test_bucket_prefix_only() {
        // Verify prefix-only buckets work (suffix = None → always matches if prefix matches).
        let buckets = vec![Bucket {
            prefix: "Foo/".into(),
            suffix: None,
            description: "test".into(),
        }];
        assert!(is_non_product_path("Foo/bar.rs", &buckets));
        assert!(is_non_product_path("Foo/bar/baz.txt", &buckets));
        // "NotFoo/" does not start with "Foo/" — the prefix must match exactly.
        assert!(!is_non_product_path("NotFoo/file.rs", &buckets));
    }

    #[test]
    fn test_bucket_prefix_and_suffix() {
        let buckets = vec![Bucket {
            prefix: "scripts/".into(),
            suffix: Some(".sh".into()),
            description: "test".into(),
        }];
        assert!(is_non_product_path("scripts/deploy.sh", &buckets));
        assert!(!is_non_product_path("scripts/deploy.py", &buckets));
        assert!(!is_non_product_path("other/deploy.sh", &buckets));
    }
}
