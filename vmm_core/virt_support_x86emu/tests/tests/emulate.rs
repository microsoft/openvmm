// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::*;
use guestmem::GuestMemory;
use iced_x86::code_asm::CodeAssembler;
use pal_async::async_test;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use virt::VpIndex;
use virt::io::CpuIo;
use virt_support_x86emu::emulate::EmuTranslateError;
use virt_support_x86emu::emulate::EmuTranslateResult;
use virt_support_x86emu::emulate::EmulatorSupport;
use virt_support_x86emu::emulate::emulate;
use x86defs::RFlags;
use x86defs::cpuid::Vendor;
use x86emu::Gp;
use x86emu::Segment;

struct MockSupport {
    state: CpuState,
    instruction_bytes: Vec<u8>,
    interruption_pending: bool,
    mmio: bool,
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
        _mode: virt_support_x86emu::emulate::TranslateMode,
    ) -> Result<(), virt_support_x86emu::emulate::EmuCheckVtlAccessError> {
        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        _mode: virt_support_x86emu::emulate::TranslateMode,
    ) -> Result<EmuTranslateResult, EmuTranslateError> {
        Ok(EmuTranslateResult {
            gpa: gva,
            overlay_page: None,
        })
    }

    fn physical_address(&self) -> Option<u64> {
        Some(0x100)
    }

    /// The gva translation included in the intercept message header, if valid.
    fn initial_gva_translation(
        &mut self,
    ) -> Option<virt_support_x86emu::emulate::InitialTranslation> {
        None
    }

    fn interruption_pending(&self) -> bool {
        self.interruption_pending
    }

    /// Generates an event (exception, guest nested page fault, etc.) in the guest.
    fn inject_pending_event(&mut self, _event_info: hvdef::HvX64PendingEvent) {
        todo!()
    }

    fn is_gpa_mapped(&self, _gpa: u64, _write: bool) -> bool {
        !self.mmio
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
    let emu_mem = virt_support_x86emu::emulate::EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };
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
        mmio: false,
    };

    emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();

    assert_eq!(support.gp(Gp::RAX), TEST_VALUE);
}

#[async_test]
async fn not_enough_bytes() {
    const TEST_ADDRESS: u64 = 0x100;
    const TEST_VALUE: u64 = 0x123456789abcdef0;

    let gm = GuestMemory::allocate(4096);
    let emu_mem = virt_support_x86emu::emulate::EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };
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
        mmio: false,
    };

    gm.write_at(support.state.rip, &instruction_bytes).unwrap();

    emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();

    assert_eq!(support.gp(Gp::RAX), TEST_VALUE);
}

#[async_test]
#[should_panic]
async fn trap_from_interrupt() {
    const TEST_ADDRESS: u64 = 0x100;
    const TEST_VALUE: u64 = 0x123456789abcdef0;

    let gm = GuestMemory::allocate(4096);
    let emu_mem = virt_support_x86emu::emulate::EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };
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
        mmio: false,
    };

    emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();
}

#[async_test]
#[should_panic]
async fn trap_from_debug() {
    const TEST_ADDRESS: u64 = 0x100;
    const TEST_VALUE: u64 = 0x123456789abcdef0;

    let gm = GuestMemory::allocate(4096);
    let emu_mem = virt_support_x86emu::emulate::EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };
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
        mmio: false,
    };

    emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();
}

struct AtomicMmio {
    value: AtomicU32,
    race: AtomicBool,
    fail: bool,
}

struct LegacyMmio(AtomicU32);

impl CpuIo for LegacyMmio {
    fn is_mmio(&self, _: u64) -> bool {
        true
    }

    fn acknowledge_pic_interrupt(&self) -> Option<u8> {
        None
    }

    fn handle_eoi(&self, _: u32) {}

    async fn read_mmio(&self, _: VpIndex, _: u64, data: &mut [u8]) {
        data.copy_from_slice(&self.0.load(Ordering::SeqCst).to_ne_bytes());
    }

    async fn write_mmio(&self, _: VpIndex, _: u64, data: &[u8]) {
        self.0.store(
            u32::from_ne_bytes(data.try_into().unwrap()),
            Ordering::SeqCst,
        );
    }

    async fn read_io(&self, _: VpIndex, _: u16, _: &mut [u8]) {
        unreachable!()
    }

    async fn write_io(&self, _: VpIndex, _: u16, _: &[u8]) {
        unreachable!()
    }

    fn fatal_error(&self, _: Box<dyn std::error::Error + Send + Sync>) -> virt::VpHaltReason {
        virt::VpHaltReason::SingleStep
    }
}

