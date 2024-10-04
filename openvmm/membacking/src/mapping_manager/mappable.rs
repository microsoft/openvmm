// Copyright (C) Microsoft Corporation. All rights reserved.

use mesh::payload::encoding::ResourceField;
use mesh::payload::DefaultEncoding;
use std::sync::Arc;

/// A section handle.
#[cfg(windows)]
type OwnedType = std::os::windows::io::OwnedHandle;

/// A file descriptor.
#[cfg(unix)]
type OwnedType = std::os::fd::OwnedFd;

/// A handle/fd to an OS object that can be mapped into memory.
///
/// This uses `Arc` to make `clone` cheap and reliable.
#[derive(Debug, Clone)]
pub struct Mappable(Arc<OwnedType>);

impl From<OwnedType> for Mappable {
    fn from(value: OwnedType) -> Self {
        Self(Arc::new(value))
    }
}

impl From<Arc<OwnedType>> for Mappable {
    fn from(value: Arc<OwnedType>) -> Self {
        Self(value)
    }
}

impl From<Mappable> for OwnedType {
    fn from(value: Mappable) -> Self {
        // Currently there is no way to avoid the unwrap here. Mesh improvements
        // to how resources are handled could make this unnecessary.
        Arc::try_unwrap(value.0).unwrap_or_else(|v| v.try_clone().expect("out of fds/handles"))
    }
}

#[cfg(unix)]
impl std::os::fd::AsFd for Mappable {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsHandle for Mappable {
    fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl DefaultEncoding for Mappable {
    type Encoding = ResourceField<OwnedType>;
}
