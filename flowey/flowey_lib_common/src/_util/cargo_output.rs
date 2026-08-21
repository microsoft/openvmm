// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Type definitions for the output of `cargo build --message-format=json`.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Debug)]
#[serde(tag = "reason")]
pub enum Message {
    #[serde(rename = "compiler-artifact")]
    CompilerArtifact {
        package_id: String,
        target: Target,
        filenames: Vec<PathBuf>,
    },
    #[serde(rename = "build-script-executed")]
    BuildScriptExecuted {
        package_id: String,
        /// The `OUT_DIR` the build script was invoked with.
        out_dir: PathBuf,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug)]
pub struct Target {
    pub kind: Vec<String>,
    pub name: String,
}
