// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local-only job that builds everything needed and runs the VMM tests

use crate::_jobs::consume_and_test_nextest_vmm_tests_archive::TestContentConfig;
use crate::build_incubator::IncubatorProfileNameOrPath;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmOutput;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipeDetailsLocalOnly;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipeType;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::build_tpm_guest_tests::TpmGuestTestsOutput;
use crate::common::CommonArch;
use crate::common::CommonPlatform;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use crate::init_vmm_tests_content_dir::VmmTestsBuiltArtifacts;
use crate::init_vmm_tests_content_dir::VmmTestsBuiltArtifactsSelections;
use crate::init_vmm_tests_content_dir::VmmTestsPreBuiltArtifactsSelections;
use crate::init_vmm_tests_env::PetriParams;
use crate::install_vmm_tests_external_deps::VmmTestsExternalDeps;
use flowey::node::prelude::*;
use petri_artifacts_vmm_test::ErasedVmmTestImage;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::num::NonZeroU64;

#[derive(Serialize, Deserialize)]
pub struct VmmTestSelections {
    /// Test filter
    pub filter: String,
    /// List of artifacts to download
    pub downloaded_artifacts: Vec<ErasedVmmTestImage>,
    /// List of artifacts to build
    pub build: VmmTestsBuiltArtifactsSelections,
    /// Prebuilt artifacts to download
    pub prebuilt_artifacts: VmmTestsPreBuiltArtifactsSelections,
    /// Prep steps variants
    pub prep_steps_variants: Vec<String>,
    /// Dependencies to install
    pub external_deps: VmmTestsExternalDeps,

    // Relative paths to artifacts used by the pipeline
    pub flowey_hvlite_path: Option<PathBuf>,

    // TODO: refactor these last two to use one artifact per arch so that
    // they can be part of `VmmTestsPreBuiltArtifactsSelections`.
    pub needs_virtio_win_drivers: bool,
    pub needs_release_igvm: bool,
}

