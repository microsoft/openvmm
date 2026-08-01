// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publish a github release

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request(pub GhReleaseParams);
}

#[derive(Serialize, Deserialize)]
pub enum GhReleaseNotes {
    Generated,
    Text(String),
}

/// What to do when a release already exists for the tag being published.
#[derive(Serialize, Deserialize)]
pub enum OnExistingRelease {
    /// Leave it alone and report success.
    ///
    /// Suits a release whose tag comes from a version in the tree, where
    /// rerunning on an unchanged version is routine and means nothing is
    /// wrong.
    Skip,
    /// Fail.
    ///
    /// Suits a release triggered by pushing a tag, where an existing release
    /// means a previous attempt already got this far. Assets are never
    /// replaced automatically, because the existing release may be one that
    /// has already been reviewed or published.
    Fail,
    /// Delete and recreate an existing draft, but refuse a published release.
    ///
    /// This makes a fallible release pipeline safely rerunnable while
    /// preserving the immutability of anything already made public.
    ReplaceDraft,
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
    /// Commit hash to target
    pub target: ReadVar<String, C>,
    /// Tag associated with the release artifact.
    pub tag: ReadVar<String, C>,
    /// Title associated with the release artifact.
    pub title: ReadVar<String, C>,
    /// Files to upload.
    pub files: ReadVar<Vec<(PathBuf, Option<String>)>, C>,
    /// Release notes to attach to the release.
    pub notes: GhReleaseNotes,
    /// Whether the release should be created as a draft
    pub draft: bool,
    /// What to do when a release already exists for this tag.
    pub on_existing: OnExistingRelease,
    /// Side effects that must complete before the release is published.
    pub prerequisites: Vec<ReadVar<SideEffect, C>>,

    pub done: WriteVar<SideEffect, C>,
}

impl GhReleaseParams {
    pub fn claim(self, ctx: &mut StepCtx<'_>) -> GhReleaseParams<VarClaimed> {
        let GhReleaseParams {
            repo_owner,
            repo_name,
            target,
            tag,
            title,
            files,
            notes,
            draft,
            on_existing,
            prerequisites,
            done,
        } = self;

        GhReleaseParams {
            repo_owner,
            repo_name,
            target: target.claim(ctx),
            tag: tag.claim(ctx),
            title: title.claim(ctx),
            files: files.claim(ctx),
            notes,
            draft,
            on_existing,
            prerequisites: prerequisites.claim(ctx),
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
                        target,
                        tag,
                        title,
                        files,
                        notes,
                        draft,
                        on_existing,
                        prerequisites,
                        done: _,
                    } = req;

                    for prerequisite in prerequisites {
                        rt.read(prerequisite);
                    }

                    let repo = format!("{repo_owner}/{repo_name}");
                    let target = rt.read(target);
                    let tag = rt.read(tag);

                    // check if the release already exists
                    //
                    // xshell doesn't give us the exit code, so we have to
                    // use the raw process API instead.
                    //
                    // Capture the output rather than letting it inherit. On the
                    // ordinary path there is no release yet, so `gh` writes
                    // "release not found", which is a confusing thing to find in
                    // the log of a run that went on to publish successfully. It
                    // is still logged when the command fails for some other
                    // reason -- an auth failure or a 5xx also exit non-zero, and
                    // are indistinguishable from "not found" without it.
                    let output = std::process::Command::new(&gh_cli)
                        .arg("release")
                        .arg("view")
                        .arg(&tag)
                        .arg("--repo")
                        .arg(&repo)
                        .args(["--json", "isDraft", "--jq", ".isDraft"])
                        .output()
                        .context("failed to spawn gh cli")?;

                    // Success means the release already exists. Query draft
                    // state in the same command so a rerun may replace a draft
                    // without ever modifying a published release.
                    if output.status.success() {
                        let is_draft =
                            String::from_utf8_lossy(&output.stdout).trim() == "true";
                        match on_existing {
                            OnExistingRelease::Skip => {
                                log::info!("GitHub release with tag {tag} already exists in repo {repo}. Skipping...");
                                continue;
                            }
                            OnExistingRelease::Fail => {
                                anyhow::bail!(
                                    "a GitHub release already exists for tag {tag} in repo \
                                     {repo}. Its assets are not replaced automatically, since \
                                     the existing release may already have been reviewed or \
                                     published. Delete it and rerun if it should be regenerated."
                                );
                            }
                            OnExistingRelease::ReplaceDraft if is_draft => {
                                flowey::shell_cmd!(
                                    rt,
                                    "{gh_cli} release delete {tag} --repo {repo} --yes"
                                )
                                .run()?;
                            }
                            OnExistingRelease::ReplaceDraft => {
                                anyhow::bail!(
                                    "GitHub release {tag} in {repo} is already published; \
                                     published releases and their assets are immutable"
                                );
                            }
                        }
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if !stderr.contains("release not found") {
                            anyhow::bail!(
                                "failed to query GitHub release {tag} in {repo}: {}",
                                stderr.trim()
                            );
                        }
                        log::debug!(
                            "assuming no release exists for tag {tag} in repo {repo}; \
                             `gh release view` exited {} with: {}",
                            output.status,
                            stderr.trim(),
                        );
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

                    let notes = match notes {
                        GhReleaseNotes::Generated => vec!["--generate-notes".to_owned()],
                        GhReleaseNotes::Text(notes) => vec!["--notes".to_owned(), notes],
                    };
                    let draft = draft.then_some("--draft");
                    flowey::shell_cmd!(rt, "{gh_cli} release create {tag} {files...} --repo {repo} --target {target} --title {title} {notes...} {draft...}").run()?;
                }

                Ok(())
            }
        });

        Ok(())
    }
}
