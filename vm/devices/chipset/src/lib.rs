// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A hodgepodge of chipset devices you'd expect to find on an x86 box.
//!
//! # A note on organization
//!
//! There's no real reason why we decided to lump these particular devices
//! together in the `chipset` crate (vs. each device having their own crates).
//! It was just convenient, and given that these devices are all pretty "small",
//! it didn't substantially bump compile times to have them live under one roof.
//!
//! Future refactors / reorganization may want to split these devices up into
//! their own crates.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod battery;
pub mod cmos_rtc;
pub mod dma;
pub mod i8042;
pub mod ioapic;
pub mod pic;
pub mod pit;
pub mod pm;
pub mod psp;
