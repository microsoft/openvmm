// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::*;
use guestmem::GuestMemory;
use hvdef::Vtl;
use iced_x86::code_asm::*;
use pal_async::async_test;
use virt_support_x86emu::emulate::*;
use vm_topology::processor::VpIndex;
use x86defs::RFlags;
use x86defs::cpuid::Vendor;
use x86emu::Gp;
use x86emu::Segment;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// Implements [`EmulatorSupport`] with some features for deliberately
/// failing vtl permissions checks and checking the resulting injected
/// event
struct MockSupport {
    state: CpuState,
    instruction_bytes: Vec<u8>,
    fail_vtl_access: Option<Vtl>,
    injected_event: Option<hvdef::HvX64PendingEvent>,
}

impl EmulatorSupport for MockSupport {
    fn vp_index(&self) -> VpIndex {
        VpIndex::BSP
    }

    fn vendor(&self) -> Vendor {
        Vendor::INTEL
    }

    fn gp(&mut self, reg: Gp) -> u64 {
        self.state.gps[reg as usize]
    }
    fn set_gp(&mut self, reg: Gp, v: u64) {
        self.state.gps[reg as usize] = v;
    }
    fn rip(&mut self) -> u64 {
        self.state.rip
    }
    fn set_rip(&mut self, v: u64) {
        self.state.rip = v;
    }

    fn segment(&mut self, reg: Segment) -> x86defs::SegmentRegister {
        self.state.segs[reg as usize]
    }

    fn efer(&mut self) -> u64 {
        self.state.efer
    }
    fn cr0(&mut self) -> u64 {
        self.state.cr0
    }
    fn rflags(&mut self) -> RFlags {
        self.state.rflags
    }
    fn set_rflags(&mut self, v: RFlags) {
        self.state.rflags = v;
    }
    fn xmm(&mut self, _reg: usize) -> u128 {
        todo!()
    }
    fn set_xmm(&mut self, _reg: usize, _v: u128) {
        todo!()
    }
    fn flush(&mut self) {}

    fn instruction_bytes(&self) -> &[u8] {
        &self.instruction_bytes
    }

    fn check_vtl_access(
        &mut self,
        _gpa: u64,
        mode: TranslateMode,
    ) -> Result<(), EmuCheckVtlAccessError> {
        if let Some(vtl) = self.fail_vtl_access {
            let flags = match mode {
                TranslateMode::Read => hvdef::HvMapGpaFlags::new().with_readable(true),
                TranslateMode::Write => hvdef::HvMapGpaFlags::new().with_writable(true),
                TranslateMode::Execute => hvdef::HvMapGpaFlags::new()
                    .with_kernel_executable(true)
                    .with_user_executable(true),
            };

            return Err(EmuCheckVtlAccessError::AccessDenied {
                vtl,
                denied_flags: flags,
            });
        }

        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        _mode: TranslateMode,
    ) -> Result<EmuTranslateResult, EmuTranslateError> {
        Ok(EmuTranslateResult {
            gpa: gva,
            overlay_page: None,
        })
    }

    fn physical_address(&self) -> Option<u64> {
        todo!()
    }

    /// The gva translation included in the intercept message header, if valid.
    fn initial_gva_translation(&mut self) -> Option<InitialTranslation> {
        None
    }

    fn interruption_pending(&self) -> bool {
        false
    }

    /// Checks that the event injected corresponds to the expected one
    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent) {
        self.injected_event = Some(event_info);
    }

    fn is_gpa_mapped(&self, _gpa: u64, _write: bool) -> bool {
        true
    }

    fn lapic_base_address(&self) -> Option<u64> {
        None
    }

    fn lapic_read(&mut self, _address: u64, _data: &mut [u8]) {
        unreachable!()
    }

    fn lapic_write(&mut self, _address: u64, _data: &[u8]) {
        unreachable!()
    }
}

const TEST_ADDRESS: u64 = 0x100;

async fn run_emulation(
    check_execute: bool,
    fail_vtl_access: Option<Vtl>,
) -> Option<hvdef::HvX64PendingEvent> {
    const TEST_VALUE: u64 = 0x123456789abcdef0;

    let gm = GuestMemory::allocate(4096);
    let emu_mem = EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };

    gm.write_at(TEST_ADDRESS, &TEST_VALUE.to_le_bytes())
        .unwrap();

    let mut asm = CodeAssembler::new(64).unwrap();
    asm.mov(rax, ptr(TEST_ADDRESS)).unwrap();

    let instruction_bytes = asm.assemble(0).unwrap();
    let truncated_instructions = if check_execute {
        instruction_bytes[..2].into()
    } else {
        instruction_bytes.clone()
    };

    let mut support = MockSupport {
        state: long_protected_mode(false),
        instruction_bytes: truncated_instructions,
        fail_vtl_access,
        injected_event: None,
    };

    if check_execute {
        gm.write_at(support.state.rip, &instruction_bytes).unwrap();
    }

    emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();

    if fail_vtl_access.is_none() {
        assert_eq!(support.gp(Gp::RAX), TEST_VALUE);
    }

    support.injected_event
}

#[async_test]
async fn check_vtl_access_read() {
    let event = run_emulation(false, None).await;
    assert!(event.is_none());
}

#[async_test]
async fn check_vtl_access_read_vtl1_denied() {
    let event = run_emulation(false, Some(Vtl::Vtl1)).await;
    assert!(event.is_some());
    validate_vtl_access_event(false, &event.unwrap(), Vtl::Vtl1);
}

#[async_test]
async fn check_vtl_access_execute() {
    let event = run_emulation(true, None).await;
    assert!(event.is_none());
}

#[async_test]
async fn check_vtl_execute_access_vtl2_denied() {
    let event = run_emulation(true, Some(Vtl::Vtl2)).await;
    assert!(event.is_some());
    validate_gpf_event(event.unwrap());
}

#[async_test]
async fn check_vtl_execute_access_vtl1_denied() {
    let event = run_emulation(true, Some(Vtl::Vtl1)).await;
    assert!(event.is_some());
    validate_vtl_access_event(true, &event.unwrap(), Vtl::Vtl1);
}

fn validate_vtl_access_event(
    check_execute: bool,
    pending_event: &hvdef::HvX64PendingEvent,
    expected_vtl: Vtl,
) {
    let event =
        hvdef::HvX64PendingEventMemoryIntercept::read_from_bytes(pending_event.as_bytes()).unwrap();

    assert!(event.event_header.event_pending());

    assert_eq!(
        event.event_header.event_type(),
        hvdef::HV_X64_PENDING_EVENT_MEMORY_INTERCEPT
    );

    assert_eq!(event.target_vtl, expected_vtl.into());

    if check_execute {
        assert_eq!(event.access_type, hvdef::HvInterceptAccessType::EXECUTE);
    } else {
        // run_emulation doesn't support writes yet
        assert_eq!(event.access_type, hvdef::HvInterceptAccessType::READ);
    }

    assert!(event.access_flags.guest_linear_address_valid());

    assert!(event.access_flags.caused_by_gpa_access());

    assert_eq!(
        event.guest_physical_address,
        (TEST_ADDRESS >> hvdef::HV_PAGE_SHIFT) << hvdef::HV_PAGE_SHIFT
    );

    assert_eq!(
        event.guest_linear_address,
        (TEST_ADDRESS >> hvdef::HV_PAGE_SHIFT) << hvdef::HV_PAGE_SHIFT
    );
}
