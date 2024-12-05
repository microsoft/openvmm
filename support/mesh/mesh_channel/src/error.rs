// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Remotable errors.

use mesh_protobuf::EncodeAs;
use mesh_protobuf::Protobuf;
use std::fmt;
use std::fmt::Display;

/// An error that can be remoted across a mesh channel.
///
/// This erases the error's type, but preserves the source error chain when sent
/// between processes.
#[derive(Protobuf)]
pub struct RemoteError(EncodeAs<BoxedError, EncodedError>);

type BoxedError = Box<dyn std::error::Error + Send + Sync>;

impl RemoteError {
    /// Returns a new remote error wrapping `error` (including the error's
    /// source).
    pub fn new<T: Into<BoxedError>>(error: T) -> Self {
        Self(error.into().into())
    }
}

impl Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl std::error::Error for RemoteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

/// An error encoded for serialization as a mesh message.
#[derive(Protobuf)]
struct EncodedError {
    errors: Vec<String>,
}

impl From<EncodedError> for BoxedError {
    fn from(error: EncodedError) -> Self {
        Box::new(DecodedError::from(error))
    }
}

impl From<BoxedError> for EncodedError {
    fn from(error: BoxedError) -> Self {
        let mut errors = Vec::new();
        let mut error = error.as_ref() as &dyn std::error::Error;
        loop {
            errors.push(error.to_string());
            if let Some(source) = error.source() {
                error = source;
            } else {
                break;
            }
        }
        Self { errors }
    }
}

/// An error decoded from an [`EncodedError`].
///
/// This is a distinct type so that we can implement
/// [`std::error::Error::source`] for it.
#[derive(Debug)]
struct DecodedError {
    source: Option<Box<DecodedError>>,
    error: String,
}

impl From<EncodedError> for DecodedError {
    fn from(value: EncodedError) -> Self {
        let mut errors = value.errors;
        let last_error = errors.pop().unwrap_or("no error information".to_string());
        let mut decoded = DecodedError {
            source: None,
            error: last_error,
        };
        for error in errors.into_iter().rev() {
            decoded = DecodedError {
                source: Some(Box::new(decoded)),
                error,
            };
        }
        decoded
    }
}

impl Display for DecodedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for DecodedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|s| s as _)
    }
}

/// Alias for a [`Result`] with a [`RemoteError`] error.
pub type RemoteResult<T> = Result<T, RemoteError>;
