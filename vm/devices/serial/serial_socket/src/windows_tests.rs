// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::windows::OpenWindowsPipeSerialConfig;
use crate::windows::WindowsPipeSerialBackend;
use futures::AsyncWriteExt;
use futures::future::poll_fn;
use pal::windows::pipe::Disposition;
use pal::windows::pipe::PipeExt;
use pal::windows::pipe::PipeMode;
use pal::windows::pipe::new_named_pipe;
use pal_async::DefaultDriver;
use pal_async_test::async_test;
use serial_core::SerialIo;
use std::fs::File;
use std::fs::OpenOptions;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::System::Pipes::PIPE_NOWAIT;

fn new_backend(driver: DefaultDriver) -> (WindowsPipeSerialBackend, String) {
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
    let backend =
        WindowsPipeSerialBackend::new(Box::new(driver), OpenWindowsPipeSerialConfig::from(server))
            .unwrap();
    (backend, path)
}

async fn connect_client(backend: &mut WindowsPipeSerialBackend, path: &str) -> File {
    let client = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    client.set_pipe_mode(PIPE_NOWAIT).unwrap();
    poll_fn(|cx| backend.poll_connect(cx)).await.unwrap();
    assert!(backend.is_connected());
    client
}

#[async_test]
async fn reconnects_after_client_close(driver: DefaultDriver) {
    let (mut backend, path) = new_backend(driver);
    let client = connect_client(&mut backend, &path).await;
    drop(client);
    assert!(backend.write(b"lost connection").await.is_err());
    assert!(!backend.is_connected());

    let _replacement = connect_client(&mut backend, &path).await;
}

#[async_test]
async fn reconnects_after_close_failure(driver: DefaultDriver) {
    let (mut backend, path) = new_backend(driver);
    let client = connect_client(&mut backend, &path).await;
    drop(client);

    let error = backend.close().await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert!(!backend.is_connected());

    let _replacement = connect_client(&mut backend, &path).await;
}
