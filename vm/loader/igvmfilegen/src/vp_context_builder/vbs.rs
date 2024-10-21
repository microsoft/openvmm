// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VBS VP context builder.

use crate::file_loader::DEFAULT_COMPATIBILITY_MASK;
use crate::file_loader::HV_NUM_VTLS;
use crate::vp_context_builder::VpContextBuilder;
use crate::vp_context_builder::VpContextPageState;
use crate::vp_context_builder::VpContextState;
use hvdef::Vtl;
use igvm::FileDataSerializer;
use igvm::IgvmDirectiveHeader;
use igvm_defs::PAGE_SIZE_4K;
use loader::importer::Aarch64Register;
use loader::importer::BootPageAcceptance;
use loader::importer::X86Register;
use std::fmt::Debug;
use std::mem::discriminant;

/// A trait used to specialize behavior based on the register type. Different
/// architectures need to do different conversions and emit different
/// [`IgvmDirectiveHeader`] types.
pub trait VbsRegister: Sized {
    /// Convert the list of registers into the corresponding
    /// [`IgvmDirectiveHeader`] for this architecture.
    fn into_igvm_header(vtl: Vtl, list: Vec<Self>) -> IgvmDirectiveHeader;
}

impl VbsRegister for X86Register {
    fn into_igvm_header(vtl: Vtl, list: Vec<Self>) -> IgvmDirectiveHeader {
        IgvmDirectiveHeader::X64VbsVpContext {
            registers: list
                .into_iter()
                .map(|reg| reg.into())
                .collect::<Vec<igvm::registers::X86Register>>(),
            vtl: (vtl as u8).try_into().expect("vtl should be valid"),
            compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
        }
    }
}

impl VbsRegister for Aarch64Register {
    fn into_igvm_header(vtl: Vtl, list: Vec<Self>) -> IgvmDirectiveHeader {
        IgvmDirectiveHeader::AArch64VbsVpContext {
            registers: list
                .into_iter()
                .map(|reg| reg.into())
                .collect::<Vec<igvm::registers::AArch64Register>>(),
            vtl: (vtl as u8).try_into().expect("vtl should be valid"),
            compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VbsVpContext<R: VbsRegister> {
    /// The acceptance to import this vp context as. This tracks if finalize
    /// will generate page data or an IGVM VP context header.
    acceptance: Option<BootPageAcceptance>,
    /// The page number to import this vp context at.
    page_number: u64,
    /// The registers set for this VP.
    registers: Vec<R>,
    /// The VTL this VP context is for.
    vtl: u8,
}

impl<R: VbsRegister> VbsVpContext<R> {
    pub fn new(vtl: u8) -> Self {
        Self {
            acceptance: None,
            page_number: 0,
            registers: Vec::new(),
            vtl,
        }
    }

    pub fn import_vp_register(&mut self, register: R) {
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

    pub fn vp_context_page(&self) -> anyhow::Result<u64> {
        match self.acceptance {
            None => Err(anyhow::anyhow!("no vp context page set")),
            Some(_) => Ok(self.page_number),
        }
    }

    pub fn set_vp_context_memory(&mut self, page_base: u64, acceptance: BootPageAcceptance) {
        assert!(
            self.acceptance.is_none(),
            "only allowed to set vp context memory once"
        );

        self.page_number = page_base;
        self.acceptance = Some(acceptance);
    }

    pub fn finalize(self) -> Option<VpContextState> {
        if self.registers.is_empty() {
            None
        } else {
            let header = R::into_igvm_header(
                self.vtl.try_into().expect("vtl should be valid"),
                self.registers,
            );

            match self.acceptance {
                None => {
                    // Serialize as a VP context IGVM header.
                    Some(VpContextState::Directive(header))
                }
                Some(acceptance) => {
                    // Serialize the same binary format as an IGVM header, but instead to be deposited as page data.
                    let mut variable_header = Vec::new();
                    let mut file_data = FileDataSerializer::new(0);
                    header
                        .write_binary_header(&mut variable_header, &mut file_data)
                        .expect("registers should be valid");

                    let file_data = file_data.take();

                    assert!(file_data.len() <= PAGE_SIZE_4K as usize);

                    Some(VpContextState::Page(VpContextPageState {
                        page_base: self.page_number,
                        page_count: 1,
                        acceptance,
                        data: file_data,
                    }))
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct VbsVpContextBuilder<R: VbsRegister> {
    contexts: [VbsVpContext<R>; HV_NUM_VTLS],
}

impl<R: VbsRegister> VbsVpContextBuilder<R> {
    pub(crate) fn new() -> Self {
        Self {
            contexts: [
                VbsVpContext::<R>::new(0),
                VbsVpContext::<R>::new(1),
                VbsVpContext::<R>::new(2),
            ],
        }
    }
}

impl<R: VbsRegister> VpContextBuilder for VbsVpContextBuilder<R> {
    type Register = R;

    fn import_vp_register(&mut self, vtl: Vtl, register: R) {
        // TODO: Importing VTL1 state not currently supported.
        assert!(vtl != Vtl::Vtl1);

        self.contexts[vtl as usize].import_vp_register(register);
    }

    fn vp_context_page(&self, vtl: Vtl) -> anyhow::Result<u64> {
        self.contexts[vtl as usize].vp_context_page()
    }

    fn set_vp_context_memory(&mut self, vtl: Vtl, page_base: u64, acceptance: BootPageAcceptance) {
        // TODO: Importing VTL1 state not currently supported.
        assert!(vtl != Vtl::Vtl1);

        self.contexts[vtl as usize].set_vp_context_memory(page_base, acceptance);
    }

    fn finalize(self: Box<Self>) -> Vec<VpContextState> {
        // TODO: Importing VTL1 state not currently supported.
        assert!(self.contexts[1].registers.is_empty());

        let mut state = Vec::new();

        for context in self.contexts {
            if let Some(v) = context.finalize() {
                state.push(v);
            }
        }

        state
    }
}
