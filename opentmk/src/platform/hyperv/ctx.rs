// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V test context implementation.

// vp_set is only used in x86_64 for now, since aarch support is not complete
#![cfg_attr(target_arch = "aarch64", expect(dead_code))] // xtask-fmt allow-target-arch sys-crate
use alloc::boxed::Box;
use alloc::collections::btree_map::BTreeMap;
use alloc::collections::btree_set::BTreeSet;
use alloc::collections::linked_list::LinkedList;
use core::fmt::Display;

use hvdef::Vtl;
use hvdef::hypercall::HvInputVtl;
use spin::Mutex;

use crate::context::VirtualProcessorPlatformTrait;
use crate::context::VtlPlatformTrait;
use crate::platform::hyperv::arch::hypercall::HvCall;
use crate::tmkdefs::TmkError;
use crate::tmkdefs::TmkResult;

type CommandTable = BTreeMap<u32, LinkedList<(Box<dyn FnOnce(&mut HvTestCtx) + 'static>, Vtl)>>;
static mut CMD: Mutex<CommandTable> = Mutex::new(BTreeMap::new());
static VP_SET: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

#[expect(static_mut_refs)]
pub(crate) fn cmdt() -> &'static Mutex<CommandTable> {
    // SAFETY: CMD is only mutated through safe APIs and is protected by a Mutex.
    unsafe { &CMD }
}

pub(crate) fn get_vp_set() -> &'static Mutex<BTreeSet<u32>> {
    // SAFETY: VP_SET is only mutated through safe APIs and is protected by a Mutex.
    &VP_SET
}

fn register_command_queue(vp_index: u32) {
    log::trace!("registering command queue for vp: {}", vp_index);
    if cmdt().lock().get(&vp_index).is_none() {
        cmdt().lock().insert(vp_index, LinkedList::new());
        log::trace!("registered command queue for vp: {}", vp_index);
    } else {
        log::trace!("command queue already registered for vp: {}", vp_index);
    }
}

/// The execution context passed to the test functions.
pub struct HvTestCtx {
    /// The hypercall interface.
    /// Exposed publicly for test code to make hypercalls in specialized cases.
    pub hvcall: HvCall,
    /// The index of the VP on which this context is running.
    pub my_vp_idx: u32,
    /// The VTL on which this context is running.
    pub my_vtl: Vtl,
}

impl Display for HvTestCtx {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "HvTestCtx {{ vp_idx: {}, vtl: {:?} }}",
            self.my_vp_idx, self.my_vtl
        )
    }
}

pub(crate) fn vtl_transform(vtl: Vtl) -> HvInputVtl {
    let vtl = match vtl {
        Vtl::Vtl0 => 0,
        Vtl::Vtl1 => 1,
        Vtl::Vtl2 => 2,
    };
    HvInputVtl::new()
        .with_target_vtl_value(vtl)
        .with_use_target_vtl(true)
}

#[cfg_attr(target_arch = "aarch64", expect(dead_code))] // xtask-fmt allow-target-arch sys-crate
impl HvTestCtx {
    /// Construct an *un-initialised* test context.  
    /// Call [`HvTestCtx::init`] before using the value.
    pub const fn new() -> Self {
        HvTestCtx {
            hvcall: HvCall::new(),
            my_vp_idx: 0,
            my_vtl: Vtl::Vtl0,
        }
    }

    /// Perform the one-time initialisation sequence:  
    /// – initialise the hypercall page,  
    /// – discover the VP count and create command queues,  
    /// – record the current VTL.
    pub fn init(&mut self, vtl: Vtl) -> TmkResult<()> {
        self.hvcall.initialize();
        let vp_count = self.get_vp_count()?;
        for i in 0..vp_count {
            register_command_queue(i);
        }
        self.my_vtl = vtl;
        self.my_vp_idx = Self::get_vp_idx();
        Ok(())
    }

    pub(crate) fn secure_exec_handler() {
        HvTestCtx::exec_handler(Vtl::Vtl1);
    }

    pub(crate) fn general_exec_handler() {
        HvTestCtx::exec_handler(Vtl::Vtl0);
    }

    /// Busy-loop executor that runs on every VP.  
    /// Extracts commands from the per-VP queue and executes them in the
    /// appropriate VTL, switching VTLs when necessary.
    fn exec_handler(vtl: Vtl) {
        let mut ctx = HvTestCtx::new();
        ctx.init(vtl).expect("error: failed to init on a VP");
        loop {
            let mut vtl: Option<Vtl> = None;
            let mut cmd: Option<Box<dyn FnOnce(&mut HvTestCtx) + 'static>> = None;

            {
                let mut cmdt = cmdt().lock();
                let d = cmdt.get_mut(&ctx.my_vp_idx);
                if let Some(d) = d {
                    if !d.is_empty() {
                        let (_c, v) = d.front().unwrap();
                        if *v == ctx.my_vtl {
                            let (c, _v) = d.pop_front().unwrap();
                            cmd = Some(c);
                        } else {
                            vtl = Some(*v);
                        }
                    }
                }
            }

            if let Some(vtl) = vtl {
                if vtl == Vtl::Vtl0 {
                    ctx.switch_to_low_vtl();
                } else {
                    ctx.switch_to_high_vtl();
                }
            }

            if let Some(cmd) = cmd {
                cmd(&mut ctx);
            }
        }
    }
}

