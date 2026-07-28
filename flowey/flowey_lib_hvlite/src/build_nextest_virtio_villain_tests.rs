// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build and archive the `virtio_villain_tests` nextest suite.
//!
//! This is the virtio-villain analogue of [`crate::build_nextest_vmm_tests`]:
//! it emits a self-contained cargo-nextest archive for the
//! `virtio_villain_tests` package.

use crate::common::CommonProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_build::CargoBuildProfile;
use flowey_lib_common::run_cargo_nextest_run::build_params::NextestBuildParams;
use flowey_lib_common::run_cargo_nextest_run::build_params::TestPackages;

/// Type-safe wrapper around a built nextest archive containing the
/// virtio-villain tests.
#[derive(Serialize, Deserialize)]
pub struct NextestVirtioVillainTestsArchive {
    #[serde(rename = "virtio_villain_tests.tar.zst")]
    pub archive_file: PathBuf,
}

impl Artifact for NextestVirtioVillainTestsArchive {}

flowey_request! {
    pub struct Request {
        /// Target triple to build the tests for.
        pub target: target_lexicon::Triple,
        /// Cargo profile to build the tests with.
        pub profile: CommonProfile,
        /// Resulting nextest archive.
        pub archive: WriteVar<NextestVirtioVillainTestsArchive>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_openvmm_rust_build_essential::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::init_cross_build::Node>();
        ctx.import::<flowey_lib_common::run_cargo_nextest_archive::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut ambient_deps = vec![ctx.reqv(crate::install_openvmm_rust_build_essential::Request)];

        if matches!(
            ctx.platform(),
            FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu)
        ) {
            ambient_deps.push(ctx.reqv(|v| {
                flowey_lib_common::install_dist_pkg::Request::Install {
                    package_names: vec![
                        "libssl-dev".into(),
                        "pkg-config".into(),
                        "build-essential".into(),
                    ],
                    done: v,
                }
            }));
        }

        for Request {
            target,
            profile,
            archive,
        } in requests
        {
            let injected_env = ctx.reqv(|v| crate::init_cross_build::Request {
                target: target.clone(),
                injected_env: v,
            });

            let build_params = NextestBuildParams {
                packages: ReadVar::from_static(TestPackages::Crates {
                    crates: vec!["virtio_villain_tests".into()],
                }),
                features: Default::default(),
                no_default_features: false,
                target: target.clone(),
                profile: match profile {
                    CommonProfile::Release => CargoBuildProfile::Release,
                    CommonProfile::Debug => CargoBuildProfile::Debug,
                },
                extra_env: injected_env,
            };

            let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

            let archive_file =
                ctx.reqv(|v| flowey_lib_common::run_cargo_nextest_archive::Request {
                    friendly_label: "virtio_villain_tests".into(),
                    working_dir: openvmm_repo_path,
                    build_params,
                    pre_run_deps: ambient_deps.clone(),
                    archive_file: v,
                });

            ctx.emit_minor_rust_step("report built virtio_villain_tests", |ctx| {
                let archive_file = archive_file.claim(ctx);
                let archive = archive.claim(ctx);
                |rt| {
                    let archive_file = rt.read(archive_file);
                    rt.write(archive, &NextestVirtioVillainTestsArchive { archive_file });
                }
            });
        }

        Ok(())
    }
}
