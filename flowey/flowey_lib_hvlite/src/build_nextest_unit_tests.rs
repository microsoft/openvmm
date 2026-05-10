// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build all cargo-nextest based unit-tests in the OpenVMM workspace.
//!
//! In the context of OpenVMM, we consider a "unit-test" to be any test which
//! doesn't require any special dependencies (e.g: additional binaries, disk
//! images, etc...), and can be run simply by invoking the test bin itself.

use crate::common::CommonArch;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoBuildProfile;
use flowey_lib_common::run_cargo_build::CargoFeatureSet;
use flowey_lib_common::run_cargo_nextest_run::TestResults;
use flowey_lib_common::run_cargo_nextest_run::build_params::NextestBuildParams;
use flowey_lib_common::run_cargo_nextest_run::build_params::TestPackages;
use std::collections::BTreeMap;

/// Type-safe wrapper around a built nextest archive containing unit tests
#[derive(Serialize, Deserialize)]
pub struct NextestUnitTestArchive {
    #[serde(rename = "unit_tests.tar.zst")]
    pub archive_file: PathBuf,
}

/// Build mode to use when building the nextest unit tests
#[derive(Serialize, Deserialize)]
pub enum BuildNextestUnitTestMode {
    /// Build, immediately run, and publish unit test results, side-stepping
    /// any intermediate archiving steps.
    ImmediatelyRun {
        nextest_profile: NextestProfile,
        /// Friendly label prefix used when publishing JUnit results. Each run
        /// is published with this prefix combined with the run's friendly
        /// name to ensure uniqueness within the pipeline.
        junit_test_label: String,
        /// If provided, also copy the published junit.xml files into this
        /// directory (only honored on local backends).
        artifact_dir: Option<ReadVar<PathBuf>>,
        /// Per-run test results, in the same order produced internally.
        results: WriteVar<Vec<TestResults>>,
        /// Signaled once every run's junit.xml has been published.
        publish_done: WriteVar<SideEffect>,
    },
    /// Build and archive the tests into nextest archive files, which can then
    /// be run via [`crate::test_nextest_unit_tests_archive`].
    Archive(WriteVar<Vec<NextestUnitTestArchive>>),
}

