// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::*;
use guestmem::GuestMemory;
use hvdef::hypercall::TranslateGvaResultCode;
use iced_x86::code_asm::*;
use pal_async::async_test;
use virt_support_x86emu::emulate::*;
use vm_topology::processor::VpIndex;
use x86defs::RFlags;
use x86defs::cpuid::Vendor;
use x86emu::Gp;
use x86emu::Segment;
use zerocopy::IntoBytes;

const INITIAL_GVA: u64 = 0x2000;
const INITIAL_GPA: u64 = 0x1000;

/// How MockSupport should handle calls to [`EmulatorSupport::translate_gva`]
enum MockSupportMode {
    /// [`translate_gva`] should succeed/fail with the given code
    Code(TranslateGvaResultCode),
    /// ['translate_gva'] should return that the translation is an overlay page
    TestOverlay,
}

/// Details of translation that MockSupport should be configured with.
struct MockSupportTranslation {
    /// the address that the test expects to access. MockSupport will
    /// then create an instruction stream accessing that gva.
    access_gva: u64,
    /// the gpa that should be returned as a result of calling [`EmulatorSupport::translate_gva`] on access_gva. Note
    /// that this is independent of the translation returned by the initial translation.
    translate_to_gpa: u64,
    /// [`MockAccess`] information for configuring the instruction stream
    access_info: MockAccess,
}

/// Implements [`EmulatorSupport`] with some added abilities for testing
/// translation failures during emulation
///
/// Will generate an instruction stream depending on the desired access type.
///
/// Execute access will generate an instruction stream that movs a given value.
/// The instruction stream will be written to the expected gpa
struct MockSupport {
    state: CpuState,
    instruction_bytes: Vec<u8>,
    test_translation: MockSupportTranslation,
    mode: MockSupportMode,
    injected_event: Option<hvdef::HvX64PendingEvent>,
    saw_execute_inst: bool,
}

/// How the tester expects to access the gva in question, and
/// what value it expects will be operated on where applicable
#[derive(Copy, Clone)]
enum MockAccess {
    /// Request an instruction stream that reads from a gva
    Read,
    /// Request an instruction stream that writes to a gva
    Write(u64),
    /// Requests that MockSupport will provide an insufficient
    /// instruction stream, resulting in an execute on a gva
    /// where the translated gpa should have the full set of
    /// instructions
    Execute(u64),
}

impl MockSupport {
    const WORKING_REGISTER: Gp = Gp::RSI;
    const ASM_WORKING_REGISTER: AsmRegister64 = rsi;

    fn new(
        mode: MockSupportMode,
        test_translation: MockSupportTranslation,
        user_mode: bool,
        gm: &GuestMemory,
    ) -> Self {
        let mut state = long_protected_mode(user_mode);
        let mut asm = CodeAssembler::new(64).unwrap();

        let MockSupportTranslation {
            access_gva: gva,
            translate_to_gpa,
            access_info,
        } = test_translation;

        match access_info {
            MockAccess::Read => {
                state.gps[Gp::RAX as usize] = gva;
                asm.mov(Self::ASM_WORKING_REGISTER, dword_ptr(rax)).unwrap();
            }
            MockAccess::Write(write_value) => {
                state.gps[Gp::RAX as usize] = gva;
                state.gps[Self::WORKING_REGISTER as usize] = write_value;
                asm.mov(dword_ptr(rax), Self::ASM_WORKING_REGISTER).unwrap();
            }
            MockAccess::Execute(mov_value) => {
                // emulator requires that instructions operate on mmio or pio
                const VALUE_GPA: u64 = 0;
                assert_ne!(translate_to_gpa, VALUE_GPA);
                assert_ne!(INITIAL_GPA, VALUE_GPA);
                gm.write_at(VALUE_GPA, mov_value.as_bytes()).unwrap();

                state.rip = gva;
                state.gps[Gp::RAX as usize] = VALUE_GPA;

                asm.mov(Self::ASM_WORKING_REGISTER, dword_ptr(rax)).unwrap();
            }
        }

        let instruction_bytes = asm.assemble(state.rip).unwrap();

        if let MockAccess::Execute(_) = access_info {
            gm.write_at(translate_to_gpa, &instruction_bytes).unwrap();
        }

        MockSupport {
            state,
            instruction_bytes: if let MockAccess::Execute(_) = access_info {
                instruction_bytes[..0].into()
            } else {
                instruction_bytes
            },
            test_translation,
            mode,
            injected_event: None,
            saw_execute_inst: false,
        }
    }

    fn injected_event(&self) -> Option<hvdef::HvX64PendingEvent> {
        self.injected_event
    }

