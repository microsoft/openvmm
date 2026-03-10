// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Classifies PR changed files to determine whether all changes fall within
//! approved non-product buckets (e.g. `Guide/**`, `repo_support/**/*.py`).
//!
//! This is used by the checkin-gates pipeline to skip expensive `vmm-tests`
//! jobs when the PR only touches non-product files.
//!
//! The result is surfaced as a GitHub Actions job output named `is_non_product`
//! via [`IS_NON_PRODUCT_JOB_OUTPUT_EXPR`].  Callers that want to expose the
//! result to dependent jobs should declare the job output using:
//!
//! ```ignore
//! job.gh_set_job_output("is_non_product", check_pr_changes::IS_NON_PRODUCT_JOB_OUTPUT_EXPR)
//! ```

use flowey::node::prelude::*;

/// GitHub Actions expression suitable for use in a job-level `outputs:` block.
///
/// References the `FLOWEY_CHECKIN_IS_NON_PRODUCT` environment variable that
/// the `.github/actions/classify-pr-changes` action writes to `$GITHUB_ENV`.
///
/// Value is `"true"` when every changed file is in a non-product bucket, and
/// `"false"` otherwise (conservative default for non-PR runs and errors).
pub const IS_NON_PRODUCT_JOB_OUTPUT_EXPR: &str = "${{ env.FLOWEY_CHECKIN_IS_NON_PRODUCT }}";

flowey_request! {
    pub struct Request {
        /// Signal that the classification has been performed.
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { done } = request;

        // Pass GitHub context values as static expressions so that no Rust step
        // (and therefore no Rust installation) is required in the classify job.
        // GitHub Actions evaluates `${{ ... }}` expressions in `with:` blocks.
        let classified = ctx
            .emit_gh_step("classify PR changes", "./.github/actions/classify-pr-changes")
            .with("github-token", "${{ github.token }}")
            .with(
                "pr-number",
                "${{ github.event.pull_request.number || 0 }}",
            )
            .with("repository", "${{ github.repository }}")
            .requires_permission(GhPermission::PullRequests, GhPermissionValue::Read)
            .finish(ctx);

        ctx.emit_side_effect_step([classified], [done]);

        Ok(())
    }
}
