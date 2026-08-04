// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared setup for running a nextest archive through the incubator target runner.

use crate::common::CommonArch;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

pub enum IncubatorSource {
    Build {
        profile_path: PathBuf,
    },
    Prebuilt {
        bin: ReadVar<PathBuf>,
        profile_path: ReadVar<PathBuf>,
    },
}

pub enum PipetteSource {
    Build,
    Prebuilt(ReadVar<crate::build_pipette::PipetteOutput>),
}

pub struct Params {
    pub target: CommonTriple,
    pub profile: CommonProfile,
    pub incubator: IncubatorSource,
    pub pipette: PipetteSource,
    pub repo_root: ReadVar<PathBuf>,
    pub test_content_dir: ReadVar<PathBuf>,
    pub nextest_archive_file: ReadVar<PathBuf>,
    pub nextest_config_file: ReadVar<PathBuf>,
    pub extra_env: ReadVar<BTreeMap<String, String>>,
}

pub fn imports(ctx: &mut ImportCtx<'_>) {
    ctx.import::<crate::build_incubator::Node>();
    ctx.import::<crate::build_pipette::Node>();
    ctx.import::<crate::resolve_openvmm_qemu::Node>();
    ctx.import::<crate::resolve_openvmm_test_initrd::Node>();
    ctx.import::<crate::resolve_openvmm_test_linux_kernel::Node>();
    ctx.import::<crate::write_incubator_target_runner::Node>();
}

pub fn configure(
    ctx: &mut NodeCtx<'_>,
    params: Params,
) -> anyhow::Result<ReadVar<BTreeMap<String, String>>> {
    let Params {
        target,
        profile,
        incubator,
        pipette,
        repo_root,
        test_content_dir,
        nextest_archive_file,
        nextest_config_file,
        extra_env,
    } = params;

    let host_arch = match ctx.arch() {
        FlowArch::X86_64 => CommonArch::X86_64,
        FlowArch::Aarch64 => CommonArch::Aarch64,
        other => anyhow::bail!("unsupported host architecture for incubator: {other:?}"),
    };
    let incubator_target = CommonTriple::Common {
        arch: host_arch,
        platform: crate::common::CommonPlatform::LinuxGnu,
    };
    let (incubator_bin, profile_path) = match incubator {
        IncubatorSource::Build { profile_path } => {
            let profile_path = profile_path
                .absolute()
                .context("failed to resolve incubator profile path")?;
            let incubator_bin = ctx
                .reqv(|v| crate::build_incubator::Request {
                    target: incubator_target,
                    profile,
                    incubator: v,
                })
                .map(ctx, |output| output.bin);
            (incubator_bin, ReadVar::from_static(profile_path))
        }
        IncubatorSource::Prebuilt { bin, profile_path } => (bin, profile_path),
    };

    let target_triple = target.as_triple();
    let guest_arch = target.common_arch()?;
    let pipette = match pipette {
        PipetteSource::Build => ctx.reqv(|pipette| crate::build_pipette::Request {
            target: CommonTriple::Common {
                arch: guest_arch,
                platform: crate::common::CommonPlatform::LinuxMusl,
            },
            profile,
            pipette,
        }),
        PipetteSource::Prebuilt(pipette) => pipette,
    };
    let pipette_staged = ctx.emit_rust_step("stage incubator pipette", |ctx| {
        let pipette = pipette.claim(ctx);
        let test_content_dir = test_content_dir.clone().claim(ctx);
        move |rt| {
            let crate::build_pipette::PipetteOutput::LinuxBin { bin, .. } = rt.read(pipette) else {
                unreachable!()
            };
            let test_content_dir = rt.read(test_content_dir);
            fs_err::create_dir_all(&test_content_dir)?;
            fs_err::copy(bin, test_content_dir.join("pipette"))?;
            Ok(())
        }
    });
    let kernel = ctx.reqv(|v| {
        crate::resolve_openvmm_test_linux_kernel::Request::Get(
            crate::resolve_openvmm_test_linux_kernel::OpenvmmTestKernelFile::Kernel,
            guest_arch,
            crate::resolve_openvmm_test_linux_kernel::INCUBATOR_LINUX_TEST_KERNEL_VERSION,
            v,
        )
    });
    let initrd = ctx.reqv(|v| crate::resolve_openvmm_test_initrd::Request::Get(guest_arch, v));
    let qemu_binary = ctx.reqv(|v| {
        crate::resolve_openvmm_qemu::Request::Get(
            crate::resolve_openvmm_qemu::QemuFile::SystemAarch64,
            host_arch,
            v,
        )
    });

    let nextest_env = ctx.reqv(|v| crate::write_incubator_target_runner::Request {
        incubator_bin,
        profile_path,
        kernel: Some(kernel),
        initrd: Some(initrd),
        repo_root,
        test_content_dir,
        extra_share_paths: vec![nextest_archive_file, nextest_config_file],
        extra_env: Some(extra_env),
        qemu_binary: Some(qemu_binary),
        target: target_triple,
        nextest_env: v,
    });
    Ok(nextest_env.depending_on(ctx, &pipette_staged))
}
