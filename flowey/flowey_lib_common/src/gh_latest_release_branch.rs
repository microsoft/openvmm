// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Gets the latest release branch from the openvmm repository
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub latest_release_branch: WriteVar<String>,
    }
}
new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::use_gh_cli::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            latest_release_branch,
        } = request;

        let gh_cli = ctx.reqv(crate::use_gh_cli::Request::Get);

        ctx.emit_rust_step("get latest release branch", |ctx| {
            let gh_cli = gh_cli.claim(ctx);
            let latest_release_branch = latest_release_branch.claim(ctx);

            move |rt| {
                let sh = xshell::Shell::new()?;
                let gh_cli = rt.read(gh_cli);

                // Get all branches from the openvmm repository
                let branches_json = xshell::cmd!(
                    sh,
                    "{gh_cli} api repos/microsoft/openvmm/branches --paginate -q '.[].name'"
                )
                .read()?;

                // Parse branch names and find release branches
                let mut release_branches = Vec::new();
                for line in branches_json.lines() {
                    let branch_name = line.trim().trim_matches('"');
                    if let Some(yymm_part) = branch_name.strip_prefix("release/") {
                        // Check if the remaining part is exactly 4 digits
                        if yymm_part.len() == 4 && yymm_part.chars().all(|c| c.is_ascii_digit()) {
                            if let Ok(yymm) = yymm_part.parse::<u32>() {
                                release_branches.push((yymm, branch_name.to_string()));
                            }
                        }
                    }
                }

                // Find the latest release branch (highest YYMM value)
                let latest_branch = if release_branches.is_empty() {
                    String::new()
                } else {
                    release_branches.sort_by_key(|(yymm, _)| *yymm);
                    release_branches.last().unwrap().1.clone()
                };

                println!(
                    "Latest release branch: {}",
                    if latest_branch.is_empty() {
                        "none found"
                    } else {
                        &latest_branch
                    }
                );
                rt.write(latest_release_branch, &latest_branch);

                Ok(())
            }
        });

        Ok(())
    }
}
