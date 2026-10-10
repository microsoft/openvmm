// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct JobIdentifiers {
    pub nodes: IdentifierAliases,
    pub variables: IdentifierAliases,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct IdentifierAliases(BTreeMap<String, String>);

impl IdentifierAliases {
    pub fn new(names: impl IntoIterator<Item = String>) -> Self {
        let names: BTreeSet<_> = names.into_iter().collect();
        let mut aliases = BTreeMap::new();
        let mut ordinal = 0;
        for name in &names {
            let alias = loop {
                let alias = format!("@{ordinal}");
                ordinal += 1;
                if !names.contains(&alias) {
                    break alias;
                }
            };
            aliases.insert(alias, name.clone());
        }

        Self(aliases)
    }

    pub fn by_name(&self) -> BTreeMap<String, String> {
        self.0
            .iter()
            .map(|(alias, name)| (name.clone(), alias.clone()))
            .collect()
    }

    pub fn resolve<'a>(&'a self, identifier: &'a str) -> anyhow::Result<&'a str> {
        if !identifier.starts_with('@')
            || self.0.is_empty()
            || self.0.values().any(|name| name == identifier)
        {
            return Ok(identifier);
        }

        self.0
            .get(identifier)
            .map(String::as_str)
            .with_context(|| format!("unknown compact identifier '{identifier}'"))
    }
}
