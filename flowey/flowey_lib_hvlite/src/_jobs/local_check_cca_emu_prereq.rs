// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! To install CCA emulation environment, we need a few tools. This job checks
//! their existence.
use flowey::node::prelude::*;
use std::fs;

flowey_request! {
    pub struct Params {
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params { done } = request;

        let required_packages_installed = ctx.reqv(|v| {
            flowey_lib_common::install_dist_pkg::Request::Install {
                package_names: vec![
                    "netcat-openbsd".into(),
                    "python3".into(),
                    "python3-pip".into(),
                    "telnet".into(),
                    "docker.io".into(),
                    "gcc-aarch64-linux-gnu".into(),
                    // flex and bison are needed when building linux kernel kconfig parser
                    "flex".into(),
                    "bison".into(),
                    "libssl-dev".into(),
                    "python3-venv".into(),
                ],
                done: v,
            }
        });

        ctx.emit_rust_step("check prerequisite of arm64 emulation environment", |ctx| {
            done.claim(ctx);
            required_packages_installed.claim(ctx);
            move |rt| {
                // Check if docker is setup
                let group_name = "docker";
                let group_file = fs::read_to_string("/etc/group").expect("Failed to read /etc/group");
                let docker_group = group_file
                    .lines()
                    .find(|line| line.starts_with(&format!("{group_name}:")));

                if docker_group.is_none() {
                    anyhow::bail!("Group '{group_name}' does not exist, please add it using 'sudo groupadd docker'");
                }

                // Check if current user is in the group
                let output = flowey::shell_cmd!(rt, "id -nG").output()?;
                let output = String::from_utf8(output.stdout)?;
                let is_member = output.split_whitespace().any(|g| g == group_name);
                if !is_member {
                    anyhow::bail!("Current user does NOT belong to the '{group_name}' group, please add it using 'sudo usermod -aG docker $USER', and restart the shell!");
                }

                Ok(())
            }
        });

        Ok(())
    }
}
