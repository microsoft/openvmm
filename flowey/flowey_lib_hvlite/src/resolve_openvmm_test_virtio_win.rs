// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download the `openvmm-test-virtio-win` archive from the `openvmm-deps`
//! GitHub release, or use a local path if specified.
//!
//! The archive contains extracted virtio-win driver files (NetKVM, etc.)
//! and is architecture-independent — a single tarball covers all Windows
//! target OS versions and architectures.

use flowey::node::prelude::*;

flowey_config! {
    /// Config for the resolve_openvmm_test_virtio_win node.
    pub struct Config {
        /// Specify version of the github release to pull from
        pub version: Option<String>,
        /// Use a locally available virtio-win driver directory
        pub local_path: Option<ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        /// Get the path to the extracted virtio-win driver directory
        Get(WriteVar<PathBuf>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::download_gh_release::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let Config {
            version,
            local_path,
        } = config;
        let mut out_vars: Vec<WriteVar<PathBuf>> = Vec::new();

        for req in requests {
            match req {
                Request::Get(var) => out_vars.push(var),
            }
        }

        if version.is_some() && local_path.is_some() {
            anyhow::bail!("Cannot specify both Version and LocalPath");
        }

        if version.is_none() && local_path.is_none() {
            anyhow::bail!("Must specify a Version or LocalPath");
        }

        if out_vars.is_empty() {
            return Ok(());
        }

        if let Some(local_path) = local_path {
            ctx.emit_rust_step("use local virtio-win drivers", |ctx| {
                let out_vars = out_vars.claim(ctx);
                let local_path = local_path.claim(ctx);
                move |rt| {
                    let path = rt.read(local_path);
                    rt.write_all(out_vars, &path);
                    Ok(())
                }
            });
            return Ok(());
        }

        let version = version.expect("local path handled above");
        let archive = ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
            repo_owner: "microsoft".into(),
            repo_name: "openvmm-deps".into(),
            needs_auth: false,
            tag: version.clone(),
            file_name: format!("openvmm-test-virtio-win.{version}.tar.gz"),
            path: v,
        });

        let persistent_dir = ctx.persistent_dir();

        ctx.emit_rust_step("unpack openvmm-test-virtio-win archive", |ctx| {
            let persistent_dir = persistent_dir.claim(ctx);
            let archive = archive.claim(ctx);
            let out_vars = out_vars.claim(ctx);
            let version = version.clone();
            move |rt| {
                let persistent_dir = persistent_dir.map(|d| rt.read(d));
                let file = rt.read(archive);
                let dir = flowey_lib_common::_util::extract::extract_tar_gz_if_new(
                    rt,
                    persistent_dir.as_deref(),
                    &file,
                    &version,
                )?;
                rt.write_all(out_vars, &dir);
                Ok(())
            }
        });

        Ok(())
    }
}
