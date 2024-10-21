// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guid::Guid;
use std::os::windows::prelude::*;
use winapi::um::minwinbase::OVERLAPPED;

pal::delayload!("vmsif.dll" {
    pub fn VmsIfNicCreateEmulated(handle: &mut RawHandle, path: *const u16) -> u32;
    pub fn VmsIfNicCreateSynthetic(handle: &mut RawHandle, path: *const u16) -> u32;

    pub fn VmsIfNicMorphToEmulatedNic(
        handle: RawHandle,
        nic_name: *const u16,
        nic_friendly_name: *const u16,
        device_guid_string: *const u16,
        vm_name: *const u16,
        vm_id: *const u16,
        mac_address: &[u8; 6],
        is_static_mac_address: bool,
        partition_id: u64,
        switch_version: u32,
    ) -> u32;

    pub fn VmsIfNicMorphToSynthNic(
        vms_if_handle: RawHandle,
        nic_name: *const u16,
        nic_friendly_name: *const u16,
        vm_name: *const u16,
        vm_id: *const u16,
        mac_address: &[u8; 6],
        is_static_mac_address: bool,
        partition_id: u64,
        vmbus_handle: RawHandle,
        loopback_channel: bool,
        channel_instance: Guid,
        switch_version: u32,
        device_naming_enabled: bool,
        modern_io_enabled: bool,
        is_interrupt_moderation_disabled: bool,
        media_type: u32,
        numa_aware_placement: bool,
        is_phu_zero: bool,
        target_vtl: u8, // added post-Fe
    ) -> u32;

    pub fn VmsIfNicResumeSynthetic(
        vms_if_handle: RawHandle,
        nic_name: *const u16,
        state_flags: u32,
        overlapped: *mut OVERLAPPED,
    ) -> u32;

    pub fn VmsIfNicSuspendSynthetic(vms_if_handle: RawHandle, nic_name: *const u16) -> u32;

    pub fn VmsIfPortCreateWithHandle(
        vms_if_handle: &mut RawHandle,
        switch_name: *const u16,
        port_name: *const u16,
        friendly_name: *const u16,
        port_type: u32,
        has_saved_state: u32,
        is_persistent: u32,
        owner_service: u32,
    ) -> u32;

    pub fn VmsIfNicConnect(
        vms_if_handle: RawHandle,
        switch_name: *const u16,
        port_name: *const u16,
        nic_name: *const u16,
        timeout: u32,
    ) -> u32;
});

pub fn chk(e: u32) -> std::io::Result<()> {
    if e == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(e as i32))
    }
}
