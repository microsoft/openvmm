// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the common VP context builder traits and methods for different isolation architectures.

pub mod snp;
pub mod tdx;
pub mod vbs;

use igvm::IgvmDirectiveHeader;
use loader::importer::BootPageAcceptance;

/// Holds memory state representing a VP context that should be imported.
pub struct VpContextPageState {
    pub page_base: u64,
    pub page_count: u64,
    pub acceptance: BootPageAcceptance,
    pub data: Vec<u8>,
}

/// The finalized VP context data that should be imported or added to the IGVM file.
pub enum VpContextState {
    /// VP context are pages to be imported.
    Page(VpContextPageState),
    /// VP context is an IGVM directive header to be added to the file directly.
    Directive(IgvmDirectiveHeader),
}

/// Common trait used to implement VP context builders for different isolation architectures.
pub trait VpContextBuilder {
    /// The register type which is different on different architectures.
    type Register;

    /// Import a register to the BSP at the given vtl.
    fn import_vp_register(&mut self, register: Self::Register);

    /// Define the base of the GPA range to be used for architecture-specific VP context data.
    fn set_vp_context_memory(&mut self, page_base: u64);

    /// Finalize all VP context data. Returns architecture specific data that should be either imported
    /// into guest memory space or added directly to the IGVM file.
    fn finalize(&mut self, state: &mut Vec<VpContextState>);
}
