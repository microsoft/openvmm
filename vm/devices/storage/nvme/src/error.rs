// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error and result related types.

use crate::spec;
use std::error::Error;

/// An NVMe error, consisting of a status code, whether its completion sets
/// Do Not Retry, and an optional error source.
#[derive(Debug)]
pub struct NvmeError {
    status: spec::Status,
    dnr: bool,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl NvmeError {
    pub fn new(status: spec::Status, source: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Self {
            status,
            dnr: false,
            source: Some(source.into()),
        }
    }

    /// An error whose completion sets Do Not Retry, for a failure that a
    /// resubmitted command repeats: "if the same command is re-submitted to
    /// any controller in the NVM subsystem, then that re-submitted command is
    /// expected to fail" (NVMe Base 2.1 section 4.2.3, Figure 100).
    pub fn do_not_retry(status: spec::Status) -> Self {
        Self {
            status,
            dnr: true,
            source: None,
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
            dnr: false,
            source: None,
        }
    }
}

/// The result of an NVMe command.
#[derive(Default)]
pub struct CommandResult {
    pub status: spec::Status,
    pub dw: [u32; 2],
    /// Do Not Retry, set only by [`NvmeError::do_not_retry`].
    pub dnr: bool,
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
            dnr: status.dnr,
        }
    }

    /// The status field of this command's completion queue entry. The queue
    /// sets the phase bit when it posts the entry.
    pub fn completion_status(&self) -> spec::CompletionStatus {
        spec::CompletionStatus::new()
            .with_status(self.status.0)
            .with_dnr(self.dnr)
    }
}

#[cfg(test)]
mod tests {
    use super::CommandResult;
    use super::NvmeError;
    use crate::spec;

    #[test]
    fn completion_status_carries_do_not_retry() {
        let refused = CommandResult::from(NvmeError::do_not_retry(
            spec::Status::INVALID_FIELD_IN_COMMAND,
        ))
        .completion_status();
        assert_eq!(refused.status(), spec::Status::INVALID_FIELD_IN_COMMAND.0);
        assert!(refused.dnr(), "do_not_retry lost DNR");

        let with_dw = CommandResult::new(
            NvmeError::do_not_retry(spec::Status::INVALID_NAMESPACE_OR_FORMAT),
            [1, 2],
        );
        assert_eq!(with_dw.dw, [1, 2]);
        assert!(with_dw.completion_status().dnr(), "new lost DNR");

        for status in [
            CommandResult::from(spec::Status::INVALID_FIELD_IN_COMMAND).completion_status(),
            CommandResult::from(NvmeError::new(
                spec::Status::DATA_TRANSFER_ERROR,
                std::io::Error::other("backend failure"),
            ))
            .completion_status(),
            CommandResult::default().completion_status(),
        ] {
            assert!(!status.dnr(), "DNR set on {:#x}", status.status());
        }
    }
}
