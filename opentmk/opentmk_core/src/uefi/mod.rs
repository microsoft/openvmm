// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI runtime support: the global allocator, the ACPI table wrapper, and
//! the boot-time initialization sequence.

pub(crate) mod acpi_wrap;
pub mod alloc;
/// Boot-time initialization: heap, logger, ACPI tables, and VTL protection.
pub mod init;
