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
use std::collections::BTreeMap;

flowey_request! {
    pub struct Params {
        /// Guest/host architecture to test. Phase 1 is Linux-only (KVM).
        pub arch: CommonArch,
        /// Also run known-failing (ignored) villain tests.
        pub run_ignored: bool,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::init_cross_build::Node>();
        ctx.import::<crate::init_openvmm_magicpath_openvmm_deps::Node>();
        ctx.import::<crate::resolve_virtio_villain::Node>();
        ctx.import::<crate::run_cargo_nextest_run::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            arch,
            run_ignored,
            done,
        } = request;

        // Phase 1: Linux host only (villain drives OpenVMM under KVM).
        let target = match arch {
            CommonArch::X86_64 => CommonTriple::X86_64_LINUX_GNU,
            CommonArch::Aarch64 => CommonTriple::AARCH64_LINUX_GNU,
        };

        // Build the OpenVMM binary the villain crate launches. Building it for
        // the same target triple that nextest uses lands it next to the test
        // binary, where the known-paths resolver looks for it.
        let openvmm_built = ctx
            .reqv(|v| crate::build_openvmm::Request {
                params: crate::build_openvmm::OpenvmmBuildParams {
                    target: target.clone(),
                    profile: CommonProfile::Debug,
                    features: Default::default(),
                },
                version: None,
                openvmm: v,
            })
            .into_side_effect();

        // Stage the openvmm-deps guest Linux kernel (and shared deps) into the
        // magicpath, so the villain crate's linux-direct firmware resolves.
        let magicpath_done =
            ctx.reqv(|done| crate::init_openvmm_magicpath_openvmm_deps::Request { arch, done });

        // Resolve the virtio-villain guest artifact (initramfs + tests.tsv).
        let villain = ctx.reqv(|v| crate::resolve_virtio_villain::Request::Get(arch, v));

        // Hand the two villain files to the crate via env vars.
        let run_env = villain.map(ctx, |a| {
            BTreeMap::from([
                (
                    "VILLAIN_INITRAMFS".to_string(),
                    a.initramfs.display().to_string(),
                ),
                ("VILLAIN_TSV".to_string(), a.tsv.display().to_string()),
            ])
        });

        // Build env for the test binary compilation (native, so effectively a
        // no-op, but keeps cross-build parity with the vmm_tests runner).
        let build_env = ctx.reqv(|v| crate::init_cross_build::Request {
            target: target.as_triple(),
            injected_env: v,
        });

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
            nextest_filter_expr: None,
            nextest_working_dir: None,
            nextest_config_file: None,
            run_ignored,
            extra_env: Some(run_env),
            pre_run_deps: vec![openvmm_built, magicpath_done],
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
