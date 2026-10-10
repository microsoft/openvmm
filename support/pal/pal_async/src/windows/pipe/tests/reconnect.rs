// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for reusing server pipes: detecting a closed client without a
//! disconnect event, and completing a listen for an already-connected
//! client.

use crate::DefaultDriver;
use crate::interest::InterestSlot;
use crate::sys::pipe::ListeningPipe;
use crate::sys::pipe::PolledPipe;
use futures::FutureExt;
use futures::future::poll_fn;
use pal::windows::chk_status;
use pal::windows::pipe::Disposition;
use pal::windows::pipe::FILE_PIPE_DISCONNECTED;
use pal::windows::pipe::FILE_PIPE_READ_READY;
use pal::windows::pipe::FILE_PIPE_WRITE_READY;
use pal::windows::pipe::PipeExt;
use pal::windows::pipe::PipeMode;
use pal::windows::pipe::new_named_pipe;
use pal_async_test::async_test;
use std::fs::OpenOptions;
use std::io;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use std::time::Duration;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;

impl PolledPipe {
    fn test_set_cached_events(&mut self, events: u32) {
        self.events = events;
    }

    fn test_set_select_events(&self, event_types: u32) -> io::Result<()> {
        self.file.set_pipe_select_event(&self._event, event_types)
    }

    fn test_is_pipe_peer_closed(&self) -> io::Result<bool> {
        self.file.is_pipe_peer_closed()
    }

    fn test_poll_closing_event_only(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.refresh_events()?;
        while self.events & FILE_PIPE_DISCONNECTED == 0 {
            ready!(
                self.wakers
                    .poll_wrapped(cx, InterestSlot::Read as usize, |cx| self
                        .wait
                        .poll_wait(cx))
            )?;

            self.refresh_events()?;
        }
        Poll::Ready(Ok(()))
    }
}

impl ListeningPipe {
    fn test_poll_without_sync_fastpath(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner = self.inner.as_mut().expect("polled after completion");
        ready!(inner.event.poll_wait(cx))?;
        let (status, _) = inner.overlapped.io_status().expect("io should be complete");
        chk_status(status)?;
        Poll::Ready(Ok(()))
    }
}

#[async_test]
async fn reuse_server_pipe_after_client_close(driver: DefaultDriver) {
    let mut id = [0; 16];
    getrandom::fill(&mut id).unwrap();
    let path = format!(r#"\\.\pipe\{:0x}"#, u128::from_ne_bytes(id));
    let server = new_named_pipe(
        &path,
        GENERIC_READ | GENERIC_WRITE,
        Disposition::Create,
        PipeMode::Byte,
    )
    .unwrap();

    let mut listener = ListeningPipe::new(&driver, server).unwrap();
    let client = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut connected = PolledPipe::new(&driver, listener.await.unwrap()).unwrap();

    drop(client);
    poll_fn(|cx| connected.poll_closing(cx)).await.unwrap();
    let server = connected.into_inner();
    server.disconnect_pipe().unwrap();

    listener = ListeningPipe::new(&driver, server).unwrap();
    let _client = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    listener.await.unwrap();
}

#[async_test]
async fn listening_pipe_ready_when_client_already_connected(driver: DefaultDriver) {
    let mut id = [0; 16];
    getrandom::fill(&mut id).unwrap();
    let path = format!(r#"\\.\pipe\{:0x}"#, u128::from_ne_bytes(id));
    let server = new_named_pipe(
        &path,
        GENERIC_READ | GENERIC_WRITE,
        Disposition::Create,
        PipeMode::Byte,
    )
    .unwrap();

    let _client = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();

    let mut listener = ListeningPipe::new(&driver, server).unwrap();
    let old_poll = poll_fn(|cx| listener.test_poll_without_sync_fastpath(cx));
    assert!(
        old_poll.now_or_never().is_none(),
        "event-only listen polling should stay pending in this setup"
    );
    let server = listener
        .now_or_never()
        .expect("listen future should complete immediately")
        .unwrap();
    assert!(server.is_pipe_connected().unwrap());
}

#[async_test]
async fn poll_closing_checks_pipe_state_without_disconnect_event(driver: DefaultDriver) {
    let mut id = [0; 16];
    getrandom::fill(&mut id).unwrap();
    let path = format!(r#"\\.\pipe\{:0x}"#, u128::from_ne_bytes(id));
    let server = new_named_pipe(
        &path,
        GENERIC_READ | GENERIC_WRITE,
        Disposition::Create,
        PipeMode::Byte,
    )
    .unwrap();

    let listener = ListeningPipe::new(&driver, server).unwrap();
    let client = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut connected = PolledPipe::new(&driver, listener.await.unwrap()).unwrap();

    connected
        .test_set_select_events(FILE_PIPE_READ_READY | FILE_PIPE_WRITE_READY)
        .unwrap();
    drop(client);
    connected.test_set_cached_events(FILE_PIPE_READ_READY | FILE_PIPE_WRITE_READY);
    for _ in 0..100 {
        if connected.test_is_pipe_peer_closed().unwrap() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(connected.test_is_pipe_peer_closed().unwrap());
    let old_poll = poll_fn(|cx| connected.test_poll_closing_event_only(cx));
    assert!(
        old_poll.now_or_never().is_none(),
        "event-only closing polling should stay pending when disconnected events are not selected"
    );

    let poll = poll_fn(|cx| connected.poll_closing(cx));
    let result = poll.now_or_never();
    assert!(
        matches!(result, Some(Ok(()))),
        "expected poll_closing to finish immediately when peer is closed"
    );
}
