// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SMMUv3 spec-derived type definitions.
//!
//! Register layouts, stream table entries, context descriptors, command/event
//! queue entries, and page table descriptors — all derived from the Arm SMMUv3
//! architecture specification (IHI 0070).
//!
//! This module contains only type definitions, not algorithms.

pub mod cd;
pub mod commands;
pub mod events;
pub mod pt;
pub mod registers;
pub mod ste;
