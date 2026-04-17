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

#[cfg(target_os = "linux")]
pub mod io_uring_tests {
    //! io-uring submission tests.
    //!
    //! These test the full path from `Driver::io_uring_submit()` through
    //! SQE queuing, flush, kernel completion, and future wakeup.

    // UNSAFETY: `IoUringSubmit::submit` is unsafe.
    #![expect(unsafe_code)]

    use crate::driver::Driver;
    use crate::io_uring::IoUringSubmit;
    use io_uring::opcode;
    use io_uring::types;
    use std::task::Poll;

    /// Runs all io-uring tests.
    pub async fn uring_tests(driver: impl Driver) {
        let uring = driver
            .io_uring_submit()
            .expect("driver does not support io-uring");

        uring_nop(uring).await;
        uring_probe(uring).await;
        uring_multiple_nops(uring).await;
        uring_sq_full(uring).await;
        uring_cq_saturation(uring).await;
        uring_remote_submit(uring).await;
        uring_remote_batch(uring).await;
        uring_busy_loop_completions(uring).await;
        uring_read_write(uring).await;
        uring_pipe_round_trip(uring).await;
    }

    /// Submit a single NOP and await its completion.
    async fn uring_nop(uring: &dyn IoUringSubmit) {
        let sqe = opcode::Nop::new().build();
        // SAFETY: NOP references no memory.
        let result = unsafe { uring.submit(sqe) }.await.unwrap();
        assert_eq!(result, 0);
    }

    /// Verify probe returns true for NOP and false for an invalid opcode.
    async fn uring_probe(uring: &dyn IoUringSubmit) {
        assert!(uring.probe(opcode::Nop::CODE));
        // Opcode 255 is not a valid io-uring opcode.
        assert!(!uring.probe(255));
    }

    /// Submit multiple NOPs concurrently and verify all complete.
    async fn uring_multiple_nops(uring: &dyn IoUringSubmit) {
        let mut futures: Vec<_> = (0..10)
            .map(|_| {
                let sqe = opcode::Nop::new().build();
                // SAFETY: NOP references no memory.
                unsafe { uring.submit(sqe) }
            })
            .collect();

        let results = futures::future::join_all(&mut futures).await;
        for result in results {
            assert_eq!(result.unwrap(), 0);
        }
    }

    /// Submit more NOPs than the SQ can hold (64), forcing the on-thread
    /// fast path to hit SQ-full and submit inline.
    async fn uring_sq_full(uring: &dyn IoUringSubmit) {
        let count = 80;
        let mut futures: Vec<_> = (0..count)
            .map(|_| {
                let sqe = opcode::Nop::new().build();
                // SAFETY: NOP references no memory.
                unsafe { uring.submit(sqe) }
            })
            .collect();

        let results = futures::future::join_all(&mut futures).await;
        assert_eq!(results.len(), count);
        for result in results {
            assert_eq!(result.unwrap(), 0);
        }
    }

    /// Submit more NOPs than the CQ can hold (default 2×SQ = 128),
    /// verifying all completions are delivered even when the CQ must
    /// be drained and refilled multiple times.
    async fn uring_cq_saturation(uring: &dyn IoUringSubmit) {
        let count = 256;
        let mut futures: Vec<_> = (0..count)
            .map(|_| {
                let sqe = opcode::Nop::new().build();
                // SAFETY: NOP references no memory.
                unsafe { uring.submit(sqe) }
            })
            .collect();

        let results = futures::future::join_all(&mut futures).await;
        assert_eq!(results.len(), count);
        for result in results {
            assert_eq!(result.unwrap(), 0);
        }
    }

    /// Submit a NOP from an off-thread caller, verifying the remote
    /// queue and eventfd wake path.
    ///
    /// Spawns a background thread that performs the first poll (which
    /// calls `queue_sqe` via the remote/off-thread path), then awaits
    /// the future on the executor thread where the event loop flushes
    /// the remote queue and delivers the completion.
    async fn uring_remote_submit(uring: &dyn IoUringSubmit) {
        let sqe = opcode::Nop::new().build();
        // SAFETY: NOP references no memory.
        let fut = unsafe { uring.submit(sqe) };
        let fut = parking_lot::Mutex::new(Some(fut));

        std::thread::scope(|s| {
            s.spawn(|| {
                let waker = futures::task::noop_waker();
                let mut cx = std::task::Context::from_waker(&waker);
                // Poll once to queue the SQE via the remote path.
                let _ = fut.lock().as_mut().unwrap().as_mut().poll(&mut cx);
            });
        });

        // Await on the executor thread — the event loop flushes the
        // remote queue and delivers the completion.
        let result = fut.into_inner().unwrap().await.unwrap();
        assert_eq!(result, 0);
    }

