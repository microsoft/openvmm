// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Confirm that a revision is contained in a reviewed OpenVMM branch.
//!
//! A release is cut from reviewed history, so the commit must already be
//! merged into a protected `main` or `release/*` branch. A dispatch may name
//! any ref, including one that never passed review, so this check gates both
//! processing the revision and publishing it.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// The revision to verify.
        pub revision: ReadVar<String>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::use_gh_cli::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { revision, done } = request;

        let gh_cli = ctx.reqv(flowey_lib_common::use_gh_cli::Request::Get);

        ctx.emit_rust_step("verify release commit is reviewed", |ctx| {
            done.claim(ctx);
            let gh_cli = gh_cli.claim(ctx);
            let revision = revision.claim(ctx);
            move |rt| {
                let gh_cli = rt.read(gh_cli);
                let revision = rt.read(revision);

                validate_commit_sha(&revision).context("invalid source release request")?;

                let branches = flowey::shell_cmd!(
                    rt,
                    "{gh_cli} api --paginate --slurp 'repos/microsoft/openvmm/branches?protected=true&per_page=100'"
                )
                .read()
                .context("failed to list the protected branches")?;
                let candidates = candidate_branches(
                    &serde_json::from_str(&branches)
                        .context("failed to parse the protected branch list")?,
                )?;

                for branch in &candidates {
                    let compare = flowey::shell_cmd!(
                        rt,
                        "{gh_cli} api repos/microsoft/openvmm/compare/{revision}...{branch}?per_page=1"
                    )
                    .read()
                    .with_context(|| format!("failed to compare {revision} against {branch}"))?;
                    let compare = serde_json::from_str(&compare)
                        .with_context(|| format!("failed to parse the comparison against {branch}"))?;
                    if !branch_contains(&revision, &compare)? {
                        continue;
                    }

                    log::info!("{revision} is contained in reviewed branch {branch}");
                    return Ok(());
                }

                anyhow::bail!(
                    "commit {revision} is not contained in any protected `main` or `release/*` \
                     branch (checked: {}). A release is cut from reviewed history, so the \
                     commit must be merged first.",
                    candidates.join(", ")
                )
            }
        });

        Ok(())
    }
}

/// The protected branches a release may be cut from, `main` first.
fn candidate_branches(pages: &serde_json::Value) -> anyhow::Result<Vec<String>> {
    let pages = pages
        .as_array()
        .context("expected the protected branch pages to be an array")?;

    let mut names = Vec::new();
    for page in pages {
        let branches = page
            .as_array()
            .context("expected every protected branch page to be an array")?;
        for branch in branches {
            let protected = branch["protected"]
                .as_bool()
                .context("expected every branch to report whether it is protected")?;
            if !protected {
                continue;
            }

            let name = branch["name"]
                .as_str()
                .context("expected every protected branch to be named")?;
            if name == "main" || name.starts_with("release/") {
                names.push(name.to_owned());
            }
        }
    }

    // Stable sorts, so `main` is tried before the release branches.
    names.sort();
    names.sort_by_key(|name| name != "main");

    if names.is_empty() {
        anyhow::bail!("no protected `main` or `release/*` branches were returned by GitHub");
    }

    Ok(names)
}

/// Whether `revision` is contained in the branch `compare` was taken against.
///
/// Containment, not tip equality: the branch advances while the release builds.
fn branch_contains(revision: &str, compare: &serde_json::Value) -> anyhow::Result<bool> {
    let merge_base = compare["merge_base_commit"]["sha"]
        .as_str()
        .context("expected the comparison to name a merge base")?;
    Ok(merge_base == revision)
}

pub(crate) fn validate_commit_sha(revision: &str) -> anyhow::Result<()> {
    if revision.len() != 40
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("revision must be a full 40-character lowercase commit SHA");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_candidates_are_protected_branches_with_main_first() {
        let pages = serde_json::json!([
            [
                { "name": "release/2505", "protected": true },
                { "name": "release/1.8.2607", "protected": true },
            ],
            [
                { "name": "main", "protected": true },
                { "name": "release/1.7.2511", "protected": true },
            ],
        ]);

        assert_eq!(
            candidate_branches(&pages).unwrap(),
            [
                "main",
                "release/1.7.2511",
                "release/1.8.2607",
                "release/2505"
            ]
        );
    }

    #[test]
    fn only_protected_main_and_release_branches_are_candidates() {
        let pages = serde_json::json!([
            [
                { "name": "main", "protected": true },
                { "name": "release/unprotected", "protected": false },
                { "name": "copilot/some-branch", "protected": true },
            ],
        ]);

        assert_eq!(candidate_branches(&pages).unwrap(), ["main"]);
    }

    #[test]
    fn rejects_a_response_without_reviewed_branches() {
        let pages = serde_json::json!([
            [
                { "name": "release/unprotected", "protected": false },
                { "name": "copilot/some-branch", "protected": true },
            ],
        ]);

        assert!(candidate_branches(&pages).is_err());
    }

    #[test]
    fn a_merged_revision_is_contained_in_the_branch() {
        let revision = "ee2fc3f0000000000000000000000000000000ee";
        let compare = serde_json::json!({
            "status": "ahead",
            "merge_base_commit": { "sha": revision },
        });

        assert!(branch_contains(revision, &compare).unwrap());
    }

    #[test]
    fn an_unmerged_revision_is_not_contained_in_the_branch() {
        let compare = serde_json::json!({
            "status": "diverged",
            "merge_base_commit": { "sha": "6bb401ccbb840000000000000000000000000000" },
        });

        assert!(!branch_contains("c8c65e554b64bb2042d94fc612719d6c1e767235", &compare).unwrap());
    }

    #[test]
    fn release_revision_must_be_a_full_lowercase_commit_sha() {
        assert!(validate_commit_sha("0123456789abcdef0123456789abcdef01234567").is_ok());
        assert!(validate_commit_sha("").is_err());
        assert!(validate_commit_sha("main").is_err());
        assert!(validate_commit_sha("0123456789ABCDEF0123456789ABCDEF01234567").is_err());
        assert!(validate_commit_sha("0123456789abcdef0123456789abcdef0123456g").is_err());
    }
}
