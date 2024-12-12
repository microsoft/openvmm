// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error types used during schema parsing.

use crate::Vtl2SettingsErrorInfo;
use crate::Vtl2SettingsErrorInfoVec;

/// A list of errors.
pub(crate) struct ParseErrorsBase {
    errors: Vec<Vtl2SettingsErrorInfo>,
}

/// A list of errors, with additional context for more detailed error
/// messages.
pub(crate) struct ParseErrors<'a> {
    base: &'a mut ParseErrorsBase,
    context: Option<ContextNode<'a>>,
}

#[derive(Copy, Clone)]
struct ContextNode<'a> {
    parent: &'a Option<ContextNode<'a>>,
    context: &'a ErrorContext,
}

impl ParseErrorsBase {
    /// Create a new error list.
    pub fn new() -> Self {
        Self { errors: Vec::new() }
    }

    /// Returns a root object for pushing errors with no additional context.
    pub fn root(&mut self) -> ParseErrors<'_> {
        ParseErrors {
            base: self,
            context: None,
        }
    }

    /// Returns `Ok(())`` if there are no errors, otherwise returns the list of
    /// errors.
    pub fn result(self) -> Result<(), Vtl2SettingsErrorInfoVec> {
        let errors = self.errors;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Vtl2SettingsErrorInfoVec { errors })
        }
    }
}

impl ParseErrors<'_> {
    /// Calls `f` with a new error list accessor with the given context.
    ///
    /// The context is used to provide more detailed error messages. It is
    /// appended to the error message when an error is pushed.
    ///
    /// This can be nested to provide multiple levels of context.
    pub fn with_context<R>(
        &mut self,
        context: ErrorContext,
        f: impl FnOnce(&mut ParseErrors<'_>) -> Result<R, ParsingStopped>,
    ) -> Result<R, ParsingStopped> {
        let node = ContextNode {
            parent: &self.context,
            context: &context,
        };
        match f(&mut ParseErrors {
            base: self.base,
            context: Some(node),
        }) {
            Ok(r) => Ok(r),
            Err(err) => {
                // If there was a synchronous error, push it onto the list now
                // to apply the appropriate context and just return the
                // sentinel error.
                if let Some(err) = err.0 {
                    ParseErrors {
                        base: self.base,
                        context: Some(node),
                    }
                    .push_inner(err);
                }
                Err(ParsingStopped(None))
            }
        }
    }

    /// Pushes a new error onto the list, updating the error with the current
    /// context.
    #[track_caller]
    pub fn push(&mut self, err: impl Into<Vtl2SettingsErrorInfo>) {
        self.push_inner(err.into())
    }

    /// Pushes a new error onto the list, updating the error with both the
    /// current context and the given context.
    #[track_caller]
    pub fn push_with_context(
        &mut self,
        context: ErrorContext,
        err: impl Into<Vtl2SettingsErrorInfo>,
    ) {
        let mut err = err.into();
        context.extend_error(&mut err);
        self.push_inner(err);
    }

    fn push_inner(&mut self, mut err: Vtl2SettingsErrorInfo) {
        let mut node = &self.context;
        while let Some(n) = node {
            n.context.extend_error(&mut err);
            node = n.parent;
        }
        self.base.errors.push(err);
    }
}

/// The context in which parsing is occurring. This is used to provide more
/// detailed error messages, by appending additional context to the end of the
/// error message.
pub(crate) enum ErrorContext {
    /// A device instance ID (mostly used to identify vmbus devices).
    InstanceId(guid::Guid),
    /// An IDE device by channel and device number.
    Ide(u32, u32),
    /// A SCSI disk, by LUN on the controller. The controller identity is
    /// provided by a separate context.
    Scsi(u32),
    /// An NVMe namespace, by namespace ID on the controller. The controller
    /// identity is provided by a separate context.
    Nvme(u32),
}

impl ErrorContext {
    fn extend_error(&self, err: &mut Vtl2SettingsErrorInfo) {
        use std::fmt::Write;

        match self {
            ErrorContext::InstanceId(instance_id) => {
                write!(err.message, ", instance ID: {instance_id}").unwrap();
            }
            ErrorContext::Nvme(nsid) => {
                write!(err.message, ", nvme namespace ID {nsid}").unwrap();
            }
            ErrorContext::Ide(channel, device) => {
                write!(err.message, ", ide device {channel}/{device}").unwrap();
            }
            ErrorContext::Scsi(lun) => {
                write!(err.message, ", scsi LUN {lun}").unwrap();
            }
        }
    }
}

/// An error returned by parse functions when parsing cannot continue.
#[derive(Debug)]
pub(crate) struct ParsingStopped(Option<Vtl2SettingsErrorInfo>);

impl<T: Into<Vtl2SettingsErrorInfo>> From<T> for ParsingStopped {
    #[track_caller]
    fn from(err: T) -> Self {
        ParsingStopped(Some(err.into()))
    }
}

/// Extension trait to accumulate a parse error into an error list without
/// stopping parsing.
///
/// This useful when you are parsing a list of items and want to continue
/// parsing the rest of the list even if one item fails to parse.
pub(crate) trait ParseResultExt<T> {
    /// Collects the error of a `Result` into the error list, returning `None`
    /// if an error occurred.
    fn collect_error(self, errors: &mut ParseErrors<'_>) -> Option<T>;
}

impl<T> ParseResultExt<T> for Result<T, ParsingStopped> {
    fn collect_error(self, errors: &mut ParseErrors<'_>) -> Option<T> {
        match self {
            Ok(v) => Some(v),
            Err(ParsingStopped(err)) => {
                if let Some(err) = err {
                    errors.push_inner(err);
                }
                None
            }
        }
    }
}
