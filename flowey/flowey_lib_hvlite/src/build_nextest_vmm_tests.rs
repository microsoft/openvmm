// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the cargo-nextest based VMM tests.

use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoBuildProfile;
use flowey_lib_common::run_cargo_nextest_run::TestResults;
use flowey_lib_common::run_cargo_nextest_run::build_params::TestPackages;
use std::collections::BTreeMap;

/// Type-safe wrapper around a built nextest archive containing VMM tests
#[derive(Serialize, Deserialize)]
pub struct NextestVmmTestsArchive {
    #[serde(rename = "vmm_tests.tar.zst")]
    pub archive_file: PathBuf,
}

/// Type-safe wrapper around a json file containing a list of test cases built in the nextest archive
#[derive(Serialize, Deserialize)]
pub struct NextestVmmTestsList {
    #[serde(rename = "vmm_tests.json")]
    pub nextest_list_json: PathBuf,
}

impl Artifact for NextestVmmTestsArchive {}
impl Artifact for NextestVmmTestsList {}

/// Build mode to use when building the nextest VMM tests
#[derive(Serialize, Deserialize)]
pub enum BuildNextestVmmTestsMode {
    /// Build and immediate run VMM tests, side-stepping any intermediate
    /// archiving steps.
    ///
    /// NOTE: The caller is responsible for setting `extra_env` and
    /// `pre_run_deps` to ensure that all tests filtered by
    /// `nextest_filter_expr` are able to run successfully.
    ImmediatelyRun {
        /// Nextest profile to use when running the source code
        nextest_profile: NextestProfile,
        /// Nextest test filter expression
        nextest_filter_expr: Option<String>,
        /// Additional env vars set when executing the tests.
        extra_env: ReadVar<BTreeMap<String, String>>,
        /// Wait for specified side-effects to resolve before building / running
        /// any tests. (e.g: to allow for some ambient packages / dependencies
        /// to get installed).
        pre_run_deps: Vec<ReadVar<SideEffect>>,

        results: WriteVar<TestResults>,
    },
    /// Build and archive the tests into a nextest archive file, which can then
    /// be run via [`crate::test_nextest_vmm_tests_archive`].
    Archive {
        nextest_list_json_file: Option<WriteVar<NextestVmmTestsList>>,
        vmm_test_archive: WriteVar<NextestVmmTestsArchive>,
    },
}

