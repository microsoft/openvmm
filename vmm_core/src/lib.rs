// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core building blocks for managing vm and vm related state to build a vmm.
//! Used by both OpenVMM and underhill today.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod acpi_builder;
pub mod cpuid;
pub mod device_builder;
pub mod emuplat;
pub mod input_distributor;
pub mod partition_unit;
pub mod platform_resolvers;
pub mod synic;
pub mod vmbus_unit;
pub mod vmotherboard_adapter;
pub mod vmtime_unit;