    fn intercept_event() -> hvdef::HvX64PendingEvent {
        hvdef::HvX64PendingEvent::from(
            hvdef::HvX64PendingExceptionEvent::new()
                .with_error_code(0xc0de)
                .with_exception_parameter(0xc0debad),
        )
    }

    fn accessed_value(&self) -> u64 {
        let MockSupportTranslation {
            access_gva: _,
            translate_to_gpa: _,
            access_info,
        } = self.test_translation;

        if let MockAccess::Write(_) = access_info {
            panic!("nothing to be read")
        }

        self.state.gps[Self::WORKING_REGISTER as usize]
    }
}

impl EmulatorSupport for MockSupport {
    fn vp_index(&self) -> VpIndex {
        VpIndex::BSP
    }

    fn vendor(&self) -> Vendor {
        Vendor::INTEL
    }

    fn flush(&mut self) {}

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

    fn instruction_bytes(&self) -> &[u8] {
        &self.instruction_bytes
    }

    fn check_vtl_access(
        &mut self,
        _gpa: u64,
        _mode: TranslateMode,
    ) -> Result<(), EmuCheckVtlAccessError> {
        Ok(())
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        mode: TranslateMode,
    ) -> Result<EmuTranslateResult, EmuTranslateError> {
        println!("translating for {:?} access", mode);

        if mode == TranslateMode::Execute {
            self.saw_execute_inst = true;
        }

        match self.mode {
            MockSupportMode::Code(c) => match c {
                TranslateGvaResultCode::INTERCEPT => Err(EmuTranslateError {
                    code: c,
                    event_info: Some(Self::intercept_event()),
                }),
                TranslateGvaResultCode::SUCCESS => {
                    let MockSupportTranslation {
                        access_gva: test_gva,
                        translate_to_gpa: test_gpa,
                        access_info: _,
                    } = self.test_translation;

                    let gpa = if (gva >> hvdef::HV_PAGE_SHIFT) == (test_gva >> hvdef::HV_PAGE_SHIFT)
                    {
                        test_gpa
                    } else if self.saw_execute_inst {
                        // Assume that the expected gva is not being accessed as a result of operating on an execute instruction
                        // located at a previously translated gva. Just return the gva.
                        gva
                    } else {
                        panic!(
                            "accessing {:x}, not the expected gva {:x}; was the instruction stream created incorrectly?",
                            gva, test_gva
                        )
                    };

                    println!("translated gva {:x} to {:x}", gva, gpa);

                    Ok(EmuTranslateResult {
                        gpa,
                        overlay_page: Some(false),
                    })
                }
                _ => Err(EmuTranslateError {
                    code: c,
                    event_info: None,
                }),
            },
            MockSupportMode::TestOverlay => Ok(EmuTranslateResult {
                gpa: gva,
                overlay_page: Some(true),
            }),
        }
    }

    fn physical_address(&self) -> Option<u64> {
        todo!()
    }

    /// The gva translation included in the intercept message header, if valid.
    fn initial_gva_translation(&mut self) -> Option<InitialTranslation> {
        Some(InitialTranslation {
            gva: INITIAL_GVA,
            gpa: INITIAL_GPA,
            translate_mode: TranslateMode::Read,
        })
    }

    fn interruption_pending(&self) -> bool {
        false
    }

    /// Checks that the event injected corresponds to the expected one
    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent) {
        self.injected_event = Some(event_info);
        let exception_event = hvdef::HvX64PendingExceptionEvent::from(u128::from(event_info.reg_0));
        println!(
            "injected event pending {}, type {}, vector {}, deliver {}, error {:x}, param {:x}",
            exception_event.event_pending(),
            exception_event.event_type(),
            exception_event.vector(),
            exception_event.deliver_error_code(),
            exception_event.error_code(),
            exception_event.exception_parameter()
        );
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
async fn basic_translate_gva() {
    let gm = GuestMemory::allocate(4096);
    let emu_mem = EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };

    const GVA: u64 = 0xbadc0ffee0ddf00d;
    const GPA: u64 = 0xFF;
    const TEST_VALUE: u64 = 0x1234;
    gm.write_at(GPA, TEST_VALUE.as_bytes()).unwrap();

    let mut support = MockSupport::new(
        MockSupportMode::Code(TranslateGvaResultCode::SUCCESS),
        MockSupportTranslation {
            access_gva: GVA,
            translate_to_gpa: GPA,
            access_info: MockAccess::Read,
        },
        false,
        &gm,
    );

    emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();

    assert_eq!(support.accessed_value(), TEST_VALUE);
    assert!(
        support.injected_event().is_none(),
        "should not have injected an event for a successful translation"
    );
}

