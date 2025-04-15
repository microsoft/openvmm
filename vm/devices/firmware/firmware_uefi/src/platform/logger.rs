// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interfaces required to support UEFI event logging.

use std::fmt::Debug;

#[derive(Debug)]
pub enum UefiEvent {
    BootSuccess(BootInfo),
    BootFailure(BootInfo),
    NoBootDevice,
    EfiDiagnosticsGpa(u32),
}

#[derive(Debug)]
pub struct BootInfo {
    pub secure_boot_succeeded: bool,
}

/// Interface to log UEFI events.
pub trait UefiLogger: Send {
    fn log_event(&self, event: UefiEvent);
}
