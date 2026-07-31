// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local job: build OpenVMM, build the `virtio_villain_tests` nextest suite
//! from source, and run it against OpenVMM.

use crate::common::CommonProfile;
use crate::common::CommonTriple;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        /// Target triple to build and run the tests for.
        pub target: CommonTriple,
        /// Optional incubator profile used as the nextest target runner.
        pub incubator_profile: Option<PathBuf>,
        /// Build OpenVMM and the tests with the release profile.
        pub release: bool,
        /// Build and stage the test archive without running it.
        pub build_only: bool,
        /// Copy split debug information into the output directory.
        pub copy_extras: bool,
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
        crate::configure_nextest_incubator::imports(ctx);
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::build_nextest_virtio_villain_tests::Node>();
        ctx.import::<crate::test_virtio_villain::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            incubator_profile,
            release,
            build_only,
            copy_extras,
            run_ignored,
            nextest_filter_expr,
            test_content_dir,
            nextest_profile,
            done,
        } = request;

        let target_triple = target.as_triple();

        let test_content_dir = test_content_dir.absolute()?;
        let profile = CommonProfile::from_release(release);

        // Build the OpenVMM binary the villain crate launches.
        let openvmm = ctx.reqv(|v| crate::build_openvmm::Request {
            params: crate::build_openvmm::OpenvmmBuildParams {
                target: target.clone(),
                profile,
                features: Default::default(),
            },
            version: None,
            openvmm: v,
        });

        let archive = ctx.reqv(
            |archive| crate::build_nextest_virtio_villain_tests::Request {
                target: target_triple,
                profile,
                archive,
            },
        );

        if build_only {
            ctx.emit_rust_step("stage virtio-villain build outputs", |ctx| {
                let openvmm = openvmm.claim(ctx);
                let archive = archive.claim(ctx);
                let done = done.claim(ctx);
                move |rt| {
                    let openvmm = rt.read(openvmm);
                    let archive = rt.read(archive);
                    fs_err::create_dir_all(&test_content_dir)?;
                    match openvmm {
                        crate::build_openvmm::OpenvmmOutput::LinuxBin { bin, dbg } => {
                            fs_err::copy(bin, test_content_dir.join("openvmm"))?;
                            if copy_extras {
                                let extras = test_content_dir.join("extras");
                                fs_err::create_dir_all(&extras)?;
                                fs_err::copy(dbg, extras.join("openvmm.dbg"))?;
                            }
                        }
                        crate::build_openvmm::OpenvmmOutput::WindowsBin { .. } => unreachable!(),
                    }
                    fs_err::copy(
                        archive.archive_file,
                        test_content_dir.join("virtio_villain_tests.tar.zst"),
                    )?;
                    rt.write(done, &());
                    Ok(())
                }
            });
            return Ok(());
        }

        let copy_extras_done = copy_extras.then(|| {
            ctx.emit_rust_step("copy virtio-villain build extras", |ctx| {
                let openvmm = openvmm.clone().claim(ctx);
                let test_content_dir = test_content_dir.clone();
                move |rt| {
                    let crate::build_openvmm::OpenvmmOutput::LinuxBin { dbg, .. } =
                        rt.read(openvmm)
                    else {
                        unreachable!()
                    };
                    let extras = test_content_dir.join("extras");
                    fs_err::create_dir_all(&extras)?;
                    fs_err::copy(dbg, extras.join("openvmm.dbg"))?;
                    Ok(())
                }
            })
        });

        let (tests_done, tests_done_write) = ctx.new_var();
        let nextest_archive_file = archive.map(ctx, |archive| archive.archive_file);
        ctx.req(crate::test_virtio_villain::Request {
            target,
            openvmm,
            nextest_archive_file,
            incubator_profile,
            profile,
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
            done: tests_done_write,
        });

        ctx.emit_side_effect_step(copy_extras_done.into_iter().chain([tests_done]), [done]);

        Ok(())
    }
}
