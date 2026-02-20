// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publish a github release

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request(pub GhReleaseParams);
}

#[derive(Serialize, Deserialize)]
pub struct GhReleaseParams<C = VarNotClaimed> {
    /// First component of a github repo path
    ///
    /// e.g: the "foo" in "github.com/foo/bar"
    pub repo_owner: String,
    /// Second component of a github repo path
    ///
    /// e.g: the "bar" in "github.com/foo/bar"
    pub repo_name: String,
    /// Tag associated with the release artifact.
    pub tag: ReadVar<String, C>,
    /// Title associated with the release artifact.
    pub title: ReadVar<String, C>,
    /// Files to upload.
    pub files: ReadVar<Vec<(PathBuf, Option<String>)>, C>,
    /// Whether the release should be created as a draft
    pub draft: bool,

    pub done: WriteVar<SideEffect, C>,
}

impl GhReleaseParams {
    pub fn claim(self, ctx: &mut StepCtx<'_>) -> GhReleaseParams<VarClaimed> {
        let GhReleaseParams {
            repo_owner,
            repo_name,
            tag,
            title,
            files,
            draft,
            done,
        } = self;

        GhReleaseParams {
            repo_owner,
            repo_name,
            tag: tag.claim(ctx),
            title: title.claim(ctx),
            files: files.claim(ctx),
            draft,
            done: done.claim(ctx),
        }
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cache::Node>();
        ctx.import::<crate::use_gh_cli::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        if requests.is_empty() {
            return Ok(());
        }

        let gh_cli = ctx.reqv(crate::use_gh_cli::Request::Get);

        ctx.emit_rust_step("publish github releases", |ctx| {
            let requests = requests
                .into_iter()
                .map(|r| r.0.claim(ctx))
                .collect::<Vec<_>>();
            let gh_cli = gh_cli.claim(ctx);

            move |rt| {
                let gh_cli = rt.read(gh_cli);

                for req in requests {
                    let GhReleaseParams {
                        repo_owner,
                        repo_name,
                        tag,
                        title,
                        files,
                        draft,
                        done: _,
                    } = req;

                    let repo = format!("{repo_owner}/{repo_name}");
                    let tag = rt.read(tag);

                    // check if the release already exists
                    //
                    // xshell doesn't give us the exit code, so we have to
                    // use the raw process API instead.
                    let mut command = std::process::Command::new(&gh_cli);
                    command
                        .arg("release").arg("view").arg(&tag).arg("--repo").arg(&repo);
                    let mut child = command.spawn().context(
                       "failed to spawn gh cli"
                    )?;
                    let status = child.wait()?;

                    // success means the release already exists, so skip publishing this release
                    if status.success() {
                        log::info!("GitHub release with tag {tag} already exists in repo {repo}. Skipping...");
                        continue;
                    };

                    let title = rt.read(title);
                    let files = rt.read(files)
                        .into_iter()
                        .map(|(path, label)| {
                            let path = path.to_string_lossy().to_string();
                            if let Some(label) = label {
                                format!("{path}#{label}")
                            } else {
                                path
                            }
                        })
                        .collect::<Vec<_>>();
                    let draft = draft.then_some("--draft");

                    flowey::shell_cmd!(rt, "{gh_cli} release create --repo {repo} {tag} --title {title} --notes-from-tag {draft...} {files...}").run()?;
                }

                Ok(())
            }
        });

        Ok(())
    }
}
