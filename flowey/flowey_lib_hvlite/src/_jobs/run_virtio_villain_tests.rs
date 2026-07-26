// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Job: build OpenVMM, stage the openvmm-deps guest kernel, resolve the
//! virtio-villain artifact, and run the `virtio_villain_tests` nextest suite
//! against OpenVMM.
//!
//! This is the "thin sibling" of the full `vmm_tests` runner: the villain crate
//! resolves everything else it needs (the guest Linux kernel, the OpenVMM
//! binary) from the known-paths magicpath / target dir, so all this job has to
//! do is make those two things exist and hand the crate its guest artifact via
//! the `VILLAIN_INITRAMFS` / `VILLAIN_TSV` env vars.
//!
//! It serves two callers: the `virtio-villain-run` xflowey pipeline (local
//! dev, `NextestProfile::Default`, no publishing) and the checkin-gates CI job
//! (`NextestProfile::Ci`, `publish: Some(..)` so JUnit + per-test petri logs
//! reach the logview website via the `upload-petri-results` workflow).
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

/// Publish JUnit + per-test petri logs so results reach the logview website.
#[derive(Serialize, Deserialize)]
pub struct VillainPublish {
    /// Artifact label prefix for the published results. To be picked up by the
    /// `upload-petri-results` workflow (which globs `*-vmm-tests-logs`), this
    /// MUST end in `-vmm-tests` so the logs attachment becomes
    /// `<label>-vmm-tests-logs`.
    pub junit_test_label: String,
    /// Local-backend only: copy JUnit + logs into this published artifact dir.
    pub artifact_dir: Option<ReadVar<PathBuf>>,
}

flowey_request! {
    pub struct Params {
        /// Guest/host architecture to test. Phase 1 is Linux-only (KVM).
        pub arch: CommonArch,
        /// Also run known-failing (ignored) villain tests.
        pub run_ignored: bool,
        /// Nextest profile: `Default` for local, `Ci` to emit JUnit.
        pub nextest_profile: NextestProfile,
        /// When set, publish results (JUnit + petri logs) for logview.
        pub publish: Option<VillainPublish>,
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
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            arch,
            run_ignored,
            nextest_profile,
            publish,
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

        // When publishing, route petri's per-test logs into a dedicated dir so
        // we can hand it to publish_test_results as the "logs" attachment.
        let test_log_path = if publish.is_some() {
            let (path, write_path) = ctx.new_var();
            ctx.emit_rust_step("create virtio-villain test output dir", |ctx| {
                let write_path = write_path.claim(ctx);
                move |rt| {
                    let dir = std::env::current_dir()?
                        .join("vmm_test_results")
                        .join("virtio_villain");
                    fs_err::create_dir_all(&dir)?;
                    rt.write(write_path, &dir);
                    Ok(())
                }
            });
            Some(path)
        } else {
            None
        };

        // Hand the two villain files (and, when publishing, TEST_OUTPUT_PATH) to
        // the crate via env vars.
        let (run_env, write_run_env) = ctx.new_var();
        ctx.emit_minor_rust_step("assemble virtio-villain test env", |ctx| {
            let villain = villain.claim(ctx);
            let test_log_path = test_log_path.clone().map(|p| p.claim(ctx));
            let write_run_env = write_run_env.claim(ctx);
            move |rt| {
                let a = rt.read(villain);
                let mut env = BTreeMap::from([
                    (
                        "VILLAIN_INITRAMFS".to_string(),
                        a.initramfs.display().to_string(),
                    ),
                    ("VILLAIN_TSV".to_string(), a.tsv.display().to_string()),
                ]);
                if let Some(p) = test_log_path {
                    let p = rt.read(p);
                    env.insert("TEST_OUTPUT_PATH".to_string(), p.display().to_string());
                }
                rt.write(write_run_env, &env);
            }
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
            nextest_profile,
            nextest_filter_expr: None,
            nextest_working_dir: None,
            nextest_config_file: None,
            run_ignored,
            extra_env: Some(run_env),
            pre_run_deps: vec![openvmm_built, magicpath_done],
            results,
        });

        // When publishing, upload JUnit + per-test petri logs so the
        // upload-petri-results workflow forwards them to logview. The publish
        // step is claimed by the report step below, so logs upload even when
        // tests fail.
        let reported_results = if let Some(VillainPublish {
            junit_test_label,
            artifact_dir,
        }) = publish
        {
            let test_log_path = test_log_path
                .expect("test_log_path is set whenever publish is set")
                .depending_on(ctx, &results);
            let junit_xml = results.clone().map(ctx, |r| r.junit_xml);
            Some(
                ctx.reqv(|v| flowey_lib_common::publish_test_results::Request {
                    junit_xml,
                    test_label: junit_test_label,
                    attachments: BTreeMap::from([("logs".to_string(), (test_log_path, false))]),
                    output_dir: artifact_dir,
                    done: v,
                }),
            )
        } else {
            None
        };

        ctx.emit_rust_step("report virtio-villain test results", |ctx| {
            if let Some(reported_results) = reported_results {
                reported_results.claim(ctx);
            }
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
