// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDX VP context builder.

use super::VpContextBuilder;
use super::VpContextState;
use crate::vp_context_builder::VpContextPageState;
use igvm_defs::PAGE_SIZE_4K;
use loader::importer::SegmentRegister;
use loader::importer::X86Register;
use loader_defs::shim::TdxTrampolineContext;
use std::mem::offset_of;
use x86defs::X64_EFER_LME;
use x86defs::X86X_MSR_DEFAULT_PAT;
use zerocopy::IntoBytes;

/// Represents a hardware context for TDX. This contains both the sets of
/// initial registers and registers set by the trampoline code.
#[derive(Debug)]
pub struct TdxHardwareContext {
    trampoline_context: TdxTrampolineContext,
    accept_lower_1mb: bool,
}

impl TdxHardwareContext {
    pub fn new(accept_lower_1mb: bool) -> Self {
        Self {
            trampoline_context: TdxTrampolineContext::default(),
            accept_lower_1mb,
        }
    }
}

impl VpContextBuilder for TdxHardwareContext {
    type Register = X86Register;

    /// Import a register into the hardware context. Only a subset of registers
    /// are allowed.
    fn import_vp_register(&mut self, register: X86Register) {
        let mut set_data_selector = |reg: SegmentRegister| {
            if self.trampoline_context.data_selector == 0 {
                self.trampoline_context.data_selector = reg.selector;
            } else if self.trampoline_context.data_selector != reg.selector {
                panic!("data selectors must be the same");
            }
        };

        match register {
            X86Register::Gdtr(reg) => {
                self.trampoline_context.gdtr_base = reg.base;
                self.trampoline_context.gdtr_limit = reg.limit;
            }
            X86Register::Idtr(reg) => {
                self.trampoline_context.idtr_base = reg.base;
                self.trampoline_context.idtr_limit = reg.limit;
            }
            X86Register::Ds(reg)
            | X86Register::Es(reg)
            | X86Register::Fs(reg)
            | X86Register::Gs(reg)
            | X86Register::Ss(reg) => set_data_selector(reg),
            X86Register::Cs(reg) => self.trampoline_context.code_selector = reg.selector,
            X86Register::Tr(reg) => {
                self.trampoline_context.task_selector = reg.selector;
            }
            X86Register::Cr0(cr0) => self.trampoline_context.cr0 = cr0,
            X86Register::Cr3(cr3) => {
                let cr3_u32: u32 = cr3.try_into().expect("cr3 must fit in u32");
                self.trampoline_context.transition_cr3 = cr3_u32;
                self.trampoline_context.cr3 = cr3;
            }
            X86Register::Cr4(cr4) => self.trampoline_context.cr4 = cr4,
            X86Register::Efer(efer) => {
                // TDX guests are not permitted to set EFER explicitly.  Verify
                // that the requested EFER value is compatible with the
                // architecturally imposed value.
                if efer & X64_EFER_LME == 0 {
                    panic!("EFER LME must be set for tdx")
                }
            }
            X86Register::Pat(pat) => {
                if pat != X86X_MSR_DEFAULT_PAT {
                    panic!("PAT must be default for tdx")
                }
            }
            X86Register::Rbp(rbp) => self.trampoline_context.rbp = rbp,
            X86Register::Rip(rip) => self.trampoline_context.initial_rip = rip,
            X86Register::Rsi(rsi) => self.trampoline_context.rsi = rsi,
            X86Register::Rsp(rsp) => self.trampoline_context.rsp = rsp,
            X86Register::R8(r8) => self.trampoline_context.r8 = r8,
            X86Register::R9(r9) => self.trampoline_context.r9 = r9,
            X86Register::R10(r10) => self.trampoline_context.r10 = r10,
            X86Register::R11(r11) => self.trampoline_context.r11 = r11,
            X86Register::R12(_) => panic!("r12 not allowed for tdx"),
            X86Register::Rflags(_) => panic!("rflags not allowed for tdx"),

            X86Register::MtrrDefType(_)
            | X86Register::MtrrPhysBase0(_)
            | X86Register::MtrrPhysMask0(_)
            | X86Register::MtrrPhysBase1(_)
            | X86Register::MtrrPhysMask1(_)
            | X86Register::MtrrPhysBase2(_)
            | X86Register::MtrrPhysMask2(_)
            | X86Register::MtrrPhysBase3(_)
            | X86Register::MtrrPhysMask3(_)
            | X86Register::MtrrPhysBase4(_)
            | X86Register::MtrrPhysMask4(_)
            | X86Register::MtrrFix64k00000(_)
            | X86Register::MtrrFix16k80000(_)
            | X86Register::MtrrFix4kE0000(_)
            | X86Register::MtrrFix4kE8000(_)
            | X86Register::MtrrFix4kF0000(_)
            | X86Register::MtrrFix4kF8000(_) => {
                tracing::warn!(?register, "Ignoring MTRR register for TDX.")
            }
        }
    }