    /// Submit NOPs from multiple threads concurrently, stressing
    /// the remote queue under contention.
    async fn uring_remote_batch(uring: &dyn IoUringSubmit) {
        let count_per_thread = 20;
        let num_threads = 4;

        let futures: parking_lot::Mutex<Vec<_>> = parking_lot::Mutex::new(Vec::new());

        std::thread::scope(|s| {
            for _ in 0..num_threads {
                s.spawn(|| {
                    let waker = futures::task::noop_waker();
                    let mut cx = std::task::Context::from_waker(&waker);
                    for _ in 0..count_per_thread {
                        let sqe = opcode::Nop::new().build();
                        // SAFETY: NOP references no memory.
                        let mut fut = unsafe { uring.submit(sqe) };
                        // Poll once to queue via remote path.
                        let _ = fut.as_mut().poll(&mut cx);
                        futures.lock().push(fut);
                    }
                });
            }
        });

        let mut all = futures.into_inner();
        assert_eq!(all.len(), num_threads * count_per_thread);
        let results = futures::future::join_all(&mut all).await;
        for result in results {
            assert_eq!(result.unwrap(), 0);
        }
    }

    /// Submit a NOP, then busy-loop the executor to verify CQ drain
    /// happens during RunAgain (not just before sleep).
    async fn uring_busy_loop_completions(uring: &dyn IoUringSubmit) {
        let sqe = opcode::Nop::new().build();
        // SAFETY: NOP references no memory.
        let mut fut = unsafe { uring.submit(sqe) };

        // Busy-loop: yield back to the executor several times while
        // re-waking ourselves, as RunAgain would.
        for _ in 0..10 {
            if let Poll::Ready(result) = futures::poll!(&mut fut) {
                assert_eq!(result.unwrap(), 0);
                return;
            }
            // Yield once, keeping the executor in RunAgain.
            std::future::poll_fn(|cx| {
                cx.waker().wake_by_ref();
                Poll::Ready(())
            })
            .await;
        }

        // If we didn't complete during busy-looping, await normally.
        let result = fut.await.unwrap();
        assert_eq!(result, 0);
    }

    /// Submit a real read via io-uring on an eventfd, verifying
    /// non-NOP operations work end-to-end.
    async fn uring_read_write(uring: &dyn IoUringSubmit) {
        // Create an eventfd for testing.
        // SAFETY: eventfd with valid flags.
        let efd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(efd >= 0, "eventfd creation failed");

        // Write a value to the eventfd.
        let write_val: u64 = 42;
        // SAFETY: valid fd and buffer.
        let ret =
            unsafe { libc::write(efd, std::ptr::from_ref(&write_val).cast(), size_of::<u64>()) };
        assert_eq!(ret, 8);

        // Read from the eventfd via io-uring.
        let mut read_buf: u64 = 0;
        let sqe = opcode::Read::new(
            types::Fd(efd),
            std::ptr::from_mut(&mut read_buf).cast(),
            size_of::<u64>() as u32,
        )
        .build();

        // SAFETY: read_buf is a local in this async fn; it lives as
        // long as the returned future. The abort-on-drop guard ensures
        // soundness on cancellation.
        let result = unsafe { uring.submit(sqe) }.await.unwrap();
        assert_eq!(result, 8);
        assert_eq!(read_buf, 42);

        // SAFETY: closing a valid fd.
        unsafe { libc::close(efd) };
    }

    /// Submit a write + read via io-uring to a pipe, verifying the
    /// data round-trips correctly.
    async fn uring_pipe_round_trip(uring: &dyn IoUringSubmit) {
        let mut fds = [0i32; 2];
        // SAFETY: pipe2 with valid args.
        let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        assert_eq!(ret, 0);
        let [read_fd, write_fd] = fds;

        // Write "hello" via io-uring.
        let write_buf = b"hello";
        let write_sqe = opcode::Write::new(
            types::Fd(write_fd),
            write_buf.as_ptr(),
            write_buf.len() as u32,
        )
        .build();

        // SAFETY: write_buf is a static-lifetime byte string literal.
        let result = unsafe { uring.submit(write_sqe) }.await.unwrap();
        assert_eq!(result, 5);

        // Read back via io-uring.
        let mut read_buf = [0u8; 5];
        let read_sqe = opcode::Read::new(
            types::Fd(read_fd),
            read_buf.as_mut_ptr(),
            read_buf.len() as u32,
        )
        .build();

        // SAFETY: read_buf is a local in this async fn.
        let result = unsafe { uring.submit(read_sqe) }.await.unwrap();
        assert_eq!(result, 5);
        assert_eq!(&read_buf, b"hello");

        // SAFETY: closing valid fds.
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
    }
}

#[cfg(windows)]
pub mod windows {
    // UNSAFETY: needed to use `OverlappedFile`.
    #![expect(unsafe_code)]

    //! Windows-specific executor tests.

    use crate::driver::Driver;
    use crate::sys::overlapped::OverlappedFile;
    use crate::sys::pipe::NamedPipeServer;
    use std::fs::OpenOptions;
    use std::os::windows::prelude::*;
    use unicycle::FuturesUnordered;
    use windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

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
                Some(ERROR_OPERATION_ABORTED as i32)
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
