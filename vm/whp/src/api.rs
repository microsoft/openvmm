// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::abi::*;
use std::ffi::c_void;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Power::DEVICE_POWER_STATE;
use windows_sys::core::GUID;
use windows_sys::core::HRESULT;

pub const WHV_E_UNKNOWN_CAPABILITY: HRESULT = 0x80370300u32 as HRESULT;
pub const WHV_E_INSUFFICIENT_BUFFER: HRESULT = 0x80370301u32 as HRESULT;

#[link(
    name = "WinHvPlatform.dll",
    kind = "raw-dylib",
    modifiers = "+verbatim"
)]
unsafe extern "system" {
    pub fn WHvGetCapability(
        CapabilityCode: WHV_CAPABILITY_CODE,
        CapabilityBuffer: *mut u8,
        CapabilityBufferSizeInBytes: u32,
        WrittenSizeInBytes: Option<&mut u32>,
    ) -> HRESULT;

    pub fn WHvCreatePartition(partition: *mut WHV_PARTITION_HANDLE) -> HRESULT;

    pub fn WHvDeletePartition(_: WHV_PARTITION_HANDLE) -> HRESULT;

    pub fn WHvSetupPartition(_: WHV_PARTITION_HANDLE) -> HRESULT;

    pub fn WHvGetPartitionProperty(
        _: WHV_PARTITION_HANDLE,
        code: WHV_PARTITION_PROPERTY_CODE,
        data: *mut u8,
        len: u32,
        out_len: &mut u32,
    ) -> HRESULT;

    pub fn WHvSetPartitionProperty(
        _: WHV_PARTITION_HANDLE,
        code: WHV_PARTITION_PROPERTY_CODE,
        data: *const u8,
        len: u32,
    ) -> HRESULT;

    pub fn WHvRequestInterrupt(
        _: WHV_PARTITION_HANDLE,
        control: *const WHV_INTERRUPT_CONTROL,
        len: u32,
    ) -> HRESULT;

    pub fn WHvCreateVirtualProcessor(_: WHV_PARTITION_HANDLE, vp: u32, flags: u32) -> HRESULT;

    pub fn WHvDeleteVirtualProcessor(_: WHV_PARTITION_HANDLE, vp: u32) -> HRESULT;

    pub fn WHvCancelRunVirtualProcessor(_: WHV_PARTITION_HANDLE, vp: u32, flags: u32) -> HRESULT;

    pub fn WHvGetVirtualProcessorRegisters(
        _: WHV_PARTITION_HANDLE,
        vp: u32,
        names: *const WHV_REGISTER_NAME,
        count: u32,
        values: *mut WHV_REGISTER_VALUE,
    ) -> HRESULT;

    pub fn WHvSetVirtualProcessorRegisters(
        _: WHV_PARTITION_HANDLE,
        vp: u32,
        names: *const WHV_REGISTER_NAME,
        count: u32,
        values: *const WHV_REGISTER_VALUE,
    ) -> HRESULT;

    pub fn WHvRunVirtualProcessor(
        _: WHV_PARTITION_HANDLE,
        vp: u32,
        context: *mut WHV_RUN_VP_EXIT_CONTEXT,
        size: u32,
    ) -> HRESULT;

    pub fn WHvTranslateGva(
        _: WHV_PARTITION_HANDLE,
        vp: u32,
        gva: u64,
        flags: u32,
        result: *mut WHV_TRANSLATE_GVA_RESULT,
        gpa: *mut u64,
    ) -> HRESULT;

    pub fn WHvMapGpaRange(
        _: WHV_PARTITION_HANDLE,
        src: *mut c_void,
        dest: u64,
        size: u64,
        flags: WHV_MAP_GPA_RANGE_FLAGS,
    ) -> HRESULT;

    pub fn WHvUnmapGpaRange(_: WHV_PARTITION_HANDLE, addr: u64, size: u64) -> HRESULT;
}

