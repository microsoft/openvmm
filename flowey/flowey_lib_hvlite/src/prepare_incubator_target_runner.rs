// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Prepare an incubator-backed cargo-nextest target runner.

use crate::build_pipette::PipetteOutput;
use crate::common::CommonPlatform;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// What target VMM tests are compiled for.
        pub target: CommonTriple,
        /// Build profile for the bootstrap incubator and pipette binaries.
        pub profile: CommonProfile,
        /// Path to the incubator profile TOML file.
        pub profile_path: ReadVar<PathBuf>,
        /// Path to the OpenVMM repo root. Must contain any repo-relative paths
        /// referenced by the runner's environment so they fall under the computed
        /// incubator share root and translate correctly into the guest.
        pub repo_root: ReadVar<PathBuf>,
        /// Directory containing VMM test runtime artifacts and test outputs.
        pub test_content_dir: ReadVar<PathBuf>,
        /// Completion indicator.
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_incubator::Node>();
        ctx.import::<crate::build_pipette::Node>();
        ctx.import::<crate::resolve_openvmm_qemu::Node>();
        ctx.import::<crate::resolve_openvmm_test_initrd::Node>();
        ctx.import::<crate::resolve_openvmm_test_linux_kernel::Node>();
        ctx.import::<crate::write_incubator_target_runner::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            profile,
            profile_path,
            repo_root,
            test_content_dir,
            done,
        } = request;

        let host_arch = ctx.arch().try_into()?;
        if !matches!(ctx.platform(), FlowPlatform::Linux(_)) {
            anyhow::bail!("incubator target-runner preparation is only supported on Linux hosts");
        }

        let guest_arch =
            crate::common::CommonArch::from_architecture(target.as_triple().architecture)?;

        let incubator = ctx.reqv(|v| crate::build_incubator::Request {
            target: CommonTriple::Common {
                arch: host_arch,
                platform: CommonPlatform::LinuxGnu,
            },
            profile,
            incubator: v,
        });
        let incubator_bin = incubator.map(ctx, |o| o.bin);

        let pipette = ctx.reqv(|v| crate::build_pipette::Request {
            target,
            profile,
            pipette: v,
        });
        let pipette_bin = pipette.map(ctx, |o| match o {
            PipetteOutput::LinuxBin { bin, .. } => bin,
            PipetteOutput::WindowsBin { exe, .. } => exe,
        });

        let qemu_binary = ctx.reqv(|v| {
            crate::resolve_openvmm_qemu::Request::Get(
                crate::resolve_openvmm_qemu::QemuFile::SystemAarch64,
                host_arch,
                v,
            )
        });
        let kernel = ctx.reqv(|v| {
            crate::resolve_openvmm_test_linux_kernel::Request::Get(
                crate::resolve_openvmm_test_linux_kernel::OpenvmmTestKernelFile::Kernel,
                guest_arch,
                crate::resolve_openvmm_test_linux_kernel::DEFAULT_LINUX_TEST_KERNEL_VERSION,
                v,
            )
        });
        let initrd = ctx.reqv(|v| crate::resolve_openvmm_test_initrd::Request::Get(guest_arch, v));

        let target_runner = ctx.reqv(|v| crate::write_incubator_target_runner::Request {
            incubator_bin,
            profile_path,
            kernel: Some(kernel),
            initrd: Some(initrd),
            repo_root,
            test_content_dir,
            extra_share_paths: Vec::new(),
            extra_env: None,
            pipette_bin: Some(pipette_bin),
            copy_incubator_bin: true,
            qemu_binary: Some(qemu_binary),
            runner_info: v,
        });

        ctx.emit_rust_step("incubator target runner ready", |ctx| {
            target_runner.claim(ctx);
            done.claim(ctx);
            |_| Ok(())
        });

        Ok(())
    }
}
