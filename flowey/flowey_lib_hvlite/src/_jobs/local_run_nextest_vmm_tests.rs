// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local-only job that runs the VMM tests out of a directory populated
//! by local_build_and_run_nextest_vmm_tests with the build_only option.

use crate::_jobs::consume_and_test_nextest_vmm_tests_archive::TestContentConfig;
use crate::_jobs::local_build_and_run_nextest_vmm_tests::build_test_label;
use crate::_jobs::local_build_and_run_nextest_vmm_tests::init_artifacts_dir;
use crate::build_incubator::IncubatorProfileNameOrPath;
use crate::common::CommonTriple;
use crate::init_vmm_tests_env::PetriParams;
use crate::install_vmm_tests_external_deps::VmmTestsExternalDeps;
use flowey::node::prelude::*;
use std::num::NonZeroU64;
use vmm_test_images::KnownTestArtifacts;

flowey_request! {
    pub struct Params {
        pub target: CommonTriple,

        /// Test content dir with all artifacts and repo root
        pub test_content_dir: PathBuf,

        // Subset of `VmmTestSelections`
        pub filter: String,
        pub downloaded_artifacts: Vec<KnownTestArtifacts>,
        pub external_deps: VmmTestsExternalDeps,
        pub prep_steps_variants: Vec<String>,
        pub needs_test_igvm_agent_rpc_server: bool,

        /// Skip the interactive VHD download prompt
        pub skip_vhd_prompt: bool,

        pub nextest_profile: crate::run_cargo_nextest_run::NextestProfile,

        pub petri_params: PetriParams,

        pub repetitions: NonZeroU64,

        /// Optional: incubator profile path. When set, tests run inside
        /// an emulated VM instead of on the host.
        pub incubator_profile: Option<IncubatorProfileNameOrPath>,

        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::_jobs::consume_and_test_nextest_vmm_tests_archive::Node>();
        ctx.import::<crate::download_openvmm_vmm_tests_artifacts::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            test_content_dir,
            filter,
            downloaded_artifacts,
            external_deps,
            prep_steps_variants,
            needs_test_igvm_agent_rpc_server,
            skip_vhd_prompt,
            nextest_profile,
            petri_params,
            repetitions,
            incubator_profile,
            done,
        } = request;

        let test_content_dir = test_content_dir.absolute()?;

        let target_triple = target.as_triple();
        let test_label = build_test_label(&target_triple);

        init_artifacts_dir(ctx, &test_content_dir, skip_vhd_prompt)?;

        ctx.req(
            crate::_jobs::consume_and_test_nextest_vmm_tests_archive::Params {
                junit_test_label: test_label,
                target: target_triple,
                nextest_profile,
                nextest_filter_expr: Some(filter),
                test_content_config: TestContentConfig::Initialized {
                    test_content_dir: ReadVar::from_static(test_content_dir),
                    needs_test_igvm_agent_rpc_server,
                },
                downloaded_artifacts,
                prep_steps_variants,
                external_deps,
                incubator_profile,
                upload_logs_on_success: true,
                fail_job_on_test_fail: true,
                repetitions,
                petri_params,
                test_content_dir_as_repo_root: true,
                done,
            },
        );

        Ok(())
    }
}
