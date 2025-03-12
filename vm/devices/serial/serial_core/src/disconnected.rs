// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Disconnected serial backend implementation.

use crate::SerialIo;
use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::InspectMut;
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

/// A [`SerialIo`] implementation that is always disconnected.
#[derive(Debug, InspectMut)]
pub struct Disconnected;

impl SerialIo for Disconnected {
    fn is_connected(&self) -> bool {
        false
    }

    fn poll_connect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }

    fn poll_disconnect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for Disconnected {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(0))
    }
}

impl AsyncWrite for Disconnected {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Resolver support for [`Disconnected`].
pub mod resolver {
    use super::Disconnected;
    use crate::resources::DisconnectedSerialBackendHandle;
    use crate::resources::ResolveSerialBackendParams;
    use crate::resources::ResolvedSerialBackend;
    use std::convert::Infallible;
    use vm_resource::IntoResource;
    use vm_resource::ResolveResource;
    use vm_resource::Resource;
    use vm_resource::declare_static_resolver;
    use vm_resource::kind::SerialBackendHandle;

    /// A resolver for [`DisconnectedSerialBackendHandle`].
    pub struct DisconnectedSerialBackendResolver;

    declare_static_resolver! {
        DisconnectedSerialBackendResolver,
        (SerialBackendHandle, DisconnectedSerialBackendHandle),
    }

    impl ResolveResource<SerialBackendHandle, DisconnectedSerialBackendHandle>
        for DisconnectedSerialBackendResolver
    {
        type Output = ResolvedSerialBackend;
        type Error = Infallible;

        fn resolve(
            &self,
            DisconnectedSerialBackendHandle: DisconnectedSerialBackendHandle,
            _input: ResolveSerialBackendParams<'_>,
        ) -> Result<Self::Output, Self::Error> {
            Ok(Disconnected.into())
        }
    }

    impl From<Disconnected> for Resource<SerialBackendHandle> {
        fn from(Disconnected: Disconnected) -> Self {
            DisconnectedSerialBackendHandle.into_resource()
        }
    }
}
