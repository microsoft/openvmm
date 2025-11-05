// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(windows)]
// UNSAFETY: Calling WHP APIs.
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks)]

pub mod abi;
mod api;
mod arm64;
mod partition_prop;
mod x64;

#[cfg(target_arch = "aarch64")]
pub use arm64::*;
#[cfg(target_arch = "x86_64")]
pub use x64::*;

use guid::Guid;
use std::alloc::Layout;
use std::ffi::c_void;
use std::fmt;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::num::NonZeroI32;
use std::num::NonZeroU16;
use std::os::windows::prelude::*;
use std::ptr::NonNull;
use std::ptr::null;
use std::ptr::null_mut;
use winapi::shared::ntdef::LUID;
use winapi::shared::winerror;
use winapi::um::winnt::DEVICE_POWER_STATE;
use winerror::ERROR_BAD_PATHNAME;

/// Functions to get the WHP platform's capabilities.
pub mod capabilities {
    use super::*;
    fn get<T>(code: abi::WHV_CAPABILITY_CODE) -> Result<T> {
        let mut val = std::mem::MaybeUninit::<T>::uninit();
        unsafe {
            let argn = size_of_val(&val) as u32;
            let argp = std::ptr::from_mut(&mut val).cast::<u8>();
            let mut outn = 0;
            check_hresult(api::WHvGetCapability(code, argp, argn, Some(&mut outn)))?;
            if outn < argn {
                panic!("output result too small");
            }
            Ok(val.assume_init())
        }
    }

    /// Are the WHP APIs available?
    pub fn hypervisor_present() -> Result<bool> {
        Ok(get::<u32>(abi::WHvCapabilityCodeHypervisorPresent)? != 0)
    }
    /// The WHP features that are available.
    pub fn features() -> Result<abi::WHV_CAPABILITY_FEATURES> {
        get(abi::WHvCapabilityCodeFeatures)
    }
    /// The extended VM exits that are available.
    pub fn extended_vm_exits() -> Result<abi::WHV_EXTENDED_VM_EXITS> {
        get(abi::WHvCapabilityCodeExtendedVmExits)
    }
    /// Exceptions that can be exited on.
    #[cfg(target_arch = "x86_64")]
    pub fn exception_exit_bitmap() -> Result<u64> {
        get(abi::WHvCapabilityCodeExceptionExitBitmap)
    }
    /// MSRs that can be exited on.
    #[cfg(target_arch = "x86_64")]
    pub fn x64_msr_exit_bitmap() -> Result<abi::WHV_X64_MSR_EXIT_BITMAP> {
        get(abi::WHvCapabilityCodeX64MsrExitBitmap)
    }
    /// Supported GPA range prefetch flags.
    pub fn gpa_range_populate_flags() -> Result<abi::WHV_ADVISE_GPA_RANGE_POPULATE_FLAGS> {
        get(abi::WHvCapabilityCodeGpaRangePopulateFlags)
    }
    /// The host's processor vendor.
    pub fn processor_vendor() -> Result<abi::WHV_PROCESSOR_VENDOR> {
        get(abi::WHvCapabilityCodeProcessorVendor)
    }
    /// The cache line flush size.
    pub fn processor_cl_flush_size() -> Result<u8> {
        get(abi::WHvCapabilityCodeProcessorClFlushSize)
    }
    /// The xsave features that are available.
    #[cfg(target_arch = "x86_64")]
    pub fn processor_xsave_features() -> Result<abi::WHV_PROCESSOR_XSAVE_FEATURES> {
        get(abi::WHvCapabilityCodeProcessorXsaveFeatures)
    }
    /// The processor TSC clock frequency.
    pub fn processor_clock_frequency() -> Result<u64> {
        get(abi::WHvCapabilityCodeProcessorClockFrequency)
    }
    /// The APIC interrupt clock frequency.
    #[cfg(target_arch = "x86_64")]
    pub fn interrupt_clock_frequency() -> Result<u64> {
        get(abi::WHvCapabilityCodeInterruptClockFrequency)
    }
    /// The available processor features.
    pub fn processor_features() -> Result<ProcessorFeatures> {
        match get::<abi::WHV_PROCESSOR_FEATURES_BANKS>(abi::WHvCapabilityCodeProcessorFeaturesBanks)
        {
            Ok(banks) => Ok(ProcessorFeatures {
                bank0: abi::WHV_PROCESSOR_FEATURES(banks.Banks[0]),
                bank1: abi::WHV_PROCESSOR_FEATURES1(banks.Banks[1]),
            }),
            Err(WHvError::WHV_E_UNKNOWN_CAPABILITY) => {
                // Fall back to the old feature query.
                Ok(ProcessorFeatures {
                    bank0: get(abi::WHvCapabilityCodeProcessorFeatures)?,
                    bank1: abi::WHV_PROCESSOR_FEATURES1(0),
                })
            }
            Err(err) => Err(err),
        }
    }
    /// Processor frequency capping capabilities.
    pub fn processor_frequency_cap() -> Result<abi::WHV_CAPABILITY_PROCESSOR_FREQUENCY_CAP> {
        get(abi::WHvCapabilityCodeProcessorFrequencyCap)
    }
    /// The available synthetic processor features.
    pub fn synthetic_processor_features() -> Result<SyntheticProcessorFeatures> {
        let b0 = match get::<abi::WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS>(
            abi::WHvCapabilityCodeSyntheticProcessorFeaturesBanks,
        ) {
            Ok(banks) => {
                assert_eq!(banks.BanksCount, 1);
                banks.Banks[0]
            }
            Err(WHvError::WHV_E_UNKNOWN_CAPABILITY) => 0,
            Err(err) => return Err(err),
        };
        Ok(SyntheticProcessorFeatures {
            bank0: abi::WHV_SYNTHETIC_PROCESSOR_FEATURES(b0),
        })
    }
    #[cfg(target_arch = "x86_64")]
    pub fn perfmon_features() -> Result<abi::WHV_PROCESSOR_PERFMON_FEATURES> {
        get(abi::WHvCapabilityCodePerfmonFeatures)
    }
    pub fn scheduler_features() -> Result<abi::WHV_SCHEDULER_FEATURES> {
        get(abi::WHvCapabilityCodeSchedulerFeatures)
    }
    pub fn reset_partition() -> bool {
        api::is_supported::WHvResetPartition()
    }
}

#[non_exhaustive]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct ProcessorFeatures {
    pub bank0: abi::WHV_PROCESSOR_FEATURES,
    pub bank1: abi::WHV_PROCESSOR_FEATURES1,
}

#[non_exhaustive]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct SyntheticProcessorFeatures {
    pub bank0: abi::WHV_SYNTHETIC_PROCESSOR_FEATURES,
}

#[derive(Clone, Eq, PartialEq)]
pub struct WHvError(NonZeroI32);

impl WHvError {
    pub const WHV_E_UNKNOWN_CAPABILITY: Self =
        Self(NonZeroI32::new(api::WHV_E_UNKNOWN_CAPABILITY).unwrap());

    const WHV_E_INSUFFICIENT_BUFFER: Self =
        Self(NonZeroI32::new(api::WHV_E_INSUFFICIENT_BUFFER).unwrap());

    const ERROR_BAD_PATHNAME: Self = Self(NonZeroI32::new(ERROR_BAD_PATHNAME as i32).unwrap());

    pub fn code(&self) -> i32 {
        self.0.get()
    }

    /// Returns the underlying hypervisor error code, if there is one.
    pub fn hv_result(&self) -> Option<NonZeroU16> {
        if self.0.get() & 0x3fff0000 == 0x00350000 {
            // This is a hypervisor facility code.
            Some(NonZeroU16::new(self.0.get() as u16).unwrap())
        } else {
            None
        }
    }
}

impl fmt::Display for WHvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&std::io::Error::from_raw_os_error(self.0.get()), f)
    }
}

