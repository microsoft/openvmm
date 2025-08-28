// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use virt::VpIndex;
use virt::io::CpuIo;
use x86defs::RFlags;
use x86defs::SegmentAttributes;
use x86defs::SegmentRegister;

pub struct CpuState {
    /// GP registers, in the canonical order (as defined by `RAX`, etc.).
    pub gps: [u64; 16],
    /// Segment registers, in the canonical order (as defined by `ES`, etc.).
    pub segs: [SegmentRegister; 6],
    pub rip: u64,
    pub rflags: RFlags,

    pub cr0: u64,
    pub efer: u64,
}

/// Return [`CpuState`] that has long mode and protected mode enabled
///
/// Configurable based on [`CpuStateFlags`]
pub fn long_protected_mode(user_mode: bool) -> CpuState {
    let mut attributes = SegmentAttributes::new().with_long(true);
    if user_mode {
        attributes.set_descriptor_privilege_level(x86defs::USER_MODE_DPL);
    }

    let seg = SegmentRegister {
        base: 0,
        limit: 0,
        attributes,
        selector: 0,
    };

    CpuState {
        gps: [0xbadc0ffee0ddf00d; 16],
        segs: [seg; 6],
        rip: 0,
        rflags: RFlags::at_reset(),
        cr0: x86defs::X64_CR0_PE,
        efer: x86defs::X64_EFER_LMA | x86defs::X64_EFER_LME,
    }
}

/// Implements CpuIo stubs for unit test purposes
pub struct MockCpu;

impl CpuIo for MockCpu {
    fn is_mmio(&self, _address: u64) -> bool {
        todo!()
    }

    fn acknowledge_pic_interrupt(&self) -> Option<u8> {
        todo!()
    }

    fn handle_eoi(&self, _irq: u32) {
        todo!()
    }

    fn signal_synic_event(
        &self,
        _vtl: hvdef::Vtl,
        _connection_id: u32,
        _flag: u16,
    ) -> hvdef::HvResult<()> {
        todo!()
    }

    fn post_synic_message(
        &self,
        _vtl: hvdef::Vtl,
        _connection_id: u32,
        _secure: bool,
        _message: &[u8],
    ) -> hvdef::HvResult<()> {
        todo!()
    }

    async fn read_mmio(&self, _vp: VpIndex, address: u64, _data: &mut [u8]) {
        panic!(
            "Attempt to read MMIO when test environment has no MMIO. address: {:x}",
            address
        )
    }

    async fn write_mmio(&self, _vp: VpIndex, address: u64, data: &[u8]) {
        panic!(
            "Attempt to write MMIO when test environment has no MMIO. address: {:x}, data: {:x?}",
            address, data
        )
    }

    async fn read_io(&self, _vp: VpIndex, _port: u16, _data: &mut [u8]) {
        todo!()
    }

    async fn write_io(&self, _vp: VpIndex, _port: u16, _data: &[u8]) {
        todo!()
    }

    fn fatal_error(&self, _error: Box<dyn std::error::Error + Send + Sync>) -> virt::VpHaltReason {
        todo!()
    }
}

/// Validates the given event is indeed a gpf
pub fn validate_gpf_event(pending_event: hvdef::HvX64PendingEvent) {
    let event = hvdef::HvX64PendingExceptionEvent::from(u128::from(pending_event.reg_0));
    assert!(event.event_pending());

    assert_eq!(event.event_type(), hvdef::HV_X64_PENDING_EVENT_EXCEPTION);

    assert_eq!(
        event.vector(),
        x86defs::Exception::GENERAL_PROTECTION_FAULT.0.into()
    );

    assert!(event.deliver_error_code());

    assert_eq!(event.error_code(), 0);

    assert_eq!(event.exception_parameter(), 0);
}
