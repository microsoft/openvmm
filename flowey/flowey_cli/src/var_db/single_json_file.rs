// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A dead simple runtime variable db, backed by a single JSON file.

use anyhow::Context;
use fs_err::File;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Seek;
use std::io::Write;
use std::path::Path;

/// On-disk format for the var db
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct VarDb {
    vars: BTreeMap<String, (bool, serde_json::Value)>,
}

/// Implements [`flowey_core::node::RuntimeVarDb`] backed by a JSON file.
pub struct SingleJsonFileVarDb {
    file: File,
}

impl SingleJsonFileVarDb {
    pub fn new(backing_file: impl AsRef<Path>) -> anyhow::Result<Self> {
        let backing_file = backing_file.as_ref();
        let exists = backing_file.exists();
        let mut file = fs_err::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(backing_file)
            .context("failed to open/create database file")?;

        // seed the database file with an empty json map
        if !exists {
            eprintln!(
                "seeding new empty database file: {}",
                backing_file.display()
            );
            file.write_all(b"{}")?;
        }

        Ok(Self { file })
    }

    fn load_db(&mut self) -> VarDb {
        self.file.rewind().unwrap();
        serde_json::from_reader(&self.file).expect("corrupt runtime variable db")
    }
}

impl flowey_core::node::RuntimeVarDb for SingleJsonFileVarDb {
    fn try_get_var(&mut self, var_name: &str) -> Option<Vec<u8>> {
        let db = self.load_db();
        let (is_secret, val) = db.vars.get(var_name)?;
        let val = val.to_string();
        if *is_secret {
            log::debug!("[db] read var: {} = <secret>", var_name);
        } else {
            log::debug!("[db] read var: {} = {}", var_name, val);
        }
        Some(val.into())
    }

    fn set_var(&mut self, var_name: &str, is_secret: bool, value: Vec<u8>) {
        if is_secret {
            log::debug!("[db] set var: {} = <secret>", var_name)
        } else {
            log::debug!(
                "[db] set var: {} = {}",
                var_name,
                String::from_utf8_lossy(&value)
            )
        };
        let mut db = self.load_db();
        let existing = db.vars.insert(
            var_name.into(),
            (is_secret, serde_json::from_slice(&value).unwrap()),
        );
        assert!(existing.is_none()); // all vars are one-time-write
        self.file.set_len(0).unwrap();
        self.file.rewind().unwrap();
        serde_json::to_writer(&self.file, &db).expect("failed to write to db JSON");
    }
}
