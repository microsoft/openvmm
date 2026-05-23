// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interfaces required to support UEFI nvram services.

pub use uefi_nvram_storage::NextVariable;
pub use uefi_nvram_storage::NvramStorage;
pub use uefi_nvram_storage::NvramStorageError;
pub use uefi_specs::uefi::time::EFI_TIME;

/// Callbacks that enable nvram services to revoke VSM on ExitBootServices if
/// requested by the guest.
///
/// This could be backed by different implementations on the host, such as in
/// Underhill asking the host to revoke VSM via a hypercall.
pub trait VsmConfig: Send {
    fn revoke_guest_vsm(&self);
}

/// Callbacks for MOR (Memory Overwrite Request) bit changes.
///
/// When the guest sets the MOR bit via the UEFI device, the platform may need
/// to take action to ensure memory is scrubbed on the next reset. In Underhill,
/// this is done by setting the `zero_memory_on_reset` flag in
/// `HvRegisterVsmPartitionConfig`.
pub trait MorConfig: Send {
    /// Called when the guest sets the MOR variable.
    ///
    /// `mor_value` is the raw byte written by the guest. Bit 0
    /// (`MOR_CLEAR_MEMORY_BIT_MASK`) indicates whether memory should be cleared
    /// on the next reset.
    fn notify_mor_set(&self, mor_value: u8);
}
