// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use thiserror::Error;

/// An error representing a failure of a channel.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct ChannelError(Box<ChannelErrorInner>);

/// The kind of channel failure.
#[derive(Debug)]
#[non_exhaustive]
pub enum ChannelErrorKind {
    /// The peer node failed.
    NodeFailure,
    /// The received message contents are invalid.
    Corruption,
}

impl ChannelError {
    /// Returns the kind of channel failure that occurred.
    pub fn kind(&self) -> ChannelErrorKind {
        match &*self.0 {
            ChannelErrorInner::NodeFailure(_) => ChannelErrorKind::NodeFailure,
            ChannelErrorInner::Corruption(_) => ChannelErrorKind::Corruption,
        }
    }
}

impl From<mesh_protobuf::Error> for ChannelError {
    fn from(err: mesh_protobuf::Error) -> Self {
        Self(Box::new(ChannelErrorInner::Corruption(err)))
    }
}

impl From<mesh_node::local_node::NodeError> for ChannelError {
    fn from(value: mesh_node::local_node::NodeError) -> Self {
        Self(Box::new(ChannelErrorInner::NodeFailure(value)))
    }
}

#[derive(Debug, Error)]
enum ChannelErrorInner {
    #[error("node failure")]
    NodeFailure(#[source] mesh_node::local_node::NodeError),
    #[error("message corruption")]
    Corruption(#[source] mesh_protobuf::Error),
}

/// An error when trying to receive a message from a channel.
#[derive(Debug, Error)]
pub enum TryRecvError {
    /// The channel is empty.
    #[error("channel empty")]
    Empty,
    /// The channel is closed.
    #[error("channel closed")]
    Closed,
    /// The channel has failed.
    #[error("channel failure")]
    Error(#[from] ChannelError),
}

/// An error when receiving a message from a channel.
#[derive(Debug, Error)]
pub enum RecvError {
    /// The channel is closed.
    #[error("channel closed")]
    Closed,
    /// The channel has failed.
    #[error("channel failure")]
    Error(#[from] ChannelError),
}
