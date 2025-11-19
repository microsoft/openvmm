// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serde defs for ADO YAML

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

mod none {
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;

    pub fn serialize<S>(_: &(), ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ser.serialize_str("none")
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<(), D::Error> {
        let s: &str = Deserialize::deserialize(d)?;
        if s != "none" {
            return Err(serde::de::Error::custom("field must be 'none'"));
        }
        Ok(())
    }
}

/// Valid names may only contain alphanumeric characters and '_' and may not
/// start with a number.
fn validate_name<S>(s: &str, ser: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if s.is_empty() {
        return Err(serde::ser::Error::custom("name cannot be empty"));
    }

    if s.chars().next().unwrap().is_ascii_digit() {
        return Err(serde::ser::Error::custom("name cannot start with a number"));
    }

    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(serde::ser::Error::custom(
            "name must be ascii alphanumeric + '_'",
        ));
    }

    ser.serialize_str(s)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerBranches {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    // Wrapping this in an Option is necessary to prevent problems when deserializing and exclude isn't present
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerTags {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    // Wrapping this in an Option is necessary to prevent problems when deserializing and exclude isn't present
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "camelCase")]
pub enum PrTrigger {
    None(#[serde(with = "none")] ()),
    #[serde(rename_all = "camelCase")]
    Some {
        auto_cancel: bool,
        drafts: bool,
        branches: TriggerBranches,
    },
    // serde has a bug with untagged and `with` during deserialization
    NoneWorkaround(String),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "camelCase")]
pub enum CiTrigger {
    None(#[serde(with = "none")] ()),
    #[serde(rename_all = "camelCase")]
    Some {
        batch: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        branches: Option<TriggerBranches>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tags: Option<TriggerTags>,
    },
    // serde has a bug with untagged and `with` during deserialization
    NoneWorkaround(String),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    // FUTURE?: proper cron validation?
    pub cron: String,
    pub display_name: String,
    pub branches: TriggerBranches,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[serde(default)]
    pub batch: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Variable {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pipeline {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<CiTrigger>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrTrigger>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedules: Option<Vec<Schedule>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variables: Option<Vec<Variable>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Vec<Parameter>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stages: Option<Vec<Stage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jobs: Option<Vec<Job>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extends: Option<Extends>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Extends {
    pub template: String,
    pub parameters: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<ResourcesRepository>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourcesRepository {
    // Alias for the specified repository.
    //
    // Acceptable values: [-_A-Za-z0-9]*.
    pub repository: String,
    /// ID of the service endpoint connecting to this repository
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Repository name. Format depends on 'type'; does not accept variables
    pub name: String,
    /// ref name to checkout; defaults to 'refs/heads/main'.
    #[serde(rename = "ref")]
    pub r#ref: String,
    #[serde(rename = "type")]
    pub r#type: ResourcesRepositoryType,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourcesRepositoryType {
    Git,
    GitHub,
    GitHubEnterprise,
    Bitbucket,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parameter {
    pub name: String,
    pub display_name: String,
    #[serde(flatten)]
    pub ty: ParameterType,
}

// ADO also has specialized types for things like steps/jobs/stages, etc... but
// at this time, it's unclear how they'd be useful in flowey.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ParameterType {
    Boolean {
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
    String {
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        values: Option<Vec<String>>,
    },
    Number {
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        values: Option<Vec<i64>>,
    },
    Object {
        #[serde(skip_serializing_if = "Option::is_none")]
        default: Option<serde_yaml::Value>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stage {
    /// Valid names may only contain alphanumeric characters and '_' and may
    /// not start with a number.
    #[serde(serialize_with = "validate_name")]
    pub stage: String,
    pub display_name: String,
    pub depends_on: BTreeSet<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    pub jobs: Vec<Job>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "camelCase")]
pub enum Pool {
    Pool(String),
    PoolWithMetadata(BTreeMap<String, serde_yaml::Value>),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    #[serde(serialize_with = "validate_name")]
    pub job: String,
    pub display_name: String,
    pub pool: Pool,
    pub depends_on: BTreeSet<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_in_minutes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variables: Option<Vec<Variable>>,
    // individual steps are not type-checked by the serde schema, as there are a
    // _lot_ of different step kinds nodes might be emitting.
    //
    // instead, trust that the user knows what they're doing when emitting yaml
    // snippets.
    pub steps: Vec<serde_yaml::Value>,
}
