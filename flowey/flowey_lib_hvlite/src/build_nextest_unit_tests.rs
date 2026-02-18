// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build all cargo-nextest based unit-tests in the OpenVMM workspace.
//!
//! In the context of OpenVMM, we consider a "unit-test" to be any test which
//! doesn't require any special dependencies (e.g: additional binaries, disk
//! images, etc...), and can be run simply by invoking the test bin itself.

use crate::init_openvmm_magicpath_openhcl_sysroot::OpenvmmSysrootArch;
use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonPlatform;
use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoBuildProfile;
use flowey_lib_common::run_cargo_build::CargoFeatureSet;
use flowey_lib_common::run_cargo_nextest_run::TestResults;
use flowey_lib_common::run_cargo_nextest_run::build_params::TestPackages;

/// Type-safe wrapper around a built nextest archive containing unit tests
#[derive(Serialize, Deserialize)]
pub struct NextestUnitTestArchive {
    #[serde(rename = "unit_tests.tar.zst")]
    pub archive_file: PathBuf,
}

/// Build mode to use when building the nextest unit tests
#[derive(Serialize, Deserialize)]
pub enum BuildNextestUnitTestMode {
    /// Build and immediate run unit tests, side-stepping any intermediate
    /// archiving steps.
    ImmediatelyRun {
        nextest_profile: NextestProfile,
        results: WriteVar<TestResults>,
    },
    /// Build and archive the tests into a nextest archive file, which can then
    /// be run via [`crate::test_nextest_unit_tests_archive`].
    Archive(WriteVar<NextestUnitTestArchive>),
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
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let flowey_platform = ctx.platform();
        let flowey_arch = ctx.arch();

        let xtask_target = CommonTriple::Common {
            arch: match flowey_arch {
                FlowArch::X86_64 => CommonArch::X86_64,
                FlowArch::Aarch64 => CommonArch::Aarch64,
                arch => anyhow::bail!("unsupported arch {arch}"),
            },
            platform: match flowey_platform {
                FlowPlatform::Windows => CommonPlatform::WindowsMsvc,
                FlowPlatform::Linux(_) => CommonPlatform::LinuxGnu,
                FlowPlatform::MacOs => CommonPlatform::MacOs,
                platform => anyhow::bail!("unsupported platform {platform}"),
            },
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
                    // TODO: document why we're skipping these first few crates
                    // (I'm just cargo-cult copying these exclusions)
                    "whp",
                    "kvm",
                    "openvmm",
                    // Skip VMM tests, they get run in a different step.
                    "vmm_tests",
                    // Skip guest_test_uefi, as it's a no_std UEFI crate
                    "guest_test_uefi",
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

            let sysroot_arch = match target.architecture {
                target_lexicon::Architecture::X86_64 => OpenvmmSysrootArch::X64,
                target_lexicon::Architecture::Aarch64(_) => OpenvmmSysrootArch::Aarch64,
                arch => anyhow::bail!("unsupported arch {arch}"),
            };

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

            // HACK: the following behavior has been cargo-culted from our old
            // CI, and at some point, we should actually improve the testing
            // story on windows, so that we can run with FeatureSet::All in CI.
            //
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

            let build_params =
                flowey_lib_common::run_cargo_nextest_run::build_params::NextestBuildParams {
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

            match build_mode {
                BuildNextestUnitTestMode::ImmediatelyRun {
                    nextest_profile,
                    results,
                } => ctx.req(crate::run_cargo_nextest_run::Request {
                    friendly_name: "unit-tests".into(),
                    run_kind: flowey_lib_common::run_cargo_nextest_run::NextestRunKind::BuildAndRun(
                        build_params,
                    ),
                    nextest_profile,
                    nextest_filter_expr: None,
                    nextest_working_dir: None,
                    nextest_config_file: None,
                    nextest_partition: None,
                    run_ignored: false,
                    extra_env: None,
                    pre_run_deps,
                    results,
                }),
                BuildNextestUnitTestMode::Archive(unit_tests_archive) => {
                    let archive_file =
                        ctx.reqv(|v| flowey_lib_common::run_cargo_nextest_archive::Request {
                            friendly_label: "unit-tests".into(),
                            working_dir: openvmm_repo_path.clone(),
                            build_params,
                            pre_run_deps,
                            archive_file: v,
                        });

                    ctx.emit_minor_rust_step("report built unit tests", |ctx| {
                        let archive_file = archive_file.claim(ctx);
                        let unit_tests = unit_tests_archive.claim(ctx);
                        |rt| {
                            let archive_file = rt.read(archive_file);
                            rt.write(unit_tests, &NextestUnitTestArchive { archive_file });
                        }
                    });
                }
            }
        }

        Ok(())
    }
}