impl Debug for WHvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for WHvError {}

impl From<WHvError> for std::io::Error {
    fn from(err: WHvError) -> Self {
        std::io::Error::from_raw_os_error(err.0.get())
    }
}

pub struct PartitionConfig {
    partition: Partition,
}

#[derive(Debug, Copy, Clone)]
pub enum PartitionProperty<'a> {
    ExtendedVmExits(abi::WHV_EXTENDED_VM_EXITS),
    #[cfg(target_arch = "x86_64")]
    ExceptionExitBitmap(u64),
    SeparateSecurityDomain(bool),
    #[cfg(target_arch = "x86_64")]
    X64MsrExitBitmap(abi::WHV_X64_MSR_EXIT_BITMAP),
    PrimaryNumaNode(u16),
    CpuReserve(u32),
    CpuCap(u32),
    CpuWeight(u32),
    CpuGroupId(u64),
    ProcessorFrequencyCap(u32),
    AllowDeviceAssignment(bool),
    DisableSmt(bool),
    ProcessorFeatures(ProcessorFeatures),
    ProcessorClFlushSize(u8),
    #[cfg(target_arch = "x86_64")]
    CpuidExitList(&'a [u32]),
    #[cfg(target_arch = "x86_64")]
    CpuidResultList(&'a [abi::WHV_X64_CPUID_RESULT]),
    #[cfg(target_arch = "x86_64")]
    LocalApicEmulationMode(abi::WHV_X64_LOCAL_APIC_EMULATION_MODE),
    #[cfg(target_arch = "x86_64")]
    ProcessorXsaveFeatures(abi::WHV_PROCESSOR_XSAVE_FEATURES),
    ProcessorClockFrequency(u64),
    #[cfg(target_arch = "x86_64")]
    InterruptClockFrequency(u64),
    #[cfg(target_arch = "x86_64")]
    ApicRemoteReadSupport(bool),
    ReferenceTime(u64),
    SyntheticProcessorFeatures(SyntheticProcessorFeatures),
    #[cfg(target_arch = "x86_64")]
    CpuidResultList2(&'a [abi::WHV_X64_CPUID_RESULT2]),
    #[cfg(target_arch = "x86_64")]
    PerfmonFeatures(abi::WHV_PROCESSOR_PERFMON_FEATURES),
    #[cfg(target_arch = "x86_64")]
    MsrActionList(&'a [abi::WHV_MSR_ACTION_ENTRY]),
    #[cfg(target_arch = "x86_64")]
    UnimplementedMsrAction(abi::WHV_MSR_ACTION),
    ProcessorCount(u32),
    PhysicalAddressWidth(u32),
    #[cfg(target_arch = "aarch64")]
    GicParameters(abi::WHV_ARM64_IC_PARAMETERS),
    // Needed to reference 'a on aarch64.
    #[doc(hidden)]
    #[cfg(target_arch = "aarch64")]
    _Dummy(std::convert::Infallible, PhantomData<&'a ()>),
}

impl PartitionConfig {
    pub fn new() -> Result<PartitionConfig> {
        let mut handle = abi::WHV_PARTITION_HANDLE(0);
        unsafe {
            check_hresult(api::WHvCreatePartition(&mut handle))?;
        }
        Ok(PartitionConfig {
            partition: Partition { handle },
        })
    }

    pub fn set_property(
        &mut self,
        property: PartitionProperty<'_>,
    ) -> Result<&mut PartitionConfig> {
        self.partition.set_property(property)?;
        Ok(self)
    }

    pub fn create(self) -> Result<Partition> {
        unsafe {
            check_hresult(api::WHvSetupPartition(self.partition.handle))?;
        }
        Ok(self.partition)
    }
}

#[derive(Debug)]
pub struct Partition {
    handle: abi::WHV_PARTITION_HANDLE,
}

impl Drop for Partition {
    fn drop(&mut self) {
        unsafe {
            check_hresult(api::WHvDeletePartition(self.handle)).unwrap();
        }
    }
}

type Result<T> = std::result::Result<T, WHvError>;

fn check_hresult(hr: i32) -> Result<()> {
    if hr >= 0 {
        Ok(())
    } else {
        Err(WHvError(NonZeroI32::new(hr).unwrap()))
    }
}

pub struct VpBuilder<'a> {
    partition: &'a Partition,
    index: u32,
    numa_node: Option<u16>,
}

impl VpBuilder<'_> {
    pub fn create(self) -> Result<()> {
        if let Some(node) = self.numa_node {
            let prop = abi::WHV_VIRTUAL_PROCESSOR_PROPERTY {
                PropertyCode: abi::WHvVirtualProcessorPropertyCodeNumaNode,
                Reserved: 0,
                u: abi::WHV_VIRTUAL_PROCESSOR_PROPERTY_u { NumaNode: node },
            };
            unsafe {
                check_hresult(api::WHvCreateVirtualProcessor2(
                    self.partition.handle,
                    self.index,
                    &prop,
                    1,
                ))
            }
        } else {
            unsafe {
                check_hresult(api::WHvCreateVirtualProcessor(
                    self.partition.handle,
                    self.index,
                    0,
                ))
            }
        }
    }
}

impl Partition {
    pub fn reset(&self) -> Result<()> {
        // SAFETY: Calling API as intended with a valid partition handle.
        unsafe { check_hresult(api::WHvResetPartition(self.handle)) }
    }

    pub fn set_property(&self, property: PartitionProperty<'_>) -> Result<()> {
        struct Input<'a>(
            abi::WHV_PARTITION_PROPERTY_CODE,
            *const u8,
            u32,
            PhantomData<&'a ()>,
        );
        fn set<T: partition_prop::AssociatedType>(code: T, val: &T::Type) -> Input<'_> {
            Input(
                code.code(),
                (&raw const *val).cast(),
                size_of_val(val).try_into().unwrap(),
                PhantomData,
            )
        }

        let banks;
        let synth_banks;
        let abi_bool;
        let data = match &property {
            PartitionProperty::ExtendedVmExits(val) => set(partition_prop::ExtendedVmExits, val),
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::ExceptionExitBitmap(val) => {
                set(partition_prop::ExceptionExitBitmap, val)
            }
            PartitionProperty::SeparateSecurityDomain(val) => {
                abi_bool = (*val).into();
                set(partition_prop::SeparateSecurityDomain, &abi_bool)
            }
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::X64MsrExitBitmap(val) => set(partition_prop::X64MsrExitBitmap, val),
            PartitionProperty::PrimaryNumaNode(val) => set(partition_prop::PrimaryNumaNode, val),
            PartitionProperty::CpuReserve(val) => set(partition_prop::CpuReserve, val),
            PartitionProperty::CpuCap(val) => set(partition_prop::CpuCap, val),
            PartitionProperty::CpuWeight(val) => set(partition_prop::CpuWeight, val),
            PartitionProperty::CpuGroupId(val) => set(partition_prop::CpuGroupId, val),
            PartitionProperty::ProcessorFrequencyCap(val) => {
                set(partition_prop::ProcessorFrequencyCap, val)
            }
            PartitionProperty::AllowDeviceAssignment(val) => {
                abi_bool = (*val).into();
                set(partition_prop::AllowDeviceAssignment, &abi_bool)
            }
            PartitionProperty::DisableSmt(val) => {
                abi_bool = (*val).into();
                set(partition_prop::DisableSmt, &abi_bool)
            }
            PartitionProperty::ProcessorFeatures(val) => {
                let ProcessorFeatures {
                    bank0: b0,
                    bank1: b1,
                } = val;

                if b1.0 == 0 {
                    // Use the old interface if possible.
                    set(partition_prop::ProcessorFeatures, b0)
                } else {
                    banks = abi::WHV_PROCESSOR_FEATURES_BANKS {
                        BanksCount: 2,
                        Reserved0: 0,
                        Banks: [b0.0, b1.0],
                    };
                    set(partition_prop::ProcessorFeaturesBanks, &banks)
                }
            }
            PartitionProperty::ProcessorClFlushSize(val) => {
                set(partition_prop::ProcessorClFlushSize, val)
            }
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::CpuidExitList(val) => set(partition_prop::CpuidExitList, val),
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::CpuidResultList(val) => set(partition_prop::CpuidResultList, val),
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::LocalApicEmulationMode(val) => {
                set(partition_prop::LocalApicEmulationMode, val)
            }
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::ProcessorXsaveFeatures(val) => {
                set(partition_prop::ProcessorXsaveFeatures, val)
            }
            PartitionProperty::ProcessorClockFrequency(val) => {
                set(partition_prop::ProcessorClockFrequency, val)
            }
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::InterruptClockFrequency(val) => {
                set(partition_prop::InterruptClockFrequency, val)
            }
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::ApicRemoteReadSupport(val) => {
                abi_bool = (*val).into();
                set(partition_prop::ApicRemoteReadSupport, &abi_bool)
            }
            PartitionProperty::ReferenceTime(val) => set(partition_prop::ReferenceTime, val),
            PartitionProperty::SyntheticProcessorFeatures(val) => {
                let SyntheticProcessorFeatures { bank0: b0 } = val;
                synth_banks = abi::WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS {
                    BanksCount: 1,
                    Reserved0: 0,
                    Banks: [b0.0],
                };
                set(
                    partition_prop::SyntheticProcessorFeaturesBanks,
                    &synth_banks,
                )
            }
            PartitionProperty::ProcessorCount(val) => set(partition_prop::ProcessorCount, val),
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::CpuidResultList2(val) => set(partition_prop::CpuidResultList2, val),
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::PerfmonFeatures(val) => {
                set(partition_prop::ProcessorPerfmonFeatures, val)
            }
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::MsrActionList(val) => set(partition_prop::MsrActionList, val),
            #[cfg(target_arch = "x86_64")]
            PartitionProperty::UnimplementedMsrAction(val) => {
                set(partition_prop::UnimplementedMsrAction, val)
            }
            PartitionProperty::PhysicalAddressWidth(val) => {
                set(partition_prop::PhysicalAddressWidth, val)
            }
            #[cfg(target_arch = "aarch64")]
            PartitionProperty::GicParameters(val) => set(partition_prop::Arm64IcParameters, val),
            #[cfg(target_arch = "aarch64")]
            PartitionProperty::_Dummy(_, _) => unreachable!(),
        };
        unsafe {
            check_hresult(api::WHvSetPartitionProperty(
                self.handle,
                data.0,
                data.1,
                data.2,
            ))
        }
    }

    fn get_property<T: partition_prop::AssociatedType>(&self, code: T) -> Result<T::Type>
    where
        T::Type: Sized,
    {
        let mut val = std::mem::MaybeUninit::<T::Type>::uninit();
        unsafe {
            let argn = size_of_val(&val) as u32;
            let argp = std::ptr::from_mut(&mut val).cast::<u8>();
            let mut outn = 0;
            check_hresult(api::WHvGetPartitionProperty(
                self.handle,
                code.code(),
                argp,
                argn,
                &mut outn,
            ))?;
            if outn < argn {
                panic!("output result too small");
            }
            Ok(val.assume_init())
        }
    }

    pub fn vp(&self, index: u32) -> Processor<'_> {
        Processor {
            partition: self,
            index,
        }
    }

    #[expect(clippy::missing_safety_doc)]
    pub unsafe fn map_range(
        &self,
        process: Option<BorrowedHandle<'_>>,
        data: *mut u8,
        size: usize,
        addr: u64,
        flags: abi::WHV_MAP_GPA_RANGE_FLAGS,
    ) -> Result<()> {
        unsafe {
            if let Some(process) = process {
                check_hresult(api::WHvMapGpaRange2(
                    self.handle,
                    process.as_raw_handle(),
                    data.cast(),
                    addr,
                    size as u64,
                    flags,
                ))
            } else {
                check_hresult(api::WHvMapGpaRange(
                    self.handle,
                    data.cast(),
                    addr,
                    size as u64,
                    flags,
                ))
            }
        }
    }

    pub fn unmap_range(&self, addr: u64, size: u64) -> Result<()> {
        unsafe { check_hresult(api::WHvUnmapGpaRange(self.handle, addr, size)) }
    }

    pub fn populate_ranges(
        &self,
        ranges: &[abi::WHV_MEMORY_RANGE_ENTRY],
        access_type: abi::WHV_MEMORY_ACCESS_TYPE,
        flags: abi::WHV_ADVISE_GPA_RANGE_POPULATE_FLAGS,
    ) -> Result<()> {
        let populate = abi::WHV_ADVISE_GPA_RANGE_POPULATE {
            Flags: flags,
            AccessType: access_type,
        };
        unsafe {
            check_hresult(api::WHvAdviseGpaRange(
                self.handle,
                ranges.as_ptr(),
                ranges.len().try_into().unwrap(),
                abi::WHvAdviseGpaRangeCodePopulate,
                std::ptr::from_ref(&populate).cast(),
                size_of_val(&populate) as u32,
            ))?;
        }
        Ok(())
    }

    pub fn pin_ranges(&self, ranges: &[abi::WHV_MEMORY_RANGE_ENTRY]) -> Result<()> {
        unsafe {
            check_hresult(api::WHvAdviseGpaRange(
                self.handle,
                ranges.as_ptr(),
                ranges.len().try_into().unwrap(),
                abi::WHvAdviseGpaRangeCodePin,
                null(),
                0,
            ))
        }
    }

    pub fn unpin_ranges(&self, ranges: &[abi::WHV_MEMORY_RANGE_ENTRY]) -> Result<()> {
        unsafe {
            check_hresult(api::WHvAdviseGpaRange(
                self.handle,
                ranges.as_ptr(),
                ranges.len().try_into().unwrap(),
                abi::WHvAdviseGpaRangeCodeUnpin,
                null(),
                0,
            ))
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub fn interrupt(
        &self,
        typ: abi::WHV_INTERRUPT_TYPE,
        mode: abi::WHV_INTERRUPT_DESTINATION_MODE,
        trigger: abi::WHV_INTERRUPT_TRIGGER_MODE,
        destination: u32,
        vector: u32,
    ) -> Result<()> {
        unsafe {
            let control = abi::WHV_INTERRUPT_CONTROL {
                Type: typ.0.try_into().unwrap(),
                Modes: abi::WHV_INTERRUPT_CONTROL_MODES::new(mode, trigger),
                Reserved: Default::default(),
                Destination: destination,
                Vector: vector,
            };
            check_hresult(api::WHvRequestInterrupt(
                self.handle,
                &control,
                size_of_val(&control) as u32,
            ))
        }
    }

    #[cfg(target_arch = "aarch64")]
    pub fn interrupt(&self, irq_id: u32, assert: bool) -> Result<()> {
        unsafe {
            let control = abi::WHV_INTERRUPT_CONTROL {
                TargetPartition: 0,
                InterruptControl: if assert {
                    abi::INTERRUPT_CONTROL_ASSERTED
                } else {
                    0
                },
                DestinationAddress: 0,
                RequestedVector: irq_id,
                TargetVtl: 0,
                ReservedZ0: 0,
                ReservedZ1: 0,
            };
            check_hresult(api::WHvRequestInterrupt(
                self.handle,
                &control,
                size_of_val(&control) as u32,
            ))
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_interrupt_target_vp_set(
        &self,
        mode: abi::WHV_INTERRUPT_DESTINATION_MODE,
        destination: u64,
    ) -> Result<Vec<u32>> {
        unsafe {
            let mut vps = Vec::with_capacity(4);
            let mut n = 0;
            let mut hr = api::WHvGetInterruptTargetVpSet(
                self.handle,
                destination,
                mode,
                vps.as_mut_ptr(),
                vps.capacity() as u32,
                &mut n,
            );
            if hr == api::WHV_E_INSUFFICIENT_BUFFER {
                vps.reserve_exact(n as usize);
                hr = api::WHvGetInterruptTargetVpSet(
                    self.handle,
                    destination,
                    mode,
                    vps.as_mut_ptr(),
                    vps.capacity() as u32,
                    &mut n,
                );
            }
            check_hresult(hr)?;
            vps.set_len(n as usize);
            Ok(vps)
        }
    }

    pub fn tsc_frequency(&self) -> Result<u64> {
        self.get_property(partition_prop::ProcessorClockFrequency)
    }

    #[cfg(target_arch = "x86_64")]
    pub fn apic_frequency(&self) -> Result<u64> {
        self.get_property(partition_prop::InterruptClockFrequency)
    }

    pub fn reference_time(&self) -> Result<u64> {
        self.get_property(partition_prop::ReferenceTime)
    }

    pub fn physical_address_width(&self) -> Result<u32> {
        self.get_property(partition_prop::PhysicalAddressWidth)
    }

    #[expect(clippy::missing_safety_doc)]
    pub unsafe fn register_doorbell(&self, m: &DoorbellMatch, event: RawHandle) -> Result<()> {
        unsafe {
            check_hresult(api::WHvRegisterPartitionDoorbellEvent(
                self.handle,
                &m.data(),
                event,
            ))
        }
    }

    pub fn unregister_doorbell(&self, m: &DoorbellMatch) -> Result<()> {
        unsafe {
            check_hresult(api::WHvUnregisterPartitionDoorbellEvent(
                self.handle,
                &m.data(),
            ))
        }
    }

    pub fn create_vp(&self, index: u32) -> VpBuilder<'_> {
        VpBuilder {
            partition: self,
            index,
            numa_node: None,
        }
    }

    pub fn delete_vp(&self, index: u32) -> Result<()> {
        unsafe { check_hresult(api::WHvDeleteVirtualProcessor(self.handle, index)) }
    }

    pub fn create_notification_port(
        &self,
        parameters: NotificationPortParameters,
        event: BorrowedHandle<'_>,
    ) -> Result<NotificationPortHandle> {
        let mut handle = abi::WHV_NOTIFICATION_PORT_HANDLE(0);
        unsafe {
            let whp_params = match parameters {
                NotificationPortParameters::Event { connection_id } => {
                    abi::WHV_NOTIFICATION_PORT_PARAMETERS {
                        NotificationPortType: abi::WHvNotificationPortTypeEvent,
                        Reserved: 0,
                        u: abi::WHV_NOTIFICATION_PORT_PARAMETERS_u {
                            Event: abi::WHV_NOTIFICATION_PORT_PARAMETERS_u_Event {
                                ConnectionId: connection_id,
                            },
                        },
                    }
                }
                NotificationPortParameters::Doorbell { match_data } => {
                    abi::WHV_NOTIFICATION_PORT_PARAMETERS {
                        NotificationPortType: abi::WHvNotificationPortTypeDoorbell,
                        Reserved: 0,
                        u: abi::WHV_NOTIFICATION_PORT_PARAMETERS_u {
                            Doorbell: match_data.data(),
                        },
                    }
                }
            };
            check_hresult(api::WHvCreateNotificationPort(
                self.handle,
                &whp_params,
                event.as_raw_handle(),
                &mut handle,
            ))?;
        }
        Ok(NotificationPortHandle(handle))
    }

    pub fn set_notification_port(
        &self,
        handle: &NotificationPortHandle,
        code: abi::WHV_NOTIFICATION_PORT_PROPERTY_CODE,
        val: u64,
    ) -> Result<()> {
        unsafe {
            check_hresult(api::WHvSetNotificationPortProperty(
                self.handle,
                handle.0,
                code,
                val,
            ))
        }
    }

    pub fn delete_notification_port(&self, handle: NotificationPortHandle) {
        unsafe {
            check_hresult(api::WHvDeleteNotificationPort(self.handle, handle.0))
                .expect("invalid notification port handle");
        }
    }

    pub fn create_trigger(
        &self,
        parameters: TriggerParameters,
    ) -> Result<(TriggerHandle, OwnedHandle)> {
        unsafe {
            let mut trigger_handle = std::mem::zeroed();
            let mut event_handle = null_mut();
            check_hresult(api::WHvCreateTrigger(
                self.handle,
                &parameters.into(),
                &mut trigger_handle,
                &mut event_handle,
            ))?;
            Ok((
                TriggerHandle(trigger_handle),
                OwnedHandle::from_raw_handle(event_handle),
            ))
        }
    }

    pub fn update_trigger(
        &self,
        handle: &TriggerHandle,
        parameters: TriggerParameters,
    ) -> Result<()> {
        unsafe {
            check_hresult(api::WHvUpdateTriggerParameters(
                self.handle,
                &parameters.into(),
                handle.0,
            ))
        }
    }

    pub fn delete_trigger(&self, handle: TriggerHandle) {
        unsafe {
            check_hresult(api::WHvDeleteTrigger(self.handle, handle.0))
                .expect("invalid trigger handle");
        }
    }

    pub fn start_migration(&self) -> Result<MigrationHandle> {
        unsafe {
            let mut handle = null_mut();
            check_hresult(api::WHvStartPartitionMigration(self.handle, &mut handle))?;
            Ok(MigrationHandle(OwnedHandle::from_raw_handle(handle)))
        }
    }

    // N.B. This function must be called with no other concurrent references.
    pub fn complete_migration(&mut self) -> Result<()> {
        unsafe { check_hresult(api::WHvCompletePartitionMigration(self.handle)) }
    }

    pub fn cancel_migration(&self) -> Result<()> {
        unsafe { check_hresult(api::WHvCancelPartitionMigration(self.handle)) }
    }

    pub fn create_device(
        &self,
        id: u64,
        resource: VpciResource,
        flags: abi::WHV_CREATE_VPCI_DEVICE_FLAGS,
        event: Option<RawHandle>,
    ) -> Result<()> {
        unsafe {
            check_hresult(api::WHvCreateVpciDevice(
                self.handle,
                id,
                resource.0.as_raw_handle(),
                flags,
                event.unwrap_or(null_mut()),
            ))
        }
    }

    pub fn delete_device(&self, id: u64) -> Result<()> {
        unsafe { check_hresult(api::WHvDeleteVpciDevice(self.handle, id)) }
    }

    pub fn device(&self, id: u64) -> Device<'_> {
        Device {
            partition: self,
            id,
        }
    }

    pub fn suspend_time(&self) -> Result<()> {
        unsafe { check_hresult(api::WHvSuspendPartitionTime(self.handle)) }
    }

    pub fn resume_time(&self) -> Result<()> {
        unsafe { check_hresult(api::WHvResumePartitionTime(self.handle)) }
    }
}

pub enum VpciResourceDescriptor<'a> {
    None,
    Sriov(&'a str, i64, u16),
    Opaque(&'a [u8]),
}

#[derive(Debug)]
pub struct VpciResource(OwnedHandle);

impl VpciResource {
    pub fn new(
        provider: Option<&Guid>,
        flags: abi::WHV_ALLOCATE_VPCI_RESOURCE_FLAGS,
        descriptor: &VpciResourceDescriptor<'_>,
    ) -> Result<Self> {
        unsafe {
            let mut sriov;
            let data: &[u8] = match descriptor {
                VpciResourceDescriptor::None => &[],
                VpciResourceDescriptor::Sriov(path, id, index) => {
                    sriov = abi::WHV_SRIOV_RESOURCE_DESCRIPTOR {
                        PnpInstanceId: [0; 200],
                        VirtualFunctionId: LUID {
                            LowPart: *id as u32,
                            HighPart: (*id >> 32) as i32,
                        },
                        VirtualFunctionIndex: *index,
                        Reserved: 0,
                    };
                    let mut path16: Vec<_> = path.encode_utf16().collect();
                    path16.push(0);
                    if path16.len() > sriov.PnpInstanceId.len() {
                        return Err(WHvError::ERROR_BAD_PATHNAME);
                    }
                    sriov.PnpInstanceId[..path16.len()].copy_from_slice(&path16);
                    std::slice::from_raw_parts(
                        std::ptr::from_ref(&sriov).cast(),
                        size_of_val(&sriov),
                    )
                }
                VpciResourceDescriptor::Opaque(d) => d,
            };
            let mut handle = null_mut();
            let guid_win = provider.map(|g| windows_sys::core::GUID::from(*g));
            check_hresult(api::WHvAllocateVpciResource(
                guid_win.as_ref(),
                flags,
                data.as_ptr().cast(),
                data.len().try_into().unwrap(),
                &mut handle,
            ))?;
            Ok(Self(OwnedHandle::from_raw_handle(handle)))
        }
    }
}

impl AsHandle for VpciResource {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl From<VpciResource> for OwnedHandle {
    fn from(resource: VpciResource) -> Self {
        resource.0
    }
}

impl From<OwnedHandle> for VpciResource {
    fn from(handle: OwnedHandle) -> Self {
        Self(handle)
    }
}

#[derive(Debug)]
pub struct NotificationPortHandle(abi::WHV_NOTIFICATION_PORT_HANDLE);

pub enum NotificationPortParameters {
    Event { connection_id: u32 },
    Doorbell { match_data: DoorbellMatch },
}

#[derive(Debug, Copy, Clone)]
pub struct DoorbellMatch {
    pub guest_address: u64,
    pub value: Option<u64>,
    pub length: Option<u32>,
}

impl DoorbellMatch {
    fn data(&self) -> abi::WHV_DOORBELL_MATCH_DATA {
        abi::WHV_DOORBELL_MATCH_DATA {
            GuestAddress: self.guest_address,
            Value: self.value.unwrap_or(0),
            Length: self.length.unwrap_or(0),
            Flags: self.value.is_some() as u32 | (self.length.is_some() as u32) << 1,
        }
    }
}

#[derive(Debug)]
pub struct TriggerHandle(abi::WHV_TRIGGER_HANDLE);

#[derive(Debug, Copy, Clone)]
pub enum TriggerParameters {
    #[cfg(target_arch = "x86_64")]
    Interrupt {
        interrupt_type: abi::WHV_INTERRUPT_TYPE,
        destination_mode: abi::WHV_INTERRUPT_DESTINATION_MODE,
        trigger_mode: abi::WHV_INTERRUPT_TRIGGER_MODE,
        destination: u32,
        vector: u32,
    },
    SynicEvent {
        vp_index: u32,
        sint: u8,
        flag: u16,
    },
    DeviceInterrupt {
        id: u64,
        address: u64,
        data: u32,
    },
}

impl From<TriggerParameters> for abi::WHV_TRIGGER_PARAMETERS {
    fn from(p: TriggerParameters) -> Self {
        abi::WHV_TRIGGER_PARAMETERS {
            TriggerType: match &p {
                #[cfg(target_arch = "x86_64")]
                TriggerParameters::Interrupt { .. } => abi::WHvTriggerTypeInterrupt,
                TriggerParameters::SynicEvent { .. } => abi::WHvTriggerTypeSynicEvent,
                TriggerParameters::DeviceInterrupt { .. } => abi::WHvTriggerTypeDeviceInterrupt,
            },
            Reserved: 0,
            u: match p {
                #[cfg(target_arch = "x86_64")]
                TriggerParameters::Interrupt {
                    interrupt_type,
                    destination_mode,
                    trigger_mode,
                    destination,
                    vector,
                } => abi::WHV_TRIGGER_PARAMETERS_u {
                    Interrupt: abi::WHV_INTERRUPT_CONTROL {
                        Type: interrupt_type.0.try_into().unwrap(),
                        Modes: abi::WHV_INTERRUPT_CONTROL_MODES::new(
                            destination_mode,
                            trigger_mode,
                        ),
                        Reserved: [0; 6],
                        Destination: destination,
                        Vector: vector,
                    },
                },
                TriggerParameters::SynicEvent {
                    vp_index,
                    sint,
                    flag,
                } => abi::WHV_TRIGGER_PARAMETERS_u {
                    SynicEvent: abi::WHV_SYNIC_EVENT_PARAMETERS {
                        VpIndex: vp_index,
                        TargetSint: sint,
                        Reserved: 0,
                        FlagNumber: flag,
                    },
                },
                TriggerParameters::DeviceInterrupt { id, address, data } => {
                    abi::WHV_TRIGGER_PARAMETERS_u {
                        DeviceInterrupt: abi::WHV_TRIGGER_PARAMETERS_u_DeviceInterrupt {
                            LogicalDeviceId: id,
                            MsiAddress: address,
                            MsiData: data,
                            Reserved: 0,
                        },
                    }
                }
            },
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Device<'a> {
    partition: &'a Partition,
    id: u64,
}

pub enum DeviceNotification {
    MmioRemapping,
    SurpriseRemoval,
}

impl Device<'_> {
    fn get_property<T>(&self, property: abi::WHV_VPCI_DEVICE_PROPERTY_CODE) -> Result<T> {
        unsafe {
            let mut data: T = std::mem::zeroed();
            let mut size = 0;
            check_hresult(api::WHvGetVpciDeviceProperty(
                self.partition.handle,
                self.id,
                property,
                std::ptr::from_mut(&mut data).cast::<c_void>(),
                size_of_val(&data) as u32,
                &mut size,
            ))?;
            Ok(data)
        }
    }

    pub fn hardware_ids(&self) -> Result<abi::WHV_VPCI_HARDWARE_IDS> {
        self.get_property(abi::WHvVpciDevicePropertyCodeHardwareIDs)
    }

    pub fn probed_bars(&self) -> Result<abi::WHV_VPCI_PROBED_BARS> {
        self.get_property(abi::WHvVpciDevicePropertyCodeProbedBARs)
    }

    pub fn get_notification(&self) -> Result<Option<DeviceNotification>> {
        unsafe {
            let mut notification = std::mem::zeroed();
            check_hresult(api::WHvGetVpciDeviceNotification(
                self.partition.handle,
                self.id,
                &mut notification,
                size_of_val(&notification) as u32,
            ))?;
            Ok(match notification.NotificationType {
                abi::WHvVpciDeviceNotificationUndefined => None,
                abi::WHvVpciDeviceNotificationMmioRemapping => {
                    Some(DeviceNotification::MmioRemapping)
                }
                abi::WHvVpciDeviceNotificationSurpriseRemoval => {
                    Some(DeviceNotification::SurpriseRemoval)
                }
                _ => panic!(
                    "unknown notification type {:#x}",
                    notification.NotificationType.0
                ),
            })
        }
    }

    pub fn map_mmio(&self) -> Result<Vec<abi::WHV_VPCI_MMIO_MAPPING>> {
        unsafe {
            let mut count = 0;
            let mut mappings = null();
            check_hresult(api::WHvMapVpciDeviceMmioRanges(
                self.partition.handle,
                self.id,
                &mut count,
                &mut mappings,
            ))?;
            Ok(std::slice::from_raw_parts(mappings, count as usize).into())
        }
    }

    pub fn unmap_mmio(&self) -> Result<()> {
        unsafe {
            check_hresult(api::WHvUnmapVpciDeviceMmioRanges(
                self.partition.handle,
                self.id,
            ))
        }
    }

    pub fn set_power_state(&self, power_state: DEVICE_POWER_STATE) -> Result<()> {
        unsafe {
            check_hresult(api::WHvSetVpciDevicePowerState(
                self.partition.handle,
                self.id,
                power_state,
            ))
        }
    }

    pub fn read_register(
        &self,
        location: abi::WHV_VPCI_DEVICE_REGISTER_SPACE,
        offset: u16,
        data: &mut [u8],
    ) -> Result<()> {
        let register = abi::WHV_VPCI_DEVICE_REGISTER {
            Location: location,
            SizeInBytes: data.len() as u32,
            OffsetInBytes: offset.into(),
        };
        unsafe {
            check_hresult(api::WHvReadVpciDeviceRegister(
                self.partition.handle,
                self.id,
                &register,
                data.as_mut_ptr().cast::<c_void>(),
            ))
        }
    }

    pub fn write_register(
        &self,
        location: abi::WHV_VPCI_DEVICE_REGISTER_SPACE,
        offset: u16,
        data: &[u8],
    ) -> Result<()> {
        let register = abi::WHV_VPCI_DEVICE_REGISTER {
            Location: location,
            SizeInBytes: data.len() as u32,
            OffsetInBytes: offset.into(),
        };
        unsafe {
            check_hresult(api::WHvWriteVpciDeviceRegister(
                self.partition.handle,
                self.id,
                &register,
                data.as_ptr() as *mut c_void,
            ))
        }
    }

    pub fn map_interrupt(
        &self,
        index: u32,
        message_count: u32,
        target: &VpciInterruptTarget,
    ) -> Result<(u64, u32)> {
        unsafe {
            let mut address = 0;
            let mut data = 0;
            check_hresult(api::WHvMapVpciDeviceInterrupt(
                self.partition.handle,
                self.id,
                index,
                message_count,
                target.header(),
                &mut address,
                &mut data,
            ))?;
            Ok((address, data))
        }
    }

    pub fn unmap_interrupt(&self, index: u32) -> Result<()> {
        unsafe {
            check_hresult(api::WHvUnmapVpciDeviceInterrupt(
                self.partition.handle,
                self.id,
                index,
            ))
        }
    }

    pub fn retarget_interrupt(
        &self,
        address: u64,
        data: u32,
        target: &VpciInterruptTarget,
    ) -> Result<()> {
        unsafe {
            check_hresult(api::WHvRetargetVpciDeviceInterrupt(
                self.partition.handle,
                self.id,
                address,
                data,
                target.header(),
            ))
        }
    }

    pub fn get_interrupt_target(
        &self,
        index: u32,
        message_number: u32,
    ) -> Result<VpciInterruptTarget> {
        unsafe {
            let mut size = 0;
            let err = check_hresult(api::WHvGetVpciDeviceInterruptTarget(
                self.partition.handle,
                self.id,
                index,
                message_number,
                null_mut(),
                0,
                &mut size,
            ))
            .unwrap_err();
            if err != WHvError::WHV_E_INSUFFICIENT_BUFFER {
                return Err(err);
            }
            let layout = Layout::from_size_align(
                size as usize,
                align_of::<abi::WHV_VPCI_INTERRUPT_TARGET>(),
            )
            .unwrap();
            let mem = NonNull::new(std::alloc::alloc(layout).cast()).unwrap();
            match check_hresult(api::WHvGetVpciDeviceInterruptTarget(
                self.partition.handle,
                self.id,
                index,
                message_number,
                mem.as_ptr(),
                size,
                &mut size,
            )) {
                Ok(()) => Ok(VpciInterruptTarget(mem)),
                Err(err) => {
                    std::alloc::dealloc(mem.as_ptr().cast(), layout);
                    Err(err)
                }
            }
        }
    }

    pub fn interrupt(&self, address: u64, data: u32) -> Result<()> {
        unsafe {
            check_hresult(api::WHvRequestVpciDeviceInterrupt(
                self.partition.handle,
                self.id,
                address,
                data,
            ))
        }
    }
}

pub struct VpciInterruptTarget(NonNull<abi::WHV_VPCI_INTERRUPT_TARGET>);

impl Drop for VpciInterruptTarget {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.0.as_ptr().cast::<u8>(), Self::layout(self.header())) }
    }
}

impl VpciInterruptTarget {
    pub fn new(
        vector: u32,
        flags: abi::WHV_VPCI_INTERRUPT_TARGET_FLAGS,
        processors: &[u32],
    ) -> Self {
        let header = abi::WHV_VPCI_INTERRUPT_TARGET {
            Vector: vector,
            Flags: flags,
            ProcessorCount: processors.len().try_into().unwrap(),
            Processors: [],
        };
        unsafe {
            let mem = NonNull::new(
                std::alloc::alloc(Self::layout(&header)).cast::<abi::WHV_VPCI_INTERRUPT_TARGET>(),
            )
            .unwrap();
            mem.as_ptr().write(header);
            let p = std::slice::from_raw_parts_mut(
                mem.as_ptr().offset(1).cast::<u32>(),
                processors.len(),
            );
            p.copy_from_slice(processors);
            Self(mem)
        }
    }

    pub fn vector(&self) -> u32 {
        self.header().Vector
    }

    pub fn flags(&self) -> abi::WHV_VPCI_INTERRUPT_TARGET_FLAGS {
        self.header().Flags
    }

    fn header(&self) -> &abi::WHV_VPCI_INTERRUPT_TARGET {
        unsafe { self.0.as_ref() }
    }

    pub fn processors(&self) -> &[u32] {
        unsafe {
            std::slice::from_raw_parts(
                self.header().Processors.as_ptr(),
                self.header().ProcessorCount as usize,
            )
        }
    }

    fn layout(target: &abi::WHV_VPCI_INTERRUPT_TARGET) -> Layout {
        Layout::new::<abi::WHV_VPCI_INTERRUPT_TARGET>()
            .extend(Layout::array::<u32>(target.ProcessorCount as usize).unwrap())
            .unwrap()
            .0
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Processor<'a> {
    partition: &'a Partition,
    index: u32,
}

impl<'a> Processor<'a> {
    pub fn partition(&self) -> &'a Partition {
        self.partition
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn cancel_run(&self) -> Result<()> {
        unsafe {
            check_hresult(api::WHvCancelRunVirtualProcessor(
                self.partition.handle,
                self.index,
                0,
            ))
        }
    }

    pub fn set_registers(
        &self,
        names: &[abi::WHV_REGISTER_NAME],
        values: &[abi::WHV_REGISTER_VALUE],
    ) -> Result<()> {
        assert_eq!(names.len(), values.len());
        if names.is_empty() {
            Ok(())
        } else {
            unsafe {
                check_hresult(api::WHvSetVirtualProcessorRegisters(
                    self.partition.handle,
                    self.index,
                    names.as_ptr(),
                    names.len() as u32,
                    values.as_ptr(),
                ))
            }
        }
    }

    pub fn get_registers(
        &self,
        names: &[abi::WHV_REGISTER_NAME],
        values: &mut [abi::WHV_REGISTER_VALUE],
    ) -> Result<()> {
        if names.len() != values.len() {
            panic!();
        }
        if names.is_empty() {
            Ok(())
        } else {
            unsafe {
                check_hresult(api::WHvGetVirtualProcessorRegisters(
                    self.partition.handle,
                    self.index,
                    names.as_ptr(),
                    names.len() as u32,
                    values.as_mut_ptr(),
                ))
            }
        }
    }

    pub fn get_register<T: RegisterName>(&self, v: T) -> Result<T::Value> {
        get_registers!(self, [v])
    }

    pub fn set_register<T: RegisterName>(&self, n: T, v: T::Value) -> Result<()> {
        set_registers!(self, [(n, v)])
    }

    pub fn get_xsave(&self) -> Result<Vec<u8>> {
        let mut r = Vec::with_capacity(4096);
        loop {
            unsafe {
                let mut n = 0;
                match check_hresult(api::WHvGetVirtualProcessorXsaveState(
                    self.partition.handle,
                    self.index,
                    r.as_mut_ptr(),
                    r.capacity() as u32,
                    &mut n,
                )) {
                    Ok(()) => {
                        r.set_len(n as usize);
                        break;
                    }
                    Err(WHvError::WHV_E_INSUFFICIENT_BUFFER) => {
                        r.reserve(n as usize);
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        Ok(r)
    }

    pub fn set_xsave(&self, data: &[u8]) -> Result<()> {
        unsafe {
            check_hresult(api::WHvSetVirtualProcessorXsaveState(
                self.partition.handle,
                self.index,
                data.as_ptr(),
                data.len() as u32,
            ))
        }
    }

    pub fn get_apic(&self) -> Result<Vec<u8>> {
        let mut r = Vec::with_capacity(4096);
        unsafe {
            let mut n = 0;
            check_hresult(api::WHvGetVirtualProcessorInterruptControllerState2(
                self.partition.handle,
                self.index,
                r.as_mut_ptr(),
                r.capacity() as u32,
                &mut n,
            ))?;
            r.set_len(n as usize);
        }
        Ok(r)
    }

    pub fn set_apic(&self, data: &[u8]) -> Result<()> {
        unsafe {
            check_hresult(api::WHvSetVirtualProcessorInterruptControllerState2(
                self.partition.handle,
                self.index,
                data.as_ptr(),
                data.len() as u32,
            ))
        }
    }

    pub fn get_state(
        &self,
        state_type: abi::WHV_VIRTUAL_PROCESSOR_STATE_TYPE,
        data: &mut [u8],
    ) -> Result<usize> {
        let mut n = 0;
        unsafe {
            check_hresult(api::WHvGetVirtualProcessorState(
                self.partition.handle,
                self.index,
                state_type,
                data.as_mut_ptr(),
                data.len() as u32,
                &mut n,
            ))?;
        }
        Ok(n as usize)
    }

    pub fn set_state(
        &self,
        state_type: abi::WHV_VIRTUAL_PROCESSOR_STATE_TYPE,
        data: &[u8],
    ) -> Result<()> {
        unsafe {
            check_hresult(api::WHvSetVirtualProcessorState(
                self.partition.handle,
                self.index,
                state_type,
                data.as_ptr(),
                data.len() as u32,
            ))
        }
    }

    pub fn runner(&self) -> ProcessorRunner<'a> {
        ProcessorRunner {
            vp: *self,
            ctx: unsafe { std::mem::zeroed() },
        }
    }

    pub fn translate_gva(
        &self,
        gva: u64,
        access_flags: abi::WHV_TRANSLATE_GVA_FLAGS,
    ) -> Result<std::result::Result<u64, abi::WHV_TRANSLATE_GVA_RESULT_CODE>> {
        Ok(unsafe {
            let mut result = std::mem::zeroed();
            let mut gpa = 0;

            check_hresult(api::WHvTranslateGva(
                self.partition.handle,
                self.index,
                gva,
                access_flags.0,
                &mut result,
                &mut gpa,
            ))?;
            match result.ResultCode {
                abi::WHvTranslateGvaResultSuccess => Ok(gpa),
                err => Err(err),
            }
        })
    }

    pub fn signal_synic_event(&self, sint: u8, flag: u16) -> Result<bool> {
        unsafe {
            let mut newly_signaled = 0;
            check_hresult(api::WHvSignalVirtualProcessorSynicEvent(
                self.partition.handle,
                abi::WHV_SYNIC_EVENT_PARAMETERS {
                    VpIndex: self.index,
                    TargetSint: sint,
                    Reserved: 0,
                    FlagNumber: flag,
                },
                &mut newly_signaled,
            ))?;
            Ok(newly_signaled != 0)
        }
    }

    pub fn post_synic_message(&self, sint: u8, message: &[u8]) -> Result<()> {
        unsafe {
            check_hresult(api::WHvPostVirtualProcessorSynicMessage(
                self.partition.handle,
                self.index,
                sint.into(),
                message.as_ptr(),
                message.len().try_into().expect("message much too big"),
            ))
        }
    }

    pub fn get_cpuid_output(&self, eax: u32, ecx: u32) -> Result<abi::WHV_CPUID_OUTPUT> {
        unsafe {
            let mut output = std::mem::zeroed();
            check_hresult(api::WHvGetVirtualProcessorCpuidOutput(
                self.partition.handle,
                self.index,
                eax,
                ecx,
                &mut output,
            ))?;
            Ok(output)
        }
    }
}

#[derive(Debug)]
pub struct MigrationHandle(OwnedHandle);

impl MigrationHandle {
    pub fn accept(self) -> Result<MigratingPartition> {
        unsafe {
            let mut handle = abi::WHV_PARTITION_HANDLE(0);
            check_hresult(api::WHvAcceptPartitionMigration(
                self.0.as_raw_handle(),
                &mut handle,
            ))?;
            Ok(MigratingPartition(Partition { handle }))
        }
    }
}

impl AsHandle for MigrationHandle {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl From<MigrationHandle> for OwnedHandle {
    fn from(handle: MigrationHandle) -> OwnedHandle {
        handle.0
    }
}

impl From<OwnedHandle> for MigrationHandle {
    fn from(handle: OwnedHandle) -> Self {
        Self(handle)
    }
}

#[derive(Debug)]
pub struct MigratingPartition(Partition);

impl MigratingPartition {
    pub fn setup(self) -> Result<Partition> {
        unsafe {
            check_hresult(api::WHvSetupPartition(self.0.handle))?;
        }
        Ok(self.0)
    }
}

pub struct ProcessorRunner<'a> {
    vp: Processor<'a>,
    ctx: abi::WHV_RUN_VP_EXIT_CONTEXT,
}

impl ProcessorRunner<'_> {
    pub fn run(&mut self) -> Result<Exit<'_>> {
        unsafe {
            check_hresult(api::WHvRunVirtualProcessor(
                self.vp.partition.handle,
                self.vp.index,
                &mut self.ctx,
                size_of_val(&self.ctx) as u32,
            ))?
        }
        Ok(Exit {
            #[cfg(target_arch = "x86_64")]
            vp_context: &self.ctx.VpContext,
            reason: ExitReason::from_context(&self.ctx),
        })
    }
}

impl<'a> ExitReason<'a> {
    #[cfg(target_arch = "x86_64")]
    fn from_context(ctx: &'a abi::WHV_RUN_VP_EXIT_CONTEXT) -> Self {
        match ctx.ExitReason {
            abi::WHvRunVpExitReasonNone => Self::None,
            abi::WHvRunVpExitReasonMemoryAccess => {
                Self::MemoryAccess(unsafe { &ctx.u.MemoryAccess })
            }
            abi::WHvRunVpExitReasonX64IoPortAccess => {
                Self::IoPortAccess(unsafe { &ctx.u.IoPortAccess })
            }
            abi::WHvRunVpExitReasonUnrecoverableException => Self::UnrecoverableException,
            abi::WHvRunVpExitReasonInvalidVpRegisterValue => Self::InvalidVpRegisterValue,
            abi::WHvRunVpExitReasonUnsupportedFeature => Self::UnsupportedFeature,
            abi::WHvRunVpExitReasonX64InterruptWindow => {
                Self::InterruptWindow(unsafe { &ctx.u.InterruptWindow })
            }
            abi::WHvRunVpExitReasonX64Halt => Self::Halt,
            abi::WHvRunVpExitReasonX64ApicEoi => Self::ApicEoi(unsafe { &ctx.u.ApicEoi }),
            abi::WHvRunVpExitReasonX64MsrAccess => Self::MsrAccess(unsafe { &ctx.u.MsrAccess }),
            abi::WHvRunVpExitReasonX64Cpuid => Self::Cpuid(unsafe { &ctx.u.CpuidAccess }),
            abi::WHvRunVpExitReasonException => Self::Exception(unsafe { &ctx.u.VpException }),
            abi::WHvRunVpExitReasonX64Rdtsc => Self::Rdtsc(unsafe { &ctx.u.ReadTsc }),
            abi::WHvRunVpExitReasonX64ApicSmiTrap => Self::ApicSmiTrap(unsafe { &ctx.u.ApicSmi }),
            abi::WHvRunVpExitReasonHypercall => Self::Hypercall(unsafe { &ctx.u.Hypercall }),
            abi::WHvRunVpExitReasonX64ApicInitSipiTrap => {
                Self::ApicInitSipiTrap(unsafe { &ctx.u.ApicInitSipi })
            }
            abi::WHvRunVpExitReasonX64ApicWriteTrap => {
                Self::ApicWriteTrap(unsafe { &ctx.u.ApicWrite })
            }
            abi::WHvRunVpExitReasonSynicSintDeliverable => {
                Self::SynicSintDeliverable(unsafe { &ctx.u.SynicSintDeliverable })
            }
            abi::WHvRunVpExitReasonCanceled => Self::Canceled,
            abi::WHV_RUN_VP_EXIT_REASON(reason) => panic!("unknown exit reason: {reason:#x}"),
        }
    }

    #[cfg(target_arch = "aarch64")]
    fn from_context(ctx: &'a abi::WHV_RUN_VP_EXIT_CONTEXT) -> Self {
        match ctx.ExitReason {
            abi::WHvRunVpExitReasonNone => Self::None,
            abi::WHvRunVpExitReasonCanceled => Self::Canceled,
            reason => Self::Hypervisor(reason.0, &ctx.u),
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[derive(Copy, Clone, Debug)]
pub enum ExitReason<'a> {
    None,
    MemoryAccess(&'a abi::WHV_MEMORY_ACCESS_CONTEXT),
    #[cfg(target_arch = "x86_64")]
    IoPortAccess(&'a abi::WHV_X64_IO_PORT_ACCESS_CONTEXT),
    UnrecoverableException,
    InvalidVpRegisterValue,
    #[cfg(target_arch = "x86_64")]
    UnsupportedFeature,
    #[cfg(target_arch = "x86_64")]
    InterruptWindow(&'a abi::WHV_X64_INTERRUPTION_DELIVERABLE_CONTEXT),
    #[cfg(target_arch = "x86_64")]
    Halt,
    #[cfg(target_arch = "x86_64")]
    ApicEoi(&'a abi::WHV_X64_APIC_EOI_CONTEXT),
    #[cfg(target_arch = "x86_64")]
    MsrAccess(&'a abi::WHV_X64_MSR_ACCESS_CONTEXT),
    #[cfg(target_arch = "x86_64")]
    Cpuid(&'a abi::WHV_X64_CPUID_ACCESS_CONTEXT),
    #[cfg(target_arch = "x86_64")]
    Exception(&'a abi::WHV_VP_EXCEPTION_CONTEXT),
    #[cfg(target_arch = "x86_64")]
    Rdtsc(&'a abi::WHV_X64_RDTSC_CONTEXT),
    #[cfg(target_arch = "x86_64")]
    ApicSmiTrap(&'a abi::WHV_X64_APIC_SMI_CONTEXT),
    Hypercall(&'a abi::WHV_HYPERCALL_CONTEXT),
    #[cfg(target_arch = "x86_64")]
    ApicInitSipiTrap(&'a abi::WHV_X64_APIC_INIT_SIPI_CONTEXT),
    #[cfg(target_arch = "x86_64")]
    ApicWriteTrap(&'a abi::WHV_X64_APIC_WRITE_CONTEXT),
    SynicSintDeliverable(&'a abi::WHV_SYNIC_SINT_DELIVERABLE_CONTEXT),
    #[cfg(target_arch = "aarch64")]
    Arm64Reset(&'a abi::WHV_ARM64_RESET_CONTEXT),
    Canceled,
}

#[cfg(target_arch = "aarch64")]
#[derive(Copy, Clone, Debug)]
pub enum ExitReason<'a> {
    None,
    Hypervisor(u32, &'a abi::WHV_RUN_VP_EXIT_CONTEXT_u),
    Canceled,
}

#[derive(Copy, Clone, Debug)]
pub struct Exit<'a> {
    #[cfg(target_arch = "x86_64")]
    pub vp_context: &'a abi::WHV_VP_EXIT_CONTEXT,
    pub reason: ExitReason<'a>,
}

/// Trait implemented by register value types.
pub trait RegisterValue {
    /// Converts the value into the ABI register value.
    fn as_abi(&self) -> abi::WHV_REGISTER_VALUE;

    /// Extracts the value from the ABI register value.
    ///
    /// This may truncate the input, but as long as the ABI value actually
    /// stores a value associated with this register, no data will be lost.
    fn from_abi(value: &abi::WHV_REGISTER_VALUE) -> Self;
}

impl RegisterValue for u128 {
    fn as_abi(&self) -> abi::WHV_REGISTER_VALUE {
        abi::WHV_REGISTER_VALUE((*self).into())
    }

    fn from_abi(value: &abi::WHV_REGISTER_VALUE) -> Self {
        value.0.into()
    }
}

impl RegisterValue for u64 {
    fn as_abi(&self) -> abi::WHV_REGISTER_VALUE {
        abi::WHV_REGISTER_VALUE((*self).into())
    }

    fn from_abi(value: &abi::WHV_REGISTER_VALUE) -> Self {
        let v: u128 = value.0.into();
        v as u64
    }
}

/// Trait implemented by register name types.
pub trait RegisterName {
    /// The value type associated with the register.
    type Value: RegisterValue;

    /// The ABI register name.
    fn as_abi(&self) -> abi::WHV_REGISTER_NAME;
}

impl RegisterName for Register64 {
    type Value = u64;

    fn as_abi(&self) -> abi::WHV_REGISTER_NAME {
        abi::WHV_REGISTER_NAME(*self as u32)
    }
}

impl RegisterName for Register128 {
    type Value = u128;

    fn as_abi(&self) -> abi::WHV_REGISTER_NAME {
        abi::WHV_REGISTER_NAME(*self as u32)
    }
}

#[doc(hidden)]
pub fn inject_helper<T: RegisterName>(_: T, value: &T::Value) -> abi::WHV_REGISTER_VALUE {
    value.as_abi()
}

#[doc(hidden)]
pub fn extract_helper<T: RegisterName>(_: T, value: &abi::WHV_REGISTER_VALUE) -> T::Value {
    T::Value::from_abi(value)
}

#[macro_export]
macro_rules! set_registers {
    ($vp:expr, [$(($name:expr, $value:expr)),+ $(,)? ] $(,)? ) => {
        {
            let names = [$($crate::RegisterName::as_abi(&($name))),+];
            #[allow(unused_parens)]
            let values = [$($crate::inject_helper(($name), &($value))),+];
            #[allow(unused_parens)]
            ($vp).set_registers(&names, &values)
        }
    }
}

#[macro_export]
macro_rules! get_registers {
    ($vp:expr, [$($name:expr),+ $(,)? ] $(,)? ) => {
        {
            let names = [$($crate::RegisterName::as_abi(&($name))),+];
            let mut values = [$($crate::get_registers!(@def $name)),+];
            ($vp).get_registers(&names, &mut values).map(|_| {
                let mut vs = &values[..];
                #[allow(unused_assignments)]
                ($({
                    let n = $name;
                    let v = &vs[0];
                    vs = &vs[1..];
                    { $crate::extract_helper(n, v) }
                }),+)
            })
        }
    };
    (@def $_:expr) => { Default::default() };
}
