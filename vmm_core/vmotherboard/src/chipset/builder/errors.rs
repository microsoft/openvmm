// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::chipset::PciConflict;
use crate::chipset::PcieConflict;
use crate::chipset::io_ranges::IoRangeConflict;
use std::fmt::Debug;
use thiserror::Error;

/// An error occurred as part of Chipset initialization
// DEVNOTE: many of these _could_ potentially be lifted to compile time through
// the use of a `typed_builder`...
#[derive(Debug, Error)]
#[expect(clippy::enum_variant_names)] // these are descriptive
pub enum ChipsetBuilderError {
    /// detected static mmio intercept region conflict
    #[error("static mmio intercept region conflict: {0}")]
    MmioConflict(IoRangeConflict<u64>),
    /// detected static pio intercept region conflict
    #[error("static pio intercept region conflict: {0}")]
    PioConflict(IoRangeConflict<u16>),
    /// detected static pci address conflict
    #[error("static pci conflict: {0}")]
    PciConflict(PciConflict),
    /// detected static pcie port conflict
    #[error("static pcie port conflict: {0}")]
    PcieConflict(PcieConflict),
}

#[derive(Debug, Error)]
#[error("detected one or more errors during vmotherboard init:")]
pub struct FinalChipsetBuilderError(#[source] pub ErrorList);

#[derive(Debug)]
pub struct ErrorList {
    err: ChipsetBuilderError,
    next: Option<Box<Self>>,
}

impl std::fmt::Display for ErrorList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.err)
    }
}

impl ErrorList {
    fn new(err: ChipsetBuilderError) -> Self {
        Self { err, next: None }
    }

    fn chain(self: Box<Self>, err: ChipsetBuilderError) -> Self {
        Self {
            err,
            next: Some(self),
        }
    }
}

pub trait ErrorListExt {
    fn append(&mut self, err: ChipsetBuilderError);
}

impl ErrorListExt for Option<ErrorList> {
    fn append(&mut self, err: ChipsetBuilderError) {
        *self = Some(match self.take() {
            Some(existing) => Box::new(existing).chain(err),
            None => ErrorList::new(err),
        })
    }
}

impl std::error::Error for ErrorList {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.next.as_ref().map(|x| x as _)
    }
}
