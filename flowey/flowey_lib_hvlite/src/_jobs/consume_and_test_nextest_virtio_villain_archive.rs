// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Consume a pre-built virtio-villain nextest archive and run it against
//! OpenVMM on a KVM test machine.
//!
//! This is the "test half" of the build/consume split (mirroring
//! [`crate::_jobs::consume_and_test_nextest_vmm_tests_archive`]): the archive
//! and the OpenVMM binary are built on a *build* machine (which has protoc, the
//! musl sysroot, etc.) and handed here as artifacts, so this job needs no Rust
//! toolchain — only the runtime bits (OpenVMM, the guest kernel, and the
//! villain guest artifact).
//!
//! Both this job and the local build-and-run job
//! ([`crate::_jobs::run_virtio_villain_tests`]) funnel through the shared
//! [`crate::test_virtio_villain`] node, which owns the staging, artifact
//! resolution, nextest run, and result publishing. The only difference is
//! expressed as the [`NextestRunKind`]: this job runs a prebuilt archive
//! ([`NextestRunKind::RunFromArchive`]).

use crate::build_nextest_virtio_villain_tests::NextestVirtioVillainTestsArchive;
use crate::build_openvmm::OpenvmmOutput;
use crate::common::CommonArch;
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
        /// Guest/host architecture to test. Phase 1 is Linux-only (KVM).
        pub arch: CommonArch,
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
            arch,
            run_ignored,
            artifact_dir,
            done,
        } = request;

        // Ad-hoc, step-local dir used as a staging ground for test content
        // (OpenVMM binary + guest kernel), mirroring the vmm_tests consume job.
        let test_content_dir = ctx.emit_rust_stepv("creating new test content dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        let nextest_archive = nextest_villain_archive.map(ctx, |x| x.archive_file);

        ctx.req(crate::test_virtio_villain::Request {
            arch,
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
