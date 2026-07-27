// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared core for running the `virtio_villain_tests` nextest suite against a
//! staged OpenVMM under KVM.
//!
//! This is the villain analogue of [`crate::test_nextest_vmm_tests_archive`]:
//! it owns everything both the local build-and-run job
//! ([`crate::_jobs::run_virtio_villain_tests`]) and the CI consume-archive job
//! ([`crate::_jobs::consume_and_test_nextest_virtio_villain_archive`]) have in
//! common, so those jobs stay thin and never drift apart. The only thing that
//! differs between them — build from source vs. run a prebuilt archive — is
//! expressed through the [`NextestRunKind`] passed in as [`Request::run_kind`].
//!
//! Concretely, this node stages OpenVMM + the linux-direct guest kernel into a
//! test content dir (via [`crate::init_vmm_tests_env`], which also exports
//! `TEST_OUTPUT_PATH` for the publishable per-test petri logs), resolves the
//! villain guest artifact (initramfs + `tests.tsv`) into the `VILLAIN_*` env
//! vars, runs the nextest suite, and publishes the JUnit + per-test logs so the
//! `upload-petri-results` workflow forwards them to the logview website.

use crate::build_openvmm::OpenvmmOutput;
use crate::common::CommonArch;
use crate::common::CommonTriple;
use crate::install_vmm_tests_deps::VmmTestsDepSelections;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_nextest_run::NextestRunKind;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// Guest/host architecture to test. Phase 1 is Linux-only (KVM).
        pub arch: CommonArch,
        /// The OpenVMM binary the villain crate launches (built from source by
        /// the local job, or resolved from an artifact by the consume job).
        pub openvmm: ReadVar<OpenvmmOutput>,
        /// How to run the suite: build it from source ([`NextestRunKind::BuildAndRun`])
        /// or run a prebuilt nextest archive ([`NextestRunKind::RunFromArchive`]).
        pub run_kind: NextestRunKind,
        /// Nextest profile to run under (e.g. `ci` to emit JUnit).
        pub nextest_profile: NextestProfile,
        /// Optional nextest filter expression to run only a subset of tests.
        pub nextest_filter_expr: Option<String>,
        /// Also run known-failing (ignored) villain tests.
        pub run_ignored: bool,
        /// Directory to stage test content into and root the per-test logs at.
        pub test_content_dir: ReadVar<PathBuf>,
        /// Artifact label prefix for the published results. To be picked up by
        /// the `upload-petri-results` workflow (which globs `*-vmm-tests-logs`),
        /// this MUST end in `-vmm-tests` so the logs attachment becomes
        /// `<junit_test_label>-logs`.
        pub junit_test_label: String,
        /// Local-backend only: copy JUnit + logs into this published artifact
        /// dir.
        pub artifact_dir: Option<ReadVar<PathBuf>>,
        /// Install the vmm-tests runtime dependencies (and grant `/dev/kvm`
        /// access on CI machines) before running. The local job leaves this to
        /// the developer's already-provisioned machine.
        pub install_deps: bool,
        /// Forwarded to [`crate::init_vmm_tests_env`]: whether to forbid
        /// downloading remote test artifacts (CI test machines provide
        /// everything locally).
        pub disable_remote_artifacts: bool,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::init_vmm_tests_env::Node>();
        ctx.import::<crate::install_vmm_tests_deps::Node>();
        ctx.import::<crate::resolve_virtio_villain::Node>();
        ctx.import::<crate::run_cargo_nextest_run::Node>();
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            arch,
            openvmm,
            run_kind,
            nextest_profile,
            nextest_filter_expr,
            run_ignored,
            test_content_dir,
            junit_test_label,
            artifact_dir,
            install_deps,
            disable_remote_artifacts,
            done,
        } = request;

        // Phase 1: Linux host only (villain drives OpenVMM under KVM).
        let target = match arch {
            CommonArch::X86_64 => CommonTriple::X86_64_LINUX_GNU,
            CommonArch::Aarch64 => CommonTriple::AARCH64_LINUX_GNU,
        }
        .as_triple();

        // Stage OpenVMM + the linux-direct guest kernel into the content dir and
        // get the env the villain crate needs (VMM_TESTS_CONTENT_DIR points the
        // known-paths resolver at OpenVMM + the kernel; TEST_OUTPUT_PATH is the
        // publishable per-test log dir). All the other `register_*` inputs are
        // vmm_tests-specific and villain does not use them.
        let (test_log_path, get_test_log_path) = ctx.new_var();
        let base_env = ctx.reqv(|get_env| crate::init_vmm_tests_env::Request {
            test_content_dir: test_content_dir.clone(),
            vmm_tests_target: target.clone(),
            register_openvmm: Some(openvmm),
            register_openvmm_vhost: None,
            register_pipette_windows: None,
            register_pipette_linux_musl: None,
            register_guest_test_uefi: None,
            register_tmks: None,
            register_tmk_vmm: None,
            register_tmk_vmm_linux_musl: None,
            register_vmgstool: None,
            register_vmgstool_dev: None,
            register_tpm_guest_tests_windows: None,
            register_tpm_guest_tests_linux: None,
            register_test_igvm_agent_rpc_server: None,
            disk_images_dir: None,
            register_openhcl_igvm_files: Vec::new(),
            get_test_log_path: Some(get_test_log_path),
            get_env,
            release_igvm_files: None,
            use_relative_paths: false,
            disable_remote_artifacts,
            reuse_prepped_vhds: false,
            // Linux-direct only: skip the UEFI firmware and Windows virtio-win
            // driver downloads, which villain never uses.
            stage_uefi_and_virtio_win: false,
        });

        // Resolve the villain guest artifact (initramfs + tests.tsv) and merge
        // its env vars into the base env.
        let villain = ctx.reqv(|v| crate::resolve_virtio_villain::Request::Get(arch, v));
        let extra_env = base_env.zip(ctx, villain).map(ctx, |(mut env, a)| {
            env.insert(
                "VILLAIN_INITRAMFS".to_string(),
                a.initramfs.display().to_string(),
            );
            env.insert("VILLAIN_TSV".to_string(), a.tsv.display().to_string());
            env
        });

        let mut pre_run_deps = Vec::new();
        if install_deps {
            // Runtime dependencies for the test machine (no Rust toolchain
            // needed to *run* an archive).
            ctx.config(crate::install_vmm_tests_deps::Config {
                selections: Some(VmmTestsDepSelections::Linux),
                auto_install: None,
            });
            pre_run_deps.push(ctx.reqv(crate::install_vmm_tests_deps::Request::Install));

            // Make /dev/kvm accessible to the test (CI machines only).
            if !matches!(ctx.backend(), FlowBackend::Local) {
                pre_run_deps.push(ctx.emit_rust_step(
                    "ensure hypervisor device is accessible",
                    |_| {
                        |rt| {
                            if Path::new("/dev/kvm").exists() {
                                flowey::shell_cmd!(rt, "sudo chmod a+rw /dev/kvm").run()?;
                            }
                            Ok(())
                        }
                    },
                ));
            }
        }

        // Use the repo's nextest config so the villain per-test slow-timeout
        // overrides (`.config/nextest.toml`) apply.
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let nextest_config_file = openvmm_repo_path
            .clone()
            .map(ctx, |p| p.join(".config").join("nextest.toml"));

        let results = ctx.reqv(|results| crate::run_cargo_nextest_run::Request {
            friendly_name: "virtio_villain_tests".into(),
            run_kind,
            nextest_profile,
            nextest_filter_expr,
            nextest_working_dir: Some(openvmm_repo_path),
            nextest_config_file: Some(nextest_config_file),
            run_ignored,
            extra_env: Some(extra_env),
            pre_run_deps,
            results,
        });

        // Publish JUnit + per-test petri logs so the upload-petri-results
        // workflow forwards them to logview. The publish step is claimed by the
        // report step below, so logs upload even when tests fail.
        let test_log_path = test_log_path.depending_on(ctx, &results);
        let junit_xml = results.clone().map(ctx, |r| r.junit_xml);
        let reported_results = ctx.reqv(|v| flowey_lib_common::publish_test_results::Request {
            junit_xml,
            test_label: junit_test_label,
            attachments: BTreeMap::from([("logs".to_string(), (test_log_path, false))]),
            output_dir: artifact_dir,
            done: v,
        });

        ctx.emit_rust_step("report virtio-villain test results", |ctx| {
            reported_results.claim(ctx);
            done.claim(ctx);
            let results = results.claim(ctx);
            move |rt| {
                let results = rt.read(results);
                if !results.all_tests_passed {
                    anyhow::bail!("virtio-villain tests failed");
                }
                Ok(())
            }
        });

        Ok(())
    }
}
