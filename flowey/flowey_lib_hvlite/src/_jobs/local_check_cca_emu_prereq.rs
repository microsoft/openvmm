// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! To install CCA emulation environment, we need a few tools. This job checks
//! their existence.
use flowey::node::prelude::RustRuntimeServices;
use flowey::node::prelude::*;
use std::fs;

flowey_request! {
    pub struct Params {
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

fn is_distro_package_installed(rt: &RustRuntimeServices<'_>, pkg: &str) -> bool {
    match flowey::shell_cmd!(rt, "dpkg -s {pkg}").output() {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params { done } = request;

        ctx.emit_rust_step("check prerequisite of arm64 emulation environment", |ctx| {
            done.claim(ctx);
            move |rt| {
                // Check if required packages are installed
                let required_packages = vec![
                    "netcat-openbsd",
                    "python3",
                    "python3-pip",
                    "telnet",
                    "docker.io",
                    "gcc-aarch64-linux-gnu",
                    "flex",  // flex and bison are needed when building linux kernel kconfig parser
                    "bison",
                    "libssl-dev",
                    "python3-venv",
                    "python3-pip",
                ];

                let mut missing_packages = Vec::new();
                for pkg in required_packages {
                    if !is_distro_package_installed(rt, pkg) {
                        missing_packages.push(pkg);
                    }
                }

                if !missing_packages.is_empty() {
                    eprintln!("The following required packages are NOT installed:\n");

                    for pkg in &missing_packages {
                        eprintln!("  - {}", pkg);
                    }

                    eprintln!("\nPlease install them using:");
                    eprintln!("  sudo apt update && sudo apt install -y {}\n", missing_packages.join(" "));
                    anyhow::bail!("Stopped emulator installation due to missing packages");
                }

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
