// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CXL component register abstractions.

use inspect::Inspect;
use vmcore::save_restore::ProtobufSaveRestore;

mod hdm_cap;
mod registers;
pub mod spec;
/// Reusable in-memory register blocks for unit tests.
pub mod test_helper;

pub use hdm_cap::CxlHdmDecoderCapability;
pub use hdm_cap::CxlHdmDecoderCapabilityError;
pub use hdm_cap::CxlHdmDecoderCapabilityOptions;
pub use hdm_cap::CxlHdmDecoderFixedConfig;
pub use registers::CxlComponentRegisters;

/// A generic CXL component register space.
pub trait CxlComponentRegister: Send + Sync + Inspect + ProtobufSaveRestore {
    /// A descriptive label for use in Save/Restore + Inspect output.
    fn label(&self) -> &str;

    /// Returns the register block type.
    fn register_type(&self) -> crate::spec::CxlComponentRegisterType;

    /// Returns the CXL capability ID for this register block.
    fn capability_id(&self) -> u16;

    /// Returns the CXL capability-version value for this register block.
    fn capability_version(&self) -> u8;

    /// Returns the component register aperture length in bytes.
    fn len(&self) -> u16;

    /// Reads a 32-bit value at a register-block-relative offset.
    fn read_u32(&self, offset: u16) -> Option<u32>;

    /// Writes a 32-bit value at a register-block-relative offset.
    ///
    /// Returns `true` when the write is accepted.
    fn write_u32(&mut self, offset: u16, val: u32) -> bool;

    /// Resets the register space.
    fn reset(&mut self);
}
