// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//
// Note: This is not the full schema, just the parts we use.

use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct OneFuzzConfigV3 {
    /// Must be 3.
    pub config_version: u32,
    pub entries: Vec<Entry>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Entry {
    pub job_notification_email: String,
    pub fuzzer: Fuzzer,
    pub job_dependencies: Vec<String>,
    pub one_fuzz_jobs: Vec<OneFuzzJob>,
    pub ado_template: AdoTemplate,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Fuzzer {
    #[serde(rename = "$type")]
    pub type_field: String,
    pub sources_allow_list_path: String,
    pub fuzzing_harness_executable_name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct OneFuzzJob {
    pub project_name: String,
    pub target_name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub target_options: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct AdoTemplate {
    pub org: String,
    pub project: String,
    pub assigned_to: String,
    pub area_path: String,
    pub iteration_path: String,
    pub ado_fields: AdoFields,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct AdoFields {
    /// Comma separated list.
    #[serde(rename = "System.Tags")]
    pub tags: String,
}