flowey_request! {
    pub struct Request {
        /// Build and run VMM tests for the specified target
        pub target: target_lexicon::Triple,
        /// Build and run VMM tests with the specified cargo profile
        pub profile: CommonProfile,
        /// Build mode to use when building the nextest VMM tests
        pub build_mode: BuildNextestVmmTestsMode,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_openvmm_rust_build_essential::Node>();
        ctx.import::<crate::run_cargo_nextest_run::Node>();
        ctx.import::<crate::run_cargo_nextest_list::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::init_openvmm_magicpath_openhcl_sysroot::Node>();
        ctx.import::<crate::init_cross_build::Node>();
        ctx.import::<flowey_lib_common::run_cargo_nextest_archive::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        // base requirements for building crates in the hvlite tree
        let ambient_deps = vec![ctx.reqv(crate::install_openvmm_rust_build_essential::Request)];

        for Request {
            target,
            profile,
            build_mode,
        } in requests
        {
            let mut ambient_deps = ambient_deps.clone();

            let sysroot_arch = match target.architecture {
                target_lexicon::Architecture::Aarch64(_) => {
                    crate::init_openvmm_magicpath_openhcl_sysroot::OpenvmmSysrootArch::Aarch64
                }
                target_lexicon::Architecture::X86_64 => {
                    crate::init_openvmm_magicpath_openhcl_sysroot::OpenvmmSysrootArch::X64
                }
                arch => anyhow::bail!("unsupported arch {arch}"),
            };

            // See comment in `crate::cargo_build` for why this is necessary.
            //
            // copied here since this node doesn't actually route through `cargo build`.
            if matches!(target.environment, target_lexicon::Environment::Musl) {
                ambient_deps.push(
                    ctx.reqv(|v| crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                        arch: sysroot_arch,
                        path: v,
                    })
                    .into_side_effect(),
                );
            }

            let injected_env = ctx.reqv(|v| crate::init_cross_build::Request {
                target: target.clone(),
                injected_env: v,
            });

            let build_params =
                flowey_lib_common::run_cargo_nextest_run::build_params::NextestBuildParams {
                    packages: ReadVar::from_static(TestPackages::Crates {
                        crates: vec!["vmm_tests".into()],
                    }),
                    features: Default::default(),
                    no_default_features: false,
                    unstable_panic_abort_tests: None, // don't run VMM tests on musl hvlite
                    target: target.clone(),
                    profile: match profile {
                        CommonProfile::Release => CargoBuildProfile::Release,
                        CommonProfile::Debug => CargoBuildProfile::Debug,
                    },
                    extra_env: injected_env,
                };

            match build_mode {
                BuildNextestVmmTestsMode::ImmediatelyRun {
                    nextest_profile,
                    nextest_filter_expr,
                    extra_env,
                    pre_run_deps,
                    results,
                } => {
                    ambient_deps.extend(pre_run_deps);

                    ctx.req(crate::run_cargo_nextest_run::Request {
                        friendly_name: "vmm_tests".into(),
                        run_kind:
                            flowey_lib_common::run_cargo_nextest_run::NextestRunKind::BuildAndRun(
                                build_params,
                            ),
                        nextest_profile,
                        nextest_filter_expr,
                        nextest_working_dir: None,
                        nextest_config_file: None,
                        run_ignored: false,
                        extra_env: Some(extra_env),
                        pre_run_deps: ambient_deps,
                        results,
                    })
                }
                BuildNextestVmmTestsMode::Archive {
                    nextest_list_json_file: nextest_list_json_write,
                    vmm_test_archive,
                } => {
                    let openvmm_repo_path =
                        ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

                    let archive_file =
                        ctx.reqv(|v| flowey_lib_common::run_cargo_nextest_archive::Request {
                            friendly_label: "vmm_tests".into(),
                            working_dir: openvmm_repo_path.clone(),
                            build_params,
                            pre_run_deps: ambient_deps,
                            archive_file: v,
                        });

                    let nextest_list_json = if nextest_list_json_write.as_ref().is_some() {
                        Some(ctx.reqv(|v| crate::run_cargo_nextest_list::Request {
                            archive_file: archive_file.clone(),
                            nextest_bin: None,
                            target: None,
                            working_dir: None,
                            config_file: None,
                            nextest_profile: "ci".into(),
                            nextest_filter_expr: None,
                            run_ignored: true,
                            extra_env: None,
                            output_dir: openvmm_repo_path,
                            pre_run_deps: vec![],
                            output_file: v,
                        }))
                    } else {
                        None
                    };

                    ctx.emit_minor_rust_step("report built vmm_tests", |ctx| {
                        let archive_file = archive_file.claim(ctx);
                        let vmm_tests = vmm_test_archive.claim(ctx);
                        let nextest_list_json = nextest_list_json.claim(ctx);
                        let nextest_list_json_write = nextest_list_json_write.claim(ctx);
                        |rt| {
                            let archive_file = rt.read(archive_file);
                            let nextest_list_json = rt.read(nextest_list_json);
                            rt.write(vmm_tests, &NextestVmmTestsArchive { archive_file });
                            if let Some(nextest_list_json) = nextest_list_json {
                                if let Some(nextest_list_json_write) = nextest_list_json_write {
                                    rt.write(
                                        nextest_list_json_write,
                                        &NextestVmmTestsList { nextest_list_json },
                                    );
                                }
                            }
                        }
                    });
                }
            }
        }

        Ok(())
    }
}