#[async_test]
async fn translate_gva_page_faults() {
    let gm = GuestMemory::allocate(4096);
    let emu_mem = EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };

    let codes = [
        (
            TranslateGvaResultCode::PAGE_NOT_PRESENT,
            x86defs::PageFaultErrorCode::new(),
            false,
        ),
        (
            TranslateGvaResultCode::PRIVILEGE_VIOLATION,
            x86defs::PageFaultErrorCode::new().with_present(true),
            false,
        ),
        (
            TranslateGvaResultCode::PRIVILEGE_VIOLATION,
            x86defs::PageFaultErrorCode::new()
                .with_present(true)
                .with_user(true),
            true,
        ),
        (
            TranslateGvaResultCode::INVALID_PAGE_TABLE_FLAGS,
            x86defs::PageFaultErrorCode::new()
                .with_present(true)
                .with_reserved(true),
            false,
        ),
    ];

    const GVA: u64 = 0xbadc0ffee0ddf00d;
    const GPA: u64 = 0xFF;

    for (c, event_error_code, cpu_flags) in codes.iter() {
        let mut support = MockSupport::new(
            MockSupportMode::Code(*c),
            MockSupportTranslation {
                access_gva: GVA,
                translate_to_gpa: GPA,
                access_info: MockAccess::Read,
            },
            *cpu_flags,
            &gm,
        );

        assert!(
            emulate(&mut support, &emu_mem, &MockCpu).await.is_ok(),
            "emulation failed for error code {:?}",
            c
        );

        let injected_event = hvdef::HvX64PendingExceptionEvent::from(u128::from(
            support.injected_event().unwrap().reg_0,
        ));

        assert!(
            injected_event.event_pending(),
            "expected event pending to be true for error code {:?}",
            c
        );

        assert_eq!(
            injected_event.event_type(),
            hvdef::HV_X64_PENDING_EVENT_EXCEPTION,
            "expected pending event exception type for {:?}",
            c
        );

        assert_eq!(
            injected_event.vector(),
            x86defs::Exception::PAGE_FAULT.0.into(),
            "expected page fault vector type for {:?}",
            c
        );

        assert!(
            injected_event.deliver_error_code(),
            "deliver error code should be true for {:?}",
            c
        );

        assert_eq!(
            injected_event.error_code(),
            u32::from(*event_error_code),
            "exception error code does not match for {:?}",
            c
        );

        assert_eq!(
            injected_event.exception_parameter(),
            GVA,
            "exception parameter has incorrect gva {:?}",
            c
        );
    }
}

#[async_test]
async fn translate_gva_protection_faults() {
    let gm = GuestMemory::allocate(4096);
    let emu_mem = EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };

    let codes = [
        TranslateGvaResultCode::GPA_NO_READ_ACCESS,
        TranslateGvaResultCode::GPA_NO_WRITE_ACCESS,
        TranslateGvaResultCode::GPA_UNMAPPED,
        TranslateGvaResultCode::GPA_ILLEGAL_OVERLAY_ACCESS,
        TranslateGvaResultCode::GPA_UNACCEPTED,
    ];

    const GVA: u64 = 0xbadc0ffee0ddf00d;
    const GPA: u64 = 0xFF;

    for c in codes.iter() {
        let mut support = MockSupport::new(
            MockSupportMode::Code(*c),
            MockSupportTranslation {
                access_gva: GVA,
                translate_to_gpa: GPA,
                access_info: MockAccess::Read,
            },
            false,
            &gm,
        );

        assert!(
            emulate(&mut support, &emu_mem, &MockCpu).await.is_ok(),
            "emulation failed for error code {:?}",
            c
        );

        assert!(
            support.injected_event().is_some(),
            "gpf not injected for {:?}",
            c
        );

        validate_gpf_event(support.injected_event().unwrap());
    }
}

#[async_test]
async fn translate_gva_intercept() {
    let gm = GuestMemory::allocate(4096);
    let emu_mem = EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };

    const GVA: u64 = 0xbadc0ffee0ddf00d;
    const GPA: u64 = 0xFF;

    let mut support = MockSupport::new(
        MockSupportMode::Code(TranslateGvaResultCode::INTERCEPT),
        MockSupportTranslation {
            access_gva: GVA,
            translate_to_gpa: GPA,
            access_info: MockAccess::Read,
        },
        false,
        &gm,
    );

    emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();

    let injected_event = support.injected_event().unwrap();

    assert!(
        injected_event.as_bytes() == MockSupport::intercept_event().as_bytes(),
        "intercept failure code should just pass through the event from the hypervisor"
    );
}

