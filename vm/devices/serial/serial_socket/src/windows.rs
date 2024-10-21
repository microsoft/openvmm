// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows named pipe serial backend.

use futures::AsyncRead;
use futures::AsyncWrite;
use futures::FutureExt;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::MeshPayload;
use pal::windows::pipe::PipeExt;
use pal_async::driver::Driver;
use pal_async::pipe::PolledPipe;
use pal_async::windows::pipe::ListeningPipe;
use serial_core::resources::ResolveSerialBackendParams;
use serial_core::resources::ResolvedSerialBackend;
use serial_core::SerialIo;
use std::fs::File;
use std::io;
use std::pin::Pin;
use std::task::ready;
use std::task::Context;
use std::task::Poll;
use vm_resource::declare_static_resolver;
use vm_resource::kind::SerialBackendHandle;
use vm_resource::ResolveResource;
use vm_resource::Resource;
use vm_resource::ResourceId;

#[derive(Debug, MeshPayload)]
pub struct OpenWindowsPipeSerialConfig {
    pub pipe: Option<File>,
}

impl From<File> for OpenWindowsPipeSerialConfig {
    fn from(pipe: File) -> Self {
        Self { pipe: Some(pipe) }
    }
}

#[derive(InspectMut)]
pub struct WindowsPipeSerialBackend {
    #[inspect(skip)]
    driver: Box<dyn Driver>,
    state: PipeState,
}

enum PipeState {
    Done,
    Listening(ListeningPipe),
    Connected(PolledPipe),
}

impl Inspect for PipeState {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.value(
            match self {
                PipeState::Done => "done",
                PipeState::Listening(_) => "listening",
                PipeState::Connected(_) => "connected",
            }
            .into(),
        )
    }
}

impl ResourceId<SerialBackendHandle> for OpenWindowsPipeSerialConfig {
    const ID: &'static str = "windows_named_pipe";
}

pub struct WindowsPipeSerialResolver;
declare_static_resolver!(
    WindowsPipeSerialResolver,
    (SerialBackendHandle, OpenWindowsPipeSerialConfig)
);

impl ResolveResource<SerialBackendHandle, OpenWindowsPipeSerialConfig>
    for WindowsPipeSerialResolver
{
    type Output = ResolvedSerialBackend;
    type Error = io::Error;

    fn resolve(
        &self,
        rsrc: OpenWindowsPipeSerialConfig,
        input: ResolveSerialBackendParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(WindowsPipeSerialBackend::new(input.driver, rsrc)?.into())
    }
}

impl WindowsPipeSerialBackend {
    pub fn new(driver: Box<dyn Driver>, config: OpenWindowsPipeSerialConfig) -> io::Result<Self> {
        let state = if let Some(file) = config.pipe {
            if file.is_pipe_connected()? {
                PipeState::Connected(PolledPipe::new(&driver, file)?)
            } else {
                PipeState::Listening(ListeningPipe::new(&driver, file)?)
            }
        } else {
            PipeState::Done
        };

        Ok(Self { driver, state })
    }

    pub fn into_config(self) -> OpenWindowsPipeSerialConfig {
        let file = match self.state {
            PipeState::Done => None,
            PipeState::Listening(accept) => Some(accept.into_inner()),
            PipeState::Connected(pipe) => Some(pipe.into_inner()),
        };
        OpenWindowsPipeSerialConfig { pipe: file }
    }

    fn disconnect(&mut self) -> io::Result<()> {
        if let PipeState::Connected(pipe) = std::mem::replace(&mut self.state, PipeState::Done) {
            let pipe = pipe.into_inner();
            pipe.disconnect_pipe()?;
            self.state = PipeState::Listening(ListeningPipe::new(&self.driver, pipe)?);
        }
        Ok(())
    }
}

impl From<WindowsPipeSerialBackend> for Resource<SerialBackendHandle> {
    fn from(value: WindowsPipeSerialBackend) -> Self {
        Resource::new(value.into_config())
    }
}

impl SerialIo for WindowsPipeSerialBackend {
    fn is_connected(&self) -> bool {
        matches!(self.state, PipeState::Connected(_))
    }

    fn poll_connect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.state {
            PipeState::Done => Poll::Pending,
            PipeState::Listening(accept) => {
                let file = ready!(accept.poll_unpin(cx));
                self.state = PipeState::Done;
                self.state = PipeState::Connected(PolledPipe::new(&self.driver, file?)?);
                Poll::Ready(Ok(()))
            }
            PipeState::Connected(_) => Poll::Ready(Ok(())),
        }
    }

    fn poll_disconnect(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.state {
            PipeState::Done | PipeState::Listening(_) => Poll::Ready(Ok(())),
            PipeState::Connected(pipe) => {
                ready!(pipe.poll_closing(cx))?;
                if let Err(err) = self.disconnect() {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "failed to prepare named pipe for reconnection"
                    );
                }
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl AsyncRead for WindowsPipeSerialBackend {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.state {
            PipeState::Done | PipeState::Listening(_) => Poll::Ready(Ok(0)),
            PipeState::Connected(pipe) => {
                let r = ready!(Pin::new(pipe).poll_read(cx, buf));
                if matches!(r, Ok(0)) {
                    if let Err(err) = self.disconnect() {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "failed to prepare named pipe for reconnection"
                        );
                    }
                }
                Poll::Ready(r)
            }
        }
    }
}

impl AsyncWrite for WindowsPipeSerialBackend {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.state {
            PipeState::Done | PipeState::Listening(_) => Poll::Ready(Ok(buf.len())),
            PipeState::Connected(pipe) => {
                let r = ready!(Pin::new(pipe).poll_write(cx, buf));
                if matches!(&r, Err(err) if err.kind() == io::ErrorKind::BrokenPipe) {
                    return Poll::Ready(Ok(buf.len()));
                }
                Poll::Ready(r)
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.state {
            PipeState::Done | PipeState::Listening(_) => Poll::Ready(Ok(())),
            PipeState::Connected(pipe) => {
                let r = ready!(Pin::new(pipe).poll_flush(cx));
                if matches!(&r, Err(err) if err.kind() == io::ErrorKind::BrokenPipe) {
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(r)
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.state {
            PipeState::Done | PipeState::Listening(_) => Poll::Ready(Ok(())),
            PipeState::Connected(pipe) => {
                let r = ready!(Pin::new(pipe).poll_close(cx));
                if matches!(&r, Err(err) if err.kind() == io::ErrorKind::BrokenPipe) {
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(r)
            }
        }
    }
}
