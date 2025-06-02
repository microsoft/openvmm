// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A dead simple runtime variable db, backed by a single JSON file.

use std::collections::BTreeMap;

/// Implements [`flowey_core::node::RuntimeVarDb`] in memory.
pub struct InMemoryVarDb {
    vars: BTreeMap<String, (bool, Vec<u8>)>,
}

impl InMemoryVarDb {
    pub fn new() -> Self {
        Self {
            vars: BTreeMap::new(),
        }
    }
}

impl flowey_core::node::RuntimeVarDb for InMemoryVarDb {
    fn try_get_var(&mut self, var_name: &str) -> Option<(Vec<u8>, bool)> {
        let (is_secret, ref val) = *self.vars.get(var_name)?;
        if is_secret {
            log::debug!("[db] read var: {} = <secret>", var_name);
        } else {
            log::debug!(
                "[db] read var: {} = {}",
                var_name,
                String::from_utf8_lossy(val)
            );
        }
        Some((val.clone(), is_secret))
    }

    fn set_var(&mut self, var_name: &str, is_secret: bool, value: Vec<u8>) {
        if is_secret {
            log::debug!("[db] set var: {} = <secret>", var_name,);
        } else {
            log::debug!(
                "[db] set var: {} = {}",
                var_name,
                String::from_utf8_lossy(&value)
            );
        }

        let existing = self.vars.insert(var_name.into(), (is_secret, value));
        assert!(existing.is_none()); // all vars are one-time-write
    }
}
