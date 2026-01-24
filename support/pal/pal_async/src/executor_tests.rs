// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests common to every executor.

// Uses futures channels, but is only test code.
#![expect(clippy::disallowed_methods)]

use crate::driver::Driver;
use crate::socket::PolledSocket;
use crate::task::Spawn;
use crate::task::with_current_task_metadata;
use crate::timer::Instant;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::FutureExt;
use futures::channel::oneshot;
use futures::executor::block_on;
use pal_event::Event;
use parking_lot::Mutex;
use std::future::poll_fn;
#[cfg(unix)]
use std::os::unix::prelude::*;
#[cfg(windows)]
use std::os::windows::prelude::*;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use unix_socket::UnixListener;
use unix_socket::UnixStream;

/// Runs waker-related tests.
pub async fn waker_tests() {
    let (send, recv) = oneshot::channel();
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(100));
        send.send(()).unwrap();
    });
    recv.await.unwrap();
}

/// Runs spawn-related tests.
pub fn spawn_tests<S, F>(mut f: impl FnMut() -> (S, F))
where
    S: Spawn,
    F: 'static + FnOnce() + Send,
{
    // Validate that there is no current task after the thread is done.
    let mut f = move || {
        let (spawn, run) = f();
        let run = move || {
            run();
            with_current_task_metadata(|metadata| assert!(metadata.is_none()));
        };
        (spawn, run)
    };

    // no tasks
    {
        let (_, run) = f();
        run();
    }

    // ready task
    {
        let (spawn, run) = f();
        let t = std::thread::spawn(run);
        let h = spawn.spawn("ready", std::future::ready(()));
        block_on(h);
        drop(spawn);
        t.join().unwrap();
    }

    // pending task
    {
        let (spawn, run) = f();
        let t = std::thread::spawn(run);
        let (send, recv) = oneshot::channel::<()>();
        let mut h = spawn.spawn("pending", recv);
        drop(spawn);
        std::thread::sleep(Duration::from_millis(100));
        assert!((&mut h).now_or_never().is_none());
        drop(send);
        let _ = block_on(h);
        t.join().unwrap();
    }
}

