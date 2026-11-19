// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Globally install a set of dependencies required to build Rust code in the
//! OpenVMM repo.
//!
//! Notably - this node installs both the required Rust toolchain, as well as a
//! protobuf compiler (which is transitively required by most OpenVMM crates).

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request(pub WriteVar<SideEffect>);
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::init_openvmm_magicpath_protoc::Node>();
        ctx.import::<crate::init_openvmm_cargo_config_deny_warnings::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        if requests.is_empty() {
            return Ok(());
        }

        let mut side_effects = vec![
            ctx.reqv(crate::init_openvmm_cargo_config_deny_warnings::Request::Done),
            ctx.reqv(crate::init_openvmm_magicpath_protoc::Request),
            ctx.reqv(flowey_lib_common::install_rust::Request::EnsureInstalled),
        ];

        let packages = match ctx.platform() {
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu) => {
                vec!["build-essential", "cmake"]
            }
            FlowPlatform::Linux(FlowPlatformLinuxDistro::OpenSuse) => {
                vec!["gcc", "gcc-c++", "glibc-devel", "make", "cmake"]
            }
            _ => Vec::new(),
        };
        if !packages.is_empty() {
            side_effects.push(ctx.reqv(|v| {
                flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: packages.into_iter().map(Into::into).collect(),
                    done: v,
                }
            }));
        }

        ctx.emit_side_effect_step(side_effects, requests.into_iter().map(|x| x.0));

        Ok(())
    }
}
