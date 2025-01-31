// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI Event Logging subsystem.

use crate::platform::logger::BootInfo;
use crate::platform::logger::UefiEvent;
use crate::platform::logger::UefiLogger;
use crate::UefiDevice;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use std::fmt::Debug;
use thiserror::Error;
use zerocopy::FromBytes;

#[derive(Debug, Error)]
pub enum EventLogError {
    #[error("error converting bytes")]
    ConvertBytes,
    #[error("could not access guest memory")]
    Memory(#[source] GuestMemoryError),
    #[error("invalid event channel data size")]
    EventChannelDataSize,
    #[error("invalid boot event size")]
    BootEventSize,
    #[error("invalid event size")]
    EventSize,
    #[error("no boot events present in log")]
    NoBootEvents,
}

#[derive(Inspect)]
pub struct EventLogServices {
    #[inspect(skip)]
    logger: Box<dyn UefiLogger>,
}

impl EventLogServices {
    pub fn new(logger: Box<dyn UefiLogger>) -> EventLogServices {
        EventLogServices { logger }
    }

    pub fn reset(&mut self) {
        // Nothing to do.
    }

    fn event_log_flush_inner(&mut self, gpa: u64, gm: &GuestMemory) -> Result<(), EventLogError> {
        use uefi_specs::hyperv::bios_event_log::BiosEventChannel;
        use uefi_specs::hyperv::bios_event_log::EfiEventDescriptor;
        use uefi_specs::hyperv::boot_bios_log::BootDeviceStatus;
        use uefi_specs::hyperv::boot_bios_log::BootEventDeviceEntry;

        let event_channel = gm
            .read_plain::<BiosEventChannel>(gpa)
            .map_err(EventLogError::Memory)?;

        // Limit max size, UEFI does not log many events
        const EVENT_CHANNEL_MAX_DATA_SIZE: u32 = 16 * 1024;

        // Sanity check data size
        if event_channel.data_size < size_of::<EfiEventDescriptor>() as u32
            || event_channel.data_size > EVENT_CHANNEL_MAX_DATA_SIZE
        {
            return Err(EventLogError::EventChannelDataSize);
        }

        // read channel data
        let mut event_data = vec![0; event_channel.data_size as usize];
        gm.read_at(gpa + size_of::<BiosEventChannel>() as u64, &mut event_data)
            .map_err(EventLogError::Memory)?;
        let mut event_data = event_data.as_slice();

        // Merge the boot events together, aggregating an arbitrary subset of
        // the available diagnostics information.
        //
        // TODO: determine if we really want to merge events in this way instead
        // of just logging them individually.
        let mut boot_succeeded = false;
        let mut no_boot_devices = false;
        let mut secure_boot_failure = None;
        let mut last_boot_event = None;

        while !event_data.is_empty() {
            let desc = EfiEventDescriptor::read_from_prefix(event_data)
                .map_err(|_| EventLogError::ConvertBytes)?
                .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

            let data = event_data
                .get(desc.header_size as usize..)
                .ok_or(EventLogError::EventSize)?
                .get(..desc.data_size as usize)
                .ok_or(EventLogError::EventSize)?;

            // Advance to the next event.
            event_data = &event_data[(desc.header_size + desc.data_size) as usize..];

            match desc.event_id {
                uefi_specs::hyperv::boot_bios_log::BOOT_DEVICE_EVENT_ID => {
                    let boot_entry = BootEventDeviceEntry::read_from_prefix(data)
                        .map_err(|_| EventLogError::BootEventSize)?
                        .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

                    tracing::debug!(?boot_entry, "boot log entry");

                    match boot_entry.status {
                        BootDeviceStatus::BOOT_DEVICE_OS_LOADED => boot_succeeded = true,
                        BootDeviceStatus::BOOT_DEVICE_NO_DEVICES => no_boot_devices = true,
                        _ if boot_entry.status.get_boot_device_status_group()
                            == BootDeviceStatus::SECURE_BOOT_FAILED.0 =>
                        {
                            secure_boot_failure = Some(boot_entry.status);
                        }
                        _ => {}
                    }

                    last_boot_event = Some(boot_entry);
                }
                id => {
                    tracelimit::warn_ratelimited!(id, "unsupported uefi event log id");
                }
            }
        }

        let last_boot_event = last_boot_event.ok_or(EventLogError::NoBootEvents)?;
        let boot_info = BootInfo {
            secure_boot_succeeded: secure_boot_failure.is_none(),
        };

        // Don't log the secure boot failure code twice if it's the reason for
        // the boot failure.
        let secure_boot_error = if secure_boot_failure != Some(last_boot_event.status) {
            secure_boot_failure.map(tracing::field::debug)
        } else {
            None
        };

        let event = if no_boot_devices {
            tracelimit::info_ratelimited!("uefi boot: no boot devices");
            UefiEvent::NoBootDevice
        } else if boot_succeeded {
            tracelimit::info_ratelimited!(secure_boot_error, "uefi boot: success");
            UefiEvent::BootSuccess(boot_info)
        } else {
            tracelimit::info_ratelimited!(
                error = ?last_boot_event.status,
                extended_status = ?last_boot_event.extended_status,
                secure_boot_error,
                "uefi boot: failure",
            );
            UefiEvent::BootFailure(boot_info)
        };
        self.logger.log_event(event);
        Ok(())
    }
}

impl UefiDevice {
    /// Reads guest memory and logs the boot status to the host.
    pub(crate) fn event_log_flush(&mut self, data: u32) {
        if let Err(err) = self
            .service
            .event_log
            .event_log_flush_inner(data.into(), &self.gm)
        {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "event log flush error"
            );
        }
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    impl SaveRestore for EventLogServices {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }
}
