// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error and result related types.

use crate::spec;
use std::error::Error;

/// An NVMe error, consisting of a status code and optional error source.
#[derive(Debug)]
pub struct NvmeError {
    status: spec::Status,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl NvmeError {
    pub fn new(status: spec::Status, source: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Self {
            status,
            source: Some(source.into()),
        }
    }
}

impl Error for NvmeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_ref().map(|x| x.as_ref() as _)
    }
}

impl std::fmt::Display for NvmeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status.status_code_type() {
            spec::StatusCodeType::GENERIC => {
                write!(f, "general error {:#x?}", self.status)
            }
            spec::StatusCodeType::COMMAND_SPECIFIC => {
                write!(f, "command-specific error {:#x?}", self.status)
            }
            spec::StatusCodeType::MEDIA_ERROR => {
                write!(f, "media error {:#x?}", self.status)
            }
            _ => write!(f, "{:#x?}", self.status),
        }
    }
}

impl From<spec::Status> for NvmeError {
    fn from(status: spec::Status) -> Self {
        NvmeError {
            status,
            source: None,
        }
    }
}

/// The result of an NVMe command.
#[derive(Default)]
pub struct CommandResult {
    pub status: spec::Status,
    pub dw: [u32; 2],
}

impl<T: Into<NvmeError>> From<T> for CommandResult {
    fn from(status: T) -> Self {
        CommandResult::new(status, [0; 2])
    }
}

impl CommandResult {
    pub fn new(status: impl Into<NvmeError>, dw: [u32; 2]) -> Self {
        let status = status.into();
        Self {
            status: status.status,
            dw,
        }
    }
}
