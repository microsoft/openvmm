// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VBS VP context builder.

use crate::file_loader::DEFAULT_COMPATIBILITY_MASK;
use crate::vp_context_builder::VpContextBuilder;
use crate::vp_context_builder::VpContextState;
use hvdef::Vtl;
use igvm::FileDataSerializer;
use igvm::IgvmDirectiveHeader;
use igvm_defs::PAGE_SIZE_4K;
use loader::importer::Aarch64Register;
use loader::importer::X86Register;
use std::fmt::Debug;
use std::mem::discriminant;

/// A trait used to specialize behavior based on the register type. Different
/// architectures need to do different conversions and emit different
/// [`IgvmDirectiveHeader`] types.
pub trait VbsRegister: Sized {
    /// Convert the list of registers into the corresponding
    /// [`IgvmDirectiveHeader`] for this architecture.
    fn into_igvm_header(vtl: Vtl, list: &[Self]) -> IgvmDirectiveHeader;
}

impl VbsRegister for X86Register {
    fn into_igvm_header(vtl: Vtl, list: &[Self]) -> IgvmDirectiveHeader {
        IgvmDirectiveHeader::X64VbsVpContext {
            registers: list
                .iter()
                .map(|&reg| reg.into())
                .collect::<Vec<igvm::registers::X86Register>>(),
            vtl: (vtl as u8).try_into().expect("vtl should be valid"),
            compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
        }
    }
}

impl VbsRegister for Aarch64Register {
    fn into_igvm_header(vtl: Vtl, list: &[Self]) -> IgvmDirectiveHeader {
        IgvmDirectiveHeader::AArch64VbsVpContext {
            registers: list
                .iter()
                .map(|&reg| reg.into())
                .collect::<Vec<igvm::registers::AArch64Register>>(),
            vtl: (vtl as u8).try_into().expect("vtl should be valid"),
            compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VbsVpContext<R: VbsRegister> {
    /// The registers set for this VP.
    registers: Vec<R>,
    /// The VTL this VP context is for.
    vtl: Vtl,
}

impl<R: VbsRegister> VbsVpContext<R> {
    pub fn new(vtl: Vtl) -> Self {
        Self {
            registers: Vec::new(),
            vtl,
        }
    }

    /// Returns this VP context encoded as a serialized page of data, in IGVM
    /// directive format.
    pub fn as_page(&self) -> Vec<u8> {
        let header = R::into_igvm_header(self.vtl, &self.registers);
        // Serialize the same binary format as an IGVM header, but instead to be deposited as page data.
        let mut variable_header = Vec::new();
        let mut file_data = FileDataSerializer::new(0);
        header
            .write_binary_header(&mut variable_header, &mut file_data)
            .expect("registers should be valid");

        let file_data = file_data.take();

        assert!(file_data.len() <= PAGE_SIZE_4K as usize);

        file_data
    }
}

impl<R: VbsRegister> VpContextBuilder for VbsVpContext<R> {
    type Register = R;

    fn import_vp_register(&mut self, register: R) {
        // Check for duplicate register
        assert!(
            !self
                .registers
                .iter()
                .any(|reg| discriminant(reg) == discriminant(&register)),
            "duplicate register import"
        );

        self.registers.push(register);
    }

    fn set_vp_context_memory(&mut self, _page_base: u64) {
        unimplemented!("not supported for VBS");
    }

    fn finalize(&mut self, state: &mut Vec<VpContextState>) {
        if self.registers.is_empty() {
            return;
        }
        // Serialize as a VP context IGVM header.
        state.push(VpContextState::Directive(R::into_igvm_header(
            self.vtl,
            &self.registers,
        )));
    }
}
