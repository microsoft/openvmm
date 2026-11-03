// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Output-only serial backend using synchronous host-file writes.

use futures::AsyncRead;
use futures::AsyncWrite;
use inspect::InspectMut;
use mesh::MeshPayload;
use serial_core::SerialIo;
use serial_core::resources::DisconnectedSerialBackendHandle;
use serial_core::resources::ResolveSerialBackendParams;
use serial_core::resources::ResolvedSerialBackend;
use std::convert::Infallible;
use std::fs::File;
use std::io;
use std::io::Write;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::task::ready;
use vm_resource::IntoResource;
use vm_resource::ResolveResource;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::declare_static_resolver;
use vm_resource::kind::SerialBackendHandle;

#[derive(Debug, MeshPayload)]
pub struct FileSerialConfig {
    output: File,
    path: String,
}

impl FileSerialConfig {
    pub fn new(output: File, path: String) -> Self {
        Self { output, path }
    }
}

impl ResourceId<SerialBackendHandle> for FileSerialConfig {
    const ID: &'static str = "file";
}

pub struct FileSerialResolver;
declare_static_resolver!(FileSerialResolver, (SerialBackendHandle, FileSerialConfig));

impl ResolveResource<SerialBackendHandle, FileSerialConfig> for FileSerialResolver {
    type Output = ResolvedSerialBackend;
    type Error = Infallible;

    fn resolve(
        &self,
        config: FileSerialConfig,
        _input: ResolveSerialBackendParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(FileSerialBackend::new(config).into())
    }
}

#[derive(InspectMut)]
struct FileSerialBackend {
    #[inspect(skip)]
    output: Option<File>,
    #[inspect(skip)]
    path: String,
    #[inspect(skip)]
    read_waker: Option<Waker>,
    #[inspect(skip)]
    disconnect_waker: Option<Waker>,
}

impl FileSerialBackend {
    fn new(config: FileSerialConfig) -> Self {
        Self {
            output: Some(config.output),
            path: config.path,
            read_waker: None,
            disconnect_waker: None,
        }
    }

    fn disconnect(&mut self) {
        self.output = None;
        if let Some(waker) = self.read_waker.take() {
            waker.wake();
        }
        if let Some(waker) = self.disconnect_waker.take() {
            waker.wake();
        }
    }

    fn fail(&mut self, error: &io::Error) {
        tracing::error!(
            error = error as &dyn std::error::Error,
            path = %self.path,
            "serial file backend stopped"
        );
        self.disconnect();
    }
}

impl From<FileSerialBackend> for Resource<SerialBackendHandle> {
    fn from(backend: FileSerialBackend) -> Self {
        match backend.output {
            Some(output) => FileSerialConfig::new(output, backend.path).into_resource(),
            None => DisconnectedSerialBackendHandle.into_resource(),
        }
    }
}

impl SerialIo for FileSerialBackend {
    fn is_connected(&self) -> bool {
        self.output.is_some()
    }

