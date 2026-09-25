// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::windows::OpenWindowsPipeSerialConfig;
use crate::windows::WindowsPipeSerialBackend;
use futures::future::poll_fn;
use pal::windows::pipe::Disposition;
use pal::windows::pipe::PipeExt;
use pal::windows::pipe::PipeMode;
use pal::windows::pipe::new_named_pipe;
use pal_async::DefaultDriver;
use pal_async_test::async_test;
use serial_core::SerialIo;
use std::fs::OpenOptions;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::System::Pipes::PIPE_NOWAIT;

#[async_test]
async fn reconnects_after_client_close(driver: DefaultDriver) {
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
    let mut backend = WindowsPipeSerialBackend::new(
        Box::new(driver.clone()),
        OpenWindowsPipeSerialConfig::from(server),
    )
    .unwrap();

    let client = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    client.set_pipe_mode(PIPE_NOWAIT).unwrap();
    poll_fn(|cx| backend.poll_connect(cx)).await.unwrap();
    drop(client);
    poll_fn(|cx| backend.poll_disconnect(cx)).await.unwrap();
    backend.disconnect_current().unwrap();

    let _replacement = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    poll_fn(|cx| backend.poll_connect(cx)).await.unwrap();
}
