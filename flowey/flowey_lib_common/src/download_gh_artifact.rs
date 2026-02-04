// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a github release artifact

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// First component of a github repo path
        ///
        /// e.g: the "foo" in "github.com/foo/bar"
        pub repo_owner: String,
        /// Second component of a github repo path
        ///
        /// e.g: the "bar" in "github.com/foo/bar"
        pub repo_name: String,
        /// Specific artifact to download.
        pub file_name: String,
        /// Path to downloaded artifact.
        pub path: WriteVar<PathBuf>,
        /// The Github actions run id to download artifacts from
        pub run_id: ReadVar<String>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cache::Node>();
        ctx.import::<crate::use_gh_cli::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            repo_owner,
            repo_name,
            file_name,
            path,
            run_id,
        } = request;

        let gh_cli = ctx.reqv(crate::use_gh_cli::Request::Get);

        ctx.emit_rust_step("download artifacts from github actions run", |ctx| {
            let gh_cli = gh_cli.claim(ctx);
            let run_id = run_id.claim(ctx);
            let path = path.claim(ctx);
            move |rt| {
                let gh_cli = rt.read(gh_cli);
                let run_id = rt.read(run_id);

                let path_end = format!("{repo_owner}/{repo_name}/{run_id}");
                let out_dir = std::env::current_dir()?.absolute()?.join(path_end);
                fs_err::create_dir_all(&out_dir)?;
                rt.sh.change_dir(&out_dir);

                flowey::shell_cmd!(rt, "{gh_cli} run download {run_id} -R {repo_owner}/{repo_name} --pattern {file_name}").run()?;

                if !out_dir.join(file_name).exists() {
                    anyhow::bail!("Failed to download artifact");
                }

                rt.write(path, &out_dir);
                Ok(())
            }
        });

        Ok(())
    }
}
