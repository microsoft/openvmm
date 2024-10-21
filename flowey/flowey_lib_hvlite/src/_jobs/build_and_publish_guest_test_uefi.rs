// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and publish the guest_test_uefi artifact

use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonProfile;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub arch: CommonArch,
        pub profile: CommonProfile,

        pub artifact_dir: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::artifact_guest_test_uefi::publish::Node>();
        ctx.import::<crate::build_guest_test_uefi::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            arch,
            profile,
            artifact_dir,
            done,
        } = request;

        let guest_test_uefi = ctx.reqv(|v| crate::build_guest_test_uefi::Request {
            arch,
            profile,
            guest_test_uefi: v,
        });

        ctx.req(crate::artifact_guest_test_uefi::publish::Request {
            guest_test_uefi,
            artifact_dir,
            done,
        });

        Ok(())
    }
}
