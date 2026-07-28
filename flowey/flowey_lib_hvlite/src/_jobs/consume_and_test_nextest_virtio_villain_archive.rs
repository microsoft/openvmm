// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Consume a pre-built virtio-villain nextest archive and run it against
//! OpenVMM.

use crate::build_nextest_virtio_villain_tests::NextestVirtioVillainTestsArchive;
use crate::build_openvmm::OpenvmmOutput;
use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_nextest_run::NextestRunKind;

flowey_request! {
    pub struct Params {
        /// Artifact label prefix for the published results. To be picked up by
        /// the `upload-petri-results` workflow (which globs `*-vmm-tests-logs`),
        /// this MUST end in `-vmm-tests` so the logs attachment becomes
        /// `<junit_test_label>-logs`.
        pub junit_test_label: String,
        /// Pre-built virtio-villain nextest archive.
        pub nextest_villain_archive: ReadVar<NextestVirtioVillainTestsArchive>,
        /// Pre-built OpenVMM binary (the villain crate launches this).
        pub openvmm: ReadVar<OpenvmmOutput>,
        /// Target triple the virtio-villain tests were compiled for.
        pub target: target_lexicon::Triple,
        /// Also run known-failing (ignored) villain tests.
        pub run_ignored: bool,
        /// Local-backend only: copy JUnit + logs into this published artifact
        /// dir.
        pub artifact_dir: Option<ReadVar<PathBuf>>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::test_virtio_villain::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            junit_test_label,
            nextest_villain_archive,
            openvmm,
            target,
            run_ignored,
            artifact_dir,
            done,
        } = request;

        // Ad-hoc, step-local dir used as a staging ground for test content
        // (OpenVMM binary + guest kernel), mirroring the vmm_tests consume job.
        let test_content_dir = ctx.emit_rust_stepv("creating new test content dir", |_| {
            |rt| Ok(rt.sh.current_dir())
        });

        let nextest_archive = nextest_villain_archive.map(ctx, |x| x.archive_file);

        ctx.req(crate::test_virtio_villain::Request {
            target,
            openvmm,
            run_kind: NextestRunKind::RunFromArchive {
                archive_file: nextest_archive,
                target: None,
                nextest_bin: None,
            },
            nextest_profile: NextestProfile::Ci,
            nextest_filter_expr: None,
            run_ignored,
            test_content_dir,
            junit_test_label,
            artifact_dir,
            // CI test machines need the runtime deps installed and /dev/kvm
            // access granted before running.
            install_deps: true,
            disable_remote_artifacts: true,
            done,
        });

        Ok(())
    }
}
