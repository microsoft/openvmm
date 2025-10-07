// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functionality to read MSR registers via `/dev/msr`.

#![cfg(guest_arch = "x86_64")]

use fs_err::os::unix::fs::FileExt;

pub(crate) struct MsrDevice(fs_err::File);

impl MsrDevice {
    /// Open `/dev/msr`.
    pub fn new(cpu: u32) -> std::io::Result<Self> {
        let path = format!("/dev/cpu/{}/msr", cpu);
        let file = fs_err::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;

        Ok(Self(file))
    }

    /// Read an MSR register.
    pub fn read_msr(&self, msr: u32) -> std::io::Result<u64> {
        let mut data = [0; 8];
        self.0.read_at(&mut data, msr as u64)?;
        Ok(u64::from_ne_bytes(data))
    }
}
