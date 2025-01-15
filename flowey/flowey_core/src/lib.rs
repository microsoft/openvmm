// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core types and traits shared between user-facing and internal flowey code.
//!
//! **If you are a flowey node / pipeline author, you should not directly import
//! this crate!** The crate you should be using is called `flowey`, which only
//! exports user-facing types / traits.

pub mod github_context;
pub mod node;
pub mod patch;
pub mod pipeline;
