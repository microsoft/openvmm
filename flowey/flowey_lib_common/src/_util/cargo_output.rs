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
        target: Target,
        filenames: Vec<PathBuf>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug)]
pub struct Target {
    pub kind: Vec<String>,
    pub name: String,
}
