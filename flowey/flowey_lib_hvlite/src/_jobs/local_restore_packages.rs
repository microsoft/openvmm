// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeSet;

flowey_request! {
    pub struct Request{
        pub arches: Vec<CommonArch>,
        pub done: WriteVar<SideEffect>,
        /// If `None`, skip downloading OpenHCL IGVM release files.
        pub release_artifact: Option<ReadVar<PathBuf>>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::init_openvmm_magicpath_openhcl_sysroot::Node>();
        ctx.import::<crate::init_openvmm_magicpath_openvmm_deps::Node>();
        ctx.import::<crate::init_openvmm_magicpath_release_openhcl_igvm::resolve::Node>();
        ctx.import::<crate::init_openvmm_magicpath_protoc::Node>();
        ctx.import::<crate::init_openvmm_magicpath_uefi_mu_msvm::Node>();
        ctx.import::<crate::init_openvmm_magicpath_virtio_win::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            arches,
            done,
            release_artifact,
        } = request;

        let mut deps = vec![
            ctx.reqv(crate::init_openvmm_magicpath_protoc::Request),
            ctx.reqv(crate::init_openvmm_magicpath_virtio_win::Request),
        ];

        for arch in arches.into_iter().collect::<BTreeSet<_>>() {
            if matches!(ctx.platform(), FlowPlatform::Linux(_)) {
                deps.push(
                    ctx.reqv(|v| crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                        arch,
                        path: v,
                    })
                    .into_side_effect(),
                );
            }
            deps.extend([
                ctx.reqv(|done| crate::init_openvmm_magicpath_uefi_mu_msvm::Request { arch, done }),
                ctx.reqv(|done| crate::init_openvmm_magicpath_openvmm_deps::Request { arch, done }),
            ]);

            if let Some(release_artifact) = &release_artifact {
                deps.push(
                    ctx.reqv(
                        |v| crate::init_openvmm_magicpath_release_openhcl_igvm::resolve::Request {
                            arch,
                            release_version:
                                crate::download_release_igvm_files_from_gh::OpenhclReleaseVersion::latest(),
                            release_artifact: release_artifact.clone(),
                            done: v,
                        },
                    )
                    .into_side_effect(),
                );
            }
        }

        ctx.emit_side_effect_step(deps, [done]);

        Ok(())
    }
}
