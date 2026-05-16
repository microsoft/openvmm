// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native VP context builder for x86_64.
//!
//! Emits [`IgvmDirectiveHeader::X64NativeVpContext`] instead of the VBS-specific
//! VP context. This is appropriate for non-isolated guests (e.g. Linux direct
//! boot on KVM) that don't need the Hyper-V register ABI.

use crate::file_loader::DEFAULT_COMPATIBILITY_MASK;
use crate::vp_context_builder::VpContextBuilder;
use crate::vp_context_builder::VpContextState;
use igvm::IgvmDirectiveHeader;
use igvm_defs::IgvmNativeVpContextX64;
use loader::importer::SegmentRegister;
use loader::importer::X86Register;
use std::mem::discriminant;
use zerocopy::FromZeros;

/// Builds an [`IgvmNativeVpContextX64`] from imported VP registers.
///
/// Registers that don't have a corresponding field in the native context
/// (e.g. MTRRs, PAT) are silently skipped.
#[derive(Debug, Clone)]
pub struct NativeVpContext {
    registers: Vec<X86Register>,
}

impl NativeVpContext {
    pub fn new() -> Self {
        Self {
            registers: Vec::new(),
        }
    }

    /// Returns true if the register has a mapping in [`IgvmNativeVpContextX64`].
    fn is_supported(register: &X86Register) -> bool {
        matches!(
            register,
            X86Register::Cr0(_)
                | X86Register::Cr3(_)
                | X86Register::Cr4(_)
                | X86Register::Efer(_)
                | X86Register::Rip(_)
                | X86Register::Rsi(_)
                | X86Register::Rsp(_)
                | X86Register::Rbp(_)
                | X86Register::R8(_)
                | X86Register::R9(_)
                | X86Register::R10(_)
                | X86Register::R11(_)
                | X86Register::R12(_)
                | X86Register::Rflags(_)
                | X86Register::Gdtr(_)
                | X86Register::Idtr(_)
                | X86Register::Cs(_)
                | X86Register::Ds(_)
                | X86Register::Es(_)
                | X86Register::Fs(_)
                | X86Register::Gs(_)
                | X86Register::Ss(_)
        )
    }
}

impl VpContextBuilder for NativeVpContext {
    type Register = X86Register;

    fn import_vp_register(&mut self, register: X86Register) {
        if !Self::is_supported(&register) {
            tracing::debug!(
                ?register,
                "skipping register not supported by native VP context"
            );
            return;
        }

        // For data segment registers (DS/ES/FS/GS/SS), they all map to
        // the same native context fields (data_selector, data_attributes,
        // data_base, data_limit). Enforce that subsequent data segment
        // registers carry the same value as the first one seen.
        if let Some(new_seg) = data_segment_value(&register) {
            if let Some(existing_seg) = self.registers.iter().find_map(data_segment_value) {
                assert_eq!(
                    new_seg, existing_seg,
                    "data segment register {register:?} has a different value than the \
                     previously imported data segment register — the native VP context \
                     only has a single set of data segment fields"
                );
                return;
            }
        } else if self
            .registers
            .iter()
            .any(|r| discriminant(r) == discriminant(&register))
        {
            return;
        }

        self.registers.push(register);
    }

    fn set_vp_context_memory(&mut self, _page_base: u64) {
        unimplemented!("not supported for native VP context");
    }

    fn finalize(&mut self, state: &mut Vec<VpContextState>) {
        if self.registers.is_empty() {
            return;
        }

        let mut context = IgvmNativeVpContextX64::new_zeroed();

        for reg in &self.registers {
            match *reg {
                X86Register::Cr0(v) => context.cr0 = v,
                X86Register::Cr3(v) => context.cr3 = v,
                X86Register::Cr4(v) => context.cr4 = v,
                X86Register::Efer(v) => context.efer = v,
                X86Register::Rip(v) => context.rip = v,
                X86Register::Rsi(v) => context.rsi = v,
                X86Register::Rsp(v) => context.rsp = v,
                X86Register::Rbp(v) => context.rbp = v,
                X86Register::R8(v) => context.r8 = v,
                X86Register::R9(v) => context.r9 = v,
                X86Register::R10(v) => context.r10 = v,
                X86Register::R11(v) => context.r11 = v,
                X86Register::R12(v) => context.r12 = v,
                X86Register::Rflags(v) => context.rflags = v,
                X86Register::Gdtr(ref table) => {
                    context.gdtr_base = table.base;
                    context.gdtr_limit = table.limit;
                }
                X86Register::Idtr(ref table) => {
                    context.idtr_base = table.base;
                    context.idtr_limit = table.limit;
                }
                X86Register::Cs(ref seg) => {
                    context.code_selector = seg.selector;
                    context.code_attributes = seg.attributes;
                    context.code_base = seg.base as u32;
                    context.code_limit = seg.limit;
                }
                X86Register::Ds(ref seg)
                | X86Register::Es(ref seg)
                | X86Register::Fs(ref seg)
                | X86Register::Gs(ref seg)
                | X86Register::Ss(ref seg) => {
                    context.data_selector = seg.selector;
                    context.data_attributes = seg.attributes;
                    context.data_base = seg.base as u32;
                    context.data_limit = seg.limit;
                }
                // All other registers are filtered out in import_vp_register.
                _ => unreachable!(),
            }
        }

        state.push(VpContextState::Directive(
            IgvmDirectiveHeader::X64NativeVpContext {
                compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
                vp_index: 0,
                context: Box::new(context),
            },
        ));
    }
}

/// Extracts the [`SegmentRegister`] value from a data segment register
/// (DS/ES/FS/GS/SS), returning `None` for all other register kinds.
fn data_segment_value(register: &X86Register) -> Option<SegmentRegister> {
    match register {
        X86Register::Ds(seg)
        | X86Register::Es(seg)
        | X86Register::Fs(seg)
        | X86Register::Gs(seg)
        | X86Register::Ss(seg) => Some(*seg),
        _ => None,
    }
}
