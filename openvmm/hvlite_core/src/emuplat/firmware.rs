// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backing implementations of various UEFI platform requirements.

use firmware_pcat::PcatEvent;
use firmware_pcat::PcatLogger;
use firmware_uefi::platform::logger::UefiEvent;
use firmware_uefi::platform::logger::UefiLogger;
use get_resources::ged::FirmwareEvent;

/// Forwards UEFI and PCAT events to via the provided [`mesh::Sender`].
#[derive(Debug)]
pub struct MeshLogger {
    sender: Option<mesh::Sender<FirmwareEvent>>,
}

impl MeshLogger {
    pub fn new(sender: Option<mesh::Sender<FirmwareEvent>>) -> Self {
        Self { sender }
    }

    fn send(&self, event: FirmwareEvent) {
        if let Some(sender) = &self.sender {
            sender.send(event);
        }
    }
}

impl UefiLogger for MeshLogger {
    fn log_event(&self, event: UefiEvent) {
        let event = match event {
            UefiEvent::BootSuccess(_) => FirmwareEvent::BootSuccess,
            UefiEvent::BootFailure(_) => FirmwareEvent::BootFailed,
            UefiEvent::NoBootDevice => FirmwareEvent::NoBootDevice,
        };
        self.send(event);
    }
}

impl PcatLogger for MeshLogger {
    fn log_event(&self, event: PcatEvent) {
        let event = match event {
            PcatEvent::BootFailure => FirmwareEvent::BootFailed,
            PcatEvent::BootAttempt => FirmwareEvent::BootAttempt,
        };
        self.send(event);
    }
}
