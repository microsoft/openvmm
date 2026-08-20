// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI runtime support: the global allocator, the ACPI table wrapper, and
//! the boot-time initialization sequence.

pub(crate) mod acpi_wrap;
/// Memory allocation backed by UEFI boot services or a capped heap.
pub mod alloc;
/// Boot-time initialization: heap, logger, ACPI tables, and VTL protection.
pub mod init;