impl From<hvdef::HvError> for TmkError {
    fn from(e: hvdef::HvError) -> Self {
        log::debug!("Converting hvdef::HvError::{:?} to TmkError", e);
        let tmk_error_type = match e {
            hvdef::HvError::InvalidHypercallCode => TmkError::InvalidHypercallCode,
            hvdef::HvError::InvalidHypercallInput => TmkError::InvalidHypercallInput,
            hvdef::HvError::InvalidAlignment => TmkError::InvalidAlignment,
            hvdef::HvError::InvalidParameter => TmkError::InvalidParameter,
            hvdef::HvError::AccessDenied => TmkError::AccessDenied,
            hvdef::HvError::InvalidPartitionState => TmkError::InvalidPartitionState,
            hvdef::HvError::OperationDenied => TmkError::OperationDenied,
            hvdef::HvError::UnknownProperty => TmkError::UnknownProperty,
            hvdef::HvError::PropertyValueOutOfRange => TmkError::PropertyValueOutOfRange,
            hvdef::HvError::InsufficientMemory => TmkError::InsufficientMemory,
            hvdef::HvError::PartitionTooDeep => TmkError::PartitionTooDeep,
            hvdef::HvError::InvalidPartitionId => TmkError::InvalidPartitionId,
            hvdef::HvError::InvalidVpIndex => TmkError::InvalidVpIndex,
            hvdef::HvError::NotFound => TmkError::NotFound,
            hvdef::HvError::InvalidPortId => TmkError::InvalidPortId,
            hvdef::HvError::InvalidConnectionId => TmkError::InvalidConnectionId,
            hvdef::HvError::InsufficientBuffers => TmkError::InsufficientBuffers,
            hvdef::HvError::NotAcknowledged => TmkError::NotAcknowledged,
            hvdef::HvError::InvalidVpState => TmkError::InvalidVpState,
            hvdef::HvError::Acknowledged => TmkError::Acknowledged,
            hvdef::HvError::InvalidSaveRestoreState => TmkError::InvalidSaveRestoreState,
            hvdef::HvError::InvalidSynicState => TmkError::InvalidSynicState,
            hvdef::HvError::ObjectInUse => TmkError::ObjectInUse,
            hvdef::HvError::InvalidProximityDomainInfo => TmkError::InvalidProximityDomainInfo,
            hvdef::HvError::NoData => TmkError::NoData,
            hvdef::HvError::Inactive => TmkError::Inactive,
            hvdef::HvError::NoResources => TmkError::NoResources,
            hvdef::HvError::FeatureUnavailable => TmkError::FeatureUnavailable,
            hvdef::HvError::PartialPacket => TmkError::PartialPacket,
            hvdef::HvError::ProcessorFeatureNotSupported => TmkError::ProcessorFeatureNotSupported,
            hvdef::HvError::ProcessorCacheLineFlushSizeIncompatible => {
                TmkError::ProcessorCacheLineFlushSizeIncompatible
            }
            hvdef::HvError::InsufficientBuffer => TmkError::InsufficientBuffer,
            hvdef::HvError::IncompatibleProcessor => TmkError::IncompatibleProcessor,
            hvdef::HvError::InsufficientDeviceDomains => TmkError::InsufficientDeviceDomains,
            hvdef::HvError::CpuidFeatureValidationError => TmkError::CpuidFeatureValidationError,
            hvdef::HvError::CpuidXsaveFeatureValidationError => {
                TmkError::CpuidXsaveFeatureValidationError
            }
            hvdef::HvError::ProcessorStartupTimeout => TmkError::ProcessorStartupTimeout,
            hvdef::HvError::SmxEnabled => TmkError::SmxEnabled,
            hvdef::HvError::InvalidLpIndex => TmkError::InvalidLpIndex,
            hvdef::HvError::InvalidRegisterValue => TmkError::InvalidRegisterValue,
            hvdef::HvError::InvalidVtlState => TmkError::InvalidVtlState,
            hvdef::HvError::NxNotDetected => TmkError::NxNotDetected,
            hvdef::HvError::InvalidDeviceId => TmkError::InvalidDeviceId,
            hvdef::HvError::InvalidDeviceState => TmkError::InvalidDeviceState,
            hvdef::HvError::PendingPageRequests => TmkError::PendingPageRequests,
            hvdef::HvError::PageRequestInvalid => TmkError::PageRequestInvalid,
            hvdef::HvError::KeyAlreadyExists => TmkError::KeyAlreadyExists,
            hvdef::HvError::DeviceAlreadyInDomain => TmkError::DeviceAlreadyInDomain,
            hvdef::HvError::InvalidCpuGroupId => TmkError::InvalidCpuGroupId,
            hvdef::HvError::InvalidCpuGroupState => TmkError::InvalidCpuGroupState,
            hvdef::HvError::OperationFailed => TmkError::OperationFailed,
            hvdef::HvError::NotAllowedWithNestedVirtActive => {
                TmkError::NotAllowedWithNestedVirtActive
            }
            hvdef::HvError::InsufficientRootMemory => TmkError::InsufficientRootMemory,
            hvdef::HvError::EventBufferAlreadyFreed => TmkError::EventBufferAlreadyFreed,
            hvdef::HvError::Timeout => TmkError::Timeout,
            hvdef::HvError::VtlAlreadyEnabled => TmkError::VtlAlreadyEnabled,
            hvdef::HvError::UnknownRegisterName => TmkError::UnknownRegisterName,
            // Add any other specific mappings here if hvdef::HvError has more variants
            _ => {
                log::warn!(
                    "Unhandled hvdef::HvError variant: {:?}. Mapping to TmkError::OperationFailed.",
                    e
                );
                TmkError::OperationFailed // Generic fallback
            }
        };
        log::debug!(
            "Mapped hvdef::HvError::{:?} to TmkError::{:?}",
            e,
            tmk_error_type
        );
        tmk_error_type
    }
}
