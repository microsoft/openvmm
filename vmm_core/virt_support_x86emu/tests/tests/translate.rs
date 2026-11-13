// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::tests::common::*;
use guestmem::GuestMemory;
use hvdef::hypercall::TranslateGvaResultCode;
use iced_x86::code_asm::*;
use pal_async::async_test;
use test_with_tracing::test;
use virt_support_x86emu::emulate::*;
use vm_topology::processor::VpIndex;
use x86defs::RFlags;
use x86defs::cpuid::Vendor;
use x86emu::Gp;
use x86emu::Segment;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

const INITIAL_GVA: u64 = 0x2000;
const INITIAL_GPA: u64 = 0x1000;

/// How MockSupport should handle calls to [`EmulatorSupport::translate_gva`]
enum MockSupportMode {
    /// [`translate_gva`] should succeed/fail with the given code
    Code(TranslateGvaResultCode),
    /// ['translate_gva'] should return that the translation is an overlay page
    TestOverlay,
    WriteFault,
    /// Translate the first page, but use the given result for the second page.
    CrossPage {
        code: TranslateGvaResultCode,
        second_gpa: u64,
        mapped: bool,
        deny_vtl: bool,
    },
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
    fn cr4(&mut self) -> u64 {
        self.state.cr4
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
        gpa: u64,
        mode: TranslateMode,
    ) -> Result<(), EmuCheckVtlAccessError> {
        if matches!(self.mode, MockSupportMode::CrossPage { deny_vtl: true, .. }) && gpa == 0x3000 {
            return Err(EmuCheckVtlAccessError::AccessDenied {
                vtl: hvdef::Vtl::Vtl1,
                denied_flags: hvdef::HvMapGpaFlags::new()
                    .with_readable(mode == TranslateMode::Read)
                    .with_writable(mode == TranslateMode::Write),
            });
        }
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
            MockSupportMode::WriteFault => {
                if mode == TranslateMode::Write {
                    Err(EmuTranslateError {
                        code: TranslateGvaResultCode::PRIVILEGE_VIOLATION,
                        event_info: None,
                    })
                } else {
                    Ok(EmuTranslateResult {
                        gpa: self.test_translation.translate_to_gpa,
                        overlay_page: None,
                    })
                }
            }
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
            MockSupportMode::CrossPage {
                code, second_gpa, ..
            } => {
                let first_page = self.test_translation.access_gva & !(hvdef::HV_PAGE_SIZE - 1);
                if gva >= first_page + hvdef::HV_PAGE_SIZE {
                    if code != TranslateGvaResultCode::SUCCESS {
                        return Err(EmuTranslateError {
                            code,
                            event_info: None,
                        });
                    }
                    Ok(EmuTranslateResult {
                        gpa: second_gpa + (gva & (hvdef::HV_PAGE_SIZE - 1)),
                        overlay_page: None,
                    })
                } else {
                    Ok(EmuTranslateResult {
                        gpa: self.test_translation.translate_to_gpa,
                        overlay_page: None,
                    })
                }
            }
        }
    }

    fn physical_address(&self) -> Option<u64> {
        Some(self.test_translation.translate_to_gpa)
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
        match self.mode {
            MockSupportMode::CrossPage { mapped, .. } => mapped,
            _ => true,
        }
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
async fn cross_page_memory_fault() {
    const VALUE: u64 = 0x123456789abcdef0;
    const GVA: u64 = 0x4ffd;
    const GPA: u64 = 0xffd;

    for (write, locked, mapped) in [
        (true, false, true),
        (true, true, true),
        (true, false, false),
        (false, false, true),
        (false, false, false),
    ] {
        for user_mode in [false, true] {
            for (code, error_code) in [
                (
                    TranslateGvaResultCode::PAGE_NOT_PRESENT,
                    x86defs::PageFaultErrorCode::new(),
                ),
                (
                    TranslateGvaResultCode::PRIVILEGE_VIOLATION,
                    x86defs::PageFaultErrorCode::new().with_present(true),
                ),
                (
                    TranslateGvaResultCode::INVALID_PAGE_TABLE_FLAGS,
                    x86defs::PageFaultErrorCode::new()
                        .with_present(true)
                        .with_reserved(true),
                ),
            ] {
                let gm = GuestMemory::allocate(4 * 4096);
                let before = vec![0xa5; 4 * 4096];
                gm.write_at(0, &before).unwrap();
                let mut support = MockSupport::new(
                    MockSupportMode::CrossPage {
                        code,
                        second_gpa: 0x3000,
                        mapped,
                        deny_vtl: false,
                    },
                    MockSupportTranslation {
                        access_gva: GVA,
                        translate_to_gpa: GPA,
                        access_info: if write {
                            MockAccess::Write(VALUE)
                        } else {
                            MockAccess::Read
                        },
                    },
                    user_mode,
                    &gm,
                );
                if locked {
                    let mut asm = CodeAssembler::new(64).unwrap();
                    asm.xchg(qword_ptr(rax), rsi).unwrap();
                    support.instruction_bytes = asm.assemble(0).unwrap();
                }
                let rip = support.state.rip;
                let registers = support.state.gps;
                let rflags = support.state.rflags;
                let emu_mem = EmulatorMemoryAccess {
                    gm: &gm,
                    kx_gm: &gm,
                    ux_gm: &gm,
                };

                emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();

                let mut after = vec![0; before.len()];
                gm.read_at(0, &mut after).unwrap();
                assert!(after == before, "faulting write modified memory");
                assert_eq!(support.state.rip, rip);
                assert_eq!(support.state.gps, registers);
                assert_eq!(support.state.rflags, rflags);
                let event = hvdef::HvX64PendingExceptionEvent::from(u128::from(
                    support.injected_event().expect("expected page fault").reg_0,
                ));
                assert!(event.event_pending());
                assert_eq!(event.event_type(), hvdef::HV_X64_PENDING_EVENT_EXCEPTION);
                assert_eq!(event.vector(), x86defs::Exception::PAGE_FAULT.0.into());
                assert!(event.deliver_error_code());
                assert_eq!(
                    event.error_code(),
                    u32::from(error_code.with_write(write).with_user(user_mode)),
                );
                assert_eq!(event.exception_parameter(), 0x5000);
            }
        }
    }
}

#[async_test]
async fn cross_page_memory_noncontiguous() {
    const VALUE: u64 = 0x123456789abcdef0;
    for write in [false, true] {
        for bytes_in_first_page in 1..=8 {
            let gm = GuestMemory::allocate(4 * 4096);
            let mut expected = vec![0xa5; 4 * 4096];
            gm.write_at(0, &expected).unwrap();
            let gpa = 0x1000 - bytes_in_first_page;
            let bytes = VALUE.to_le_bytes();
            expected[gpa..0x1000].copy_from_slice(&bytes[..bytes_in_first_page]);
            expected[0x3000..0x3000 + 8 - bytes_in_first_page]
                .copy_from_slice(&bytes[bytes_in_first_page..]);
            if !write {
                gm.write_at(0, &expected).unwrap();
            }
            let mut support = MockSupport::new(
                MockSupportMode::CrossPage {
                    code: TranslateGvaResultCode::SUCCESS,
                    second_gpa: 0x3000,
                    mapped: true,
                    deny_vtl: false,
                },
                MockSupportTranslation {
                    access_gva: 0x5000 - bytes_in_first_page as u64,
                    translate_to_gpa: gpa as u64,
                    access_info: if write {
                        MockAccess::Write(VALUE)
                    } else {
                        MockAccess::Read
                    },
                },
                false,
                &gm,
            );
            let instruction_len = support.instruction_bytes.len();
            let rip = support.state.rip;
            let emu_mem = EmulatorMemoryAccess {
                gm: &gm,
                kx_gm: &gm,
                ux_gm: &gm,
            };

            emulate(&mut support, &emu_mem, &MockCpu).await.unwrap();

            if !write {
                assert_eq!(support.accessed_value(), VALUE);
            }
            let mut actual = vec![0; expected.len()];
            gm.read_at(0, &mut actual).unwrap();
            assert!(actual == expected, "write did not follow page translations");
            assert_eq!(support.state.rip, rip + instruction_len as u64);
            assert!(support.injected_event().is_none());
        }
    }
}

#[async_test]
async fn locked_cross_page_memory() {
    const VALUE: u64 = 0x123456789abcdef0;
    for second_gpa in [0x1000, 0x3000] {
        for bytes_in_first_page in 1..=8 {
            let gm = GuestMemory::allocate(0x4000);
            let mut expected = vec![0xa5; 0x4000];
            gm.write_at(0, &expected).unwrap();
            let gpa = 0x1000 - bytes_in_first_page;
            let mut support = MockSupport::new(
                MockSupportMode::CrossPage {
                    code: TranslateGvaResultCode::SUCCESS,
                    second_gpa: second_gpa as u64,
                    mapped: true,
                    deny_vtl: false,
                },
                MockSupportTranslation {
                    access_gva: 0x5000 - bytes_in_first_page as u64,
                    translate_to_gpa: gpa as u64,
                    access_info: MockAccess::Write(VALUE),
                },
                false,
                &gm,
            );
            let mut asm = CodeAssembler::new(64).unwrap();
            asm.xchg(qword_ptr(rax), rsi).unwrap();
            support.instruction_bytes = asm.assemble(0).unwrap();
            let mut registers = support.state.gps;
            let flags = support.state.rflags;
            let rip = support.state.rip;
            let result = emulate(
                &mut support,
                &EmulatorMemoryAccess {
                    gm: &gm,
                    kx_gm: &gm,
                    ux_gm: &gm,
                },
                &MockCpu,
            )
            .await;

            if bytes_in_first_page < 8 {
                assert!(matches!(
                    result,
                    Err(virt::VpHaltReason::TripleFault { .. })
                ));
                assert_eq!(support.state.rip, rip);
            } else {
                result.unwrap();
                let bytes = VALUE.to_le_bytes();
                expected[gpa..0x1000].copy_from_slice(&bytes[..bytes_in_first_page]);
                expected[second_gpa..second_gpa + 8 - bytes_in_first_page]
                    .copy_from_slice(&bytes[bytes_in_first_page..]);
                registers[Gp::RSI as usize] = u64::from_le_bytes([0xa5; 8]);
                assert_eq!(
                    support.state.rip,
                    rip + support.instruction_bytes.len() as u64
                );
            }
            assert_eq!(support.state.gps, registers);
            assert_eq!(support.state.rflags, flags);
            assert!(support.injected_event().is_none());
            let mut actual = vec![0; expected.len()];
            gm.read_at(0, &mut actual).unwrap();
            assert!(actual == expected, "locked write modified the wrong memory");
        }
    }
}

#[async_test]
async fn locked_ram_alignment() {
    const VALUE: u64 = 0x123456789abcdef0;
    for size in [1, 2, 4, 8] {
        for offset in 0..size {
            let gpa = 0x400 + offset;
            let gm = GuestMemory::allocate(0x1000);
            let mut expected = vec![0xa5; 0x1000];
            gm.write_at(0, &expected).unwrap();
            let mut support = MockSupport::new(
                MockSupportMode::Code(TranslateGvaResultCode::SUCCESS),
                MockSupportTranslation {
                    access_gva: gpa as u64,
                    translate_to_gpa: gpa as u64,
                    access_info: MockAccess::Write(VALUE),
                },
                false,
                &gm,
            );
            let mut asm = CodeAssembler::new(64).unwrap();
            match size {
                1 => asm.xchg(byte_ptr(rax), sil),
                2 => asm.xchg(word_ptr(rax), si),
                4 => asm.xchg(dword_ptr(rax), esi),
                8 => asm.xchg(qword_ptr(rax), rsi),
                _ => unreachable!(),
            }
            .unwrap();
            support.instruction_bytes = asm.assemble(0).unwrap();
            let mut registers = support.state.gps;
            let flags = support.state.rflags;
            let result = emulate(
                &mut support,
                &EmulatorMemoryAccess {
                    gm: &gm,
                    kx_gm: &gm,
                    ux_gm: &gm,
                },
                &MockCpu,
            )
            .await;

            if offset != 0 {
                assert!(matches!(
                    result,
                    Err(virt::VpHaltReason::TripleFault { .. })
                ));
                assert_eq!(support.state.rip, 0);
            } else {
                result.unwrap();
                expected[gpa..gpa + size].copy_from_slice(&VALUE.to_le_bytes()[..size]);
                registers[Gp::RSI as usize] = match size {
                    1 => 0x123456789abcdea5,
                    2 => 0x123456789abca5a5,
                    4 => 0xa5a5a5a5,
                    8 => 0xa5a5a5a5a5a5a5a5,
                    _ => unreachable!(),
                };
                assert_eq!(support.state.rip, support.instruction_bytes.len() as u64);
            }
            assert_eq!(support.state.gps, registers);
            assert_eq!(support.state.rflags, flags);
            assert!(support.injected_event().is_none());
            let mut actual = vec![0; expected.len()];
            gm.read_at(0, &mut actual).unwrap();
            assert!(actual == expected, "locked write modified the wrong memory");
        }
    }
}

#[async_test]
async fn second_page_vtl_fault_address() {
    for write in [false, true] {
        let gm = GuestMemory::allocate(0x4000);
        let mut support = MockSupport::new(
            MockSupportMode::CrossPage {
                code: TranslateGvaResultCode::SUCCESS,
                second_gpa: 0x3000,
                mapped: true,
                deny_vtl: true,
            },
            MockSupportTranslation {
                access_gva: 0x4ffd,
                translate_to_gpa: 0xffd,
                access_info: if write {
                    MockAccess::Write(0x1234)
                } else {
                    MockAccess::Read
                },
            },
            false,
            &gm,
        );
        emulate(
            &mut support,
            &EmulatorMemoryAccess {
                gm: &gm,
                kx_gm: &gm,
                ux_gm: &gm,
            },
            &MockCpu,
        )
        .await
        .unwrap();
        let pending = support.injected_event().unwrap();
        let event =
            hvdef::HvX64PendingEventMemoryIntercept::read_from_bytes(pending.as_bytes()).unwrap();
        assert_eq!(event.guest_linear_address, 0x5000);
        assert_eq!(event.guest_physical_address, 0x3000);
        assert_eq!(
            event.access_type,
            if write {
                hvdef::HvInterceptAccessType::WRITE
            } else {
                hvdef::HvInterceptAccessType::READ
            }
        );
        assert_eq!(support.state.rip, 0);
        let mut memory = [0; 8];
        gm.read_at(0xffd, &mut memory).unwrap();
        assert_eq!(memory, [0; 8]);
    }
}

#[async_test]
async fn cmpxchg16b_ram() {
    const OLD: u64 = u64::from_le_bytes([0xa5; 8]);
    const NEW: u64 = 0x123456789abcdef0;
    for matches in [false, true] {
        for locked in [false, true] {
            let gm = GuestMemory::allocate(0x1000);
            gm.fill_at(0x400, 0xa5, 16).unwrap();
            let mut support = MockSupport::new(
                MockSupportMode::Code(TranslateGvaResultCode::SUCCESS),
                MockSupportTranslation {
                    access_gva: 0x400,
                    translate_to_gpa: 0x400,
                    access_info: MockAccess::Read,
                },
                false,
                &gm,
            );
            support.state.gps[Gp::RDI as usize] = 0x400;
            support.state.gps[Gp::RAX as usize] = if matches { OLD } else { 0 };
            support.state.gps[Gp::RDX as usize] = if matches { OLD } else { 0 };
            support.state.gps[Gp::RBX as usize] = NEW;
            support.state.gps[Gp::RCX as usize] = NEW;
            let mut asm = CodeAssembler::new(64).unwrap();
            let assembler = if locked { asm.lock() } else { &mut asm };
            assembler.cmpxchg16b(xmmword_ptr(rdi)).unwrap();
            support.instruction_bytes = asm.assemble(0).unwrap();
            let mut registers = support.state.gps;
            let mut flags = support.state.rflags;
            let rip = support.state.rip;
            let result = emulate(
                &mut support,
                &EmulatorMemoryAccess {
                    gm: &gm,
                    kx_gm: &gm,
                    ux_gm: &gm,
                },
                &MockCpu,
            )
            .await;

            let mut expected = [0xa5; 16];
            if locked {
                assert!(matches!(
                    result,
                    Err(virt::VpHaltReason::TripleFault { .. })
                ));
                assert_eq!(support.state.rip, rip);
            } else {
                result.unwrap();
                if matches {
                    expected[..8].copy_from_slice(&NEW.to_le_bytes());
                    expected[8..].copy_from_slice(&NEW.to_le_bytes());
                } else {
                    registers[Gp::RAX as usize] = OLD;
                    registers[Gp::RDX as usize] = OLD;
                }
                flags.set_zero(matches);
                assert_eq!(
                    support.state.rip,
                    rip + support.instruction_bytes.len() as u64
                );
            }
            assert_eq!(support.state.gps, registers);
            assert_eq!(support.state.rflags, flags);
            assert!(support.injected_event().is_none());
            let mut actual = [0; 16];
            gm.read_at(0x400, &mut actual).unwrap();
            assert_eq!(actual, expected);
        }
    }
}

#[async_test]
async fn cmpxchg_mismatch_requires_write_access() {
    for (size, wide) in [
        (1, false),
        (2, false),
        (4, false),
        (8, false),
        (8, true),
        (16, true),
    ] {
        for locked in [false, true] {
            let gm = GuestMemory::allocate(0x1000);
            gm.fill_at(0x400, 0xa5, 16).unwrap();
            let mut support = MockSupport::new(
                MockSupportMode::WriteFault,
                MockSupportTranslation {
                    access_gva: 0x400,
                    translate_to_gpa: 0x400,
                    access_info: MockAccess::Read,
                },
                false,
                &gm,
            );
            support.state.gps[Gp::RDI as usize] = 0x400;
            let mut asm = CodeAssembler::new(64).unwrap();
            let assembler = if locked { asm.lock() } else { &mut asm };
            match (size, wide) {
                (1, false) => assembler.cmpxchg(byte_ptr(rdi), cl),
                (2, false) => assembler.cmpxchg(word_ptr(rdi), cx),
                (4, false) => assembler.cmpxchg(dword_ptr(rdi), ecx),
                (8, false) => assembler.cmpxchg(qword_ptr(rdi), rcx),
                (8, true) => assembler.cmpxchg8b(qword_ptr(rdi)),
                (16, true) => assembler.cmpxchg16b(xmmword_ptr(rdi)),
                _ => unreachable!(),
            }
            .unwrap();
            support.instruction_bytes = asm.assemble(0).unwrap();
            let gps = support.state.gps;
            let flags = support.state.rflags;
            emulate(
                &mut support,
                &EmulatorMemoryAccess {
                    gm: &gm,
                    kx_gm: &gm,
                    ux_gm: &gm,
                },
                &MockCpu,
            )
            .await
            .unwrap();
            let event = hvdef::HvX64PendingExceptionEvent::from(u128::from(
                support.injected_event().unwrap().reg_0,
            ));
            assert_eq!(event.vector(), x86defs::Exception::PAGE_FAULT.0.into());
            assert_eq!(
                event.error_code(),
                u32::from(
                    x86defs::PageFaultErrorCode::new()
                        .with_present(true)
                        .with_write(true)
                )
            );
            assert_eq!(event.exception_parameter(), 0x400);
            assert_eq!(support.state.gps, gps);
            assert_eq!(support.state.rflags, flags);
            assert_eq!(support.state.rip, 0);
            let mut bytes = [0; 16];
            gm.read_at(0x400, &mut bytes).unwrap();
            assert_eq!(bytes, [0xa5; 16]);
        }
    }
}

#[async_test]
async fn port_input_preserves_unwritten_register_bits() {
    for (len, expected) in [
        (1, 0x1234_5678_9abc_de78),
        (2, 0x1234_5678_9abc_5678),
        (4, 0x1234_5678),
    ] {
        let mut value = 0x1234_5678_9abc_def0;
        emulate_io(VpIndex::BSP, false, 0x70, &mut value, len, &MockCpu).await;
        assert_eq!(value, expected);
    }
}

#[async_test]
async fn cmov_false_still_faults_on_source() {
    let gm = GuestMemory::allocate(0x1000);
    let mut support = MockSupport::new(
        MockSupportMode::Code(TranslateGvaResultCode::PAGE_NOT_PRESENT),
        MockSupportTranslation {
            access_gva: 0x400,
            translate_to_gpa: 0x400,
            access_info: MockAccess::Read,
        },
        false,
        &gm,
    );
    support.state.rflags.set_zero(false);
    let mut asm = CodeAssembler::new(64).unwrap();
    asm.cmove(esi, dword_ptr(rax)).unwrap();
    support.instruction_bytes = asm.assemble(0).unwrap();
    let gps = support.state.gps;
    emulate(
        &mut support,
        &EmulatorMemoryAccess {
            gm: &gm,
            kx_gm: &gm,
            ux_gm: &gm,
        },
        &MockCpu,
    )
    .await
    .unwrap();
    let event = hvdef::HvX64PendingExceptionEvent::from(u128::from(
        support.injected_event().unwrap().reg_0,
    ));
    assert_eq!(event.vector(), x86defs::Exception::PAGE_FAULT.0.into());
    assert_eq!(event.error_code(), 0);
    assert_eq!(event.exception_parameter(), 0x400);
    assert_eq!(support.state.gps, gps);
    assert_eq!(support.state.rip, 0);
}

#[async_test]
async fn narrow_register_loads_preserve_other_bits() {
    const INITIAL: u64 = 0x1234_5678_9abc_def0;
    for (size, expected) in [
        (1, 0x1234_5678_9abc_deaa),
        (2, 0x1234_5678_9abc_aaf0),
        (3, 0x1234_5678_9abc_bbaa),
        (4, 0xddcc_bbaa),
    ] {
        let gm = GuestMemory::allocate(0x1000);
        gm.write_at(0x400, &[0xaa, 0xbb, 0xcc, 0xdd]).unwrap();
        let mut support = MockSupport::new(
            MockSupportMode::Code(TranslateGvaResultCode::SUCCESS),
            MockSupportTranslation {
                access_gva: 0x400,
                translate_to_gpa: 0x400,
                access_info: MockAccess::Read,
            },
            false,
            &gm,
        );
        support.state.gps[Gp::RCX as usize] = 0x400;
        support.state.gps[Gp::RAX as usize] = INITIAL;
        let mut asm = CodeAssembler::new(64).unwrap();
        match size {
            1 => asm.mov(al, byte_ptr(rcx)),
            2 => asm.mov(ah, byte_ptr(rcx)),
            3 => asm.mov(ax, word_ptr(rcx)),
            4 => asm.mov(eax, dword_ptr(rcx)),
            _ => unreachable!(),
        }
        .unwrap();
        support.instruction_bytes = asm.assemble(0).unwrap();
        emulate(
            &mut support,
            &EmulatorMemoryAccess {
                gm: &gm,
                kx_gm: &gm,
                ux_gm: &gm,
            },
            &MockCpu,
        )
        .await
        .unwrap();
        assert_eq!(support.state.gps[Gp::RAX as usize], expected);
        assert!(support.injected_event().is_none());
    }
}

#[async_test]
async fn canonical_address_faults_and_la57() {
    for la57 in [false, true] {
        for stack in [false, true] {
            for write in [false, true] {
                for (gva, valid) in [
                    (0x1234, true),
                    (0x0000_8000_0000_1234, la57),
                    (0x0100_0000_0000_1234, false),
                    (0xffff_8000_0000_1234, true),
                ] {
                    let gm = GuestMemory::allocate(0x1000);
                    let mut support = MockSupport::new(
                        MockSupportMode::Code(TranslateGvaResultCode::SUCCESS),
                        MockSupportTranslation {
                            access_gva: gva,
                            translate_to_gpa: 0x234,
                            access_info: if write {
                                MockAccess::Write(42)
                            } else {
                                MockAccess::Read
                            },
                        },
                        false,
                        &gm,
                    );
                    support.state.cr4 = if la57 { x86defs::X64_CR4_LA57 } else { 0 };
                    if !valid {
                        support.state.cr0 |= x86defs::X64_CR0_AM;
                        support.state.rflags.set_alignment_check(true);
                        support.state.segs[Segment::SS as usize]
                            .attributes
                            .set_descriptor_privilege_level(3);
                    }
                    support.state.gps[Gp::RBP as usize] = gva;
                    let mut asm = CodeAssembler::new(64).unwrap();
                    let memory = if stack {
                        qword_ptr(rbp)
                    } else {
                        qword_ptr(rax)
                    };
                    if write {
                        asm.mov(memory, rsi).unwrap();
                    } else {
                        asm.mov(rsi, memory).unwrap();
                    }
                    support.instruction_bytes = asm.assemble(0).unwrap();
                    emulate(
                        &mut support,
                        &EmulatorMemoryAccess {
                            gm: &gm,
                            kx_gm: &gm,
                            ux_gm: &gm,
                        },
                        &MockCpu,
                    )
                    .await
                    .unwrap();
                    if valid {
                        assert!(support.injected_event().is_none());
                    } else {
                        let event = hvdef::HvX64PendingExceptionEvent::from(u128::from(
                            support.injected_event().unwrap().reg_0,
                        ));
                        assert_eq!(
                            event.vector(),
                            if stack {
                                x86defs::Exception::STACK_SEGMENT_FAULT.0.into()
                            } else {
                                x86defs::Exception::GENERAL_PROTECTION_FAULT.0.into()
                            }
                        );
                        assert!(event.deliver_error_code());
                        assert_eq!(event.error_code(), 0);
                        assert_eq!(support.state.rip, 0);
                    }
                }
            }
        }
    }
}

#[async_test]
async fn long_mode_segment_overrides() {
    for (base, register, stack) in [
        (rax, Gp::RAX, false),
        (rbp, Gp::RBP, true),
        (rsp, Gp::RSP, true),
    ] {
        for prefix in [
            None,
            Some(Segment::CS),
            Some(Segment::DS),
            Some(Segment::ES),
            Some(Segment::SS),
            Some(Segment::FS),
            Some(Segment::GS),
        ] {
            for write in [false, true] {
                for (gva, valid) in [(0x1200, true), (0x0000_8000_0000_0200, false)] {
                    let gm = GuestMemory::allocate(0x1000);
                    gm.write_plain(0x200, &0x1234u64).unwrap();
                    let mut support = MockSupport::new(
                        MockSupportMode::Code(TranslateGvaResultCode::SUCCESS),
                        MockSupportTranslation {
                            access_gva: gva,
                            translate_to_gpa: 0x200,
                            access_info: if write {
                                MockAccess::Write(0x5678)
                            } else {
                                MockAccess::Read
                            },
                        },
                        false,
                        &gm,
                    );
                    let fs_gs = matches!(prefix, Some(Segment::FS | Segment::GS));
                    support.state.gps[register as usize] = if fs_gs { gva - 0x1000 } else { gva };
                    support.state.segs[Segment::FS as usize].base = 0x1000;
                    support.state.segs[Segment::GS as usize].base = 0x1000;
                    let memory = qword_ptr(base);
                    let memory = match prefix {
                        None => memory,
                        Some(Segment::CS) => memory.cs(),
                        Some(Segment::DS) => memory.ds(),
                        Some(Segment::ES) => memory.es(),
                        Some(Segment::SS) => memory.ss(),
                        Some(Segment::FS) => memory.fs(),
                        Some(Segment::GS) => memory.gs(),
                    };
                    let mut asm = CodeAssembler::new(64).unwrap();
                    if write {
                        asm.mov(memory, rsi).unwrap();
                    } else {
                        asm.mov(rsi, memory).unwrap();
                    }
                    support.instruction_bytes = asm.assemble(0).unwrap();
                    let initial = support.state.gps;
                    let instruction_len = support.instruction_bytes.len();
                    emulate(
                        &mut support,
                        &EmulatorMemoryAccess {
                            gm: &gm,
                            kx_gm: &gm,
                            ux_gm: &gm,
                        },
                        &MockCpu,
                    )
                    .await
                    .unwrap();
                    if valid {
                        assert!(support.injected_event().is_none());
                        assert_eq!(support.state.rip, instruction_len as u64);
                        assert_eq!(
                            gm.read_plain::<u64>(0x200).unwrap(),
                            if write { 0x5678 } else { 0x1234 }
                        );
                        if !write {
                            assert_eq!(support.accessed_value(), 0x1234);
                        }
                    } else {
                        let event = hvdef::HvX64PendingExceptionEvent::from(u128::from(
                            support.injected_event().unwrap().reg_0,
                        ));
                        assert_eq!(
                            event.vector(),
                            if stack && !fs_gs {
                                x86defs::Exception::STACK_SEGMENT_FAULT.0.into()
                            } else {
                                x86defs::Exception::GENERAL_PROTECTION_FAULT.0.into()
                            },
                            "base {base:?}, prefix {prefix:?}",
                        );
                        assert!(event.deliver_error_code());
                        assert_eq!(event.error_code(), 0);
                        assert_eq!(support.state.rip, 0);
                        assert_eq!(support.state.gps, initial);
                        assert_eq!(gm.read_plain::<u64>(0x200).unwrap(), 0x1234);
                    }
                }
            }
        }
    }
}

#[async_test]
async fn canonical_address_range_checks_the_last_byte() {
    for stack in [false, true] {
        for write in [false, true] {
            let gm = GuestMemory::allocate(0x4000);
            gm.write_at(0xffc, &[0xa5; 8]).unwrap();
            let mut support = MockSupport::new(
                MockSupportMode::Code(TranslateGvaResultCode::SUCCESS),
                MockSupportTranslation {
                    access_gva: 0x0000_7fff_ffff_fffc,
                    translate_to_gpa: 0xffc,
                    access_info: if write {
                        MockAccess::Write(42)
                    } else {
                        MockAccess::Read
                    },
                },
                true,
                &gm,
            );
            support.state.cr0 |= x86defs::X64_CR0_AM;
            support.state.rflags.set_alignment_check(true);
            support.state.gps[Gp::RBP as usize] = 0x0000_7fff_ffff_fffc;
            let mut asm = CodeAssembler::new(64).unwrap();
            let memory = if stack {
                qword_ptr(rbp)
            } else {
                qword_ptr(rax)
            };
            if write {
                asm.mov(memory, rsi).unwrap();
            } else {
                asm.mov(rsi, memory).unwrap();
            }
            support.instruction_bytes = asm.assemble(0).unwrap();
            let gps = support.state.gps;
            emulate(
                &mut support,
                &EmulatorMemoryAccess {
                    gm: &gm,
                    kx_gm: &gm,
                    ux_gm: &gm,
                },
                &MockCpu,
            )
            .await
            .unwrap();
            let event = hvdef::HvX64PendingExceptionEvent::from(u128::from(
                support.injected_event().unwrap().reg_0,
            ));
            assert_eq!(
                event.vector(),
                if stack {
                    x86defs::Exception::STACK_SEGMENT_FAULT.0.into()
                } else {
                    x86defs::Exception::GENERAL_PROTECTION_FAULT.0.into()
                }
            );
            assert!(event.deliver_error_code());
            assert_eq!(event.error_code(), 0);
            assert_eq!(support.state.rip, 0);
            assert_eq!(support.state.gps, gps);
            let mut bytes = [0; 8];
            gm.read_at(0xffc, &mut bytes).unwrap();
            assert_eq!(bytes, [0xa5; 8]);
        }
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

    const GVA: u64 = 0x1234_5678_f00d;
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

    const GVA: u64 = 0x1234_5678_f00d;
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

    const GVA: u64 = 0x1234_5678_f00d;
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

    const GVA: u64 = 0x1234_5678_f00d;
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

    const GVA: u64 = 0x1234_5678_f00d;
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
