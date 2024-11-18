// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::*;
use guestmem::GuestMemory;
use iced_x86::code_asm::CodeAssembler;
use pal_async::async_test;
use virt::VpIndex;
use virt_support_x86emu::emulate::emulate;
use virt_support_x86emu::emulate::EmuTranslateError;
use virt_support_x86emu::emulate::EmuTranslateResult;
use virt_support_x86emu::emulate::EmulatorSupport;
use x86defs::cpuid::Vendor;
use x86emu::CpuState;

struct MockSupport {
    state: CpuState,
    instruction_bytes: Vec<u8>,
    interruption_pending: bool,
}

impl EmulatorSupport for MockSupport {
    type Error = std::convert::Infallible;

    fn vp_index(&self) -> VpIndex {
        VpIndex::BSP
    }

    fn vendor(&self) -> Vendor {
        Vendor::INTEL
    }

    fn state(&mut self) -> Result<CpuState, Self::Error> {
        Ok(self.state.clone())
    }

    fn set_state(&mut self, state: CpuState) -> Result<(), Self::Error> {
        self.state = state;
        Ok(())
    }

    fn instruction_bytes(&self) -> &[u8] {
        &self.instruction_bytes
    }

    fn check_vtl_access(
        &mut self,
        _gpa: u64,
        _mode: virt_support_x86emu::emulate::TranslateMode,
    ) -> Result<(), virt_support_x86emu::emulate::EmuCheckVtlAccessError<Self::Error>> {
        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        _mode: virt_support_x86emu::emulate::TranslateMode,
    ) -> Result<Result<EmuTranslateResult, EmuTranslateError>, Self::Error> {
        Ok(Ok(EmuTranslateResult {
            gpa: gva,
            overlay_page: None,
        }))
    }

    fn physical_address(&self) -> Option<u64> {
        todo!()
    }

    /// The gva translation included in the intercept message header, if valid.
    fn initial_gva_translation(&self) -> Option<virt_support_x86emu::emulate::InitialTranslation> {
        None
    }

    fn interruption_pending(&self) -> bool {
        self.interruption_pending
    }

    /// Generates an event (exception, guest nested page fault, etc.) in the guest.
    fn inject_pending_event(&mut self, _event_info: hvdef::HvX64PendingEvent) {
        todo!()
    }

    fn get_xmm(&mut self, _reg: usize) -> Result<u128, Self::Error> {
        todo!()
    }

    fn set_xmm(&mut self, _reg: usize, _value: u128) -> Result<(), Self::Error> {
        todo!()
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

#[async_test]
async fn basic_mov() {
    const TEST_ADDRESS: u64 = 0x100;
    const TEST_VALUE: u64 = 0x123456789abcdef0;

    let gm = GuestMemory::allocate(4096);
    gm.write_at(TEST_ADDRESS, &TEST_VALUE.to_le_bytes())
        .unwrap();

    let mut asm = CodeAssembler::new(64).unwrap();
    asm.mov(
        iced_x86::code_asm::rax,
        iced_x86::code_asm::ptr(TEST_ADDRESS),
    )
    .unwrap();

    let instruction_bytes = asm.assemble(0).unwrap();

    let mut support = MockSupport {
        state: long_protected_mode(false),
        instruction_bytes,
        interruption_pending: false,
    };

    emulate(&mut support, &gm, &MockCpu).await.unwrap();

    assert_eq!(support.state.gps[CpuState::RAX], TEST_VALUE);
}

#[async_test]
async fn not_enough_bytes() {
    const TEST_ADDRESS: u64 = 0x100;
    const TEST_VALUE: u64 = 0x123456789abcdef0;

    let gm = GuestMemory::allocate(4096);
    gm.write_at(TEST_ADDRESS, &TEST_VALUE.to_le_bytes())
        .unwrap();

    let mut asm = CodeAssembler::new(64).unwrap();
    {
        use iced_x86::code_asm::*;
        asm.mov(rax, ptr(TEST_ADDRESS))
    }
    .unwrap();

    let instruction_bytes = asm.assemble(0).unwrap();
    assert!(instruction_bytes.len() > 2);

    let mut support = MockSupport {
        state: long_protected_mode(false),
        instruction_bytes: instruction_bytes[..2].into(),
        interruption_pending: false,
    };

    gm.write_at(support.state.rip, &instruction_bytes).unwrap();

    emulate(&mut support, &gm, &MockCpu).await.unwrap();

    assert_eq!(support.state.gps[CpuState::RAX], TEST_VALUE);
}

#[async_test]
#[should_panic]
async fn trap_from_interrupt() {
    const TEST_ADDRESS: u64 = 0x100;
    const TEST_VALUE: u64 = 0x123456789abcdef0;

    let gm = GuestMemory::allocate(4096);
    gm.write_at(TEST_ADDRESS, &TEST_VALUE.to_le_bytes())
        .unwrap();

    let mut asm = CodeAssembler::new(64).unwrap();
    asm.mov(
        iced_x86::code_asm::rax,
        iced_x86::code_asm::ptr(TEST_ADDRESS),
    )
    .unwrap();

    let instruction_bytes = asm.assemble(0).unwrap();

    let mut support = MockSupport {
        state: long_protected_mode(false),
        instruction_bytes,
        interruption_pending: true,
    };

    emulate(&mut support, &gm, &MockCpu).await.unwrap();
}

#[async_test]
#[should_panic]
async fn trap_from_debug() {
    const TEST_ADDRESS: u64 = 0x100;
    const TEST_VALUE: u64 = 0x123456789abcdef0;

    let gm = GuestMemory::allocate(4096);
    gm.write_at(TEST_ADDRESS, &TEST_VALUE.to_le_bytes())
        .unwrap();

    let mut asm = CodeAssembler::new(64).unwrap();
    asm.mov(
        iced_x86::code_asm::rax,
        iced_x86::code_asm::ptr(TEST_ADDRESS),
    )
    .unwrap();

    let instruction_bytes = asm.assemble(0).unwrap();

    let mut state = long_protected_mode(false);
    state.rflags.set_trap(true);

    let mut support = MockSupport {
        state,
        instruction_bytes,
        interruption_pending: false,
    };

    emulate(&mut support, &gm, &MockCpu).await.unwrap();
}
