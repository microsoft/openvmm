// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request(pub WriteVar<ProblemMatcher>);
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let vars = requests.into_iter().map(|Request(v)| v).collect::<Vec<_>>();

        if ctx.backend() == FlowBackend::Github {
            ctx.emit_rust_step("write GitHub problem matchers", |ctx| {
                let vars = vars.claim(ctx);
                |rt| {
                    let path = "gh_problem_matcher.json".absolute()?;
                    fs_err::write(&path, include_bytes!("gh_problem_matcher.json"))?;
                    rt.write_all(
                        vars,
                        &ProblemMatcher {
                            path: Some(path),
                            owners: ["flowey-rustc", "flowey-rust-panic"]
                                .map(Into::into)
                                .to_vec(),
                        },
                    );
                    Ok(())
                }
            });
        } else {
            for v in vars {
                v.write_static(
                    ctx,
                    ProblemMatcher {
                        path: None,
                        owners: Vec::new(),
                    },
                )
            }
        }

        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProblemMatcher {
    path: Option<PathBuf>,
    owners: Vec<String>,
}

impl ProblemMatcher {
    pub fn enable(&self) -> EnabledProblemMatcher<'_> {
        let owners = if let Some(path) = &self.path {
            println!("::add-matcher::{}", path.display());
            self.owners.as_slice()
        } else {
            &[]
        };
        EnabledProblemMatcher(owners)
    }
}

#[must_use = "pattern matcher is disabled when this is dropped"]
pub struct EnabledProblemMatcher<'a>(&'a [String]);

impl Drop for EnabledProblemMatcher<'_> {
    fn drop(&mut self) {
        for owner in self.0 {
            println!("::remove-matcher owner={owner}::");
        }
    }
}