// These APIs were added after the first release and so may not be present.
pal::delayload!("WinHvPlatform.dll" {
    pub fn WHvResetPartition(partition: WHV_PARTITION_HANDLE) -> HRESULT;

    pub fn WHvRegisterPartitionDoorbellEvent(
        partition: WHV_PARTITION_HANDLE,
        match_data: &WHV_DOORBELL_MATCH_DATA,
        event_handle: HANDLE,
    ) -> HRESULT;

    pub fn WHvUnregisterPartitionDoorbellEvent(
        partition: WHV_PARTITION_HANDLE,
        match_data: &WHV_DOORBELL_MATCH_DATA,
    ) -> HRESULT;

    pub fn WHvGetVirtualProcessorXsaveState(
        partition: WHV_PARTITION_HANDLE,
        vp_index: u32,
        buffer: *mut u8,
        buffer_size_in_bytes: u32,
        bytes_written: *mut u32,
    ) -> HRESULT;

    pub fn WHvSetVirtualProcessorXsaveState(
        partition: WHV_PARTITION_HANDLE,
        vp_index: u32,
        buffer: *const u8,
        buffer_size_in_bytes: u32,
    ) -> HRESULT;

    pub fn WHvGetVirtualProcessorInterruptControllerState2(
        partition: WHV_PARTITION_HANDLE,
        vp_index: u32,
        state: *mut u8,
        state_size: u32,
        written_size: *mut u32,
    ) -> HRESULT;

    pub fn WHvSetVirtualProcessorInterruptControllerState2(
        partition: WHV_PARTITION_HANDLE,
        vp_index: u32,
        state: *const u8,
        state_size: u32,
    ) -> HRESULT;

    pub fn WHvGetVirtualProcessorState(
        partition: WHV_PARTITION_HANDLE,
        vp_index: u32,
        state_type: WHV_VIRTUAL_PROCESSOR_STATE_TYPE,
        buffer: *mut u8,
        buffer_size: u32,
        written_size: *mut u32,
    ) -> HRESULT;

    pub fn WHvSetVirtualProcessorState(
        partition: WHV_PARTITION_HANDLE,
        vp_index: u32,
        state_type: WHV_VIRTUAL_PROCESSOR_STATE_TYPE,
        state: *const u8,
        state_size: u32,
    ) -> HRESULT;


    pub fn WHvAdviseGpaRange(
        partition: WHV_PARTITION_HANDLE,
        ranges: *const WHV_MEMORY_RANGE_ENTRY,
        range_count: u32,
        advise: WHV_ADVISE_GPA_RANGE_CODE,
        buffer: *const c_void,
        len: u32,
    ) -> HRESULT;

    pub fn WHvSignalVirtualProcessorSynicEvent(
        Partition: WHV_PARTITION_HANDLE,
        SynicEvent: WHV_SYNIC_EVENT_PARAMETERS,
        NewlySignaled: *mut u32,
    ) -> HRESULT;

    pub fn WHvPostVirtualProcessorSynicMessage(
        partition: WHV_PARTITION_HANDLE,
        vp_index: u32,
        sint_index: u32,
        message: *const u8,
        message_size: u32,
    ) -> HRESULT;

    pub fn WHvCreateNotificationPort(
        Partition: WHV_PARTITION_HANDLE,
        Parameters: &WHV_NOTIFICATION_PORT_PARAMETERS,
        EventHandle: HANDLE,
        PortHandle: &mut WHV_NOTIFICATION_PORT_HANDLE,
    ) -> HRESULT;

    pub fn WHvSetNotificationPortProperty(
        Partition: WHV_PARTITION_HANDLE,
        PortHandle: WHV_NOTIFICATION_PORT_HANDLE,
        PropertyCode: WHV_NOTIFICATION_PORT_PROPERTY_CODE,
        PropertyValue: u64,
    ) -> HRESULT;

    pub fn WHvDeleteNotificationPort(
        Partition: WHV_PARTITION_HANDLE,
        PortHandle: WHV_NOTIFICATION_PORT_HANDLE,
    ) -> HRESULT;

    pub fn WHvGetVirtualProcessorCpuidOutput(
        Partition: WHV_PARTITION_HANDLE,
        VpIndex: u32,
        Eax: u32,
        Ecx: u32,
        CpuidOutput: *mut WHV_CPUID_OUTPUT,
    ) -> HRESULT;

    pub fn WHvMapGpaRange2(
        Partition: WHV_PARTITION_HANDLE,
        Process: HANDLE,
        Src: *mut c_void,
        Dest: u64,
        Size: u64,
        Flags: WHV_MAP_GPA_RANGE_FLAGS,
    ) -> HRESULT;

    pub fn WHvStartPartitionMigration(
        Partition: WHV_PARTITION_HANDLE,
        MigrationHandle: *mut HANDLE,
    ) -> HRESULT;

    pub fn WHvCancelPartitionMigration(Partition: WHV_PARTITION_HANDLE) -> HRESULT;

    pub fn WHvCompletePartitionMigration(Partition: WHV_PARTITION_HANDLE) -> HRESULT;

    pub fn WHvAcceptPartitionMigration(
        MigrationHandle: HANDLE,
        Partition: *mut WHV_PARTITION_HANDLE,
    ) -> HRESULT;

    #[cfg(target_arch = "x86_64")]
    pub fn WHvGetInterruptTargetVpSet(
        Partition: WHV_PARTITION_HANDLE,
        Destination: u64,
        DestinationMode: WHV_INTERRUPT_DESTINATION_MODE,
        TargetVps: *mut u32,
        VpCount: u32,
        TargetVpCount: *mut u32,
    ) -> HRESULT;

    pub fn WHvCreateVirtualProcessor2(
        Partition: WHV_PARTITION_HANDLE,
        VpIndex: u32,
        Properties: *const WHV_VIRTUAL_PROCESSOR_PROPERTY,
        PropertyCount: u32,
    ) -> HRESULT;

    pub fn WHvAllocateVpciResource(
        ProviderId: Option<&GUID>,
        Flags: WHV_ALLOCATE_VPCI_RESOURCE_FLAGS,
        ResourceDescriptor: *const c_void,
        ResourceDescriptorSizeInBytes: u32,
        VpciResource: *mut HANDLE,
    ) -> HRESULT;

    pub fn WHvCreateVpciDevice(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        VpciResource: HANDLE,
        Flags: WHV_CREATE_VPCI_DEVICE_FLAGS,
        NotificationEventHandle: HANDLE,
    ) -> HRESULT;

    pub fn WHvDeleteVpciDevice(Partition: WHV_PARTITION_HANDLE, LogicalDeviceId: u64) -> HRESULT;

    pub fn WHvGetVpciDeviceProperty(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        PropertyCode: WHV_VPCI_DEVICE_PROPERTY_CODE,
        PropertyBuffer: *mut c_void,
        PropertyBufferSizeInBytes: u32,
        WrittenSizeInBytes: *mut u32,
    ) -> HRESULT;

    pub fn WHvGetVpciDeviceNotification(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        Notification: *mut WHV_VPCI_DEVICE_NOTIFICATION,
        NotificationSizeInBytes: u32,
    ) -> HRESULT;

    pub fn WHvMapVpciDeviceMmioRanges(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        MappingCount: *mut u32,
        Mappings: *mut *const WHV_VPCI_MMIO_MAPPING,
    ) -> HRESULT;

    pub fn WHvUnmapVpciDeviceMmioRanges(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
    ) -> HRESULT;

    pub fn WHvSetVpciDevicePowerState(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        PowerState: DEVICE_POWER_STATE,
    ) -> HRESULT;

    pub fn WHvReadVpciDeviceRegister(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        Register: &WHV_VPCI_DEVICE_REGISTER,
        Data: *mut c_void,
    ) -> HRESULT;

    pub fn WHvWriteVpciDeviceRegister(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        Register: &WHV_VPCI_DEVICE_REGISTER,
        Data: *const c_void,
    ) -> HRESULT;

    pub fn WHvMapVpciDeviceInterrupt(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        Index: u32,
        MessageCount: u32,
        Target: *const WHV_VPCI_INTERRUPT_TARGET,
        MsiAddress: *mut u64,
        MsiData: *mut u32,
    ) -> HRESULT;

    pub fn WHvUnmapVpciDeviceInterrupt(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        Index: u32,
    ) -> HRESULT;

    pub fn WHvRetargetVpciDeviceInterrupt(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        MsiAddress: u64,
        MsiData: u32,
        Target: *const WHV_VPCI_INTERRUPT_TARGET,
    ) -> HRESULT;

    pub fn WHvRequestVpciDeviceInterrupt(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        MsiAddress: u64,
        MsiData: u32,
    ) -> HRESULT;

    pub fn WHvGetVpciDeviceInterruptTarget(
        Partition: WHV_PARTITION_HANDLE,
        LogicalDeviceId: u64,
        Index: u32,
        MultiMessageNumber: u32,
        Target: *mut WHV_VPCI_INTERRUPT_TARGET,
        TargetSizeInBytes: u32,
        BytesWritten: *mut u32,
    ) -> HRESULT;

    pub fn WHvCreateTrigger(
        Partition: WHV_PARTITION_HANDLE,
        Parameters: *const WHV_TRIGGER_PARAMETERS,
        TriggerHandle: *mut WHV_TRIGGER_HANDLE,
        EventHandle: *mut HANDLE,
    ) -> HRESULT;

    pub fn WHvUpdateTriggerParameters(
        Partition: WHV_PARTITION_HANDLE,
        Parameters: *const WHV_TRIGGER_PARAMETERS,
        TriggerHandle: WHV_TRIGGER_HANDLE,
    ) -> HRESULT;

    pub fn WHvDeleteTrigger(
        Partition: WHV_PARTITION_HANDLE,
        TriggerHandle: WHV_TRIGGER_HANDLE,
    ) -> HRESULT;

    pub fn WHvSuspendPartitionTime(
        Partition: WHV_PARTITION_HANDLE,
    ) -> HRESULT;

    pub fn WHvResumePartitionTime(
        Partition: WHV_PARTITION_HANDLE,
    ) -> HRESULT;
});
