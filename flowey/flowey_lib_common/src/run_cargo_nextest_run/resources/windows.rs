// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::DiskSample;
use flowey::node::prelude::*;
use windows_disk_io::PhysicalDiskCounters;

pub(super) struct DiskSampler(PhysicalDiskCounters);

impl DiskSampler {
    pub(super) fn new() -> anyhow::Result<Self> {
        Ok(Self(PhysicalDiskCounters::new()?))
    }

    pub(super) fn sample(&mut self) -> anyhow::Result<Vec<DiskSample>> {
        Ok(self
            .0
            .sample()?
            .into_iter()
            .map(|sample| {
                let mut errors = Vec::new();
                let mut value = |counter: std::io::Result<u64>, label| match counter {
                    Ok(bytes) => Some(bytes),
                    Err(err) => {
                        errors.push(format!("PhysicalDisk {} {label}: {err}", sample.instance));
                        None
                    }
                };
                DiskSample {
                    read_bytes: value(sample.read_bytes, "read"),
                    written_bytes: value(sample.written_bytes, "write"),
                    name: format!("PhysicalDisk {}", sample.instance),
                    mount_point: String::new(),
                    error: (!errors.is_empty()).then(|| errors.join("; ")),
                }
            })
            .collect())
    }
}
