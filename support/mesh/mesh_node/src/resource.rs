// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh resource definitions.
//!
//! Resources are things that can be sent across a mesh node boundary that
//! cannot be serialized to a protobuf field type. This includes mesh ports,
//! Unix file descriptors, and Windows file handles and sockets.

use crate::local_node::Port;
use thiserror::Error;

#[derive(Debug)]
pub enum OsResource {
    #[cfg(unix)]
    Fd(std::os::unix::io::OwnedFd),
    #[cfg(windows)]
    Handle(std::os::windows::io::OwnedHandle),
    #[cfg(windows)]
    Socket(std::os::windows::io::OwnedSocket),
}

/// A resource that can be sent via a port.
#[derive(Debug)]
pub enum Resource {
    /// Another port.
    Port(Port),
    /// An OS resource (file descriptor, Windows handle, or socket).
    Os(OsResource),
}

impl From<Port> for Resource {
    fn from(port: Port) -> Self {
        Self::Port(port)
    }
}

#[derive(Debug, Error)]
pub enum ResourceError {
    #[error("wrong resource type")]
    BadResourceType,
    #[cfg(windows)]
    #[error("failed to convert handle to socket")]
    HandleToSocket(#[source] std::io::Error),
}

impl TryFrom<Resource> for Port {
    type Error = ResourceError;

    fn try_from(value: Resource) -> Result<Self, ResourceError> {
        match value {
            Resource::Port(port) => Ok(port),
            _ => Err(ResourceError::BadResourceType),
        }
    }
}

#[cfg(unix)]
impl From<std::os::unix::io::OwnedFd> for Resource {
    fn from(fd: std::os::unix::io::OwnedFd) -> Self {
        Self::Os(OsResource::Fd(fd))
    }
}

#[cfg(unix)]
impl TryFrom<Resource> for std::os::unix::io::OwnedFd {
    type Error = ResourceError;

    fn try_from(value: Resource) -> Result<Self, ResourceError> {
        match value {
            Resource::Os(OsResource::Fd(fd)) => Ok(fd),
            _ => Err(ResourceError::BadResourceType),
        }
    }
}

#[cfg(windows)]
impl From<std::os::windows::io::OwnedHandle> for Resource {
    fn from(port: std::os::windows::io::OwnedHandle) -> Self {
        Self::Os(OsResource::Handle(port))
    }
}

#[cfg(windows)]
impl TryFrom<Resource> for std::os::windows::io::OwnedHandle {
    type Error = ResourceError;

    fn try_from(value: Resource) -> Result<Self, ResourceError> {
        match value {
            Resource::Os(OsResource::Handle(handle)) => Ok(handle),
            _ => Err(ResourceError::BadResourceType),
        }
    }
}

#[cfg(windows)]
impl From<std::os::windows::io::OwnedSocket> for Resource {
    fn from(port: std::os::windows::io::OwnedSocket) -> Self {
        Self::Os(OsResource::Socket(port))
    }
}

#[cfg(windows)]
impl TryFrom<Resource> for std::os::windows::io::OwnedSocket {
    type Error = ResourceError;

    fn try_from(value: Resource) -> Result<Self, ResourceError> {
        match value {
            Resource::Os(OsResource::Socket(socket)) => Ok(socket),
            Resource::Os(OsResource::Handle(handle)) => {
                pal::windows::OwnedSocketExt::from_handle(handle)
                    .map_err(ResourceError::HandleToSocket)
            }
            _ => Err(ResourceError::BadResourceType),
        }
    }
}

pub type SerializedMessage = mesh_protobuf::SerializedMessage<Resource>;
