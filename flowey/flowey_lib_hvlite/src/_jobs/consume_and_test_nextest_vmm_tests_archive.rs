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
use crate::install_vmm_tests_deps::VmmTestsDepSelectionsLinux;
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
        /// Run the tests this number of times
        pub repetitions: std::num::NonZeroU64,

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
            prep_steps_variants,
            hugetlb_2mb_overcommit_pages,
            incubator_profile,
            fail_job_on_test_fail,
            repetitions,
            done,
        } = request;

        // use a test content dir with
        // - short path name to avoid issues with long paths
        // - relative to github.workspace so that the correct disk is used on CI machines.
        let test_content_dir = match ctx.backend() {
            FlowBackend::Local => panic!("local backend not supported"),
            FlowBackend::Ado => ctx.get_ado_variable(AdoRuntimeVar::PIPELINE_WORKSPACE),
            FlowBackend::Github => ctx.get_gh_context_var().global().runner_temp(),
        }
        .map(ctx, |w| PathBuf::from(w).join("test"));

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

        let prepare_vhost_vsock = incubator_profile.is_none()
            && matches!(
                target.operating_system,
                target_lexicon::OperatingSystem::Linux
            )
            && matches!(target.architecture, target_lexicon::Architecture::X86_64);

        ctx.config(crate::install_vmm_tests_deps::Config {
            selections: Some(match target.operating_system {
                target_lexicon::OperatingSystem::Windows => {
                    VmmTestsDepSelections::Windows(VmmTestsDepSelectionsWindows {
                        hyperv: true,
                        whp: true,
                        // this is currently manually set on CVM runners
                        hardware_isolation: false,
                    })
                }
                target_lexicon::OperatingSystem::Linux => {
                    VmmTestsDepSelections::Linux(VmmTestsDepSelectionsLinux {
                        hugetlb_2mb_overcommit_pages,
                        prepare_vhost_vsock,
                    })
                }
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

        let installed_deps = ctx.reqv(crate::install_vmm_tests_deps::Request::Install);
        let mut pre_run_deps = vec![installed_deps.clone()];

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
            require_2mb_hugetlb: hugetlb_2mb_overcommit_pages.is_some(),
        });

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let nextest_config_file = openvmm_repo_path
            .clone()
            .map(ctx, |p| p.join(".config").join("nextest.toml"));

        let igvm_agent_env = extra_env.clone();

        let extra_env = if let Some(profile_name) = incubator_profile {
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

            let nextest_archive = nextest_vmm_tests_archive
                .clone()
                .map(ctx, |x| x.archive_file);

            ctx.reqv(|v| crate::write_incubator_target_runner::Request {
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
            })
        } else {
            extra_env
        };

        if !prep_steps_variants.is_empty() {
            let prep_steps = register_prep_steps
                .expect("Prep steps variants requested but missing binary")
                .depending_on(ctx, &installed_deps);
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

        let repetitions = repetitions.get();
        let mut all_results = Vec::with_capacity(repetitions as usize);
        let mut all_log_dirs = Vec::with_capacity(repetitions as usize);
        for i in 0..repetitions {
            let mut pre_run_deps_iteration = pre_run_deps.clone();
            // Start the test_igvm_agent_rpc_server before running tests.
            // Currently X64 Windows only.
            // The binary must already exist in the test content dir.
            // The server runs in the background for the duration of the test run.
            let previous_done = all_log_dirs
                .last()
                .map(|x: &ReadVar<PathBuf>| x.clone().into_side_effect());
            if matches!(ctx.platform(), FlowPlatform::Windows) {
                pre_run_deps_iteration.push(ctx.reqv(|done| {
                    crate::run_test_igvm_agent_rpc_server::Request {
                        env: igvm_agent_env.clone(),
                        done,
                        previous_done,
                    }
                }));
            // make the repetitions run in order
            } else if let Some(previous) = previous_done {
                pre_run_deps_iteration.push(previous);
            }

            let results = ctx.reqv(|v| crate::test_nextest_vmm_tests_archive::Request {
                nextest_archive_file: nextest_vmm_tests_archive.clone(),
                nextest_profile,
                nextest_filter_expr: nextest_filter_expr.clone(),
                nextest_working_dir: Some(openvmm_repo_path.clone()),
                nextest_config_file: Some(nextest_config_file.clone()),
                nextest_bin: None,
                target: None,
                extra_env: extra_env.clone(),
                pre_run_deps: pre_run_deps_iteration,
                results: v,
            });

            // Stop the test_igvm_agent_rpc_server after tests complete (Windows only).
            // This ensures we clean up the background process.
            let rpc_server_stopped = matches!(ctx.platform(), FlowPlatform::Windows).then(|| {
                ctx.reqv(|done| crate::stop_test_igvm_agent_rpc_server::Request {
                    after_tests: results.clone().into_side_effect(),
                    done,
                })
            });

            // Bind the externally generated output paths together with the results
            // to create a dependency on the VMM tests having actually run.
            let current_test_log_path = test_log_path.depending_on(
                ctx,
                &rpc_server_stopped.unwrap_or(results.clone().into_side_effect()),
            );

            let current_test_log_path = if repetitions > 1 {
                // WARNING: on platforms that rely on the JUnit file to upload
                // test artifacts (ADO in our case), renaming the folder here
                // will break those paths and result in them failing to upload.
                // TODO: fix paths in JUnit or change the TEST_OUTPUT_PATH
                // environment variable for each repetition.
                ctx.emit_rust_stepv("rename and create new log dir", |ctx| {
                    let current_test_log_path = current_test_log_path.claim(ctx);
                    move |rt| {
                        let log_dir = rt.read(current_test_log_path);

                        // rename the log dir and create a fresh one
                        let log_dir_archive = log_dir_for_iteration(&log_dir, i)?;
                        log::info!(
                            "renaming {} to {}",
                            log_dir.to_string_lossy(),
                            log_dir_archive.to_string_lossy()
                        );
                        if log_dir_archive.exists() {
                            fs_err::remove_dir_all(&log_dir_archive)?;
                        }
                        fs_err::rename(&log_dir, &log_dir_archive)?;
                        fs_err::create_dir(&log_dir)?;

                        Ok(log_dir_archive)
                    }
                })
            } else {
                current_test_log_path
            };

            all_results.push((results, current_test_log_path.clone()));
            all_log_dirs.push(current_test_log_path);
        }

        let test_label_for_iteration = {
            let test_label = junit_test_label.clone();
            move |i| {
                if repetitions > 1 {
                    format!("{test_label}-{i}")
                } else {
                    test_label.clone()
                }
            }
        };

        // A failing VMM test dumps its entire captured stdout -- guest serial
        // console, OpenHCL kmsg, pipette, and petri tracing -- into the job
        // log, which makes it impractical to tell at a glance which tests
        // actually failed. Emit a scannable summary alongside it.
        let summarized = {
            let is_github = matches!(ctx.backend(), FlowBackend::Github);
            let all_log_dirs = all_log_dirs.clone();
            let test_label = junit_test_label.clone();
            let test_label_for_iteration = test_label_for_iteration.clone();
            ctx.emit_rust_step("summarize failing vmm tests", |ctx| {
                let all_log_dirs = all_log_dirs.claim(ctx);
                move |rt| {
                    let all_log_dirs = rt.read(all_log_dirs);

                    let mut test_failures = BTreeMap::new();

                    // Summarizing is ancillary to the test run. If it fails,
                    // warn rather than propagate: this step is ordered before
                    // the one that reports test failures, so returning an
                    // error here would replace "encountered test failures"
                    // with an unrelated error and hide what actually broke.
                    for (i, log_dir) in all_log_dirs.iter().enumerate() {
                        match failure_summary::collect_failed_tests(log_dir) {
                            Ok(failures) => {
                                let log_artifact_name =
                                    format!("{}-logs", test_label_for_iteration(i));
                                if test_failures.insert(log_artifact_name, failures).is_some() {
                                    anyhow::bail!("tests should not have the same label")
                                }
                            }
                            Err(err) => {
                                failure_summary::warn_summary_unavailable(is_github, &err);
                            }
                        };
                    }

                    failure_summary::report_failed_tests(&test_failures, is_github, &test_label);

                    let test_failures = test_failures.into_values().flatten().collect::<Vec<_>>();

                    let (failures_by_test, failures_by_mode) =
                        failure_summary::bucketize_failures(&test_failures);
                    failure_summary::report_failure_buckets(failures_by_test, failures_by_mode);

                    Ok(())
                }
            })
        };

        let mut reported_results = Vec::new();

        for (i, (results, log_dir)) in all_results.iter().enumerate() {
            let junit_xml = results.map(ctx, |r| r.junit_xml);
            let test_label = test_label_for_iteration(i);
            reported_results.push(
                ctx.reqv(|v| flowey_lib_common::publish_test_results::Request {
                    junit_xml,
                    test_label,
                    attachments: BTreeMap::from([(
                        "logs".to_string(),
                        (log_dir.to_owned(), false),
                    )]),
                    output_dir: None,
                    done: v,
                }),
            );
        }

        ctx.emit_rust_step("report test results to overall pipeline status", |ctx| {
            reported_results.claim(ctx);
            summarized.claim(ctx);
            done.claim(ctx);

            let all_results = all_results.clone().claim(ctx);
            move |rt| {
                let all_results = rt.read(all_results);
                if all_results.iter().all(|x| x.0.all_tests_passed) {
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

fn log_dir_for_iteration(log_dir: &Path, i: u64) -> anyhow::Result<PathBuf> {
    let mut log_dir_basename = log_dir.file_name().context("invalid path")?.to_owned();
    log_dir_basename.push(format!("_{i}"));
    Ok(log_dir
        .parent()
        .context("invalid path")?
        .join(log_dir_basename))
}

/// Summarizes failing VMM tests by scanning the per-test output directories
/// that petri writes during a run.
///
/// Nextest reports which tests failed, but for VMM tests it also replays each
/// failing test's entire captured stdout -- guest serial console, OpenHCL
/// kmsg, pipette, and petri tracing -- which routinely buries the list of
/// failures under megabytes of log. This module produces a compact,
/// linkable report to sit alongside that output.
mod failure_summary {
    use flowey::node::prelude::*;
    use std::collections::BTreeMap;
    use std::collections::VecDeque;
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Read;
    use std::path::Path;
    use std::path::PathBuf;

    /// Maximum number of log lines to show inline per failing test. The tail
    /// is kept, since the entries immediately preceding a failure are almost
    /// always the relevant ones.
    const MAX_EXCERPT_LINES: usize = 20;

    /// Maximum length of a failure reason shown in a summary table cell.
    const MAX_REASON_CHARS: usize = 300;

    /// Base URL of the petri log viewer.
    const LOG_VIEWER_BASE_URL: &str = "https://openvmm.dev/test-results";

    const ERROR_BUCKETS: &[&str] = &[
        "Kernel indicates VP is both halted and idle",
        "the guest operating system requested an operation that is not supported by Hyper-V",
        "an unrecoverable error occurred on a virtual processor that caused a triple fault",
        "an unrecoverable error occurred while accessing a virtual processor register which caused a triple fault",
        "failed to start worker process",
        "Failed to create a new virtual machine",
        "Not enough memory in the system to start the virtual machine",
        "Test timed out",
    ];

    /// How a test finished, for tests that did not pass.
    #[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    enum Outcome {
        Failed,
        FailedUnstable,
        /// Started but never recorded a result: the test process was killed or
        /// crashed before it could report.
        Incomplete,
    }

    impl Outcome {
        /// Short description used in the job log and the summary table.
        fn describe(self) -> &'static str {
            match self {
                Outcome::Failed => "failed",
                Outcome::FailedUnstable => "failed (unstable)",
                Outcome::Incomplete => "did not complete",
            }
        }
    }

    /// A failing test discovered by scanning the petri log directory.
    #[derive(Serialize, Deserialize)]
    pub struct FailedTest {
        /// Full test name, e.g. `x86_64::openhcl_linux_direct_boot`.
        name: String,
        /// Name of the test's directory within the log directory.
        dir_name: String,
        /// How the test finished.
        outcome: Outcome,
        /// The error petri recorded, if it got as far as recording one.
        error: Option<String>,
        /// ERROR / WARN entries from the tail of `petri.jsonl`.
        excerpt: Vec<String>,
        /// Path to the test results on the local system
        path: PathBuf,
    }

    /// Scans `log_dir` for the per-test marker files petri writes, returning
    /// everything that did not pass, sorted by test name.
    pub fn collect_failed_tests(log_dir: &Path) -> anyhow::Result<Vec<FailedTest>> {
        let mut failures = Vec::new();
        if !log_dir.exists() {
            return Ok(failures);
        }

        for entry in fs_err::read_dir(log_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }

            let dir = entry.path();
            let started = dir.join("petri.test");
            if dir.join("petri.passed").exists() {
                continue;
            }

            // The error is the marker's contents; an absent marker means petri
            // never got to write one.
            let (outcome, error) = if let Ok(err) = fs_err::read_to_string(dir.join("petri.failed"))
            {
                (Outcome::Failed, Some(err))
            } else if let Ok(err) = fs_err::read_to_string(dir.join("petri.failed_unstable")) {
                (Outcome::FailedUnstable, Some(err))
            } else if started.exists() {
                (Outcome::Incomplete, None)
            } else {
                // Not a petri test directory.
                continue;
            };

            let dir_name = entry.file_name().to_string_lossy().into_owned();
            // petri records the test's real name up front; fall back to
            // undoing the directory-name mangling if it isn't there.
            let name = fs_err::read_to_string(&started)
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| dir_name.replace("__", "::"));

            let excerpt = fs_err::File::open(dir.join("petri.jsonl"))
                .map(excerpt_from_petri_jsonl)
                .unwrap_or_default();

            failures.push(FailedTest {
                name,
                dir_name,
                outcome,
                error: error.map(|e| one_line(&e)).filter(|e| !e.is_empty()),
                excerpt,
                path: entry.path(),
            });
        }

        failures.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(failures)
    }

    /// Collapses whitespace and truncates, so that a multi-line error can be
    /// used in a summary table cell or a single-line workflow command.
    fn one_line(s: &str) -> String {
        let mut collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.chars().count() > MAX_REASON_CHARS {
            collapsed = collapsed
                .chars()
                .take(MAX_REASON_CHARS)
                .collect::<String>()
                .trim_end()
                .to_owned();
            collapsed.push('…');
        }
        collapsed
    }

    /// Extracts the trailing ERROR / WARN entries from a `petri.jsonl` stream.
    ///
    /// The log is streamed rather than read into memory: a buggy test can
    /// produce a log of essentially unbounded size, and only the last handful
    /// of entries are ever shown. Malformed lines are skipped rather than
    /// treated as errors; this is best-effort reporting layered on top of an
    /// already-failing test run.
    fn excerpt_from_petri_jsonl(reader: impl Read) -> Vec<String> {
        let mut excerpt = VecDeque::with_capacity(MAX_EXCERPT_LINES);
        for line in BufReader::new(reader).lines() {
            // Stop on a read or encoding error rather than risk spinning on a
            // reader that keeps failing.
            let Ok(line) = line else { break };

            let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let severity = entry.get("severity").and_then(|v| v.as_str()).unwrap_or("");
            if !matches!(severity, "ERROR" | "WARN") {
                continue;
            }
            let source = entry.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let message = entry.get("message").and_then(|v| v.as_str()).unwrap_or("");

            if excerpt.len() == MAX_EXCERPT_LINES {
                excerpt.pop_front();
            }
            excerpt.push_back(format!("[{severity:>5}] [{source:>10}] {message}"));
        }
        excerpt.into()
    }

    /// Builds a deep link into the petri log viewer for a failing test.
    ///
    /// Returns `None` when the run ID is unknown (i.e. outside of GitHub
    /// Actions), since the viewer is keyed on it.
    fn log_viewer_url(run_id_attempt: &str, log_artifact_name: &str, test: &FailedTest) -> String {
        format!(
            "{LOG_VIEWER_BASE_URL}/#/runs/{run_id_attempt}/{}/{}",
            percent_encode(log_artifact_name),
            percent_encode(&test.dir_name),
        )
    }

    /// Renders the per-failure detail written to the job log.
    ///
    /// On GitHub the detail is wrapped in workflow-command groups so that it
    /// collapses by default and the list of failures stays scannable.
    fn render_job_log(
        failures: &BTreeMap<String, Vec<FailedTest>>,
        is_github: bool,
        run_id_attempt: Option<&str>,
    ) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        for (log_artifact_name, tests) in failures {
            for test in tests {
                let label = match test.outcome {
                    Outcome::Failed => "FAIL",
                    Outcome::FailedUnstable => "FAIL (unstable)",
                    Outcome::Incomplete => "INCOMPLETE",
                };
                if is_github {
                    let _ = writeln!(out, "::group::{label} {}", test.name);
                } else {
                    let _ = writeln!(out, "--- {label} {} ---", test.name);
                }

                if let Some(error) = &test.error {
                    let _ = writeln!(out, "  error: {error}");
                }

                if test.excerpt.is_empty() {
                    let _ = writeln!(out, "  (no ERROR or WARN entries found in petri.jsonl)");
                } else {
                    for line in &test.excerpt {
                        let _ = writeln!(out, "  {line}");
                    }
                }

                if let Some(run_id_attempt) = run_id_attempt {
                    let _ = writeln!(
                        out,
                        "  full logs: {}",
                        log_viewer_url(run_id_attempt, log_artifact_name, test)
                    );
                }

                if is_github {
                    let _ = writeln!(out, "::endgroup::");
                }
            }
        }
        out
    }

    /// Renders the markdown table appended to the GitHub Actions job summary.
    fn render_job_summary(
        failures: &BTreeMap<String, Vec<FailedTest>>,
        run_id_attempt: Option<&str>,
        test_label: &str,
    ) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        let _ = writeln!(out, "### Failed VMM tests: {test_label}");
        let _ = writeln!(out);
        let _ = writeln!(out, "| Test | Result | Reason | Logs |");
        let _ = writeln!(out, "| --- | --- | --- | --- |");
        for (log_artifact_name, tests) in failures {
            for test in tests {
                let logs = match run_id_attempt {
                    Some(run_id_attempt) => format!(
                        "[view]({})",
                        log_viewer_url(run_id_attempt, log_artifact_name, test)
                    ),
                    None => format!("`{log_artifact_name}` artifact"),
                };
                // Escape pipes so a reason containing one can't break the table.
                let reason = match &test.error {
                    Some(error) => error.replace('|', "\\|"),
                    None => String::new(),
                };
                let _ = writeln!(
                    out,
                    "| `{}` | {} | {reason} | {logs} |",
                    test.name,
                    test.outcome.describe(),
                );
            }
        }
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Logs become available once the whole workflow run has completed."
        );
        out
    }

    /// Writes the failure report to the job log and, on GitHub, to the job
    /// summary.
    pub fn report_failed_tests(
        failures: &BTreeMap<String, Vec<FailedTest>>,
        is_github: bool,
        test_label: &str,
    ) {
        if failures.is_empty() {
            return;
        }

        // The log viewer is fed by a separate workflow that uploads the test
        // log artifacts once the whole run completes, so these links only
        // become live after the run finishes.
        let run_id = std::env::var("GITHUB_RUN_ID").ok();
        let run_attempt = std::env::var("GITHUB_RUN_ATTEMPT").ok();
        let run_id_attempt = run_id
            .zip(run_attempt)
            .map(|(id, attempt)| format!("{id}_{attempt}"));
        print!(
            "{}",
            render_job_log(failures, is_github, run_id_attempt.as_deref())
        );

        let Ok(summary_path) = std::env::var("GITHUB_STEP_SUMMARY") else {
            return;
        };
        let summary = render_job_summary(failures, run_id_attempt.as_deref(), test_label);
        // Other steps may have already appended to the summary file.
        let write_summary = || -> std::io::Result<()> {
            let mut file = fs_err::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&summary_path)?;
            std::io::Write::write_all(&mut file, summary.as_bytes())
        };
        if let Err(err) = write_summary() {
            log::warn!("failed to write job summary: {err}");
        }
    }

    /// Renders the workflow command warning that the summary is missing.
    fn render_summary_warning(message: &str) -> String {
        // Workflow commands are terminated by a newline, so the message has to
        // be collapsed onto one line.
        format!("::warning title=vmm test summary::{}\n", one_line(message))
    }

    /// Reports that the failure summary could not be produced.
    ///
    /// Callers deliberately do not fail the job over this: the summary exists
    /// to explain a test failure, so replacing that failure with an error
    /// about the summary itself would defeat the purpose. Warn instead, and on
    /// GitHub surface it on the run summary page so that an absent table is
    /// noticed rather than silently ignored.
    pub fn warn_summary_unavailable(is_github: bool, err: &anyhow::Error) {
        let message = format!("failed to summarize failing vmm tests: {err:#}");
        log::warn!("{message}");
        if is_github {
            print!("{}", render_summary_warning(&message));
        }
    }

    /// Percent-encodes anything that isn't unreserved in a URL path segment.
    fn percent_encode(s: &str) -> String {
        let mut encoded = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    encoded.push(b as char)
                }
                _ => encoded.push_str(&format!("%{b:02X}")),
            }
        }
        encoded
    }

    pub fn bucketize_failures(
        test_failures: &[FailedTest],
    ) -> (
        BTreeMap<String, BTreeMap<&'static str, Vec<PathBuf>>>,
        BTreeMap<&'static str, BTreeMap<String, Vec<PathBuf>>>,
    ) {
        let mut failures_by_test: BTreeMap<String, BTreeMap<&'static str, Vec<PathBuf>>> =
            BTreeMap::new();
        let mut failures_by_mode: BTreeMap<&'static str, BTreeMap<String, Vec<PathBuf>>> =
            BTreeMap::new();

        for FailedTest {
            name,
            excerpt,
            path,
            ..
        } in test_failures
        {
            let mut error = "Unknown";
            for err in ERROR_BUCKETS {
                for line in excerpt {
                    if line.contains(err) {
                        error = err;
                    }
                }
            }

            failures_by_test
                .entry(name.clone())
                .or_default()
                .entry(error)
                .or_default()
                .push(path.clone());
            failures_by_mode
                .entry(error)
                .or_default()
                .entry(name.clone())
                .or_default()
                .push(path.clone());
        }

        (failures_by_test, failures_by_mode)
    }

    pub fn report_failure_buckets(
        failures_by_test: BTreeMap<String, BTreeMap<&'static str, Vec<PathBuf>>>,
        failures_by_mode: BTreeMap<&'static str, BTreeMap<String, Vec<PathBuf>>>,
    ) {
        println!("\nFailures by test\n");
        if !failures_by_test.is_empty() {
            for (name, failures) in failures_by_test {
                let total_failures: usize = failures.values().map(|x| x.len()).sum();

                println!(
                    "test failed {} times in {} ways: {}",
                    total_failures,
                    failures.len(),
                    name,
                );
                for (error, log_dirs) in failures {
                    println!(
                        "  error occurred {} times in this test: {}",
                        log_dirs.len(),
                        error
                    );
                    for dir in log_dirs {
                        println!("    {}", dir.to_string_lossy());
                    }
                }
                println!();
            }
        }

        println!("\nFailures by mode\n");
        if !failures_by_mode.is_empty() {
            for (error, tests) in failures_by_mode {
                let total_failures: usize = tests.values().map(|x| x.len()).sum();

                println!(
                    "error occurred {} times in {} tests: {}",
                    total_failures,
                    tests.len(),
                    error
                );
                for (name, log_dirs) in tests {
                    println!(
                        "  test failed {} times in this way: {}",
                        log_dirs.len(),
                        name
                    );
                    for dir in log_dirs {
                        println!("    {}", dir.to_string_lossy());
                    }
                }
                println!();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::PathBuf;

        /// Writes a test output directory of the shape petri produces.
        ///
        /// `markers` are `(filename, contents)` pairs, mirroring the
        /// `petri.test` start marker and the result marker petri writes.
        fn write_test_dir(root: &Path, dir_name: &str, markers: &[(&str, &str)], jsonl: &str) {
            let dir = root.join(dir_name);
            fs_err::create_dir_all(&dir).unwrap();
            for (marker_name, contents) in markers {
                fs_err::write(dir.join(marker_name), contents).unwrap();
            }
            if !jsonl.is_empty() {
                fs_err::write(dir.join("petri.jsonl"), jsonl).unwrap();
            }
        }

        fn entry(severity: &str, source: &str, message: &str) -> String {
            format!(
                r#"{{"timestamp":"2026-07-27T00:00:00Z","source":"{source}","severity":"{severity}","message":"{message}"}}"#
            )
        }

        #[test]
        fn excerpt_keeps_only_error_and_warn() {
            let jsonl = [
                entry("INFO", "petri", "booting"),
                entry("WARN", "serial", "something odd"),
                entry("DEBUG", "petri", "noise"),
                entry("ERROR", "petri", "test failed"),
            ]
            .join("\n");

            let excerpt = excerpt_from_petri_jsonl(jsonl.as_bytes());

            assert_eq!(
                excerpt,
                vec![
                    "[ WARN] [    serial] something odd".to_owned(),
                    "[ERROR] [     petri] test failed".to_owned(),
                ]
            );
        }

        #[test]
        fn excerpt_skips_malformed_lines() {
            let jsonl = format!(
                "not json\n{}\n\n{{\"unterminated\":\n",
                entry("ERROR", "petri", "kept")
            );

            let excerpt = excerpt_from_petri_jsonl(jsonl.as_bytes());

            assert_eq!(excerpt, vec!["[ERROR] [     petri] kept".to_owned()]);
        }

        #[test]
        fn excerpt_is_truncated_to_the_tail() {
            let jsonl = (0..MAX_EXCERPT_LINES + 5)
                .map(|i| entry("ERROR", "petri", &format!("line {i}")))
                .collect::<Vec<_>>()
                .join("\n");

            let excerpt = excerpt_from_petri_jsonl(jsonl.as_bytes());

            assert_eq!(excerpt.len(), MAX_EXCERPT_LINES);
            // The oldest entries are dropped, not the newest.
            assert!(excerpt.first().unwrap().ends_with("line 5"));
            assert!(
                excerpt
                    .last()
                    .unwrap()
                    .ends_with(&format!("line {}", MAX_EXCERPT_LINES + 4))
            );
        }

        #[test]
        fn excerpt_of_a_large_log_keeps_only_the_tail() {
            // Far more entries than can be excerpted, to exercise the bounded
            // ring buffer over a log that is never held in memory whole.
            let mut jsonl = String::new();
            for i in 0..50_000 {
                jsonl.push_str(&entry("ERROR", "petri", &format!("boom {i}")));
                jsonl.push('\n');
            }

            let excerpt = excerpt_from_petri_jsonl(jsonl.as_bytes());

            assert_eq!(excerpt.len(), MAX_EXCERPT_LINES);
            assert!(
                excerpt
                    .first()
                    .unwrap()
                    .ends_with(&format!("boom {}", 50_000 - MAX_EXCERPT_LINES))
            );
            assert!(excerpt.last().unwrap().ends_with("boom 49999"));
        }

        #[test]
        fn excerpt_of_empty_input_is_empty() {
            assert!(excerpt_from_petri_jsonl(b"".as_slice()).is_empty());
        }

        #[test]
        fn collect_finds_failed_and_unstable_but_not_passed() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            write_test_dir(
                root,
                "x86_64__zzz_failed",
                &[
                    ("petri.test", "x86_64::zzz_failed"),
                    ("petri.failed", "guest did not boot"),
                ],
                &entry("ERROR", "petri", "boom"),
            );
            write_test_dir(
                root,
                "x86_64__aaa_unstable",
                &[
                    ("petri.test", "x86_64::aaa_unstable"),
                    ("petri.failed_unstable", "flaked"),
                ],
                "",
            );
            write_test_dir(
                root,
                "x86_64__passed",
                &[("petri.test", "x86_64::passed"), ("petri.passed", "")],
                "",
            );

            let failures = collect_failed_tests(root).unwrap();

            // Sorted by test name, so the unstable one comes first.
            let names: Vec<_> = failures.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(names, ["x86_64::aaa_unstable", "x86_64::zzz_failed"]);
            assert!(failures[0].outcome == Outcome::FailedUnstable);
            assert_eq!(failures[0].error.as_deref(), Some("flaked"));
            assert!(failures[0].excerpt.is_empty());
            assert!(failures[1].outcome == Outcome::Failed);
            assert_eq!(failures[1].error.as_deref(), Some("guest did not boot"));
            assert_eq!(failures[1].excerpt, ["[ERROR] [     petri] boom"]);
            assert_eq!(failures[1].dir_name, "x86_64__zzz_failed");
        }

        #[test]
        fn collect_reports_a_started_test_with_no_result_as_incomplete() {
            let tmp = tempfile::tempdir().unwrap();
            // A test killed by a timeout, or one that crashed hard enough to
            // skip unwinding, never writes a result marker.
            write_test_dir(
                tmp.path(),
                "x86_64__hung",
                &[("petri.test", "x86_64::hung")],
                "",
            );

            let failures = collect_failed_tests(tmp.path()).unwrap();

            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].name, "x86_64::hung");
            assert!(failures[0].outcome == Outcome::Incomplete);
            assert!(failures[0].error.is_none());
        }

        #[test]
        fn collect_collapses_multiline_errors() {
            let tmp = tempfile::tempdir().unwrap();
            write_test_dir(
                tmp.path(),
                "x86_64__boot",
                &[(
                    "petri.failed",
                    "test panicked: assertion failed\n  left: []\n right: [1]\n",
                )],
                "",
            );

            let failures = collect_failed_tests(tmp.path()).unwrap();

            assert_eq!(
                failures[0].error.as_deref(),
                Some("test panicked: assertion failed left: [] right: [1]")
            );
        }

        #[test]
        fn collect_truncates_a_very_long_error() {
            let tmp = tempfile::tempdir().unwrap();
            write_test_dir(
                tmp.path(),
                "x86_64__boot",
                &[("petri.failed", &"x".repeat(MAX_REASON_CHARS * 2))],
                "",
            );

            let failures = collect_failed_tests(tmp.path()).unwrap();

            let error = failures[0].error.as_deref().unwrap();
            assert_eq!(error.chars().count(), MAX_REASON_CHARS + 1);
            assert!(error.ends_with('…'));
        }

        #[test]
        fn collect_falls_back_to_dir_name_without_a_start_marker() {
            let tmp = tempfile::tempdir().unwrap();
            write_test_dir(tmp.path(), "x86_64__boot", &[("petri.failed", "")], "");

            let failures = collect_failed_tests(tmp.path()).unwrap();

            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].name, "x86_64::boot");
            assert!(failures[0].error.is_none());
        }

        #[test]
        fn collect_ignores_loose_files_and_missing_dir() {
            let tmp = tempfile::tempdir().unwrap();
            fs_err::write(tmp.path().join("junit.xml"), "<testsuites/>").unwrap();

            assert!(collect_failed_tests(tmp.path()).unwrap().is_empty());
            assert!(
                collect_failed_tests(&PathBuf::from("this/does/not/exist"))
                    .unwrap()
                    .is_empty()
            );
        }

        #[test]
        fn summary_warning_is_a_single_workflow_command() {
            // A multi-line error must not break the workflow command, which is
            // terminated by the first newline.
            let rendered = render_summary_warning("broke:\n  because\n  reasons");

            assert_eq!(
                rendered,
                "::warning title=vmm test summary::broke: because reasons\n"
            );
            assert_eq!(rendered.lines().count(), 1);
        }

        #[test]
        fn percent_encoding_escapes_unsafe_characters() {
            // Ordinary test and artifact names pass through untouched.
            assert_eq!(percent_encode("x86_64__boot"), "x86_64__boot");
            assert_eq!(
                percent_encode("x64-linux-vmm-tests-logs"),
                "x64-linux-vmm-tests-logs"
            );
            // Parameterized test names can contain characters that would
            // otherwise break the URL.
            assert_eq!(percent_encode("boot[vbs]"), "boot%5Bvbs%5D");
            assert_eq!(percent_encode("a/b?c#d"), "a%2Fb%3Fc%23d");
        }
    }
}
