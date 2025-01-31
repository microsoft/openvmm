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
                            owner: "flowey".to_string(),
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
                        owner: String::new(),
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
    owner: String,
}

impl ProblemMatcher {
    pub fn enable(&self) -> EnabledProblemMatcher<'_> {
        let owner = if let Some(path) = &self.path {
            println!("::add-matcher::{}", path.display());
            Some(self.owner.as_ref())
        } else {
            None
        };
        EnabledProblemMatcher(owner)
    }
}

#[must_use = "pattern matcher is disabled when this is dropped"]
pub struct EnabledProblemMatcher<'a>(Option<&'a str>);

impl Drop for EnabledProblemMatcher<'_> {
    fn drop(&mut self) {
        if let Some(owner) = self.0 {
            println!("::remove-matcher owner={owner}::");
        }
    }
}
