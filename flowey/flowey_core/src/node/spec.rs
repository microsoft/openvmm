// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Type definitions from GitHub events and payloads. <https://docs.github.com/en/webhooks/webhook-events-and-payloads>

use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize)]
pub struct Head {
    #[serde(rename = "ref")]
    pub head_ref: String,
}

#[derive(Serialize, Deserialize)]
pub struct GhContextVarReaderEventPullRequest {
    pub head: Head,
    pub number: u32,
}
