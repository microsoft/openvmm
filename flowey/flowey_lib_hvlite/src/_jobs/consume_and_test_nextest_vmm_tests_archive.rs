// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run a pre-built cargo-nextest based VMM tests archive.

use crate::build_guest_test_uefi::GuestTestUefiOutput;
use crate::build_nextest_vmm_tests::NextestVmmTestsArchive;
use crate::build_openvmm::OpenvmmOutput;
use crate::build_pipette::PipetteOutput;
use crate::build_tmk_vmm::TmkVmmOutput;
use crate::build_tmks::TmksOutput;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use vmm_test_images::KnownIso;
use vmm_test_images::KnownVhd;

#[derive(Serialize, Deserialize)]
pub struct VmmTestsDepArtifacts {
    pub openvmm: Option<ReadVar<OpenvmmOutput>>,
    pub pipette_windows: Option<ReadVar<PipetteOutput>>,
    pub pipette_linux_musl: Option<ReadVar<PipetteOutput>>,
    pub guest_test_uefi: Option<ReadVar<GuestTestUefiOutput>>,
    pub artifact_dir_openhcl_igvm_files: Option<ReadVar<PathBuf>>,
    pub tmks: Option<ReadVar<TmksOutput>>,
    pub tmk_vmm: Option<ReadVar<TmkVmmOutput>>,
    pub tmk_vmm_linux_musl: Option<ReadVar<TmkVmmOutput>>,
}

flowey_request! {
    pub struct Params {
        /// Friendly label for report JUnit test results
        pub junit_test_label: String,
        /// Existing VMM tests archive
        pub nextest_vmm_tests_archive: ReadVar<NextestVmmTestsArchive>,
        /// What target VMM tests were compiled for (determines required deps).
        pub target: target_lexicon::Triple,
        /// Nextest profile to use when running the source code
        pub nextest_profile: NextestProfile,
        /// Nextest test filter expression.
        pub nextest_filter_expr: Option<String>,
        /// Artifacts corresponding to required test dependencies
        pub dep_artifact_dirs: VmmTestsDepArtifacts,
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
        ctx.import::<crate::artifact_openhcl_igvm_from_recipe_extras::resolve::Node>();
        ctx.import::<crate::artifact_openhcl_igvm_from_recipe::resolve::Node>();
        ctx.import::<crate::download_openvmm_vmm_tests_vhds::Node>();
        ctx.import::<crate::init_openvmm_magicpath_uefi_mu_msvm::Node>();
        ctx.import::<crate::init_hyperv_tests::Node>();
        ctx.import::<crate::init_vmm_tests_env::Node>();
        ctx.import::<crate::test_nextest_vmm_tests_archive::Node>();
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            junit_test_label,
            nextest_vmm_tests_archive,
            target,
            nextest_profile,
            nextest_filter_expr,
            dep_artifact_dirs,
            vhds,
            isos,
            fail_job_on_test_fail,
            artifact_dir,
            done,
        } = request;

        // use an ad-hoc, step-local dir as a staging ground for test content
        let test_content_dir = ctx.emit_rust_stepv("creating new test content dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        let VmmTestsDepArtifacts {
            openvmm: register_openvmm,
            pipette_windows: register_pipette_windows,
            pipette_linux_musl: register_pipette_linux_musl,
            guest_test_uefi: register_guest_test_uefi,
            artifact_dir_openhcl_igvm_files,
            tmks: register_tmks,
            tmk_vmm: register_tmk_vmm,
            tmk_vmm_linux_musl: register_tmk_vmm_linux_musl,
        } = dep_artifact_dirs;

        let register_openhcl_igvm_files = artifact_dir_openhcl_igvm_files.map(|artifact_dir| {
            ctx.reqv(
                |v| crate::artifact_openhcl_igvm_from_recipe::resolve::Request {
                    artifact_dir,
                    igvm_files: v,
                },
            )
        });

        ctx.requests::<crate::download_openvmm_vmm_tests_vhds::Node>([
            crate::download_openvmm_vmm_tests_vhds::Request::DownloadVhds(vhds),
            crate::download_openvmm_vmm_tests_vhds::Request::DownloadIsos(isos),
        ]);

        let disk_images_dir =
            ctx.reqv(crate::download_openvmm_vmm_tests_vhds::Request::GetDownloadFolder);

        // FUTURE: once we move away from the known_paths resolver, this will no
        // longer be an ambient pre-run dependency.
        let mu_msvm_arch = match target.architecture {
            target_lexicon::Architecture::X86_64 => {
                crate::download_uefi_mu_msvm::MuMsvmArch::X86_64
            }
            target_lexicon::Architecture::Aarch64(_) => {
                crate::download_uefi_mu_msvm::MuMsvmArch::Aarch64
            }
            arch => anyhow::bail!("unsupported arch {arch}"),
        };
        let pre_run_deps = vec![
            ctx.reqv(|v| crate::init_openvmm_magicpath_uefi_mu_msvm::Request {
                arch: mu_msvm_arch,
                done: v,
            }),
            ctx.reqv(crate::init_hyperv_tests::Request),
        ];

        let (test_log_path, get_test_log_path) = ctx.new_var();

        let extra_env = ctx.reqv(|v| crate::init_vmm_tests_env::Request {
            test_content_dir,
            vmm_tests_target: target.clone(),
            register_openvmm,
            register_pipette_windows,
            register_pipette_linux_musl,
            register_guest_test_uefi,
            register_tmks,
            register_tmk_vmm,
            register_tmk_vmm_linux_musl,
            disk_images_dir: Some(disk_images_dir),
            register_openhcl_igvm_files,
            get_test_log_path: Some(get_test_log_path),
            get_env: v,
        });

        let results = ctx.reqv(|v| crate::test_nextest_vmm_tests_archive::Request {
            nextest_archive_file: nextest_vmm_tests_archive,
            nextest_profile,
            nextest_filter_expr,
            extra_env,
            pre_run_deps,
            results: v,
        });

        // TODO: Get correct path on linux and more reliably on windows
        let crash_dumps_path = ReadVar::from_static(PathBuf::from(match ctx.platform().kind() {
            FlowPlatformKind::Windows => r#"C:\Users\cloudtest\AppData\Local\CrashDumps"#,
            FlowPlatformKind::Unix => "/will/not/exist",
        }));

        // Bind the externally generated output paths together with the results
        // to create a dependency on the VMM tests having actually run.
        let test_log_path = test_log_path.depending_on(ctx, &results);
        let crash_dumps_path = crash_dumps_path.depending_on(ctx, &results);

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

        ctx.emit_rust_step("report test results to overall pipeline status", |ctx| {
            reported_results.claim(ctx);
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
