// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and run the cargo-nextest based VMM tests.

use crate::build_nextest_vmm_tests::BuildNextestVmmTestsMode;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use vmm_test_images::KnownIso;
use vmm_test_images::KnownVhd;

flowey_request! {
    pub struct Params {
        /// Friendly label for report JUnit test results
        pub junit_test_label: String,
        /// Build and run VMM tests for the specified target
        pub target: target_lexicon::Triple,
        /// Build and run VMM tests with the specified cargo profile
        pub profile: CommonProfile,
        /// Nextest profile to use when running the source code
        pub nextest_profile: NextestProfile,
        /// Nextest test filter expression.
        pub nextest_filter_expr: Option<String>,
        /// If the VMM tests requires openhcl - specify a custom target for it.
        pub openhcl_custom_target: Option<CommonTriple>,
        /// VHDs to download
        pub vhds: Vec<KnownVhd>,
        /// ISOs to download
        pub isos: Vec<KnownIso>,

        /// Whether the job should fail if any test has failed
        pub fail_job_on_test_fail: bool,
        /// If provided, also publish junit.xml test results as an artifact.
        pub artifact_dir: Option<ReadVar<PathBuf>>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_guest_test_uefi::Node>();
        ctx.import::<crate::build_igvmfilegen::Node>();
        ctx.import::<crate::build_nextest_vmm_tests::Node>();
        ctx.import::<crate::build_openhcl_igvm_from_recipe::Node>();
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::build_pipette::Node>();
        ctx.import::<crate::download_openvmm_vmm_tests_vhds::Node>();
        ctx.import::<crate::init_vmm_tests_env::Node>();
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            junit_test_label,
            target,
            profile,
            nextest_profile,
            nextest_filter_expr,
            openhcl_custom_target,
            vhds,
            isos,
            fail_job_on_test_fail,
            artifact_dir,
            done,
        } = request;

        let arch = match target.architecture {
            target_lexicon::Architecture::X86_64 => CommonArch::X86_64,
            target_lexicon::Architecture::Aarch64(_) => CommonArch::Aarch64,
            arch => anyhow::bail!("unsupported arch {arch}"),
        };

        struct TestTargets<'a> {
            windows: CommonTriple,
            linux: CommonTriple,
            openhcl_recipies: &'a [OpenhclIgvmRecipe],
        }

        let targets = match arch {
            CommonArch::X86_64 => TestTargets {
                windows: CommonTriple::X86_64_WINDOWS_MSVC,
                linux: CommonTriple::X86_64_LINUX_MUSL,
                openhcl_recipies: &[
                    OpenhclIgvmRecipe::X64,
                    OpenhclIgvmRecipe::X64Devkern,
                    OpenhclIgvmRecipe::X64TestLinuxDirect,
                    OpenhclIgvmRecipe::X64Cvm,
                ],
            },
            CommonArch::Aarch64 => TestTargets {
                windows: CommonTriple::AARCH64_WINDOWS_MSVC,
                linux: CommonTriple::AARCH64_LINUX_MUSL,
                openhcl_recipies: &[OpenhclIgvmRecipe::Aarch64],
            },
        };

        // FUTURE: we can be smarter with the feature-set openvmm gets built
        // with depending on what tests are being run.
        //
        // e.g: would be nice to avoid the comptime hit of using `blob_disk` if
        // it's not necessary.
        let register_openvmm = ctx.reqv(|v| {
            crate::build_openvmm::Request {
                params: crate::build_openvmm::OpenvmmBuildParams {
                    profile,
                    target: CommonTriple::Custom(target.clone()),
                    features: {
                        // TPM tests only run on linux at the moment
                        if matches!(
                            (target.operating_system, target.environment),
                            (
                                target_lexicon::OperatingSystem::Linux,
                                target_lexicon::Environment::Gnu
                            )
                        ) {
                            [crate::build_openvmm::OpenvmmFeature::Tpm].into()
                        } else {
                            [].into()
                        }
                    },
                },
                openvmm: v,
            }
        });

        let mut register_openhcl_igvm_files = Vec::new();
        for recipe in targets.openhcl_recipies {
            let (_read_built_openvmm_hcl, built_openvmm_hcl) = ctx.new_var();
            let (read_built_openhcl_igvm, built_openhcl_igvm) = ctx.new_var();
            let (_read_built_openhcl_boot, built_openhcl_boot) = ctx.new_var();
            let (_read_built_sidecar, built_sidecar) = ctx.new_var();
            ctx.req(crate::build_openhcl_igvm_from_recipe::Request {
                profile: match profile {
                    CommonProfile::Release => {
                        crate::build_openvmm_hcl::OpenvmmHclBuildProfile::OpenvmmHclShip
                    }
                    CommonProfile::Debug => crate::build_openvmm_hcl::OpenvmmHclBuildProfile::Debug,
                },
                recipe: recipe.clone(),
                custom_target: openhcl_custom_target.clone(),
                built_openvmm_hcl,
                built_openhcl_boot,
                built_openhcl_igvm,
                built_sidecar,
            });

            register_openhcl_igvm_files.push(read_built_openhcl_igvm.map(ctx, {
                let recipe = recipe.clone();
                |x| (recipe, x)
            }));
        }

        let register_openhcl_igvm_files = ReadVar::transpose_vec(ctx, register_openhcl_igvm_files);

        let register_pipette_windows = ctx.reqv(|v| crate::build_pipette::Request {
            target: targets.windows,
            profile,
            pipette: v,
        });

        let register_pipette_linux_musl = ctx.reqv(|v| crate::build_pipette::Request {
            target: targets.linux,
            profile,
            pipette: v,
        });

        let register_guest_test_uefi = ctx.reqv(|v| crate::build_guest_test_uefi::Request {
            arch,
            profile,
            guest_test_uefi: v,
        });

        ctx.requests::<crate::download_openvmm_vmm_tests_vhds::Node>([
            crate::download_openvmm_vmm_tests_vhds::Request::DownloadVhds(vhds),
            crate::download_openvmm_vmm_tests_vhds::Request::DownloadIsos(isos),
        ]);

        let disk_images_dir =
            ctx.reqv(crate::download_openvmm_vmm_tests_vhds::Request::GetDownloadFolder);

        let test_content_dir = ctx.persistent_dir().ok_or(anyhow::anyhow!(
            "build and run VMM tests only works locally"
        ))?;

        let (test_log_path, get_test_log_path) = ctx.new_var();

        let extra_env = ctx.reqv(|v| crate::init_vmm_tests_env::Request {
            test_content_dir,
            vmm_tests_target: target.clone(),
            register_openvmm: Some(register_openvmm),
            register_pipette_windows: Some(register_pipette_windows),
            register_pipette_linux_musl: Some(register_pipette_linux_musl),
            register_guest_test_uefi: Some(register_guest_test_uefi),
            disk_images_dir: Some(disk_images_dir),
            register_openhcl_igvm_files: Some(register_openhcl_igvm_files),
            get_test_log_path: Some(get_test_log_path),
            get_env: v,
        });

        let results = ctx.reqv(|v| crate::build_nextest_vmm_tests::Request {
            profile,
            target,
            build_mode: BuildNextestVmmTestsMode::ImmediatelyRun {
                nextest_profile,
                nextest_filter_expr,
                extra_env,
                pre_run_deps: Vec::new(),
                results: v,
            },
        });

        let mut side_effects = Vec::new();

        // TODO: Get correct path on linux and more reliably on windows
        let crash_dumps_path = ReadVar::from_static(PathBuf::from(match ctx.platform().kind() {
            FlowPlatformKind::Windows => r#"C:\Users\cloudtest\AppData\Local\CrashDumps"#,
            FlowPlatformKind::Unix => "/will/not/exist",
        }));

        let junit_xml = results.map(ctx, |r| r.junit_xml);
        let reported_results = ctx.reqv(|v| flowey_lib_common::publish_test_results::Request {
            junit_xml,
            test_label: junit_test_label,
            attachments: BTreeMap::from([
                ("logs".to_string(), (test_log_path, false)),
                ("crash-dumps".to_string(), (crash_dumps_path, true)),
            ]),
            output_dir: artifact_dir,
            done: v,
        });

        side_effects.push(reported_results);

        ctx.emit_rust_step("report test results to overall pipeline status", |ctx| {
            side_effects.claim(ctx);
            done.claim(ctx);

            let results = results.clone().claim(ctx);
            move |rt| {
                let results = rt.read(results);
                if results.all_tests_passed {
                    log::info!("all tests passed!");
                } else {
                    if fail_job_on_test_fail {
                        anyhow::bail!("encountered test failures.")
                    } else {
                        log::error!("encountered test failures.")
                    }
                }

                Ok(())
            }
        });

        Ok(())
    }
}
