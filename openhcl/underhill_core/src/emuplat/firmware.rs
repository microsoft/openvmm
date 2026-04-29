// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use cvm_tracing::CVM_ALLOWED;
use firmware_uefi::platform::logger::UefiEvent;
use firmware_uefi::platform::logger::UefiLogger;
use guest_emulation_transport::GuestEmulationTransportClient;
use guest_emulation_transport::api::EventLogId;
use std::sync::Weak;
use virt_mshv_vtl::UhPartition;

/// An Underhill specific logger used to log UEFI and PCAT events.
#[derive(Debug)]
pub struct UnderhillLogger {
    pub get: GuestEmulationTransportClient,
}

impl UefiLogger for UnderhillLogger {
    fn log_event(&self, event: UefiEvent) {
        let log_event_id = match event {
            UefiEvent::BootSuccess(boot_info) => {
                if boot_info.secure_boot_succeeded {
                    EventLogId::BOOT_SUCCESS
                } else {
                    EventLogId::BOOT_SUCCESS_SECURE_BOOT_FAILED
                }
            }
            UefiEvent::BootFailure(boot_info) => {
                if boot_info.secure_boot_succeeded {
                    EventLogId::BOOT_FAILURE
                } else {
                    EventLogId::BOOT_FAILURE_SECURE_BOOT_FAILED
                }
            }
            UefiEvent::NoBootDevice => EventLogId::NO_BOOT_DEVICE,
        };
        self.get.event_log(log_event_id);
    }
}

#[cfg(guest_arch = "x86_64")]
impl firmware_pcat::PcatLogger for UnderhillLogger {
    fn log_event(&self, event: firmware_pcat::PcatEvent) {
        let log_event_id = match event {
            firmware_pcat::PcatEvent::BootFailure => EventLogId::BOOT_FAILURE,
            firmware_pcat::PcatEvent::BootAttempt => EventLogId::BOOT_ATTEMPT,
        };
        self.get.event_log(log_event_id);
    }
}

#[derive(Debug)]
pub struct UnderhillVsmConfig {
    pub partition: Weak<UhPartition>,
}

impl firmware_uefi::platform::nvram::VsmConfig for UnderhillVsmConfig {
    fn revoke_guest_vsm(&self) {
        if let Some(partition) = self.partition.upgrade() {
            if let Err(err) = partition.revoke_guest_vsm() {
                tracing::warn!(
                    CVM_ALLOWED,
                    error = &err as &dyn std::error::Error,
                    "failed to revoke guest vsm"
                );
            }
        }
    }
}

/// MOR (Memory Overwrite Request) configuration for Underhill.
///
/// When the guest sets the MOR bit, this notifies the hypervisor to ensure
/// memory is scrubbed on the next partition reset by setting the
/// `zero_memory_on_reset` flag in `HvRegisterVsmPartitionConfig`.
#[derive(Debug)]
pub struct UnderhillMorConfig {
    pub partition: Weak<UhPartition>,
}

impl firmware_uefi::platform::nvram::MorConfig for UnderhillMorConfig {
    fn notify_mor_set(&self, mor_value: u8) {
        const MOR_CLEAR_MEMORY_BIT_MASK: u8 = 0x01;

        let clear_memory = (mor_value & MOR_CLEAR_MEMORY_BIT_MASK) != 0;

        if clear_memory {
            if let Some(partition) = self.partition.upgrade() {
                if let Err(err) = partition.set_zero_memory_on_reset(true) {
                    tracing::warn!(
                        CVM_ALLOWED,
                        error = &err as &dyn std::error::Error,
                        "failed to set zero_memory_on_reset for MOR"
                    );
                }
            }
        }
    }
}