flowey_request! {
    pub struct Params {
        pub target: CommonTriple,

        /// Toolchain platform to use when cross-compiling Windows *guest*
        /// payloads (e.g. pipette). On a non-WSL Linux build host this is
        /// [`CommonPlatform::WindowsGnu`], since the MSVC toolchain is
        /// unavailable there; otherwise it is [`CommonPlatform::WindowsMsvc`].
        pub windows_guest_platform: CommonPlatform,

        pub test_content_dir: PathBuf,

        pub selections: VmmTestSelections,

        /// Release build instead of debug build
        pub release: bool,

        /// Whether to run the tests or just build and archive
        pub build_only: bool,
        /// Copy extras to output dir (symbols, etc)
        pub copy_extras: bool,

        /// Optional: provide a custom kernel modules cpio or directory for initrd layering
        pub custom_kernel_modules: Option<PathBuf>,
        /// Optional: provide a custom kernel image to embed in IGVM (forces UEFI)
        pub custom_kernel: Option<PathBuf>,

        /// Skip the interactive VHD download prompt
        pub skip_vhd_prompt: bool,

        pub nextest_profile: crate::run_cargo_nextest_run::NextestProfile,

        pub petri_params: PetriParams,

        pub disable_secure_avic: bool,

        pub repetitions: NonZeroU64,

        /// Optional: incubator profile path. When set, tests run inside
        /// an emulated VM instead of on the host.
        pub incubator_profile: Option<IncubatorProfileNameOrPath>,

        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_guest_test_uefi::Node>();
        ctx.import::<crate::build_incubator::Node>();
        ctx.import::<crate::build_nextest_vmm_tests::Node>();
        ctx.import::<crate::build_openhcl_igvm_from_recipe::Node>();
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::build_openvmm_vhost::Node>();
        ctx.import::<crate::build_pipette::Node>();
        ctx.import::<crate::build_prep_steps::Node>();
        ctx.import::<crate::build_tmks::Node>();
        ctx.import::<crate::build_tmk_vmm::Node>();
        ctx.import::<crate::build_tpm_guest_tests::Node>();
        ctx.import::<crate::build_test_igvm_agent_rpc_server::Node>();
        ctx.import::<crate::download_openvmm_vmm_tests_artifacts::Node>();
        ctx.import::<crate::init_vmm_tests_content_dir::Node>();
        ctx.import::<crate::test_nextest_vmm_tests_archive::Node>();
        ctx.import::<crate::build_vmgstool::Node>();
        ctx.import::<crate::_jobs::build_and_publish_openhcl_igvm_from_recipe::Node>();
        ctx.import::<crate::_jobs::consume_and_test_nextest_vmm_tests_archive::Node>();
        ctx.import::<crate::build_flowey_hvlite::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            windows_guest_platform,
            test_content_dir,
            selections,
            release,
            build_only,
            copy_extras,
            custom_kernel_modules,
            custom_kernel,
            skip_vhd_prompt,
            nextest_profile,
            petri_params,
            disable_secure_avic,
            repetitions,
            incubator_profile,
            done,
        } = request;

        let test_content_dir = test_content_dir.absolute()?;
        let custom_kernel_modules_abs = custom_kernel_modules.map(|p| p.absolute()).transpose()?;
        let custom_kernel_abs = custom_kernel.map(|p| p.absolute()).transpose()?;

        let target_triple = target.as_triple();
        let arch = target.common_arch().unwrap();
        let test_label = build_test_label(&target_triple);

        // this is kind of a hack, since the artifacts will still appear in an
        // ouput directly labeled MSVC, but it allows for windows-gnu local
        // builds to continue to work without defining new windows-gnu artifact
        // variants for everything.
        let windows_guest_environment = match windows_guest_platform {
            CommonPlatform::WindowsMsvc => target_lexicon::Environment::Msvc,
            CommonPlatform::WindowsGnu => target_lexicon::Environment::Gnu,
            _ => anyhow::bail!("invalid windows guest platform"),
        };
        let is_linux_build_env = matches!(ctx.platform(), FlowPlatform::Linux(_));
        let modify_and_validate_target =
            |mut target: target_lexicon::Triple| -> target_lexicon::Triple {
                match target.operating_system {
                    target_lexicon::OperatingSystem::Windows => {
                        target.environment = windows_guest_environment;
                    }
                    target_lexicon::OperatingSystem::Linux if !is_linux_build_env => {
                        panic!(
                            "Selected tests require artifacts that can only be built on linux. Try building from WSL2.",
                        )
                    }
                    _ => {}
                }

                target
            };

        let mut copy_to_dir = Vec::new();
        let extras_dir = Path::new("extras");

        let VmmTestSelections {
            filter: nextest_filter_expr,
            downloaded_artifacts,
            build,
            prebuilt_artifacts,
            prep_steps_variants,
            external_deps,
            flowey_hvlite_path,
            needs_virtio_win_drivers,
            needs_release_igvm,
        } = selections;

        let openvmm_hcl_profile = if release {
            OpenvmmHclBuildProfile::OpenvmmHclShip
        } else {
            OpenvmmHclBuildProfile::Debug
        };
        let openhcl_extras_dir = extras_dir.join("openhcl");

        let mut build_openhcl = |recipe: OpenhclIgvmRecipe| -> ReadVar<OpenhclIgvmOutput> {
            let (igvm, openhcl_igvm) = ctx.new_var();
            let (extras, openhcl_igvm_extras) = ctx.new_var();

            let custom_recipe =
                if custom_kernel_modules_abs.is_some() || custom_kernel_abs.is_some() {
                    let mut details = recipe.recipe_details(release);
                    if custom_kernel_abs.is_some() {
                        details.with_uefi = true;
                    }
                    assert!(details.local_only.is_none());
                    details.local_only = Some(OpenhclIgvmRecipeDetailsLocalOnly {
                        openvmm_hcl_no_strip: false,
                        openhcl_initrd_extra_params: None,
                        custom_openvmm_hcl: None,
                        custom_openhcl_boot: None,
                        custom_kernel: custom_kernel_abs.clone(),
                        custom_sidecar: None,
                        custom_extra_rootfs: vec![],
                    });
                    OpenhclIgvmRecipeType::LocalOnlyCustom(details)
                } else {
                    OpenhclIgvmRecipeType::WellKnown(recipe.clone())
                };

            ctx.req(crate::build_openhcl_igvm_from_recipe::Request {
                build_profile: openvmm_hcl_profile,
                release_cfg: release,
                recipe: custom_recipe,
                custom_target: None,
                extra_features: BTreeSet::new(),
                disable_secure_avic,
                confidential_debug: true,
                openhcl_igvm,
                openhcl_igvm_extras,
            });

            if copy_extras {
                let dir = openhcl_extras_dir.join(recipe.non_production_tag());
                copy_to_dir.extend_from_slice(&[
                    (dir.clone(), extras.map(ctx, |x| Some(x.openvmm_hcl.bin))),
                    (dir.clone(), extras.map(ctx, |x| x.openvmm_hcl.dbg)),
                    (dir.clone(), extras.map(ctx, |x| Some(x.openhcl_boot.bin))),
                    (dir.clone(), extras.map(ctx, |x| Some(x.openhcl_boot.dbg))),
                    (dir.clone(), extras.map(ctx, |x| x.sidecar.map(|y| y.bin))),
                    (dir.clone(), extras.map(ctx, |x| x.sidecar.map(|y| y.dbg))),
                ]);
            } else {
                extras.claim_unused(ctx);
            }
            igvm
        };

        let openhcl_standard_x64 = build
            .openhcl_standard_x64
            .then(|| build_openhcl(OpenhclIgvmRecipe::X64));
        let openhcl_standard_aarch64 = build
            .openhcl_standard_aarch64
            .then(|| build_openhcl(OpenhclIgvmRecipe::Aarch64));
        let openhcl_standard_dev_x64 = build
            .openhcl_standard_dev_x64
            .then(|| build_openhcl(OpenhclIgvmRecipe::X64Devkern));
        let openhcl_standard_dev_aarch64 = build
            .openhcl_standard_dev_aarch64
            .then(|| build_openhcl(OpenhclIgvmRecipe::Aarch64Devkern));
        let openhcl_cvm_x64 = build
            .openhcl_cvm_x64
            .then(|| build_openhcl(OpenhclIgvmRecipe::X64Cvm));
        let openhcl_linux_direct_x64 = build
            .openhcl_linux_direct_x64
            .then(|| build_openhcl(OpenhclIgvmRecipe::X64TestLinuxDirect));

        let mut build_openvmm = |target| {
            let output = ctx.reqv(|v| crate::build_openvmm::Request {
                params: crate::build_openvmm::OpenvmmBuildParams {
                    target: CommonTriple::Custom(modify_and_validate_target(target)),
                    profile: CommonProfile::from_release(release),
                    // FIXME: this relies on openvmm default features
                    features: [].into(),
                },
                openvmm: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_openvmm::OpenvmmOutput::WindowsBin { exe: _, pdb } => pdb,
                        crate::build_openvmm::OpenvmmOutput::LinuxBin { bin: _, dbg } => dbg,
                    }),
                ));
            }
            output
        };

        let openvmm_windows_x64 = build
            .openvmm_windows_x64
            .then(|| build_openvmm(VmmTestsBuiltArtifacts::openvmm_windows_x64_target()));
        let openvmm_windows_aarch64 = build
            .openvmm_windows_aarch64
            .then(|| build_openvmm(VmmTestsBuiltArtifacts::openvmm_windows_aarch64_target()));
        let openvmm_linux_x64 = build
            .openvmm_linux_x64
            .then(|| build_openvmm(VmmTestsBuiltArtifacts::openvmm_linux_x64_target()));
        let openvmm_linux_aarch64 = build
            .openvmm_linux_aarch64
            .then(|| build_openvmm(VmmTestsBuiltArtifacts::openvmm_linux_aarch64_target()));
        let openvmm_linux_musl_x64 = build
            .openvmm_linux_musl_x64
            .then(|| build_openvmm(VmmTestsBuiltArtifacts::openvmm_linux_musl_x64_target()));
        let openvmm_linux_musl_aarch64 = build
            .openvmm_linux_musl_aarch64
            .then(|| build_openvmm(VmmTestsBuiltArtifacts::openvmm_linux_musl_aarch64_target()));

        let mut build_openvmm_vhost = |target| {
            let output = ctx.reqv(|v| crate::build_openvmm_vhost::Request {
                params: crate::build_openvmm_vhost::OpenvmmVhostBuildParams {
                    target: CommonTriple::Custom(modify_and_validate_target(target)),
                    profile: CommonProfile::from_release(release),
                },
                openvmm_vhost: v,
            });
            if copy_extras {
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| x.dbg)));
            }
            output
        };

        let openvmm_vhost_linux_x64 = build
            .openvmm_vhost_linux_x64
            .then(|| build_openvmm_vhost(VmmTestsBuiltArtifacts::openvmm_vhost_linux_x64_target()));
        let openvmm_vhost_linux_aarch64 = build.openvmm_vhost_linux_aarch64.then(|| {
            build_openvmm_vhost(VmmTestsBuiltArtifacts::openvmm_vhost_linux_aarch64_target())
        });
        let openvmm_vhost_linux_musl_x64 = build.openvmm_vhost_linux_musl_x64.then(|| {
            build_openvmm_vhost(VmmTestsBuiltArtifacts::openvmm_vhost_linux_musl_x64_target())
        });
        let openvmm_vhost_linux_musl_aarch64 = build.openvmm_vhost_linux_musl_aarch64.then(|| {
            build_openvmm_vhost(VmmTestsBuiltArtifacts::openvmm_vhost_linux_musl_aarch64_target())
        });

        let mut built_pipette = |target| {
            let output = ctx.reqv(|v| crate::build_pipette::Request {
                target: CommonTriple::Custom(modify_and_validate_target(target)),
                profile: CommonProfile::from_release(release),
                pipette: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.join(match arch {
                        CommonArch::X86_64 => "x64",
                        CommonArch::Aarch64 => "aarch64",
                    }),
                    output.map(ctx, |x| match x {
                        crate::build_pipette::PipetteOutput::WindowsBin { exe: _, pdb } => pdb,
                        crate::build_pipette::PipetteOutput::LinuxBin { bin: _, dbg } => dbg,
                    }),
                ));
            }
            output
        };

        let pipette_windows_x64 = build
            .pipette_windows_x64
            .then(|| built_pipette(VmmTestsBuiltArtifacts::pipette_windows_x64_target()));
        let pipette_windows_aarch64 = build
            .pipette_windows_aarch64
            .then(|| built_pipette(VmmTestsBuiltArtifacts::pipette_windows_aarch64_target()));
        let pipette_linux_musl_x64 = build
            .pipette_linux_musl_x64
            .then(|| built_pipette(VmmTestsBuiltArtifacts::pipette_linux_musl_x64_target()));
        let pipette_linux_musl_aarch64 = build
            .pipette_linux_musl_aarch64
            .then(|| built_pipette(VmmTestsBuiltArtifacts::pipette_linux_musl_aarch64_target()));

        let mut build_guest_test_uefi = |arch| {
            let output = ctx.reqv(|v| crate::build_guest_test_uefi::Request {
                arch,
                profile: CommonProfile::from_release(release),
                guest_test_uefi: v,
            });
            if copy_extras {
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| x.efi)));
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| x.pdb)));
            }
            output
        };

        let guest_test_uefi_x64 = build
            .guest_test_uefi_x64
            .then(|| build_guest_test_uefi(CommonArch::X86_64));
        let guest_test_uefi_aarch64 = build
            .guest_test_uefi_aarch64
            .then(|| build_guest_test_uefi(CommonArch::Aarch64));

        let mut build_tmks = |arch| {
            let output = ctx.reqv(|v| crate::build_tmks::Request {
                arch,
                profile: CommonProfile::from_release(release),
                tmks: v,
            });
            if copy_extras {
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| x.dbg)));
            }
            output
        };

        let tmks_x64 = build.tmks_x64.then(|| build_tmks(CommonArch::X86_64));
        let tmks_aarch64 = build.tmks_aarch64.then(|| build_tmks(CommonArch::Aarch64));

        let mut build_tpm_guest_tests = |target| {
            let output = ctx.reqv(|v| crate::build_tpm_guest_tests::Request {
                target: CommonTriple::Custom(modify_and_validate_target(target)),
                profile: CommonProfile::from_release(release),
                tpm_guest_tests: v,
            });

            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        TpmGuestTestsOutput::WindowsBin { pdb, .. } => pdb.clone(),
                        TpmGuestTestsOutput::LinuxBin { dbg, .. } => dbg.clone(),
                    }),
                ));
            }
            output
        };

        let tpm_guest_tests_windows_x64 = build.tpm_guest_tests_windows_x64.then(|| {
            build_tpm_guest_tests(VmmTestsBuiltArtifacts::tpm_guest_tests_windows_x64_target())
        });
        let tpm_guest_tests_linux_x64 = build.tpm_guest_tests_linux_x64.then(|| {
            build_tpm_guest_tests(VmmTestsBuiltArtifacts::tpm_guest_tests_linux_x64_target())
        });

        let mut build_test_igvm_agent_rpc_server = |target| {
            let output = ctx.reqv(|v| crate::build_test_igvm_agent_rpc_server::Request {
                target: CommonTriple::Custom(modify_and_validate_target(target)),
                profile: CommonProfile::from_release(release),
                test_igvm_agent_rpc_server: v,
            });

            if copy_extras {
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| x.pdb.clone())));
            }
            output
        };

        let test_igvm_agent_rpc_server_windows_x64 =
            build.test_igvm_agent_rpc_server_windows_x64.then(|| {
                build_test_igvm_agent_rpc_server(
                    VmmTestsBuiltArtifacts::test_igvm_agent_rpc_server_windows_x64_target(),
                )
            });

        let mut build_tmk_vmm = |target| {
            let output = ctx.reqv(|v| crate::build_tmk_vmm::Request {
                target: CommonTriple::Custom(modify_and_validate_target(target)),
                profile: CommonProfile::from_release(release),
                tmk_vmm: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_tmk_vmm::TmkVmmOutput::WindowsBin { exe: _, pdb } => pdb,
                        crate::build_tmk_vmm::TmkVmmOutput::LinuxBin { bin: _, dbg } => dbg,
                    }),
                ));
            }
            output
        };

        let tmk_vmm_windows_x64 = build
            .tmk_vmm_windows_x64
            .then(|| build_tmk_vmm(VmmTestsBuiltArtifacts::tmk_vmm_windows_x64_target()));
        let tmk_vmm_windows_aarch64 = build
            .tmk_vmm_windows_aarch64
            .then(|| build_tmk_vmm(VmmTestsBuiltArtifacts::tmk_vmm_windows_aarch64_target()));
        let tmk_vmm_linux_musl_x64 = build
            .tmk_vmm_linux_musl_x64
            .then(|| build_tmk_vmm(VmmTestsBuiltArtifacts::tmk_vmm_linux_musl_x64_target()));
        let tmk_vmm_linux_musl_aarch64 = build
            .tmk_vmm_linux_musl_aarch64
            .then(|| build_tmk_vmm(VmmTestsBuiltArtifacts::tmk_vmm_linux_musl_aarch64_target()));

        let mut build_prep_steps = |target| {
            let output = ctx.reqv(|v| crate::build_prep_steps::Request {
                target: CommonTriple::Custom(modify_and_validate_target(target)),
                profile: CommonProfile::from_release(release),
                prep_steps: v,
            });

            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_prep_steps::PrepStepsOutput::WindowsBin { exe: _, pdb } => pdb,
                        crate::build_prep_steps::PrepStepsOutput::LinuxBin { bin: _, dbg } => dbg,
                    }),
                ));
            }
            output
        };

        let prep_steps_windows_x64 = build
            .prep_steps_windows_x64
            .then(|| build_prep_steps(VmmTestsBuiltArtifacts::prep_steps_windows_x64_target()));
        let prep_steps_linux_musl_x64 = build
            .prep_steps_linux_musl_x64
            .then(|| build_prep_steps(VmmTestsBuiltArtifacts::prep_steps_linux_musl_x64_target()));

        let mut build_vmgstool = |target, with_test_helpers| {
            let output = ctx.reqv(|v| crate::build_vmgstool::Request {
                target: CommonTriple::Custom(modify_and_validate_target(target)),
                profile: CommonProfile::from_release(release),
                with_crypto: true,
                with_test_helpers,
                vmgstool: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_vmgstool::VmgstoolOutput::WindowsBin { exe: _, pdb } => pdb,
                        crate::build_vmgstool::VmgstoolOutput::LinuxBin { bin: _, dbg } => dbg,
                    }),
                ));
            }
            output
        };

        let vmgstool_windows_x64 = build
            .vmgstool_windows_x64
            .then(|| build_vmgstool(VmmTestsBuiltArtifacts::vmgstool_windows_x64_target(), false));
        let vmgstool_windows_aarch64 = build.vmgstool_windows_aarch64.then(|| {
            build_vmgstool(
                VmmTestsBuiltArtifacts::vmgstool_windows_aarch64_target(),
                false,
            )
        });
        let vmgstool_linux_x64 = build
            .vmgstool_linux_x64
            .then(|| build_vmgstool(VmmTestsBuiltArtifacts::vmgstool_linux_x64_target(), false));
        let vmgstool_dev_windows_x64 = build.vmgstool_dev_windows_x64.then(|| {
            build_vmgstool(
                VmmTestsBuiltArtifacts::vmgstool_dev_windows_x64_target(),
                true,
            )
        });
        let vmgstool_dev_windows_aarch64 = build.vmgstool_dev_windows_aarch64.then(|| {
            build_vmgstool(
                VmmTestsBuiltArtifacts::vmgstool_dev_windows_aarch64_target(),
                true,
            )
        });
        let vmgstool_dev_linux_x64 = build.vmgstool_dev_linux_x64.then(|| {
            build_vmgstool(
                VmmTestsBuiltArtifacts::vmgstool_dev_linux_x64_target(),
                true,
            )
        });

        let mut build_incubator = |target| {
            let output = ctx.reqv(|v| crate::build_incubator::Request {
                target: CommonTriple::Custom(modify_and_validate_target(target)),
                profile: if release {
                    CommonProfile::Release
                } else {
                    CommonProfile::Debug
                },
                incubator: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        let crate::build_incubator::IncubatorOutput { bin: _, dbg } = x;
                        dbg
                    }),
                ));
            }
            output
        };

        let incubator_linux_x64 = build
            .incubator_linux_x64
            .then(|| build_incubator(VmmTestsBuiltArtifacts::incubator_linux_x64_target()));

        let mut build_vmm_tests_nextest_archive = |target| {
            ctx.reqv(|v| crate::build_nextest_vmm_tests::Request {
                target,
                profile: CommonProfile::from_release(release),
                build_mode: crate::build_nextest_vmm_tests::BuildNextestVmmTestsMode::Archive(v),
            })
        };

        let nextest_vmm_tests_archive_windows_x64 =
            build.nextest_vmm_tests_archive_windows_x64.then(|| {
                build_vmm_tests_nextest_archive(
                    VmmTestsBuiltArtifacts::nextest_vmm_tests_archive_windows_x64_target(),
                )
            });
        let nextest_vmm_tests_archive_windows_aarch64 =
            build.nextest_vmm_tests_archive_windows_aarch64.then(|| {
                build_vmm_tests_nextest_archive(
                    VmmTestsBuiltArtifacts::nextest_vmm_tests_archive_windows_aarch64_target(),
                )
            });
        let nextest_vmm_tests_archive_linux_x64 =
            build.nextest_vmm_tests_archive_linux_x64.then(|| {
                build_vmm_tests_nextest_archive(
                    VmmTestsBuiltArtifacts::nextest_vmm_tests_archive_linux_x64_target(),
                )
            });
        let nextest_vmm_tests_archive_linux_musl_x64 =
            build.nextest_vmm_tests_archive_linux_musl_x64.then(|| {
                build_vmm_tests_nextest_archive(
                    VmmTestsBuiltArtifacts::nextest_vmm_tests_archive_linux_musl_x64_target(),
                )
            });
        let nextest_vmm_tests_archive_linux_musl_aarch64 =
            build.nextest_vmm_tests_archive_linux_musl_aarch64.then(|| {
                build_vmm_tests_nextest_archive(
                    VmmTestsBuiltArtifacts::nextest_vmm_tests_archive_linux_musl_aarch64_target(),
                )
            });

        let mut build_flowey_hvlite = |target| {
            let output = ctx.reqv(|v| crate::build_flowey_hvlite::Request {
                target: CommonTriple::Custom(modify_and_validate_target(target)),
                flowey_hvlite: v,
            });

            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_flowey_hvlite::FloweyHvliteOutput::WindowsBin {
                            exe: _,
                            pdb,
                        } => pdb,
                        crate::build_flowey_hvlite::FloweyHvliteOutput::LinuxBin {
                            bin: _,
                            dbg,
                        } => dbg,
                    }),
                ));
            }
            output
        };

        let flowey_hvlite_windows_x64 = build.flowey_hvlite_windows_x64.then(|| {
            build_flowey_hvlite(VmmTestsBuiltArtifacts::flowey_hvlite_windows_x64_target())
        });
        let flowey_hvlite_windows_aarch64 = build.flowey_hvlite_windows_aarch64.then(|| {
            build_flowey_hvlite(VmmTestsBuiltArtifacts::flowey_hvlite_windows_aarch64_target())
        });
        let flowey_hvlite_linux_x64 = build
            .flowey_hvlite_linux_x64
            .then(|| build_flowey_hvlite(VmmTestsBuiltArtifacts::flowey_hvlite_linux_x64_target()));

        let built_artifacts = VmmTestsBuiltArtifacts {
            flowey_hvlite_windows_x64,
            flowey_hvlite_windows_aarch64,
            flowey_hvlite_linux_x64,
            nextest_vmm_tests_archive_windows_x64,
            nextest_vmm_tests_archive_windows_aarch64,
            nextest_vmm_tests_archive_linux_x64,
            nextest_vmm_tests_archive_linux_musl_x64,
            nextest_vmm_tests_archive_linux_musl_aarch64,
            incubator_linux_x64,
            prep_steps_windows_x64,
            prep_steps_linux_musl_x64,
            test_igvm_agent_rpc_server_windows_x64,
            openvmm_windows_x64,
            openvmm_windows_aarch64,
            openvmm_linux_x64,
            openvmm_linux_aarch64,
            openvmm_linux_musl_x64,
            openvmm_linux_musl_aarch64,
            openvmm_vhost_linux_x64,
            openvmm_vhost_linux_aarch64,
            openvmm_vhost_linux_musl_x64,
            openvmm_vhost_linux_musl_aarch64,
            pipette_windows_x64,
            pipette_windows_aarch64,
            pipette_linux_musl_x64,
            pipette_linux_musl_aarch64,
            guest_test_uefi_x64,
            guest_test_uefi_aarch64,
            openhcl_standard_x64,
            openhcl_standard_aarch64,
            openhcl_standard_dev_x64,
            openhcl_standard_dev_aarch64,
            openhcl_cvm_x64,
            openhcl_linux_direct_x64,
            tmks_x64,
            tmks_aarch64,
            tmk_vmm_windows_x64,
            tmk_vmm_windows_aarch64,
            tmk_vmm_linux_musl_x64,
            tmk_vmm_linux_musl_aarch64,
            vmgstool_windows_x64,
            vmgstool_windows_aarch64,
            vmgstool_linux_x64,
            vmgstool_dev_windows_x64,
            vmgstool_dev_windows_aarch64,
            vmgstool_dev_linux_x64,
            tpm_guest_tests_windows_x64,
            tpm_guest_tests_linux_x64,
        };

        let mut side_effects = Vec::new();

        if !copy_to_dir.is_empty() {
            side_effects.push(ctx.emit_rust_step(
                "copy additional files to test content dir",
                |ctx| {
                    let copy_to_dir = copy_to_dir
                        .into_iter()
                        .map(|(dst, src)| (dst, src.claim(ctx)))
                        .collect::<Vec<_>>();
                    let test_content_dir = test_content_dir.clone();

                    move |rt| {
                        for (dst, src) in copy_to_dir {
                            let src = rt.read(src);

                            if let Some(src) = src {
                                // TODO: specify files names for everything
                                let dst = if dst.starts_with("extras") {
                                    test_content_dir
                                        .join(dst)
                                        .join(src.file_name().context("no file name")?)
                                } else {
                                    test_content_dir.join(dst)
                                };

                                fs_err::create_dir_all(dst.parent().context("no parent")?)?;
                                fs_err::copy(src, dst)?;
                            }
                        }

                        Ok(())
                    }
                },
            ));
        }

        if build_only {
            let initialized = ctx.reqv(|v| crate::init_vmm_tests_content_dir::Request {
                test_content_dir: ReadVar::from_static(test_content_dir.clone()),
                vmm_tests_target: target_triple.clone(),
                built_artifacts,
                prebuilt_artifacts,
                is_repo_root: true,
                needs_incubator_profiles: incubator_profile.is_some(),
                needs_virtio_win_drivers,
                needs_release_igvm,
                done: v,
            });

            side_effects.push(initialized.clone());

            side_effects.push(ctx.emit_rust_step("write script", |ctx| {
                // place this job at the end so the log is visible for convenience
                initialized.claim(ctx);
                move |rt| {
                    let flowey_hvlite_path = flowey_hvlite_path
                        .context("flowey_hvlite must exist in build_only mode")?;
                    let flowey_hvlite_arg =
                        flowey_hvlite_path.to_str().context("path not unicode")?;
                    let (script_name, dir, flowey_hvlite_bin) = match target_triple.operating_system
                    {
                        target_lexicon::OperatingSystem::Windows => (
                            "run.ps1",
                            "$PSScriptRoot",
                            format!(".\\{}", flowey_hvlite_arg),
                        ),
                        _ => (
                            "run.sh",
                            "\"$(dirname \"${BASH_SOURCE[0]}\")\"",
                            format!("./{}", flowey_hvlite_arg),
                        ),
                    };

                    let target_cli = match target {
                        CommonTriple::AARCH64_WINDOWS_MSVC => "windows-aarch64",
                        CommonTriple::X86_64_WINDOWS_MSVC => "windows-x64",
                        CommonTriple::X86_64_LINUX_GNU => "linux-x64",
                        CommonTriple::AARCH64_LINUX_MUSL => "linux-aarch64-musl",
                        _ => unreachable!(),
                    };

                    let mut run_target_args: Vec<OsString> = vec![
                        "cd".into(),
                        dir.into(),
                        ";".into(),
                        flowey_hvlite_bin.into(),
                        "pipeline".into(),
                        "run".into(),
                        "vmm-tests-run-target".into(),
                        "--target".into(),
                        target_cli.into(),
                        "--dir".into(),
                        ".".into(),
                        "--filter".into(),
                        format!("'{nextest_filter_expr}'").into(),
                        "--repetitions".into(),
                        repetitions.get().to_string().into(),
                    ];

                    if !downloaded_artifacts.is_empty() {
                        run_target_args.push("--artifacts".into());
                        run_target_args.push(
                            downloaded_artifacts
                                .iter()
                                .map(|a| a.name())
                                .collect::<Vec<_>>()
                                .join(",")
                                .into(),
                        );
                    }

                    if !prep_steps_variants.is_empty() {
                        run_target_args.push("--prep-steps".into());
                        run_target_args.push(prep_steps_variants.join(",").into());
                    }

                    if skip_vhd_prompt {
                        run_target_args.push("--skip-vhd-prompt".into());
                    }

                    if matches!(
                        nextest_profile,
                        crate::run_cargo_nextest_run::NextestProfile::Ci
                    ) {
                        run_target_args.push("--ci-profile".into());
                    }

                    if !petri_params.reuse_prepped_vhds {
                        run_target_args.push("--no-reuse-prepped-vhds".into());
                    }

                    if matches!(
                        external_deps,
                        VmmTestsExternalDeps::Windows(ref deps) if deps.hardware_isolation
                    ) {
                        run_target_args.push("--needs-hardware-isolation".into());
                    }

                    if build.test_igvm_agent_rpc_server_windows_x64 {
                        run_target_args.push("--needs-igvm-agent".into());
                    }

                    if let Some(profile) = &incubator_profile {
                        run_target_args.push("--incubator".into());
                        run_target_args.push(profile.to_string().into());
                    }

                    let dst = test_content_dir.join(script_name);

                    fs_err::write(
                        &dst,
                        run_target_args.join(OsStr::new(" ")).as_encoded_bytes(),
                    )?;
                    dst.make_executable()?;

                    match target_triple.operating_system {
                        target_lexicon::OperatingSystem::Windows => {
                            let dst = if flowey_lib_common::_util::running_in_wsl(rt) {
                                flowey_lib_common::_util::wslpath::linux_to_win(rt, dst)
                                    .to_string_lossy()
                                    .replace("\\", "\\\\")
                            } else {
                                dst.to_string_lossy().to_string()
                            };
                            log::info!("Run the vmm tests with: powershell.exe {dst}");
                        }
                        _ => {
                            log::info!("Run the vmm tests with: {}", dst.display());
                        }
                    }

                    Ok(())
                }
            }));
        } else {
            init_artifacts_dir(ctx, &test_content_dir, skip_vhd_prompt)?;

            let test_content_config = TestContentConfig::Uninitialized {
                test_content_dir: Some(ReadVar::from_static(test_content_dir)),
                built_artifacts,
                prebuilt_artifacts,
                needs_virtio_win_drivers,
                needs_release_igvm,
            };

            side_effects.push(ctx.reqv(|v| {
                crate::_jobs::consume_and_test_nextest_vmm_tests_archive::Params {
                    junit_test_label: test_label,
                    target: target_triple,
                    nextest_profile,
                    nextest_filter_expr: Some(nextest_filter_expr),
                    test_content_config,
                    downloaded_artifacts,
                    prep_steps_variants,
                    external_deps,
                    incubator_profile,
                    upload_logs_on_success: true,
                    fail_job_on_test_fail: true,
                    repetitions,
                    petri_params,
                    test_content_dir_as_repo_root: true,
                    done: v,
                }
            }));
        }

        ctx.emit_side_effect_step(side_effects, [done]);

        Ok(())
    }
}

pub(crate) fn build_test_label(target: &target_lexicon::Triple) -> String {
    let arch = CommonArch::from_triple(target).unwrap();
    let arch_tag = match arch {
        CommonArch::X86_64 => "x64",
        CommonArch::Aarch64 => "aarch64",
    };
    let platform_tag = match target.operating_system {
        target_lexicon::OperatingSystem::Windows => "windows",
        target_lexicon::OperatingSystem::Linux => "linux",
        _ => unreachable!(),
    };
    format!("{arch_tag}-{platform_tag}-vmm-tests")
}

pub(crate) fn init_artifacts_dir(
    ctx: &mut NodeCtx<'_>,
    test_content_dir: &Path,
    skip_vhd_prompt: bool,
) -> anyhow::Result<()> {
    let vmm_test_artifacts_dir = test_content_dir.join("images");
    ctx.config(crate::download_openvmm_vmm_tests_artifacts::Config {
        custom_cache_dir: Some(vmm_test_artifacts_dir.clone()),
        skip_prompt: Some(skip_vhd_prompt),
        ..Default::default()
    });
    Ok(())
}
