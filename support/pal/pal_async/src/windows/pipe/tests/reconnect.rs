// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for server reuse, closure during event registration, and listening
//! for an already-connected client.

use crate::DefaultDriver;
use crate::sys::pipe::ListeningPipe;
use crate::sys::pipe::PolledPipe;
use futures::AsyncReadExt;
use futures::AsyncWrite;
use futures::FutureExt;
use futures::future::poll_fn;
use pal::windows::pipe::Disposition;
use pal::windows::pipe::PipeExt;
use pal::windows::pipe::PipeMode;
use pal::windows::pipe::new_named_pipe;
use pal_async_test::async_test;
use std::fs::OpenOptions;
use std::io::Read;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use test_with_tracing::test;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;

#[async_test]
async fn reuse_server_pipe_after_client_close(driver: DefaultDriver) {
    let mut id = [0; 16];
    getrandom::fill(&mut id).unwrap();
    let path = format!(r#"\\.\pipe\{:0x}"#, u128::from_ne_bytes(id));
    let mut server = new_named_pipe(
        &path,
        GENERIC_READ | GENERIC_WRITE,
        Disposition::Create,
        PipeMode::Byte,
    )
    .unwrap();

    // Close before, after, or during registration, or after a pending poll.
    for generation in 0..16 {
        let listener = ListeningPipe::new(&driver, server).unwrap();
        let mut client = Some(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap(),
        );
        let file = listener.await.unwrap();
        let timing = generation % 4;
        if timing == 0 {
            drop(client.take());
        }
        let closer = if timing == 3 {
            let client = client.take().unwrap();
            Some(std::thread::spawn(move || drop(client)))
        } else {
            None
        };
        let mut connected = PolledPipe::new(&driver, file).unwrap();
        let mut closing = poll_fn(|cx| connected.poll_closing(cx));
        if timing == 2 {
            assert!(futures::poll!(&mut closing).is_pending());
        }
        drop(client);
        if let Some(closer) = closer {
            closer.join().unwrap();
        }
        closing.await.unwrap();
        server = connected.into_inner();
        server.disconnect_pipe().unwrap();
    }
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

    let listener = ListeningPipe::new(&driver, server).unwrap();
    let server = listener
        .now_or_never()
        .expect("listen future should complete immediately")
        .unwrap();
    assert!(server.is_pipe_connected().unwrap());
}

#[async_test]
async fn poll_closing_ready_when_client_closed_before_registration(driver: DefaultDriver) {
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
    let server = listener.await.unwrap();
    drop(client);
    let mut connected = PolledPipe::new(&driver, server).unwrap();

    let poll = poll_fn(|cx| connected.poll_closing(cx));
    let result = poll.now_or_never();
    assert!(
        matches!(result, Some(Ok(()))),
        "expected poll_closing to finish immediately when peer is closed"
    );
}

#[async_test]
async fn client_read_completes_when_server_disconnects(driver: DefaultDriver) {
    let mut id = [0; 16];
    getrandom::fill(&mut id).unwrap();
    let path = format!(r"\\.\pipe\{:0x}", u128::from_ne_bytes(id));
    let server = new_named_pipe(
        &path,
        GENERIC_READ | GENERIC_WRITE,
        Disposition::Create,
        PipeMode::Byte,
    )
    .unwrap();
    let client = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut client = PolledPipe::new(&driver, client).unwrap();
    let mut buffer = [0];
    let mut read = client.read(&mut buffer);
    assert!(futures::poll!(&mut read).is_pending());
    server.disconnect_pipe().unwrap();
    assert_eq!(read.await.unwrap(), 0);
}

#[async_test]
async fn poll_closing_preserves_write_readiness(driver: DefaultDriver) {
    let mut id = [0; 16];
    getrandom::fill(&mut id).unwrap();
    let path = format!(r"\\.\pipe\{:0x}", u128::from_ne_bytes(id));
    let server = new_named_pipe(
        &path,
        GENERIC_READ | GENERIC_WRITE,
        Disposition::Create,
        PipeMode::Byte,
    )
    .unwrap();
    let listener = ListeningPipe::new(&driver, server).unwrap();
    let mut client = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut connected = PolledPipe::new(&driver, listener.await.unwrap()).unwrap();
    let mut cx = Context::from_waker(std::task::Waker::noop());
    let mut written = 0;
    loop {
        match Pin::new(&mut connected).poll_write(&mut cx, &[0; 4096]) {
            Poll::Ready(Ok(n)) => {
                assert!(n > 0);
                written += n;
            }
            Poll::Ready(Err(err)) => panic!("{err}"),
            Poll::Pending => break,
        }
    }
    assert!(connected.poll_closing(&mut cx).is_pending());
    client.read_exact(&mut vec![0; written]).unwrap();
    let n = poll_fn(|cx| {
        assert!(connected.poll_closing(cx).is_pending());
        Pin::new(&mut connected).poll_write(cx, b"x")
    })
    .await
    .unwrap();
    assert_eq!(n, 1);
}