flowey_request! {
    pub struct Request {
        /// Build and run unit tests for the specified target
        pub target: target_lexicon::Triple,
        /// Build and run unit tests with the specified cargo profile
        pub profile: CommonProfile,
        /// Build mode to use when building the nextest unit tests
        pub build_mode: BuildNextestUnitTestMode,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_xtask::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::init_openvmm_magicpath_openhcl_sysroot::Node>();
        ctx.import::<crate::install_openvmm_rust_build_essential::Node>();
        ctx.import::<crate::run_cargo_nextest_run::Node>();
        ctx.import::<crate::init_cross_build::Node>();
        ctx.import::<flowey_lib_common::run_cargo_nextest_archive::Node>();
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let xtask_target = CommonTriple::Common {
            arch: ctx.arch().try_into()?,
            platform: ctx.platform().try_into()?,
        };
        let xtask = ctx.reqv(|v| crate::build_xtask::Request {
            target: xtask_target,
            xtask: v,
        });

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        // building these packages in the OpenVMM repo requires installing some
        // additional deps
        let ambient_deps = vec![ctx.reqv(crate::install_openvmm_rust_build_essential::Request)];

        let test_packages = ctx.emit_rust_stepv("determine unit test exclusions", |ctx| {
            let xtask = xtask.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.clone().claim(ctx);
            move |rt| {
                let xtask = rt.read(xtask);
                let openvmm_repo_path = rt.read(openvmm_repo_path);

                let mut exclude = [
                    // Skip VMM tests, they get run in a different step.
                    "vmm_tests",
                    // Skip guest_test_uefi, as it's a no_std UEFI crate
                    "guest_test_uefi",
                    // Skip crypto, handle it separately due to its non-additive features
                    "crypto",
                    // Exclude various proc_macro crates, since they don't compile successfully
                    // under --test with panic=abort targets.
                    // https://github.com/rust-lang/cargo/issues/4336 is tracking this.
                    //
                    // In any case though, it's not like these crates should have unit tests
                    // anyway.
                    "inspect_derive",
                    "mesh_derive",
                    "save_restore_derive",
                    "test_with_tracing_macro",
                    "pal_async_test",
                    "vmm_test_macros",
                ]
                .map(|x| x.to_string())
                .to_vec();

                // Exclude fuzz crates, since there libfuzzer-sys doesn't play
                // nice with unit tests
                {
                    let xtask_bin = match xtask {
                        crate::build_xtask::XtaskOutput::LinuxBin { bin, dbg: _ } => bin,
                        crate::build_xtask::XtaskOutput::WindowsBin { exe, pdb: _ } => exe,
                    };

                    rt.sh.change_dir(openvmm_repo_path);
                    let output =
                        flowey::shell_cmd!(rt, "{xtask_bin} fuzz list --crates").output()?;
                    let output = String::from_utf8(output.stdout)?;

                    let fuzz_crates = output.trim().split('\n').map(|s| s.to_owned());
                    exclude.extend(fuzz_crates);
                }

                Ok(TestPackages::Workspace { exclude })
            }
        });

        for Request {
            target,
            profile,
            build_mode,
        } in requests
        {
            let mut pre_run_deps = ambient_deps.clone();

            let sysroot_arch = CommonArch::from_architecture(target.architecture)?;

            // See comment in `crate::cargo_build` for why this is necessary.
            //
            // copied here since this node doesn't actually route through `cargo build`.
            if matches!(target.environment, target_lexicon::Environment::Musl) {
                pre_run_deps.push(
                    ctx.reqv(|v| crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                        arch: sysroot_arch,
                        path: v,
                    })
                    .into_side_effect(),
                );
            }

            // On windows, we can't run with all features, as many crates
            // require openSSL for crypto, which isn't supported yet.
            //
            // Adding the the "ci" feature is also used to skip certain tests
            // that fail in CI.
            let features = if matches!(
                target.operating_system,
                target_lexicon::OperatingSystem::Windows
            ) {
                CargoFeatureSet::Specific(vec!["ci".into()])
            } else {
                CargoFeatureSet::All
            };

            let injected_env = ctx.reqv(|v| crate::init_cross_build::Request {
                target: target.clone(),
                injected_env: v,
            });

            let base_build_params = NextestBuildParams {
                packages: test_packages.clone(),
                features,
                no_default_features: false,
                target: target.clone(),
                profile: match profile {
                    CommonProfile::Release => CargoBuildProfile::Release,
                    CommonProfile::Debug => CargoBuildProfile::Debug,
                },
                extra_env: injected_env,
            };

            // The first run is the main workspace run with --all-features.
            let mut runs: Vec<(String, NextestBuildParams)> =
                vec![("unit-tests".into(), base_build_params.clone())];

            // crypto has non-additive features, so it gets its own runs to
            // ensure full coverage of different backends. Always test the
            // 'native' no-feature and pure-rust backends. On linux additionally
            // test the openssl & symcrypt backends and --all-features fallback.
            // We could test openssl on non-linux targets too, but setting up
            // builds for them is a pain. We could test Symcrypt on non-musl
            // linux targets too, but we don't currently have a prebuilt
            // library for them.
            let mut crypto_feature_sets = vec![
                ("none", CargoFeatureSet::None),
                ("rust", CargoFeatureSet::Specific(vec!["rust".into()])),
            ];
            if matches!(
                target.operating_system,
                target_lexicon::OperatingSystem::Linux
            ) {
                crypto_feature_sets
                    .push(("openssl", CargoFeatureSet::Specific(vec!["openssl".into()])));
                // Only test the symcrypt backend on musl targets with our prebuilt lib
                if matches!(target.environment, target_lexicon::Environment::Musl) {
                    crypto_feature_sets.push((
                        "symcrypt",
                        CargoFeatureSet::Specific(vec!["symcrypt".into()]),
                    ));
                }
                crypto_feature_sets.push(("all", CargoFeatureSet::All));
            }
            for (name, features) in crypto_feature_sets {
                runs.push((
                    format!("unit-tests crypto ({})", name),
                    NextestBuildParams {
                        packages: ReadVar::from_static(TestPackages::Crates {
                            crates: vec!["crypto".into()],
                        }),
                        features,
                        ..base_build_params.clone()
                    },
                ));
            }

            match build_mode {
                BuildNextestUnitTestMode::ImmediatelyRun {
                    nextest_profile,
                    junit_test_label,
                    artifact_dir,
                    results,
                    publish_done,
                } => {
                    let test_results: Vec<_> = runs
                        .into_iter()
                        .map(|(friendly_name, build_params)| {
                            let r = ctx.reqv(|v| crate::run_cargo_nextest_run::Request {
                                friendly_name: friendly_name.clone(),
                                run_kind:
                                    flowey_lib_common::run_cargo_nextest_run::NextestRunKind::BuildAndRun(
                                        build_params,
                                    ),
                                nextest_profile,
                                nextest_filter_expr: None,
                                nextest_working_dir: None,
                                nextest_config_file: None,
                                run_ignored: false,
                                extra_env: None,
                                pre_run_deps: pre_run_deps.clone(),
                                results: v,
                            });
                            (friendly_name, r)
                        })
                        .collect();

                    // Emit a publish_test_results request per run, so each
                    // run's junit.xml gets uploaded with a distinct label.
                    let publish_dones: Vec<_> = test_results
                        .iter()
                        .map(|(friendly_name, r)| {
                            let junit_xml = r.clone().map(ctx, |t| t.junit_xml);
                            ctx.reqv(|v| flowey_lib_common::publish_test_results::Request {
                                junit_xml,
                                test_label: format!("{junit_test_label}-{friendly_name}"),
                                attachments: BTreeMap::new(),
                                output_dir: artifact_dir.clone(),
                                done: v,
                            })
                        })
                        .collect();

                    ctx.emit_minor_rust_step("merge unit test results", |ctx| {
                        let test_results = test_results
                            .into_iter()
                            .map(|(_, r)| r.claim(ctx))
                            .collect::<Vec<_>>();
                        let results = results.claim(ctx);
                        move |rt| {
                            let flattened = test_results.into_iter().map(|t| rt.read(t)).collect();
                            rt.write(results, &flattened);
                        }
                    });

                    ctx.emit_side_effect_step(publish_dones, [publish_done]);
                }
                BuildNextestUnitTestMode::Archive(unit_tests_archive) => {
                    let archive_files: Vec<_> = runs
                        .into_iter()
                        .map(|(friendly_name, build_params)| {
                            ctx.reqv(|v| flowey_lib_common::run_cargo_nextest_archive::Request {
                                friendly_label: friendly_name,
                                working_dir: openvmm_repo_path.clone(),
                                build_params,
                                pre_run_deps: pre_run_deps.clone(),
                                archive_file: v,
                            })
                        })
                        .collect();

                    ctx.emit_minor_rust_step("report built unit tests", |ctx| {
                        let archive_files = archive_files.claim(ctx);
                        let unit_tests = unit_tests_archive.claim(ctx);
                        |rt| {
                            let flattened = archive_files
                                .into_iter()
                                .map(|t| NextestUnitTestArchive {
                                    archive_file: rt.read(t),
                                })
                                .collect::<Vec<_>>();
                            rt.write(unit_tests, &flattened);
                        }
                    });
                }
            }
        }

        Ok(())
    }
}
