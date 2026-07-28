// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local job: build OpenVMM, build the `virtio_villain_tests` nextest suite
//! from source, and run it against OpenVMM.

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
        /// Target triple to build and run the tests for.
        pub target: CommonTriple,
        /// Also run known-failing (ignored) villain tests.
        pub run_ignored: bool,
        /// Optional nextest filter expression to run only a subset of tests.
        pub nextest_filter_expr: Option<String>,
        /// Directory to stage test content into and publish per-test logs
        /// (JUnit + petri logs) under.
        pub test_content_dir: PathBuf,
        /// Nextest profile to run under (e.g. `ci` to emit JUnit).
        pub nextest_profile: NextestProfile,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::init_cross_build::Node>();
        ctx.import::<crate::test_virtio_villain::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            run_ignored,
            nextest_filter_expr,
            test_content_dir,
            nextest_profile,
            done,
        } = request;

        let target_triple = target.as_triple();

        let test_content_dir = test_content_dir.absolute()?;

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

        // Build env for the test binary compilation (native, so effectively a
        // no-op, but keeps cross-build parity with the vmm_tests runner).
        let build_env = ctx.reqv(|v| crate::init_cross_build::Request {
            target: target_triple.clone(),
            injected_env: v,
        });

        let build_params = NextestBuildParams {
            packages: ReadVar::from_static(TestPackages::Crates {
                crates: vec!["virtio_villain_tests".into()],
            }),
            features: Default::default(),
            no_default_features: false,
            target: target_triple.clone(),
            profile: CargoBuildProfile::Debug,
            extra_env: build_env,
        };

        ctx.req(crate::test_virtio_villain::Request {
            target: target_triple,
            openvmm,
            run_kind: NextestRunKind::BuildAndRun(build_params),
            nextest_profile,
            nextest_filter_expr,
            run_ignored,
            test_content_dir: ReadVar::from_static(test_content_dir.clone()),
            junit_test_label: "virtio-villain-tests".into(),
            // Publish JUnit + logs into the same content dir (mirrors
            // vmm-tests-run), so a local run leaves a self-contained results
            // tree under `--dir` rather than in the internal flowey work dir.
            artifact_dir: Some(ReadVar::from_static(test_content_dir)),
            // Local dev machines are assumed already provisioned; don't install
            // deps or chmod /dev/kvm out from under the developer.
            install_deps: false,
            disable_remote_artifacts: false,
            done,
        });

        Ok(())
    }
}
