// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run a pre-built cargo-nextest based VMM tests archive.

use crate::build_guest_test_uefi::GuestTestUefiOutput;
use crate::build_incubator::IncubatorOutput;
use crate::build_nextest_vmm_tests::NextestVmmTestsArchive;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmOutput;
use crate::build_openvmm::OpenvmmOutput;
use crate::build_openvmm_vhost::OpenvmmVhostOutput;
use crate::build_pipette::PipetteOutput;
use crate::build_prep_steps::PrepStepsOutput;
use crate::build_test_igvm_agent_rpc_server::TestIgvmAgentRpcServerOutput;
use crate::build_tmk_vmm::TmkVmmOutput;
use crate::build_tmks::TmksOutput;
use crate::build_tpm_guest_tests::TpmGuestTestsOutput;
use crate::build_vmgstool::VmgstoolOutput;
use crate::install_vmm_tests_deps::VmmTestsDepSelections;
use crate::install_vmm_tests_deps::VmmTestsDepSelectionsWindows;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use vmm_test_images::KnownTestArtifacts;

#[derive(Serialize, Deserialize, Default)]
pub struct VmmTestsDepArtifacts {
    /// Incubator binary (bundling its profiles directory) used to run tests
    /// inside an emulated VM (e.g. QEMU TCG). Only set when running via
    /// [`Params::incubator_profile`].
    pub incubator: Option<ReadVar<IncubatorOutput>>,
    pub openvmm: Option<ReadVar<OpenvmmOutput>>,
    pub openvmm_vhost: Option<ReadVar<OpenvmmVhostOutput>>,
    pub pipette_windows: Option<ReadVar<PipetteOutput>>,
    pub pipette_linux_musl: Option<ReadVar<PipetteOutput>>,
    pub guest_test_uefi: Option<ReadVar<GuestTestUefiOutput>>,
    pub prep_steps: Option<ReadVar<PrepStepsOutput>>,
    pub openhcl_standard: Option<ReadVar<OpenhclIgvmOutput>>,
    pub openhcl_standard_dev: Option<ReadVar<OpenhclIgvmOutput>>,
    pub openhcl_cvm: Option<ReadVar<OpenhclIgvmOutput>>,
    pub openhcl_linux_direct: Option<ReadVar<OpenhclIgvmOutput>>,
    pub tmks: Option<ReadVar<TmksOutput>>,
    pub tmk_vmm: Option<ReadVar<TmkVmmOutput>>,
    pub tmk_vmm_linux_musl: Option<ReadVar<TmkVmmOutput>>,
    pub vmgstool: Option<ReadVar<VmgstoolOutput>>,
    pub vmgstool_dev: Option<ReadVar<VmgstoolOutput>>,
    pub tpm_guest_tests_windows: Option<ReadVar<TpmGuestTestsOutput>>,
    pub tpm_guest_tests_linux: Option<ReadVar<TpmGuestTestsOutput>>,
    pub test_igvm_agent_rpc_server: Option<ReadVar<TestIgvmAgentRpcServerOutput>>,
}

