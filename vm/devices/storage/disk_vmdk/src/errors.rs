// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Errors returned while parsing descriptors and opening VMDK disks.

use crate::MAX_DESCRIPTOR_SIZE;
use std::io;
use std::path::PathBuf;
use thiserror::Error;

/// An error parsing or validating the contents of a VMDK descriptor.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ParseError {
    /// The descriptor is not UTF-8 text.
    #[error("VMDK descriptor is not valid UTF-8")]
    InvalidEncoding(#[from] std::str::Utf8Error),
    /// A required descriptor field is missing.
    #[error("VMDK descriptor is missing required field '{0}'")]
    MissingField(&'static str),
    /// The descriptor content is invalid or unsupported.
    #[error("invalid VMDK descriptor: {0}")]
    Invalid(String),
}

/// An error opening a VMDK or validating its descriptor, sources, and extent map.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The requested VMDK does not support writable access.
    #[error("the requested VMDK does not support writable access")]
    ReadWriteUnsupported,
    /// The descriptor file could not be opened.
    #[error("failed to open VMDK descriptor '{}'", path.display())]
    OpenDescriptor {
        /// Descriptor path.
        path: PathBuf,
        /// I/O error.
        #[source]
        source: io::Error,
    },
    /// Descriptor metadata could not be read.
    #[error("failed to read metadata for VMDK descriptor '{}'", path.display())]
    ReadDescriptorMetadata {
        /// Descriptor path.
        path: PathBuf,
        /// I/O error.
        #[source]
        source: io::Error,
    },
    /// The descriptor is not a regular file.
    #[error("VMDK descriptor '{}' is not a regular file", .0.display())]
    DescriptorNotRegularFile(PathBuf),
    /// The descriptor is larger than the accepted limit.
    #[error("VMDK descriptor is larger than {MAX_DESCRIPTOR_SIZE} bytes")]
    DescriptorTooLarge,
    /// The descriptor file could not be read.
    #[error("failed to read VMDK descriptor '{}'", path.display())]
    ReadDescriptor {
        /// Descriptor path.
        path: PathBuf,
        /// I/O error.
        #[source]
        source: io::Error,
    },
    /// The descriptor content is invalid or unsupported.
    #[error(transparent)]
    Parse(#[from] ParseError),
    /// A referenced extent file could not be opened.
    #[error("failed to open VMDK extent '{}'", path.display())]
    OpenExtent {
        /// Extent path.
        path: PathBuf,
        /// I/O error.
        #[source]
        source: io::Error,
    },
    /// Extent metadata could not be read.
    #[error("failed to read metadata for VMDK source file {index}")]
    ReadExtentMetadata {
        /// Source file index.
        index: usize,
        /// I/O error.
        #[source]
        source: io::Error,
    },
    /// A referenced extent is not a regular file.
    #[error("VMDK source file {0} is not a regular file")]
    ExtentNotRegularFile(usize),
    /// The supplied extent map is invalid.
    #[error("invalid VMDK extent map: {0}")]
    InvalidExtentMap(String),
}
