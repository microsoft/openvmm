// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A connected serial backend that discards all output.

use crate::SerialIo;
use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::InspectMut;
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

/// A connected serial backend that accepts and discards output and never
/// produces input.
#[derive(Debug, InspectMut)]
pub struct NullSerialBackend;

impl AsyncRead for NullSerialBackend {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Pending
    }
}

impl AsyncWrite for NullSerialBackend {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl SerialIo for NullSerialBackend {
    fn is_connected(&self) -> bool {
        true
    }

    fn poll_connect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_disconnect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

/// Resolver support for [`NullSerialBackend`].
pub mod resolver {
    use super::NullSerialBackend;
    use crate::resources::NullSerialBackendHandle;
    use crate::resources::ResolveSerialBackendParams;
    use crate::resources::ResolvedSerialBackend;
    use std::convert::Infallible;
    use vm_resource::IntoResource;
    use vm_resource::ResolveResource;
    use vm_resource::Resource;
    use vm_resource::declare_static_resolver;
    use vm_resource::kind::SerialBackendHandle;

    /// A resolver for [`NullSerialBackendHandle`].
    pub struct NullSerialBackendResolver;

    declare_static_resolver! {
        NullSerialBackendResolver,
        (SerialBackendHandle, NullSerialBackendHandle),
    }

    impl ResolveResource<SerialBackendHandle, NullSerialBackendHandle> for NullSerialBackendResolver {
        type Output = ResolvedSerialBackend;
        type Error = Infallible;

        fn resolve(
            &self,
            NullSerialBackendHandle: NullSerialBackendHandle,
            _input: ResolveSerialBackendParams<'_>,
        ) -> Result<Self::Output, Self::Error> {
            Ok(NullSerialBackend.into())
        }
    }

    impl From<NullSerialBackend> for Resource<SerialBackendHandle> {
        fn from(NullSerialBackend: NullSerialBackend) -> Self {
            NullSerialBackendHandle.into_resource()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Waker;
    use test_with_tracing::test;

    #[test]
    fn null_backend_is_connected_and_discards_output() {
        let mut backend = NullSerialBackend;
        let mut cx = Context::from_waker(Waker::noop());

        assert!(backend.is_connected());
        assert!(matches!(backend.poll_connect(&mut cx), Poll::Ready(Ok(()))));
        assert!(matches!(
            Pin::new(&mut backend).poll_write(&mut cx, b"discarded"),
            Poll::Ready(Ok(9))
        ));
        assert!(matches!(
            Pin::new(&mut backend).poll_flush(&mut cx),
            Poll::Ready(Ok(()))
        ));

        let mut input = [0];
        assert!(
            Pin::new(&mut backend)
                .poll_read(&mut cx, &mut input)
                .is_pending()
        );
        assert!(backend.poll_disconnect(&mut cx).is_pending());
    }
}