    fn poll_connect(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.is_connected() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.is_connected() {
            self.disconnect_waker = Some(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

impl AsyncRead for FileSerialBackend {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() || !self.is_connected() {
            Poll::Ready(Ok(0))
        } else {
            self.read_waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl AsyncWrite for FileSerialBackend {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let Some(output) = &mut self.output else {
            return Poll::Ready(Ok(buf.len()));
        };

        // Intentionally synchronous: successful writes have reached the host
        // file before returning, with no relay or background work to reclaim.
        let result = loop {
            match output.write(buf) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(0) => break Err(io::ErrorKind::WriteZero.into()),
                result => break result,
            }
        };
        if let Err(error) = &result {
            self.fail(error);
        }
        Poll::Ready(result)
    }

    fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = match &mut self.output {
            Some(output) => output.flush(),
            None => Ok(()),
        };
        if let Err(error) = &result {
            self.fail(error);
        }
        Poll::Ready(result)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.as_mut().poll_flush(cx))?;
        self.disconnect();
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncWriteExt;
    use futures::task::ArcWake;
    use pal_async::DefaultPool;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;

    fn backend(output: File) -> FileSerialBackend {
        FileSerialBackend::new(FileSerialConfig::new(output, "test-console.log".into()))
    }

    #[test]
    fn writes_complete_before_returning() {
        let output = tempfile::NamedTempFile::new().unwrap();
        let mut io = backend(output.reopen().unwrap());
        let mut cx = Context::from_waker(Waker::noop());
        let mut expected = Vec::new();

        for len in [1, 20, 4096, 128 * 1024] {
            let data = vec![b'a'; len];
            let mut remaining = data.as_slice();
            while !remaining.is_empty() {
                let Poll::Ready(Ok(n)) = Pin::new(&mut io).poll_write(&mut cx, remaining) else {
                    panic!("file write must complete synchronously");
                };
                assert!(n > 0);
                expected.extend_from_slice(&remaining[..n]);
                remaining = &remaining[n..];
                assert_eq!(std::fs::read(output.path()).unwrap(), expected);
            }
        }
        assert!(matches!(
            Pin::new(&mut io).poll_write(&mut cx, &[]),
            Poll::Ready(Ok(0))
        ));
        drop(io);
        assert_eq!(std::fs::read(output.path()).unwrap(), expected);
    }

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl ArcWake for WakeCount {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn failure_and_close_disconnect_and_wake_waiters() {
        for fail in [false, true] {
            let output = tempfile::NamedTempFile::new().unwrap();
            let file = if fail {
                File::open(output.path()).unwrap()
            } else {
                output.reopen().unwrap()
            };
            let mut io = backend(file);
            let read_wakes = Arc::new(WakeCount::default());
            let disconnect_wakes = Arc::new(WakeCount::default());
            let read_waker = futures::task::waker(read_wakes.clone());
            let disconnect_waker = futures::task::waker(disconnect_wakes.clone());
            let mut cx = Context::from_waker(Waker::noop());
            assert!(io.is_connected());
            assert!(matches!(io.poll_connect(&mut cx), Poll::Ready(Ok(()))));
            assert!(matches!(
                Pin::new(&mut io).poll_read(&mut cx, &mut []),
                Poll::Ready(Ok(0))
            ));
            assert!(
                Pin::new(&mut io)
                    .poll_read(&mut Context::from_waker(&read_waker), &mut [0])
                    .is_pending()
            );
            assert!(
                io.poll_disconnect(&mut Context::from_waker(&disconnect_waker))
                    .is_pending()
            );
            if fail {
                assert!(matches!(
                    Pin::new(&mut io).poll_write(&mut cx, b"read-only file"),
                    Poll::Ready(Err(_))
                ));
            } else {
                assert!(matches!(
                    Pin::new(&mut io).poll_close(&mut cx),
                    Poll::Ready(Ok(()))
                ));
            }
            assert!(!io.is_connected());
            assert!(read_wakes.0.load(Ordering::Relaxed) > 0);
            assert!(disconnect_wakes.0.load(Ordering::Relaxed) > 0);
            assert!(io.poll_connect(&mut cx).is_pending());
            assert!(matches!(io.poll_disconnect(&mut cx), Poll::Ready(Ok(()))));
            assert!(matches!(
                Pin::new(&mut io).poll_read(&mut cx, &mut [0]),
                Poll::Ready(Ok(0))
            ));
            assert!(matches!(
                Pin::new(&mut io).poll_write(&mut cx, b"discarded"),
                Poll::Ready(Ok(9))
            ));
            assert!(matches!(
                Pin::new(&mut io).poll_flush(&mut cx),
                Poll::Ready(Ok(()))
            ));
        }
    }

    #[test]
    fn reclaim_preserves_append_and_truncate() {
        DefaultPool::run_with(async |driver| {
            for append in [false, true] {
                let output = tempfile::NamedTempFile::new().unwrap();
                std::fs::write(output.path(), b"stale").unwrap();
                let file = File::options()
                    .write(true)
                    .append(append)
                    .truncate(!append)
                    .open(output.path())
                    .unwrap();
                let config =
                    FileSerialConfig::new(file, output.path().to_string_lossy().into_owned());
                let mut resolver = vm_resource::ResourceResolver::new();
                resolver.add_resolver(FileSerialResolver);
                let mut resolved = resolver
                    .resolve(
                        config.into_resource(),
                        ResolveSerialBackendParams {
                            driver: Box::new(driver.clone()),
                            _async_trait_workaround: &(),
                        },
                    )
                    .await
                    .unwrap();
                resolved.0.as_io_mut().write_all(b"first").await.unwrap();
                let mut expected = if append {
                    b"stale".to_vec()
                } else {
                    Vec::new()
                };
                expected.extend_from_slice(b"first");
                assert_eq!(std::fs::read(output.path()).unwrap(), expected);
                let resource = resolved.0.into_resource();

                let resolved = resolver
                    .resolve(
                        resource,
                        ResolveSerialBackendParams {
                            driver: Box::new(driver.clone()),
                            _async_trait_workaround: &(),
                        },
                    )
                    .await
                    .unwrap();
                let mut io = resolved.0.into_io();
                io.write_all(b"second").await.unwrap();
                expected.extend_from_slice(b"second");
                assert_eq!(std::fs::read(output.path()).unwrap(), expected);
                drop(io);
                assert_eq!(std::fs::read(output.path()).unwrap(), expected);
            }
        });
    }
}
