// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM's concrete command-line frontend for the Flowey pipeline framework.
//!
//! This host executable registers the repository's local and CI workflows.
//! The generic `flowey_cli` runtime resolves and executes the resulting
//! dependency graph.
//!
//! Developers should invoke it through the `cargo xflowey <pipeline>` alias,
//! which supplies Flowey's implicit `pipeline run` arguments and lightweight
//! Cargo profile. Generated CI also calls the same pipeline definitions.

#![forbid(unsafe_code)]

fn main() {
    flowey_cli::flowey_main::<flowey_hvlite::pipelines::OpenvmmPipelines>(
        "flowey_hvlite",
        &flowey_hvlite::repo_root(),
    )
}
