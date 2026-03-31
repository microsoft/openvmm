// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CI job that validates all petri artifact IDs have a corresponding mapping in
//! [`artifact_to_build_mapping`](crate::artifact_to_build_mapping).
//!
//! Composes with [`local_discover_vmm_tests_artifacts`](crate::_jobs::local_discover_vmm_tests_artifacts)
//! to discover all artifacts, then validates each one resolves through the
//! mapping logic.

use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        /// Target to discover and validate artifact mappings for.
        pub target: CommonTriple,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::_jobs::local_discover_vmm_tests_artifacts::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
        ctx.import::<flowey_lib_common::install_cargo_nextest::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { target, done } = request;

        let triple = target.as_triple();
        let arch = triple.architecture;
        let os = triple.operating_system;

        // Ensure rust and nextest are installed before the discover step runs.
        let rust_installed = ctx.reqv(flowey_lib_common::install_rust::Request::EnsureInstalled);
        let nextest_installed = ctx.reqv(flowey_lib_common::install_cargo_nextest::Request);

        // Wire up artifact discovery: match ALL tests to get the full artifact list.
        let (artifacts_json, artifacts_json_write) = ctx.new_var::<String>();
        let (discover_done, discover_done_write) = ctx.new_var::<SideEffect>();

        ctx.req(crate::_jobs::local_discover_vmm_tests_artifacts::Params {
            target,
            filter: "test(/.*/)".to_string(),
            output: None,
            release: false,
            done: discover_done_write,
            artifacts_json_out: Some(artifacts_json_write),
            pre_build_done: vec![rust_installed, nextest_installed],
        });

        // Validation step: runs after discovery completes (data dependency on artifacts_json).
        ctx.emit_rust_step("validate artifact mapping completeness", |ctx| {
            discover_done.claim(ctx);
            let artifacts_json = artifacts_json.claim(ctx);
            done.claim(ctx);
            move |rt| {
                let json = rt.read(artifacts_json);

                let resolved =
                    crate::artifact_to_build_mapping::ResolvedArtifactSelections::from_artifact_list_json(
                        &json,
                        arch,
                        os,
                    )?;

                if !resolved.unknown.is_empty() {
                    log::error!(
                        "The following {} artifact(s) have no mapping in artifact_to_build_mapping.rs:",
                        resolved.unknown.len()
                    );
                    for id in &resolved.unknown {
                        log::error!("  - {id}");
                    }
                    anyhow::bail!(
                        "{} artifact(s) are missing from artifact_to_build_mapping::resolve_artifact(). \
                         Add a match arm for each missing artifact.",
                        resolved.unknown.len()
                    );
                }

                log::info!("All artifact IDs have valid mappings.");
                Ok(())
            }
        });

        Ok(())
    }
}
