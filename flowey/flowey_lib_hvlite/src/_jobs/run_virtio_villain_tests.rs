// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local job: build OpenVMM, stage the openvmm-deps guest kernel, resolve the
//! virtio-villain artifact, and run the `virtio_villain_tests` nextest suite
//! against OpenVMM.
//!
//! This is the *local* (xflowey `virtio-villain-run`) runner: it builds
//! everything from source on the developer's machine and runs it in one shot.
//! CI instead splits the work — [`crate::build_nextest_virtio_villain_tests`]
//! builds a nextest archive on a build machine and
//! [`crate::_jobs::consume_and_test_nextest_virtio_villain_archive`] runs it on
//! a KVM test machine that has no Rust toolchain.
//!
//! The villain crate resolves everything else it needs (the guest Linux kernel,
//! the OpenVMM binary) from the known-paths magicpath / target dir, so all this
//! job has to do is make those two things exist and hand the crate its guest
//! artifact via the `VILLAIN_INITRAMFS` / `VILLAIN_TSV` env vars.
//!
//! Known-failing villain tests are marked *ignored* by the harness, so they are
//! skipped by default. Pass `run_ignored` to run them too (e.g. during fix
//! development).

use crate::common::CommonArch;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoBuildProfile;
use flowey_lib_common::run_cargo_nextest_run::NextestRunKind;
use flowey_lib_common::run_cargo_nextest_run::build_params::NextestBuildParams;
use flowey_lib_common::run_cargo_nextest_run::build_params::TestPackages;

flowey_request! {
    pub struct Params {
        /// Guest/host architecture to test. Phase 1 is Linux-only (KVM).
        pub arch: CommonArch,
        /// Also run known-failing (ignored) villain tests.
        pub run_ignored: bool,
        /// Optional nextest filter expression to run only a subset of tests.
        pub nextest_filter_expr: Option<String>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::init_cross_build::Node>();
        ctx.import::<crate::init_vmm_tests_env::Node>();
        ctx.import::<crate::resolve_virtio_villain::Node>();
        ctx.import::<crate::run_cargo_nextest_run::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            arch,
            run_ignored,
            nextest_filter_expr,
            done,
        } = request;

        // Phase 1: Linux host only (villain drives OpenVMM under KVM).
        let target = match arch {
            CommonArch::X86_64 => CommonTriple::X86_64_LINUX_GNU,
            CommonArch::Aarch64 => CommonTriple::AARCH64_LINUX_GNU,
        };

        // Build the OpenVMM binary the villain crate launches.
        let openvmm = ctx.reqv(|v| crate::build_openvmm::Request {
            params: crate::build_openvmm::OpenvmmBuildParams {
                target: target.clone(),
                profile: CommonProfile::Debug,
                features: Default::default(),
            },
            version: None,
            openvmm: v,
        });

        // Stage OpenVMM + the linux-direct guest kernel into a content dir and
        // get the env the villain crate needs. Reusing `init_vmm_tests_env`
        // (exactly as the vmm_tests local + CI runners do) is what makes the
        // known-paths resolver find OpenVMM and the kernel via
        // `VMM_TESTS_CONTENT_DIR`, regardless of the nextest target triple.
        let test_content_dir = ctx.emit_rust_stepv("creating new test content dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });
        let base_env = ctx.reqv(|get_env| crate::init_vmm_tests_env::Request {
            test_content_dir: test_content_dir.clone(),
            vmm_tests_target: target.as_triple(),
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
            get_test_log_path: None,
            get_env,
            release_igvm_files: None,
            use_relative_paths: false,
            disable_remote_artifacts: false,
            reuse_prepped_vhds: false,
            // Linux-direct only: skip the UEFI firmware and Windows virtio-win
            // driver downloads, which villain never uses.
            stage_uefi_and_virtio_win: false,
        });

        // Resolve the virtio-villain guest artifact (initramfs + tests.tsv) and
        // merge its env vars into the base env.
        let villain = ctx.reqv(|v| crate::resolve_virtio_villain::Request::Get(arch, v));
        let run_env = base_env.zip(ctx, villain).map(ctx, |(mut env, a)| {
            env.insert(
                "VILLAIN_INITRAMFS".to_string(),
                a.initramfs.display().to_string(),
            );
            env.insert("VILLAIN_TSV".to_string(), a.tsv.display().to_string());
            env
        });

        // Build env for the test binary compilation (native, so effectively a
        // no-op, but keeps cross-build parity with the vmm_tests runner).
        let build_env = ctx.reqv(|v| crate::init_cross_build::Request {
            target: target.as_triple(),
            injected_env: v,
        });

        // Use the repo's nextest config so the villain per-test slow-timeout
        // overrides (`.config/nextest.toml`) apply.
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let nextest_config_file =
            openvmm_repo_path.map(ctx, |p| p.join(".config").join("nextest.toml"));

        let build_params = NextestBuildParams {
            packages: ReadVar::from_static(TestPackages::Crates {
                crates: vec!["virtio_villain_tests".into()],
            }),
            features: Default::default(),
            no_default_features: false,
            target: target.as_triple(),
            profile: CargoBuildProfile::Debug,
            extra_env: build_env,
        };

        let results = ctx.reqv(|results| crate::run_cargo_nextest_run::Request {
            friendly_name: "virtio_villain_tests".into(),
            run_kind: NextestRunKind::BuildAndRun(build_params),
            nextest_profile: NextestProfile::Default,
            nextest_filter_expr,
            nextest_working_dir: None,
            nextest_config_file: Some(nextest_config_file),
            run_ignored,
            extra_env: Some(run_env),
            pre_run_deps: vec![],
            results,
        });

        ctx.emit_rust_step("report virtio-villain test results", |ctx| {
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