#[async_test]
async fn default_mmio_compare_exchange_remains_unconditional() {
    let device = LegacyMmio(AtomicU32::new(1));
    assert!(
        device
            .compare_exchange_mmio(
                VpIndex::BSP,
                0x100,
                &0u32.to_ne_bytes(),
                &2u32.to_ne_bytes()
            )
            .await
            .unwrap()
    );
    assert_eq!(device.0.load(Ordering::SeqCst), 2);
}

impl CpuIo for AtomicMmio {
    fn is_mmio(&self, _: u64) -> bool {
        true
    }

    fn acknowledge_pic_interrupt(&self) -> Option<u8> {
        None
    }

    fn handle_eoi(&self, _: u32) {}

    async fn read_mmio(&self, _: VpIndex, _: u64, data: &mut [u8]) {
        data.copy_from_slice(&self.value.load(Ordering::SeqCst).to_ne_bytes());
        if self.race.swap(false, Ordering::SeqCst) {
            self.value.store(2, Ordering::SeqCst);
        }
    }

    async fn write_mmio(&self, _: VpIndex, _: u64, _: &[u8]) {
        panic!("atomic access must not be sent as an unconditional write");
    }

    async fn compare_exchange_mmio(
        &self,
        _: VpIndex,
        _: u64,
        current: &[u8],
        new: &[u8],
    ) -> std::io::Result<bool> {
        if self.fail {
            return Err(std::io::Error::other("atomic MMIO failed"));
        }
        let current = u32::from_ne_bytes(current.try_into().unwrap());
        let new = u32::from_ne_bytes(new.try_into().unwrap());
        Ok(self
            .value
            .compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok())
    }

    async fn read_io(&self, _: VpIndex, _: u16, _: &mut [u8]) {
        unreachable!()
    }

    async fn write_io(&self, _: VpIndex, _: u16, _: &[u8]) {
        unreachable!()
    }

    fn fatal_error(&self, _: Box<dyn std::error::Error + Send + Sync>) -> virt::VpHaltReason {
        virt::VpHaltReason::SingleStep
    }
}

#[async_test]
async fn locked_mmio_compare_exchange_retries_on_race() {
    use iced_x86::code_asm::*;

    let gm = GuestMemory::allocate(4096);
    let emu_mem = virt_support_x86emu::emulate::EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };
    let mut asm = CodeAssembler::new(64).unwrap();
    asm.lock().cmpxchg(dword_ptr(0x100), ecx).unwrap();
    let bytes = asm.assemble(0).unwrap();

    for race in [false, true] {
        let device = AtomicMmio {
            value: AtomicU32::new(1),
            race: AtomicBool::new(race),
            fail: false,
        };
        let mut support = MockSupport {
            state: long_protected_mode(false),
            instruction_bytes: bytes.clone(),
            interruption_pending: false,
            mmio: true,
        };
        support.state.gps[Gp::RAX as usize] = 1;
        support.state.gps[Gp::RCX as usize] = 3;
        emulate(&mut support, &emu_mem, &device).await.unwrap();
        assert_eq!(
            device.value.load(Ordering::SeqCst),
            if race { 2 } else { 3 }
        );
        if race {
            assert_eq!(support.state.rip, 0);
            emulate(&mut support, &emu_mem, &device).await.unwrap();
        }

        assert_eq!(support.state.rflags.zero(), !race);
        if race {
            assert_eq!(support.gp(Gp::RAX), 2);
        }
    }
}

#[async_test]
async fn locked_mmio_compare_exchange_failure_does_not_write() {
    use iced_x86::code_asm::*;

    let gm = GuestMemory::allocate(4096);
    let emu_mem = virt_support_x86emu::emulate::EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };
    let mut asm = CodeAssembler::new(64).unwrap();
    asm.lock().cmpxchg(dword_ptr(0x100), ecx).unwrap();
    let device = AtomicMmio {
        value: AtomicU32::new(1),
        race: AtomicBool::new(false),
        fail: true,
    };
    let mut support = MockSupport {
        state: long_protected_mode(false),
        instruction_bytes: asm.assemble(0).unwrap(),
        interruption_pending: false,
        mmio: true,
    };
    support.state.gps[Gp::RAX as usize] = 1;
    support.state.gps[Gp::RCX as usize] = 3;
    assert!(matches!(
        emulate(&mut support, &emu_mem, &device).await,
        Err(virt::VpHaltReason::SingleStep)
    ));
    assert_eq!(device.value.load(Ordering::SeqCst), 1);
    assert_eq!(support.state.rip, 0);
}