#[async_test]
async fn initial_gva_translation() {
    let gm = GuestMemory::allocate(2 * 4096);
    let emu_mem = EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };

    const INITIAL_GPA_VALUE: u64 = 0x1234;
    const DECOY_VALUE: u64 = 0xabcd;
    const DECOY_GPA: u64 = 0;

    gm.write_at(INITIAL_GPA, INITIAL_GPA_VALUE.as_bytes())
        .unwrap();
    gm.write_at(DECOY_GPA, DECOY_VALUE.as_bytes()).unwrap();

    let mut support = MockSupport::new(
        MockSupportMode::Code(TranslateGvaResultCode::PAGE_NOT_PRESENT),
        MockSupportTranslation {
            access_gva: INITIAL_GVA,
            translate_to_gpa: DECOY_GPA,
            access_info: MockAccess::Read,
        },
        false,
        &gm,
    );

    emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();
    assert_eq!(support.accessed_value(), INITIAL_GPA_VALUE);
    assert!(
        support.injected_event().is_none(),
        "should not have injected an event for a successful translation"
    );

    const OFFSET_VALUE: u64 = 0x12ab;
    const OFFSET: u64 = 0xFF;

    let mut support = MockSupport::new(
        MockSupportMode::Code(TranslateGvaResultCode::PAGE_NOT_PRESENT),
        MockSupportTranslation {
            access_gva: INITIAL_GVA + OFFSET,
            translate_to_gpa: DECOY_GPA, // Should be somewhere different from the initial gpa to be sure the initial gpa is used
            access_info: MockAccess::Read,
        },
        false,
        &gm,
    );

    gm.write_at(INITIAL_GPA + OFFSET, OFFSET_VALUE.as_bytes())
        .unwrap();

    emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();
    assert_eq!(support.accessed_value(), OFFSET_VALUE);
    assert!(
        support.injected_event().is_none(),
        "should not have injected an event for a successful translation"
    );
}

#[async_test]
async fn initial_gva_translation_misses() {
    let gm = GuestMemory::allocate(2 * 4096);
    let emu_mem = EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };

    const DECOY_VALUE: u64 = 0xabcd;
    const CORRECT_VALUE: u64 = 0x1234;

    // The cache has stored a translation for read (via the initial
    // translation), so check that this will be ignored and that
    // the value will be written at the newly provided translation
    // instead
    println!("Testing that write permissions results in a non-initial translation");
    gm.write_at(INITIAL_GPA, DECOY_VALUE.as_bytes()).unwrap();

    const MISS_GPA: u64 = 0x10;

    let mut support = MockSupport::new(
        MockSupportMode::Code(TranslateGvaResultCode::SUCCESS),
        MockSupportTranslation {
            access_gva: INITIAL_GVA,
            translate_to_gpa: MISS_GPA,
            access_info: MockAccess::Write(CORRECT_VALUE),
        },
        false,
        &gm,
    );

    emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();
    assert!(support.injected_event().is_none());

    let mut mem_val = [0; 8];
    gm.read_at(MISS_GPA, &mut mem_val).unwrap();

    assert_eq!(&mem_val, CORRECT_VALUE.as_bytes());

    // The cache has stored a translation for read (via the initial
    // translation), so check that this will be ignored and that
    // the instructions at the newly provided translation will
    // be observed instead
    println!("Testing that execute permissions results in a non-initial translation");
    let mut support = MockSupport::new(
        MockSupportMode::Code(TranslateGvaResultCode::SUCCESS),
        MockSupportTranslation {
            access_gva: INITIAL_GVA,
            translate_to_gpa: MISS_GPA,
            access_info: MockAccess::Execute(CORRECT_VALUE),
        },
        false,
        &gm,
    );

    let mut asm = CodeAssembler::new(64).unwrap();
    support.set_gp(Gp::R9, DECOY_VALUE);
    asm.mov(rsi, r9).unwrap();
    let instruction_bytes = asm.assemble(support.state.rip).unwrap();
    gm.write_at(INITIAL_GPA, &instruction_bytes).unwrap();

    emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();
    assert!(support.injected_event().is_none());
    assert_eq!(support.accessed_value(), CORRECT_VALUE);
}

#[async_test]
async fn translate_gva_overlay_page() {
    let gm = GuestMemory::allocate(4096);
    let emu_mem = EmulatorMemoryAccess {
        gm: &gm,
        kx_gm: &gm,
        ux_gm: &gm,
    };

    const GVA: u64 = 0xbadc0ffee0ddf00d;
    const GPA: u64 = 0xFF;
    const WRITE_VAL: u64 = 0x1234;

    let mut support = MockSupport::new(
        MockSupportMode::TestOverlay,
        MockSupportTranslation {
            access_gva: GVA,
            translate_to_gpa: GPA,
            access_info: MockAccess::Write(WRITE_VAL),
        },
        false,
        &gm,
    );

    assert!(emulate(&mut support, &emu_mem, &MockCpu).await.is_ok());

    assert!(
        support.injected_event().is_some(),
        "gpf not injected for write to overlay page"
    );

    validate_gpf_event(support.injected_event().unwrap());
}
