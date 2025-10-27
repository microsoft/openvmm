// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Flowey pipelines used by the OpenVMM project

#![forbid(unsafe_code)]

fn main() {
    flowey_cli::flowey_main::<flowey_hvlite::pipelines::OpenvmmPipelines>(
        "flowey_hvlite",
        &flowey_hvlite::repo_root(),
    )
}