/// Runs timer-related tests.
pub async fn sleep_tests(driver: impl Driver) {
    let now = Instant::now();
    let duration = Duration::from_millis(250);
    let mut timer = driver.new_dyn_timer();
    timer.set_deadline(now);
    poll_fn(|cx| timer.poll_timer(cx, Some(now + duration))).await;
    assert!(Instant::now() - now >= duration);

    let timer = Arc::new(Mutex::new(driver.new_dyn_timer()));
    let started = Instant::now();
    timer
        .lock()
        .set_deadline(started + Duration::from_secs(1000));
    let (send, mut recv) = oneshot::channel();
    std::thread::spawn({
        let timer = timer.clone();
        move || {
            let now = block_on(poll_fn(|cx| timer.lock().poll_timer(cx, None)));
            send.send(now).unwrap();
        }
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!((&mut recv).now_or_never().is_none());
    timer.lock().set_deadline(started + duration);
    let done_at = recv.await.unwrap();
    let now = Instant::now();
    assert!(done_at >= started + duration);
    assert!(done_at <= now);
}

async fn pend_once() {
    let mut once = false;
    poll_fn(|cx| {
        cx.waker().wake_by_ref();
        if once {
            Poll::Ready(())
        } else {
            once = true;
            Poll::Pending
        }
    })
    .await
}

/// Runs wait-related tests.
pub async fn wait_tests(driver: impl Driver) {
    let event = Event::new();
    #[cfg(windows)]
    let mut poller = driver
        .new_dyn_wait(event.as_handle().as_raw_handle())
        .unwrap();
    #[cfg(unix)]
    let mut poller = driver.new_dyn_wait(event.as_fd().as_raw_fd(), 8).unwrap();
    let mut op = poll_fn(|cx| poller.poll_wait(cx));
    assert!(futures::poll!(&mut op).is_pending());
    pend_once().await;
    event.signal();
    op.await.unwrap();
    assert!(poll_fn(|cx| poller.poll_wait(cx)).now_or_never().is_none());
    event.signal();
    // Kick off a poll.
    assert!(poll_fn(|cx| poller.poll_wait(cx)).now_or_never().is_none());
    // Pend so that the poll completes internally.
    pend_once().await;
    // Cancel. For some executors, the signal will be present.
    if poll_fn(|cx| poller.poll_cancel_wait(cx)).await {
        println!("signal was present at cancel");
        // A second cancel should not return a signal.
        assert!(!poll_fn(|cx| poller.poll_cancel_wait(cx)).await);
    }
    pend_once().await;
    assert!(poll_fn(|cx| poller.poll_wait(cx)).now_or_never().is_none());
}

/// Runs socket-related tests.
pub async fn socket_tests(driver: impl Driver) {
    // send/close/recv
    {
        let (a, b) = UnixStream::pair().unwrap();
        let mut a = PolledSocket::new(&driver, a).unwrap();
        let mut b = PolledSocket::new(&driver, b).unwrap();
        let mut buf = Vec::new();
        let mut op = a.read_to_end(&mut buf);
        assert!(futures::poll!(&mut op).is_pending());

        b.write_all(b"hello world").await.unwrap();
        b.close().await.unwrap();
        op.await.unwrap();
        assert_eq!(&buf, b"hello world");
    }

    // accept/connect
    {
        let listener = tempfile::Builder::new()
            .make(|path| UnixListener::bind(path))
            .unwrap();
        let mut l = PolledSocket::new(&driver, listener.as_file()).unwrap();
        let _c = PolledSocket::connect_unix(&driver, listener.path())
            .await
            .unwrap();
        let _s = l.accept().await.unwrap();
    }

    // read then write, to check for changing interests
    {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut a = PolledSocket::new(&driver, a).unwrap();
        let mut v = [0; 8];
        assert!(a.read(&mut v).now_or_never().is_none());
        a.write_all(b"hello world").await.unwrap();
    }
}

#[cfg(windows)]
pub mod windows {
    // UNSAFETY: needed to use `OverlappedFile`.
    #![allow(unsafe_code)]

    //! Windows-specific executor tests.

    use crate::driver::Driver;
    use crate::sys::overlapped::OverlappedFile;
    use crate::sys::pipe::NamedPipeServer;
    use std::fs::OpenOptions;
    use std::os::windows::prelude::*;
    use unicycle::FuturesUnordered;
    use winapi::um::winbase::FILE_FLAG_OVERLAPPED;

    /// Runs overlapped file tests.
    pub async fn overlapped_file_tests(driver: impl Driver) {
        // ordinary file
        {
            let temp_file = tempfile::NamedTempFile::new().unwrap();
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .attributes(FILE_FLAG_OVERLAPPED)
                .open(temp_file.path())
                .unwrap();
            // SAFETY: file is owned exclusively by the caller.
            let file = unsafe { OverlappedFile::new(&driver, file).unwrap() };
            file.write_at(0x1000, &b"abcdefg"[..]).await.0.unwrap();
            let b = vec![0u8; 7];
            let (r, b) = file.read_at(0, b).await;
            r.unwrap();
            assert_eq!(b.as_slice(), &[0; 7]);
            let (r, b) = file.read_at(0x1000, b).await;
            r.unwrap();
            assert_eq!(b.as_slice(), b"abcdefg");
        }

        // named pipe
        {
            let mut path = [0; 16];
            getrandom::fill(&mut path).unwrap();
            let path = format!(r#"\\.\pipe\{:0x}"#, u128::from_ne_bytes(path));
            let server = NamedPipeServer::create(&path).unwrap();
            let accept = server.accept(&driver).unwrap();
            let mut fut = FuturesUnordered::new();
            fut.push(accept);
            assert!(futures::poll!(fut.next()).is_pending());

            let client_pipe = OpenOptions::new()
                .read(true)
                .write(true)
                .attributes(FILE_FLAG_OVERLAPPED)
                .open(&path)
                .unwrap();

            let server_pipe = fut.next().await.unwrap().unwrap();

            // SAFETY: file is owned exclusively by the caller.
            let client_pipe = unsafe { OverlappedFile::new(&driver, client_pipe).unwrap() };
            // SAFETY: file is owned exclusively by the caller.
            let server_pipe = unsafe { OverlappedFile::new(&driver, server_pipe).unwrap() };

            // Drop case.
            let mut read = server_pipe.read_at(0, vec![0; 256]);
            assert!(futures::poll!(&mut read).is_pending());
            drop(read);

            // Cancel case.
            let mut read = server_pipe.read_at(0, vec![0; 256]);
            assert!(futures::poll!(&mut read).is_pending());
            read.cancel();
            assert_eq!(
                read.await.0.unwrap_err().raw_os_error(),
                Some(winapi::shared::winerror::ERROR_OPERATION_ABORTED as i32)
            );

            // Success case.
            let mut read = server_pipe.read_at(0, vec![0; 256]);
            assert!(futures::poll!(&mut read).is_pending());

            let data = b"hello, pipe!".as_slice();
            assert_eq!(client_pipe.write_at(0, data).await.0.unwrap(), data.len());
            let (n, buf) = read.await;
            let n = n.unwrap();
            assert_eq!(n, data.len());
            assert_eq!(&buf[..n], data);

            // Drop with pending IO.
            let mut read = server_pipe.read_at(0, vec![0; 256]);
            assert!(futures::poll!(&mut read).is_pending());
            drop(server_pipe);
        }
    }
}