pub type ResolveVmmTestsDepArtifacts =
    Box<dyn Fn(&mut flowey::pipeline::prelude::PipelineJobCtx<'_>) -> VmmTestsDepArtifacts>;

#[macro_export]
macro_rules! vmm_tests_artifact_builder {
    (
        $name:ty,
        (
            $($artifact:ident => $output:ty),* $(,)?
        )
    ) => {
        ::paste::paste! {
            #[derive(Default, Clone)]
            pub struct $name {
                $(pub [<use_ $artifact>]: Option<::flowey::pipeline::prelude::UseTypedArtifact<$output>>,)*
            }

            impl $name {
                pub fn finish(self) -> Result<::flowey_lib_hvlite::_jobs::consume_and_test_nextest_vmm_tests_archive::ResolveVmmTestsDepArtifacts, &'static str> {
                    let $name {
                        $([<use_ $artifact>],)*
                    } = self;

                    $(let [<use_ $artifact>] = [<use_ $artifact>].ok_or(stringify!($artifact))?;)*

                    Ok(Box::new(move |ctx| ::flowey_lib_hvlite::_jobs::consume_and_test_nextest_vmm_tests_archive::VmmTestsDepArtifacts {
                        $($artifact: Some(ctx.use_typed_artifact(&[<use_ $artifact>])),)*
                        .. Default::default()
                    }))
                }
            }
        }
    };
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
        /// Test artifacts to download
        pub test_artifacts: Vec<KnownTestArtifacts>,
        /// Which prep_steps variants to run before tests (e.g. "standard", "no-vmbus").
        /// Empty means no prep steps are needed.
        pub prep_steps_variants: Vec<String>,
        /// If set, configure this 2 MiB hugetlb surplus page overcommit limit before running tests.
        pub hugetlb_2mb_overcommit_pages: Option<u64>,

        /// If set, run tests inside an incubator using the named profile,
        /// instead of directly on the host. The profile name (without the
        /// `.toml` extension) is resolved against the profiles directory
        /// bundled in the incubator artifact supplied via
        /// [`VmmTestsDepArtifacts::incubator`] (e.g. "aarch64-tcg-pcie").
        pub incubator_profile: Option<String>,

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
        ctx.import::<crate::download_openvmm_vmm_tests_artifacts::Node>();
        ctx.import::<crate::download_release_igvm_files_from_gh::resolve::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::init_openvmm_magicpath_uefi_mu_msvm::Node>();
        ctx.import::<crate::install_vmm_tests_deps::Node>();
        ctx.import::<crate::init_vmm_tests_env::Node>();
        ctx.import::<crate::resolve_openvmm_qemu::Node>();
        ctx.import::<crate::resolve_openvmm_test_initrd::Node>();
        ctx.import::<crate::resolve_openvmm_test_linux_kernel::Node>();
        ctx.import::<crate::run_prep_steps::Node>();
        ctx.import::<crate::run_test_igvm_agent_rpc_server::Node>();
        ctx.import::<crate::stop_test_igvm_agent_rpc_server::Node>();
        ctx.import::<crate::test_nextest_vmm_tests_archive::Node>();
        ctx.import::<crate::write_incubator_target_runner::Node>();
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
            test_artifacts,
            fail_job_on_test_fail,
            incubator_profile,
            prep_steps_variants,
            hugetlb_2mb_overcommit_pages,
            artifact_dir,
            done,
        } = request;

        // use an ad-hoc, step-local dir as a staging ground for test content
        let test_content_dir = ctx.emit_rust_stepv("creating new test content dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        let VmmTestsDepArtifacts {
            incubator: register_incubator,
            openvmm: register_openvmm,
            openvmm_vhost: register_openvmm_vhost,
            pipette_windows: register_pipette_windows,
            pipette_linux_musl: register_pipette_linux_musl,
            guest_test_uefi: register_guest_test_uefi,
            prep_steps: register_prep_steps,
            openhcl_standard,
            openhcl_standard_dev,
            openhcl_cvm,
            openhcl_linux_direct,
            tmks: register_tmks,
            tmk_vmm: register_tmk_vmm,
            tmk_vmm_linux_musl: register_tmk_vmm_linux_musl,
            vmgstool: register_vmgstool,
            vmgstool_dev: register_vmgstool_dev,
            tpm_guest_tests_windows: register_tpm_guest_tests_windows,
            tpm_guest_tests_linux: register_tpm_guest_tests_linux,
            test_igvm_agent_rpc_server: register_test_igvm_agent_rpc_server,
        } = dep_artifact_dirs;

        let register_openhcl_igvm_files = [
            openhcl_standard,
            openhcl_standard_dev,
            openhcl_cvm,
            openhcl_linux_direct,
        ]
        .into_iter()
        .flatten()
        .collect();

        ctx.req(crate::download_openvmm_vmm_tests_artifacts::Request::Download(test_artifacts));

        let disk_images_dir =
            ctx.reqv(crate::download_openvmm_vmm_tests_artifacts::Request::GetDownloadFolder);

        ctx.config(crate::install_vmm_tests_deps::Config {
            selections: Some(match target.operating_system {
                target_lexicon::OperatingSystem::Windows => {
                    VmmTestsDepSelections::Windows(VmmTestsDepSelectionsWindows {
                        hyperv: true,
                        whp: true,
                        hardware_isolation: false,
                    })
                }
                target_lexicon::OperatingSystem::Linux => VmmTestsDepSelections::Linux,
                os => anyhow::bail!("unsupported target operating system: {os}"),
            }),
            auto_install: None,
        });

        let arch = crate::common::CommonArch::from_architecture(target.architecture)?;
        let release_igvm_files = if !matches!(ctx.backend(), FlowBackend::Ado) {
            Some(ctx.reqv(
                |v| crate::download_release_igvm_files_from_gh::resolve::Request {
                    arch,
                    release_igvm_files: v,
                    release_version:
                        crate::download_release_igvm_files_from_gh::OpenhclReleaseVersion::latest(),
                },
            ))
        } else {
            None
        };

        let mut pre_run_deps = vec![ctx.reqv(crate::install_vmm_tests_deps::Request::Install)];

        let (test_log_path, get_test_log_path) = ctx.new_var();

        let extra_env = ctx.reqv(|v| crate::init_vmm_tests_env::Request {
            test_content_dir: test_content_dir.clone(),
            vmm_tests_target: target.clone(),
            register_openvmm,
            register_openvmm_vhost,
            register_pipette_windows,
            register_pipette_linux_musl,
            register_guest_test_uefi,
            register_tmks,
            register_tmk_vmm,
            register_tmk_vmm_linux_musl,
            register_vmgstool,
            register_vmgstool_dev,
            register_tpm_guest_tests_windows,
            register_tpm_guest_tests_linux,
            register_test_igvm_agent_rpc_server,
            disk_images_dir: Some(disk_images_dir),
            register_openhcl_igvm_files,
            get_test_log_path: Some(get_test_log_path),
            get_env: v,
            release_igvm_files,
            use_relative_paths: false,
            disable_remote_artifacts: true,
            reuse_prepped_vhds: false,
        });

        // Start the test_igvm_agent_rpc_server before running tests (Windows only).
        // This must happen after init_vmm_tests_env which copies the binary.
        // The server runs in the background for the duration of the test run.
        if matches!(ctx.platform(), FlowPlatform::Windows) {
            pre_run_deps.push(
                ctx.reqv(|done| crate::run_test_igvm_agent_rpc_server::Request {
                    env: extra_env.clone(),
                    done,
                }),
            );
        }

        if !prep_steps_variants.is_empty() {
            let prep_steps = register_prep_steps.expect("Test run indicated prep_steps was needed but built prep_steps binary was not given");
            for variant in &prep_steps_variants {
                pre_run_deps.push(ctx.reqv(|done| crate::run_prep_steps::Request {
                    prep_steps: prep_steps.clone(),
                    args: vec![variant.clone()],
                    env: extra_env.clone(),
                    done,
                }));
            }
        } else if let Some(register_prep_steps) = register_prep_steps {
            register_prep_steps.claim_unused(ctx);
        }

        let (extra_env, nextest_working_dir, nextest_config_file) = if let Some(profile_name) =
            incubator_profile
        {
            let incubator = register_incubator.ok_or_else(|| {
                anyhow::anyhow!("incubator profile was set but no incubator artifact was provided")
            })?;

            let arch = crate::common::CommonArch::from_architecture(target.architecture)?;

            let kernel = ctx.reqv(|v| {
                crate::resolve_openvmm_test_linux_kernel::Request::Get(
                    crate::resolve_openvmm_test_linux_kernel::OpenvmmTestKernelFile::Kernel,
                    arch,
                    crate::resolve_openvmm_test_linux_kernel::INCUBATOR_LINUX_TEST_KERNEL_VERSION,
                    v,
                )
            });
            let initrd = ctx.reqv(|v| crate::resolve_openvmm_test_initrd::Request::Get(arch, v));

            let host_arch: crate::common::CommonArch = ctx.arch().try_into()?;
            let qemu_binary = ctx.reqv(|v| {
                crate::resolve_openvmm_qemu::Request::Get(
                    crate::resolve_openvmm_qemu::QemuFile::SystemAarch64,
                    host_arch,
                    v,
                )
            });

            // Resolve the incubator binary and the selected profile from the
            // incubator artifact (which bundles the profiles directory).
            let incubator_bin = incubator.clone().map(ctx, |o| o.bin);
            let profile_path = incubator.map(ctx, move |o| {
                o.profiles.join(format!("{profile_name}.toml"))
            });

            let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
            let nextest_config_file = openvmm_repo_path
                .clone()
                .map(ctx, |p| p.join(".config").join("nextest.toml"));
            let nextest_archive = nextest_vmm_tests_archive
                .clone()
                .map(ctx, |x| x.archive_file);

            let extra_env = ctx.reqv(|v| crate::write_incubator_target_runner::Request {
                incubator_bin,
                profile_path,
                kernel: Some(kernel),
                initrd: Some(initrd),
                repo_root: openvmm_repo_path.clone(),
                test_content_dir: test_content_dir.clone(),
                extra_share_paths: vec![nextest_archive, nextest_config_file.clone()],
                extra_env: Some(extra_env),
                qemu_binary: Some(qemu_binary),
                target: target.clone(),
                nextest_env: v,
            });

            (
                extra_env,
                Some(openvmm_repo_path),
                Some(nextest_config_file),
            )
        } else {
            if let Some(register_incubator) = register_incubator {
                register_incubator.claim_unused(ctx);
            }
            (extra_env, None, None)
        };

        let results = ctx.reqv(|v| crate::test_nextest_vmm_tests_archive::Request {
            nextest_archive_file: nextest_vmm_tests_archive,
            nextest_profile,
            nextest_filter_expr,
            nextest_working_dir,
            nextest_config_file,
            nextest_bin: None,
            target: None,
            extra_env,
            pre_run_deps,
            hugetlb_2mb_overcommit_pages,
            results: v,
        });

        // Stop the test_igvm_agent_rpc_server after tests complete (Windows only).
        // This ensures we clean up the background process.
        let rpc_server_stopped = if matches!(ctx.platform(), FlowPlatform::Windows) {
            let after_tests = results.map(ctx, |_| ());
            Some(
                ctx.reqv(|done| crate::stop_test_igvm_agent_rpc_server::Request {
                    after_tests,
                    done,
                }),
            )
        } else {
            None
        };

        // Bind the externally generated output paths together with the results
        // to create a dependency on the VMM tests having actually run.
        let test_log_path = test_log_path.depending_on(ctx, &results);

        let junit_xml = results.map(ctx, |r| r.junit_xml);
        let reported_results = ctx.reqv(|v| flowey_lib_common::publish_test_results::Request {
            junit_xml,
            test_label: junit_test_label,
            attachments: BTreeMap::from([("logs".to_string(), (test_log_path, false))]),
            output_dir: artifact_dir,
            done: v,
        });

        ctx.emit_rust_step("report test results to overall pipeline status", |ctx| {
            reported_results.claim(ctx);
            if let Some(rpc_server_stopped) = rpc_server_stopped {
                rpc_server_stopped.claim(ctx);
            }
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
