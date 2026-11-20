// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The minimal UEFI environment in which the shim runs the Linux EFI stub.
//!
//! The shim provides the boot services that the x86 EFI stub uses: page and
//! pool allocation, the memory map, `LOADED_IMAGE` for the command line,
//! `LoadFile2` on the `LINUX_EFI_INITRD_MEDIA_GUID` device path for the
//! initrd, configuration tables, and `ExitBootServices`. Every protocol
//! lookup that the shim does not implement returns `EFI_NOT_FOUND`, and the
//! stub treats that as an absent feature.
//!
//! The configuration tables describe ACPI, SMBIOS, the SEV-SNP CC blob, and
//! (through the RT properties table) no runtime services. The runtime-services
//! table exists, but every service reports `EFI_UNSUPPORTED`. When
//! `SetVirtualAddressMap` fails, Linux turns off EFI runtime services. This
//! matters: some Linux code checks only that flag, and not the RT properties
//! table, before it calls a runtime service.
//!
//! The stub decompresses the kernel, loads the initrd, builds the E820 map
//! from the memory map, sets up lazy acceptance for unaccepted memory, calls
//! `ExitBootServices`, and enters the kernel. It does not return on success.

mod layout;
mod services;
mod tables;

#[cfg(any(minimal_rt, test))]
mod entry;

#[cfg(minimal_rt)]
pub(crate) use entry::enter_linux;
#[cfg(minimal_rt)]
pub(crate) use layout::ImageLayout;
#[cfg(minimal_rt)]
pub(crate) use layout::build_memory_map;
#[cfg(minimal_rt)]
pub(crate) use layout::initial_acceptance;
#[cfg(minimal_rt)]
pub(crate) use tables::Boot;
