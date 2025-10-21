// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serde defs for GitHub YAML

use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeMap;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Valid names may only contain alphanumeric characters and '_' and may not
/// start with a number.
fn validate_name<S, T>(s: &BTreeMap<String, T>, ser: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize,
{
    let mut ser_map = ser.serialize_map(Some(s.len()))?;
    for (name, v) in s {
        if name.is_empty() {
            return Err(serde::ser::Error::custom("name cannot be empty"));
        }

        if name.chars().next().unwrap().is_ascii_digit() {
            return Err(serde::ser::Error::custom("name cannot start with a number"));
        }

        if name.starts_with("GITHUB_") {
            return Err(serde::ser::Error::custom(
                "name cannot start with 'GITHUB_'",
            ));
        }

        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(serde::ser::Error::custom(
                "name must be ascii alphanumeric + '_'",
            ));
        }
        ser_map.serialize_entry(name, v)?;
    }
    ser_map.end()
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WorkflowCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Inputs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Outputs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Secrets>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WorkflowDispatch {
    pub inputs: Inputs,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PrTrigger {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub branches_ignore: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CiTrigger {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub branches_ignore: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags_ignore: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Cron {
    pub cron: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Concurrency {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_in_progress: Option<bool>,
}

#[derive(Serialize, Deserialize)]
pub struct Triggers {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_call: Option<WorkflowCall>,
    pub workflow_dispatch: Option<WorkflowDispatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PrTrigger>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push: Option<CiTrigger>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub schedule: Vec<Cron>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionValue {
    Read,
    Write,
    None,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum Permissions {
    Actions,
    Attestations,
    Checks,
    Contents,
    Deployments,
    Discussions,
    IdToken,
    Issues,
    Packages,
    Pages,
    PullRequests,
    RepositoryProjects,
    SecurityEvents,
    Statuses,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Pipeline {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<Triggers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<Concurrency>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<Input>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jobs: Option<Jobs>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Inputs {
    #[serde(flatten, serialize_with = "validate_name")]
    pub inputs: BTreeMap<String, Input>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Input {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Default>,
    pub required: bool,
    #[serde(flatten)]
    pub ty: InputType,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum Default {
    Boolean(bool),
    String(String),
    Number(i64),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputType {
    Boolean,
    String,
    Number,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Outputs {
    #[serde(flatten, serialize_with = "validate_name")]
    pub outputs: BTreeMap<String, Output>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Output {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub value: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING-KEBAB-CASE")]
pub struct Secrets {
    #[serde(flatten, serialize_with = "validate_name")]
    pub secrets: BTreeMap<String, Secret>,
}

#[derive(Serialize, Deserialize)]
pub struct Secret {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Serialize, Deserialize)]
pub enum RunnerOsLabel {
    #[serde(rename = "ubuntu-latest")]
    UbuntuLatest,
    #[serde(rename = "ubuntu-24.04")]
    Ubuntu2404,
    #[serde(rename = "ubuntu-22.04")]
    Ubuntu2204,
    #[serde(rename = "windows-latest")]
    WindowsLatest,
    #[serde(rename = "windows-2025")]
    Windows2025,
    #[serde(rename = "windows-2022")]
    Windows2022,
    #[serde(rename = "ubuntu-24.04-arm")]
    Ubuntu2404Arm,
    #[serde(rename = "ubuntu-22.04-arm")]
    Ubuntu2204Arm,
    #[serde(rename = "windows-11-arm")]
    Windows11Arm,
    #[serde(untagged)]
    Custom(String),
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum Runner {
    GhHosted(RunnerOsLabel),
    SelfHosted(Vec<String>),
    Group {
        group: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        labels: Vec<String>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Job {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs_on: Option<Runner>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub permissions: BTreeMap<Permissions, PermissionValue>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub needs: BTreeSet<String>,
    #[serde(rename = "if", skip_serializing_if = "Option::is_none")]
    pub r#if: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub steps: Vec<serde_yaml::Value>,
}

#[derive(Serialize, Deserialize)]
pub struct Jobs {
    #[serde(flatten, serialize_with = "validate_name")]
    pub jobs: BTreeMap<String, Job>,
}
