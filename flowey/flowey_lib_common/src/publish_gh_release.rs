// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a github release artifact

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request{
        /// The github repo path
        pub repo: ReadVar<String>,
        /// Release tag
        pub tag: ReadVar<String>,
        /// Release title
        pub title: ReadVar<String>,
        /// Target branch or full commit SHA
        pub target: ReadVar<String>,
        /// Files to upload
        pub files: Vec<ReadVar<PathBuf>>,
        /// Github token to authenticate with
        pub gh_token: ReadVar<String>,

        pub done: WriteVar<SideEffect>,
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
            repo,
            tag,
            title,
            target,
            files,
            gh_token,
            done,
        } = request;

        ctx.req(crate::use_gh_cli::Request::WithAuth(
            crate::use_gh_cli::GhCliAuth::AuthToken(gh_token),
        ));
        let gh_cli = ctx.reqv(crate::use_gh_cli::Request::Get);

        ctx.emit_rust_step("publish github releases", |ctx| {
            let gh_cli = gh_cli.claim(ctx);

            let repo = repo.claim(ctx);
            let tag = tag.claim(ctx);
            let title = title.claim(ctx);
            let target = target.claim(ctx);
            let files=  files.claim(ctx);

            done.claim(ctx);

            move |rt| {
                let gh_cli = rt.read(gh_cli);

                let repo = rt.read(repo);
                let tag = rt.read(tag);
                let title = rt.read(title);
                let target = rt.read(target);
                let files = rt.read(files);

                let sh = xshell::Shell::new()?;

                xshell::cmd!(
                    sh,
                    "{gh_cli} release create --repo {repo} --title {title} --target {target} {tag} {files...}"
                )
                .run()?;

                Ok(())
            }
        });

        Ok(())
    }
}
