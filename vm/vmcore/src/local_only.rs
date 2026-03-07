// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Type definitions for types that cannot cross a mesh process boundary.

#![forbid(unsafe_code)]

use mesh::payload::Error;
use mesh::payload::FieldDecode;
use mesh::payload::FieldEncode;
use mesh::payload::inplace::InplaceOption;
use mesh::payload::protobuf::FieldReader;
use mesh::payload::protobuf::FieldSizer;
use mesh::payload::protobuf::FieldWriter;
use thiserror::Error;

/// A wrapper type that skips serializing the type and fails deserialization.
/// This allows the type to be used in a MeshPayload derive but will fail when
/// sent across a process boundary at runtime.
#[derive(Debug, Clone)]
pub struct LocalOnly<T>(pub T);

impl<T> mesh::payload::DefaultEncoding for LocalOnly<T> {
    type Encoding = LocalOnlyField;
}

impl<T> From<T> for LocalOnly<T> {
    fn from(value: T) -> Self {
        LocalOnly(value)
    }
}

/// A field encoder for fields that should be ignored on write and fail on read.
///
/// This is useful for enum variants that can't be sent across processes.
pub struct LocalOnlyField;

/// Error when attempting to deserialize an instance of [`LocalOnly`].
#[derive(Debug, Error)]
#[error("decoding local-only type")]
pub struct LocalOnlyError;

impl<T, R> FieldEncode<T, R> for LocalOnlyField {
    fn write_field(_item: T, writer: FieldWriter<'_, '_, R>) {
        tracing::warn!(
            type_name = std::any::type_name::<T>(),
            "encoding local-only type"
        );
        writer.message(|_| ());
    }

    fn compute_field_size(_item: &mut T, sizer: FieldSizer<'_>) {
        sizer.message(|_| ());
    }
}

impl<T, R> FieldDecode<'_, T, R> for LocalOnlyField {
    fn read_field(
        _item: &mut InplaceOption<'_, T>,
        _reader: FieldReader<'_, '_, R>,
    ) -> Result<(), Error> {
        Err(Error::new(LocalOnlyError))
    }

    fn default_field(_item: &mut InplaceOption<'_, T>) -> Result<(), Error> {
        Err(Error::new(LocalOnlyError))
    }
}
