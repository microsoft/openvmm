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
