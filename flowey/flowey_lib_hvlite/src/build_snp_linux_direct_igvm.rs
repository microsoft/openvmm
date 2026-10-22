// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the x86_64 SNP Linux-direct test IGVM.

use crate::build_igvmfilegen::IgvmfilegenBuildParams;
use crate::build_igvmfilegen::IgvmfilegenOutput;
use crate::common::CommonArch;
use crate::common::CommonPlatform;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use crate::resolve_openvmm_test_linux_kernel::OpenvmmTestKernelFile;
use crate::resolve_openvmm_test_linux_kernel::SNP_GUEST_LINUX_TEST_KERNEL_VERSION;
use crate::run_cargo_build::BuildProfile;
use anyhow::Context;
use flowey::node::prelude::*;
use igvmfilegen_config::ResourceType;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct SnpLinuxDirectIgvmOutput {
    #[serde(rename = "snp-linux-direct.bin")]
    pub igvm_bin: PathBuf,
    #[serde(
        rename = "snp-linux-direct.bin.map",
        skip_serializing_if = "Option::is_none"
    )]
    pub igvm_map: Option<PathBuf>,
}

impl Artifact for SnpLinuxDirectIgvmOutput {}

flowey_request! {
    pub struct Request {
        pub snp_linux_direct_igvm: WriteVar<SnpLinuxDirectIgvmOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_igvmfilegen::Node>();
        ctx.import::<crate::build_pipette::Node>();
        ctx.import::<crate::build_snp_bootshim::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::resolve_openvmm_test_linux_kernel::Node>();
        ctx.import::<crate::resolve_openvmm_test_initrd::Node>();
        ctx.import::<crate::run_igvmfilegen::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        if !matches!(ctx.platform(), FlowPlatform::Linux(_)) {
            anyhow::bail!("SNP Linux-direct IGVM builds require a Linux host");
        }

        let host_arch: CommonArch = ctx.arch().try_into()?;
        let igvmfilegen = ctx.reqv(|v| crate::build_igvmfilegen::Request {
            build_params: IgvmfilegenBuildParams {
                target: CommonTriple::Common {
                    arch: host_arch,
                    platform: CommonPlatform::LinuxGnu,
                },
                profile: BuildProfile::Light,
            },
            igvmfilegen: v,
        });
        let igvmfilegen = igvmfilegen.map(ctx, |o| match o {
            IgvmfilegenOutput::LinuxBin { bin, .. } => bin,
            IgvmfilegenOutput::WindowsBin { .. } => unreachable!("Linux host build"),
        });
        let bootshim = ctx.reqv(|v| crate::build_snp_bootshim::Request { snp_bootshim: v });
        let kernel = ctx.reqv(|v| {
            crate::resolve_openvmm_test_linux_kernel::Request::Get(
                OpenvmmTestKernelFile::BzImage,
                CommonArch::X86_64,
                SNP_GUEST_LINUX_TEST_KERNEL_VERSION,
                v,
            )
        });
        let initrd =
            ctx.reqv(|v| crate::resolve_openvmm_test_initrd::Request::Get(CommonArch::X86_64, v));
        let pipette = ctx.reqv(|v| crate::build_pipette::Request {
            target: CommonTriple::X86_64_LINUX_MUSL,
            profile: CommonProfile::Release,
            pipette: v,
        });
        let initrd = ctx.emit_rust_stepv("embed pipette in SNP test initrd", |ctx| {
            claim_vars!(ctx, (initrd, pipette));
            move |rt| {
                let initrd = rt.read(initrd);
                let pipette = match rt.read(pipette) {
                    crate::build_pipette::PipetteOutput::LinuxBin { bin, .. } => bin,
                    crate::build_pipette::PipetteOutput::WindowsBin { .. } => {
                        anyhow::bail!("SNP guest requires a Linux pipette")
                    }
                };
                let compressed = initrd_cpio::inject_into_initrd(
                    &std::fs::read(&initrd).context("reading SNP test initrd")?,
                    "pipette",
                    &std::fs::read(&pipette).context("reading Linux pipette")?,
                    0o100755,
                )
                .context("embedding pipette in SNP test initrd")?;
                let path = rt.sh.current_dir().join("snp-linux-direct-initrd.cpio.gz");
                std::fs::write(&path, compressed).context("writing SNP test initrd")?;
                Ok(path)
            }
        });
        let repo = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let manifest = repo.map(ctx, |p| {
            p.join("vm/loader/manifests/snp-linux-direct-pipette.json")
        });

        let resources = ctx.emit_minor_rust_stepv("enumerate SNP IGVM resources", |ctx| {
            claim_vars!(ctx, (kernel, initrd, bootshim));
            |rt| {
                BTreeMap::from([
                    (ResourceType::LinuxKernel, rt.read(kernel)),
                    (ResourceType::LinuxInitrd, rt.read(initrd)),
                    (ResourceType::SnpBootshim, rt.read(bootshim).bin),
                ])
            }
        });
        let igvm = ctx.reqv(|v| crate::run_igvmfilegen::Request {
            igvmfilegen,
            manifest,
            resources,
            disable_secure_avic: false,
            confidential_debug: false,
            add_temp_snp_id_block: false,
            igvm: v,
        });
        igvm.write_into_with(ctx, request.snp_linux_direct_igvm, |o| {
            SnpLinuxDirectIgvmOutput {
                igvm_bin: o.igvm_bin,
                igvm_map: o.igvm_map,
            }
        });

        Ok(())
    }
}