    fn set_vp_context_memory(&mut self, _page_base: u64) {
        unimplemented!("not supported for TDX");
    }

    fn finalize(&mut self, state: &mut Vec<VpContextState>) {
        // Construct and load an initial temporary GDT to use for the transition
        // to long mode.  A single selector (0008:) is defined as a 64-bit code
        // segment.
        self.trampoline_context.static_gdt[0x08] = 0xFF;
        self.trampoline_context.static_gdt[0x09] = 0xFF;
        self.trampoline_context.static_gdt[0x0D] = 0x9B;
        self.trampoline_context.static_gdt[0x0E] = 0xA0;

        self.trampoline_context.static_gdt_limit = 0xF;
        self.trampoline_context.static_gdt_base =
            0xFFFFF000 + offset_of!(TdxTrampolineContext, static_gdt) as u32;

        // Generate a 32-bit assembly trampoline to enable long mode and transfer
        // to the specified context.
        let mut byte_offset = 0xFF0;

        // Fill the reset page with INT 3 as a standard code fill value.
        let mut reset_page = vec![0xCCu8; PAGE_SIZE_4K as usize];

        // Copy trampoline_context to the start of the reset page.
        let trampoline_context = self.trampoline_context.as_bytes();
        reset_page[0..trampoline_context.len()].copy_from_slice(trampoline_context);

        let copy_instr =
            |trampoline_page: &mut Vec<u8>, byte_offset, instruction: &[u8]| -> usize {
                trampoline_page[byte_offset..byte_offset + instruction.len()]
                    .copy_from_slice(instruction);
                byte_offset + instruction.len()
            };

        // jmp InitialCode
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0xE9]);
        let mut relative_offset =
            (trampoline_context.len() as u32).wrapping_sub((byte_offset + 4) as u32);
        copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        byte_offset = trampoline_context.len();

        // L0:
        let l0_offset = byte_offset;

        // lgdt, [staticGdt]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x0F, 0x01, 0x15]);
        relative_offset = 0xFFFFF000 + offset_of!(TdxTrampolineContext, static_gdt_limit) as u32;
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // Load the control registers.  CR0 must be last so long mode is properly
        // enabled (the architecture sets LME prior to initial entry), and the CR0
        // load must be followed by a far jump to complete long mode
        // configuration.

        // mov eax, [initialCr4]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x8B, 0x05]);
        relative_offset = 0xFFFFF000 + offset_of!(TdxTrampolineContext, cr4) as u32;
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov cr4, eax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x0F, 0x22, 0xE0]);

        // mov eax, [transitionCr3]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x8B, 0x05]);
        relative_offset = 0xFFFFF000 + offset_of!(TdxTrampolineContext, transition_cr3) as u32;
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov cr3, eax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x0F, 0x22, 0xD8]);

        // mov eax, [initialCr0]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x8B, 0x05]);
        relative_offset = 0xFFFFF000 + offset_of!(TdxTrampolineContext, cr0) as u32;
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov cr0, eax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x0F, 0x22, 0xC0]);

        // jmp far L2
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0xEA]);
        relative_offset = 0xFFFFF000 + byte_offset as u32 + 6;
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x08, 0x00]);

        // L2:

        // Load the 64-bit CR3 now that long mode is active.

        // mov rax, [initialCr3]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x48, 0x8B, 0x05]);
        relative_offset =
            (offset_of!(TdxTrampolineContext, cr3) as u32).wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov cr3, rax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x0F, 0x22, 0xD8]);

        // Load descriptor tables and selectors, except CS which will be loaded in
        // the final jump.  If no GDT is specified, then skip loading all
        // selectors.

        // mov ax, [initialGdtrLimit]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x8B, 0x05]);
        relative_offset = (offset_of!(TdxTrampolineContext, gdtr_limit) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // test ax, ax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x85, 0xC0]);

        // jz L4
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x74]);
        byte_offset += 1;
        let l4_offset = byte_offset as u32;

        // lgdt [initialGdtr]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x0F, 0x01, 0x15]);
        relative_offset = (offset_of!(TdxTrampolineContext, gdtr_limit) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // @@:
        reset_page[l0_offset.wrapping_sub(1)] = (byte_offset.wrapping_sub(l0_offset)) as u8;

        // mov ax, [initialIdtrLimit]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x8B, 0x05]);
        relative_offset = (offset_of!(TdxTrampolineContext, idtr_limit) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // test ax, ax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x85, 0xC0]);

        // jz @f
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x74]);
        byte_offset += 1;
        let jump_offset = byte_offset;

        // lidt [initialIdtr]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x0F, 0x01, 0x1D]);
        relative_offset = (offset_of!(TdxTrampolineContext, idtr_limit) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // @@:
        reset_page[jump_offset.wrapping_sub(1)] = (byte_offset.wrapping_sub(jump_offset)) as u8;

        // mov ax, [dataSelector]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x8B, 0x05]);
        relative_offset = (offset_of!(TdxTrampolineContext, data_selector) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov ss, ax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x8E, 0xD0]);

        // mov ds, ax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x8E, 0xD8]);

        // mov es, ax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x8E, 0xC0]);

        // mov fs, ax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x8E, 0xE0]);

        // mov gs, ax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x8E, 0xE8]);

        // mov ax, [taskSelector]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x8B, 0x05]);
        relative_offset = (offset_of!(TdxTrampolineContext, task_selector) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // test ax, ax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x85, 0xC0]);

        // jz @f
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x74]);
        byte_offset += 1;
        let jump_offset = byte_offset;

        // ltr ax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x0F, 0x00, 0xD8]);

        // @@:
        reset_page[jump_offset.wrapping_sub(1)] = (byte_offset.wrapping_sub(jump_offset)) as u8;

        // L4:
        reset_page[(l4_offset as usize).wrapping_sub(1)] =
            (byte_offset.wrapping_sub(l4_offset as usize)) as u8;

        // test esi, esi
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x85, 0xF6]);

        // Skip the mailbox spinloop if we are on the BSP

        // jz skip_mailbox_for_bsp
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x74]);
        byte_offset += 1;
        let skip_mailbox_for_bsp = byte_offset;

        // Read the APIC_ID of this AP with a TDG.VP.VMCALL hypercall

        // xor eax, eax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x31, 0xC0]);

        // mov ecx, 1c00
        byte_offset = copy_instr(
            &mut reset_page,
            byte_offset,
            &[0xB9, 0x00, 0x1C, 0x00, 0x00],
        );

        // xor r10, r10
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x4D, 0x31, 0xD2]);

        // mov r11d, 01f
        byte_offset = copy_instr(
            &mut reset_page,
            byte_offset,
            &[0x41, 0xBB, 0x1F, 0x00, 0x00, 0x00],
        );

        // mov r12d, 802h
        byte_offset = copy_instr(
            &mut reset_page,
            byte_offset,
            &[0x41, 0xBC, 0x02, 0x08, 0x00, 0x00],
        );

        // tdcall
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x0F, 0x01, 0xCC]);

        // Spin until the kernel requests this AP to continue in the mailbox

        let mailbox_spinloop = byte_offset;
        // mov eax, [mailbox_apic_id]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x8B, 0x05]);
        relative_offset = (offset_of!(TdxTrampolineContext, mailbox_apic_id) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        //cmp r11d, eax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x41, 0x39, 0xC3]);

        // jne mailbox_spinloop
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x75]);
        byte_offset += 1;
        reset_page[byte_offset.wrapping_sub(1)] =
            (mailbox_spinloop.wrapping_sub(byte_offset)) as u8;

        // xor ebx, ebx
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x31, 0xDB]);

        // mov ebx, [mailbox_command]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x8B, 0x1D]);
        relative_offset = (offset_of!(TdxTrampolineContext, mailbox_command) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov edx, 01h
        byte_offset = copy_instr(
            &mut reset_page,
            byte_offset,
            &[0xBA, 0x01, 0x00, 0x00, 0x00],
        );

        // cmp ebx, edx
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x39, 0xD3]);

        // jne mailbox_spinloop
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x75]);
        byte_offset += 1;

        // skip_mailbox_for_bsp:
        reset_page[skip_mailbox_for_bsp.wrapping_sub(1)] =
            (byte_offset.wrapping_sub(skip_mailbox_for_bsp)) as u8;

        // Execute TDG.MEM.PAGE.ACCEPT to accept the low 1 MB of the address
        // space.  This is only required if the start context is in VTL 0, and
        // only on the BSP.
        if self.accept_lower_1mb {
            // test esi, esi
            byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x85, 0xF6]);

            // jnz L3
            byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x75]);
            byte_offset += 1;
            let l3_offset = byte_offset;

            // L2:
            // xor ecx, ecx
            byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x33, 0xC9]);

            // xor edx, edx
            byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x33, 0xD2]);

            // mov edi, 0100000h
            byte_offset = copy_instr(
                &mut reset_page,
                byte_offset,
                &[0xBF, 0x00, 0x00, 0x10, 0x00],
            );

            // L1:
            let jump_offset = byte_offset;

            // mov eax, 06h
            byte_offset = copy_instr(
                &mut reset_page,
                byte_offset,
                &[0xB8, 0x06, 0x00, 0x00, 0x00],
            );

            // tdcall
            byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x0F, 0x01, 0xCC]);

            // test rax, rax
            byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x48, 0x85, 0xC0]);

            // jne BreakPoint
            byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x0F, 0x85]);
            byte_offset += 4;
            let relative_offset = 0xFEF - byte_offset;
            copy_instr(
                &mut reset_page,
                byte_offset.wrapping_sub(4),
                relative_offset.as_bytes(),
            );

            // add ecx, 01000h
            byte_offset = copy_instr(
                &mut reset_page,
                byte_offset,
                &[0x81, 0xC1, 0x00, 0x10, 0x00, 0x00],
            );

            // cmp ecx, edi
            byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x3B, 0xCF]);

            // jb L1
            byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x72]);
            byte_offset += 1;
            reset_page[byte_offset.wrapping_sub(1)] = (jump_offset.wrapping_sub(byte_offset)) as u8;

            // L3:
            reset_page[l3_offset.wrapping_sub(1)] = (byte_offset.wrapping_sub(l3_offset)) as u8;
        }

        // Load entry register state and transfer to the image.

        // test esi, esi
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x85, 0xF6]);

        // jz L7
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x74]);
        byte_offset += 1;
        let l7_offset = byte_offset;

        // xor rax, rax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x48, 0x31, 0xC0]);

        // mov [mailbox_command], ax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x89, 0x05]);
        relative_offset = (offset_of!(TdxTrampolineContext, mailbox_command) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov rax, [mailbox_wakeup_vector]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x48, 0x8b, 0x05]);
        relative_offset = (offset_of!(TdxTrampolineContext, mailbox_wakeup_vector) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov [initialRip], rax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x48, 0x89, 0x05]);
        relative_offset = (offset_of!(TdxTrampolineContext, initial_rip) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // L7:
        reset_page[l7_offset.wrapping_sub(1)] = (byte_offset.wrapping_sub(l7_offset)) as u8;

        // mov rsp, [initialRsp]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x48, 0x8B, 0x25]);
        relative_offset =
            (offset_of!(TdxTrampolineContext, rsp) as u32).wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov rbp, [initialRbp]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x48, 0x8B, 0x2D]);
        relative_offset =
            (offset_of!(TdxTrampolineContext, rbp) as u32).wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov ecx, esi
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x8B, 0xCE]);

        // mov rsi, [initialRsi]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x48, 0x8B, 0x35]);
        relative_offset =
            (offset_of!(TdxTrampolineContext, rsi) as u32).wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov r8, [initialR8]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x4C, 0x8B, 0x05]);
        relative_offset =
            (offset_of!(TdxTrampolineContext, r8) as u32).wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov r9, [initialR9]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x4C, 0x8B, 0x0D]);
        relative_offset =
            (offset_of!(TdxTrampolineContext, r9) as u32).wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov r10, [initialR10]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x4C, 0x8B, 0x15]);
        relative_offset =
            (offset_of!(TdxTrampolineContext, r10) as u32).wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov r11, [initialR11]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x4C, 0x8B, 0x1D]);
        relative_offset =
            (offset_of!(TdxTrampolineContext, r11) as u32).wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // mov ax, [initialCs]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x8B, 0x05]);
        relative_offset = (offset_of!(TdxTrampolineContext, code_selector) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // test ax, ax
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x66, 0x85, 0xC0]);

        // jz @f
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x74]);
        byte_offset += 1;
        let jump_offset = byte_offset;

        // jmp far [initialRip]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x48, 0xFF, 0x2D]);
        relative_offset = (offset_of!(TdxTrampolineContext, initial_rip) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        byte_offset = copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // @@:
        reset_page[jump_offset.wrapping_sub(1)] = (byte_offset.wrapping_sub(jump_offset)) as u8;

        // jmp [initialRip]
        byte_offset = copy_instr(&mut reset_page, byte_offset, &[0x48, 0xFF, 0x25]);
        relative_offset = (offset_of!(TdxTrampolineContext, initial_rip) as u32)
            .wrapping_sub((byte_offset + 4) as u32);
        copy_instr(&mut reset_page, byte_offset, relative_offset.as_bytes());

        // Add this data to the architectural reset page.
        state.push(VpContextState::Page(VpContextPageState {
            page_base: 0xFFFFF,
            page_count: 1,
            acceptance: loader::importer::BootPageAcceptance::Exclusive,
            data: reset_page,
        }));
    }
}
