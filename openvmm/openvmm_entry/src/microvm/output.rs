// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Delivers microVM console output to the host before a guest-requested
//! snapshot capture and drains it, with its host output relays, before process
//! exit: portb's, and the boot console's when it writes to the terminal.

use anyhow::Context;
use chipset_resources::microvm::MicrovmPortbDrain;
use futures::AsyncRead;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::executor::block_on;
use futures::future::Either;
use futures::io::AllowStdIo;
use mesh::CancelContext;
use mesh::OneshotReceiver;
use mesh::oneshot;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend;
use std::io;
use std::pin::pin;
use std::thread;
use std::time::Duration;

pub(crate) type OutputCompletion = OneshotReceiver<io::Result<()>>;

pub(crate) fn spawn_output(
    name: &str,
    input: impl AsyncRead + Send + Unpin + 'static,
    output: impl io::Write + Send + 'static,
) -> io::Result<OutputCompletion> {
    let (complete, completed) = oneshot();
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let result = block_on(async {
                let mut output = AllowStdIo::new(output);
                futures::io::copy(input, &mut output).await?;
                output.flush().await
            });
            complete.send(result);
        })?;
    Ok(completed)
}

/// Asks a relay that [`spawn_drainable_output`] started to write the output
/// that its pair holds.
pub(crate) struct RelayDrain(mesh::Sender<FailableRpc<(), ()>>);

impl RelayDrain {
    /// Waits until the relay has written and flushed every byte that its pair
    /// held when the relay took the request.
    async fn write_pending(&self) -> anyhow::Result<()> {
        self.0
            .call_failable(std::convert::identity, ())
            .await
            .context("failed to write the microVM boot console's output")
    }
}

/// Starts a relay that copies `input` to `output`, as [`spawn_output`] does,
/// and that also writes, when asked, the output that its pair holds.
///
/// The reactor reports `input`'s data some time after the device writes it, so
/// a process that exits once the guest does could lose the output that the
/// relay has yet to see. `pending` is a non-blocking duplicate of `input`'s
/// object, from which the relay reads the pair's contents without waiting for
/// the reactor.
pub(crate) fn spawn_drainable_output(
    name: &str,
    input: impl AsyncRead + Send + Unpin + 'static,
    pending: impl io::Read + Send + 'static,
    output: impl io::Write + Send + 'static,
) -> io::Result<(OutputCompletion, RelayDrain)> {
    let (complete, completed) = oneshot();
    let (requests_send, requests) = mesh::channel();
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            complete.send(block_on(relay(input, pending, output, requests)));
        })?;
    Ok((completed, RelayDrain(requests_send)))
}

async fn relay(
    mut input: impl AsyncRead + Unpin,
    mut pending: impl io::Read,
    mut output: impl io::Write,
    mut requests: mesh::Receiver<FailableRpc<(), ()>>,
) -> io::Result<()> {
    enum Event {
        Read(io::Result<usize>),
        Request(Option<FailableRpc<(), ()>>),
    }

    let mut buffer = vec![0; 8192];
    let mut requests_open = true;
    loop {
        // An unfinished read holds no data, so dropping it for a request loses
        // nothing.
        let event = {
            let read = pin!(input.read(&mut buffer));
            let request = pin!(async {
                if requests_open {
                    requests.recv().await.ok()
                } else {
                    std::future::pending().await
                }
            });
            match futures::future::select(read, request).await {
                Either::Left((read, _)) => Event::Read(read),
                Either::Right((request, _)) => Event::Request(request),
            }
        };
        match event {
            Event::Read(read) => {
                let count = read?;
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count])?;
            }
            Event::Request(Some(request)) => request
                .handle_failable_sync(|()| write_pending(&mut pending, &mut buffer, &mut output)),
            Event::Request(None) => requests_open = false,
        }
    }
    output.flush()
}

/// Writes and flushes what `pending`, a non-blocking end of a pair, holds,
/// without waiting for more.
fn write_pending(
    pending: &mut impl io::Read,
    buffer: &mut [u8],
    output: &mut impl io::Write,
) -> io::Result<()> {
    loop {
        match pending.read(buffer) {
            Ok(0) => break,
            Ok(count) => output.write_all(&buffer[..count])?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if pair_is_empty(&error) => break,
            Err(error) => return Err(error),
        }
    }
    output.flush()
}

/// Returns whether a read of a non-blocking pair failed only because the pair
/// holds nothing: a socket reports `WouldBlock`, and a Windows pipe in
/// `PIPE_NOWAIT` mode reports `ERROR_NO_DATA`.
fn pair_is_empty(error: &io::Error) -> bool {
    #[cfg(windows)]
    if error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_NO_DATA as i32) {
        return true;
    }
    error.kind() == io::ErrorKind::WouldBlock
}

pub(crate) struct MicrovmOutputDrain {
    portb: mesh::Sender<FailableRpc<MicrovmPortbDrain, ()>>,
    output: Option<OutputCompletion>,
    console: Option<RelayDrain>,
}

impl MicrovmOutputDrain {
    /// Returns a drain of portb, whose relay is `output`, and of the boot
    /// console's relay to the terminal, `console`, with the receiver of
    /// portb's requests.
    pub(crate) fn new(
        output: Option<OutputCompletion>,
        console: Option<RelayDrain>,
    ) -> (Self, mesh::Receiver<FailableRpc<MicrovmPortbDrain, ()>>) {
        let (portb, requests) = mesh::channel();
        (
            Self {
                portb,
                output,
                console,
            },
            requests,
        )
    }

    /// Writes and flushes the output accepted by portb to its endpoint, which
    /// stays open, and the output that the boot console's terminal relay
    /// holds.
    ///
    /// Has no deadline, as the capture that it precedes has none: an endpoint
    /// whose reader stops reading stalls it until the process that launched
    /// OpenVMM ends OpenVMM. The host relay of portb is not awaited because it
    /// completes only once the endpoint closes.
    pub(crate) async fn flush(&self) -> anyhow::Result<()> {
        self.portb
            .call_failable(std::convert::identity, MicrovmPortbDrain::Flush)
            .await
            .context("failed to flush the microVM portb endpoint")?;
        if let Some(console) = &self.console {
            console.write_pending().await?;
        }
        Ok(())
    }

    pub(crate) async fn drain(self) -> anyhow::Result<()> {
        self.drain_with_timeout(Duration::from_secs(5)).await
    }

    async fn drain_with_timeout(self, timeout: Duration) -> anyhow::Result<()> {
        let Self {
            portb,
            output,
            console,
        } = self;
        CancelContext::new()
            .with_timeout(timeout)
            .until_cancelled(async {
                portb
                    .call_failable(std::convert::identity, MicrovmPortbDrain::Close)
                    .await
                    .context("failed to drain the microVM portb endpoint")?;
                if let Some(output) = output {
                    output
                        .await
                        .context("microVM console output relay stopped without completing")?
                        .context("microVM console output relay failed")?;
                }
                // The guest has stopped, and the boot console wrote each of
                // its descriptors to the pair before completing it, so the
                // pair holds the rest of the guest's output.
                if let Some(console) = &console {
                    console.write_pending().await?;
                }
                tracing::debug!("microVM console output drained");
                anyhow::Ok(())
            })
            .await
            .context("microVM console output drain timed out")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use futures::poll;
    use parking_lot::Mutex;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::mpsc;
    use test_with_tracing::test;

    #[test]
    fn drain_waits_for_both_device_and_relay() {
        block_on(async {
            let (complete, completed) = oneshot();
            let (drain, mut requests) = MicrovmOutputDrain::new(Some(completed), None);
            let mut drain = pin!(drain.drain());
            assert!(poll!(&mut drain).is_pending());
            let request = requests.recv().await.unwrap();
            assert_eq!(*request.input(), MicrovmPortbDrain::Close);
            request.complete(Ok(()));
            assert!(poll!(&mut drain).is_pending());
            complete.send(Ok(()));
            drain.await.unwrap();
        });
    }

    #[test]
    fn flush_keeps_the_endpoint_open_without_waiting_for_the_relay() {
        block_on(async {
            let (_complete, completed) = oneshot();
            let (drain, mut requests) = MicrovmOutputDrain::new(Some(completed), None);
            {
                let mut flush = pin!(drain.flush());
                assert!(poll!(&mut flush).is_pending());
                let request = requests.recv().await.unwrap();
                assert_eq!(*request.input(), MicrovmPortbDrain::Flush);
                request.complete(Ok(()));
                flush.await.unwrap();
            }
            // The same handle still drains and closes portb at process exit.
            let mut exit = pin!(drain.drain());
            assert!(poll!(&mut exit).is_pending());
            assert_eq!(
                *requests.recv().await.unwrap().input(),
                MicrovmPortbDrain::Close
            );
        });
    }

    #[test]
    fn flush_reports_endpoint_failure() {
        block_on(async {
            let (drain, mut requests) = MicrovmOutputDrain::new(None, None);
            let mut flush = pin!(drain.flush());
            assert!(poll!(&mut flush).is_pending());
            requests
                .recv()
                .await
                .unwrap()
                .fail(io::Error::from(io::ErrorKind::BrokenPipe));
            assert!(format!("{:#}", flush.await.unwrap_err()).contains("portb endpoint"));
        });
    }

    #[test]
    fn flush_has_no_deadline() {
        block_on(async {
            let (drain, mut requests) = MicrovmOutputDrain::new(None, None);
            let mut flush = pin!(drain.flush());
            assert!(poll!(&mut flush).is_pending());
            let request = requests.recv().await.unwrap();
            // An endpoint whose reader has stopped reading answers late, and
            // the flush waits for it instead of failing.
            thread::sleep(Duration::from_millis(100));
            assert!(poll!(&mut flush).is_pending());
            request.complete(Ok(()));
            flush.await.unwrap();
        });
    }

    #[test]
    fn drain_reports_endpoint_failure() {
        block_on(async {
            let (drain, mut requests) = MicrovmOutputDrain::new(None, None);
            let mut drain = pin!(drain.drain());
            assert!(poll!(&mut drain).is_pending());
            requests
                .recv()
                .await
                .unwrap()
                .fail(io::Error::from(io::ErrorKind::BrokenPipe));
            assert!(format!("{:#}", drain.await.unwrap_err()).contains("portb endpoint"));
        });
    }

    #[test]
    fn drain_reports_relay_failure() {
        block_on(async {
            let (complete, completed) = oneshot();
            let (drain, mut requests) = MicrovmOutputDrain::new(Some(completed), None);
            let mut drain = pin!(drain.drain());
            assert!(poll!(&mut drain).is_pending());
            requests.recv().await.unwrap().complete(Ok(()));
            complete.send(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
            assert!(format!("{:#}", drain.await.unwrap_err()).contains("relay failed"));
        });
    }

    #[test]
    fn drain_has_a_bounded_deadline() {
        block_on(async {
            let (drain, _requests) = MicrovmOutputDrain::new(None, None);
            let error = drain
                .drain_with_timeout(Duration::from_millis(10))
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains("timed out"));
        });
    }

    #[test]
    fn output_completion_follows_the_final_write() {
        struct DelayedOutput {
            bytes: Arc<Mutex<Vec<u8>>>,
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl io::Write for DelayedOutput {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
                self.bytes.lock().extend_from_slice(buffer);
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let payload = b"OPENVMM-SNAPSHOT-RESTORE-OK\n";
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (entered, waiting) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut completed = spawn_output(
            "microvm-output-test",
            futures::io::Cursor::new(payload),
            DelayedOutput {
                bytes: bytes.clone(),
                entered,
                release: released,
            },
        )
        .unwrap();
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!((&mut completed).now_or_never().is_none());
        release.send(()).unwrap();
        block_on(completed).unwrap().unwrap();
        assert_eq!(*bytes.lock(), payload);
    }

    #[test]
    fn output_completion_reports_write_failure() {
        struct FailingOutput;
        impl io::Write for FailingOutput {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let completed = spawn_output(
            "microvm-output-error-test",
            futures::io::Cursor::new(b"marker"),
            FailingOutput,
        )
        .unwrap();
        assert_eq!(
            block_on(completed).unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn drain_without_relay_only_waits_for_the_endpoint() {
        let (drain, mut requests) = MicrovmOutputDrain::new(None, None);
        let mut drain = Box::pin(drain.drain());
        assert!(drain.as_mut().now_or_never().is_none());
        requests.try_recv().unwrap().complete(Ok(()));
        block_on(drain).unwrap();
    }

    /// The output of a relay, which the test reads after the relay writes it.
    #[derive(Clone, Default)]
    struct SharedOutput(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedOutput {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Returns a relay input whose data the test supplies, and which ends once
    /// the test drops the sender.
    fn relay_input() -> (
        mesh::Sender<Vec<u8>>,
        impl AsyncRead + Send + Unpin + 'static,
    ) {
        struct ChannelInput {
            receiver: mesh::Receiver<Vec<u8>>,
            data: Vec<u8>,
        }

        impl AsyncRead for ChannelInput {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buffer: &mut [u8],
            ) -> std::task::Poll<io::Result<usize>> {
                let this = &mut *self;
                if this.data.is_empty() {
                    match std::task::ready!(this.receiver.poll_recv(cx)) {
                        Ok(data) => this.data = data,
                        Err(_) => return std::task::Poll::Ready(Ok(0)),
                    }
                }
                let count = buffer.len().min(this.data.len());
                buffer[..count].copy_from_slice(&this.data[..count]);
                this.data.drain(..count);
                std::task::Poll::Ready(Ok(count))
            }
        }

        let (send, receiver) = mesh::channel();
        (
            send,
            ChannelInput {
                receiver,
                data: Vec::new(),
            },
        )
    }

    #[test]
    fn exit_drain_writes_the_boot_console_output_that_the_reactor_has_yet_to_report() {
        // The reactor has not reported the pair's last bytes, so only the
        // pending handle reads them.
        let (input_send, input) = relay_input();
        let output = SharedOutput::default();
        let (completed, relay) = spawn_drainable_output(
            "microvm-boot-console-test",
            input,
            io::Cursor::new(b"NVX-FINAL-OUTPUT\n".to_vec()),
            output.clone(),
        )
        .unwrap();
        block_on(async {
            let (drain, mut requests) = MicrovmOutputDrain::new(None, Some(relay));
            let (drained, ()) = futures::join!(drain.drain(), async {
                requests.recv().await.unwrap().complete(Ok(()));
            });
            drained.unwrap();
        });
        assert_eq!(*output.0.lock(), b"NVX-FINAL-OUTPUT\n");
        drop(input_send);
        block_on(completed).unwrap().unwrap();
    }

    #[test]
    fn capture_flush_writes_the_boot_console_output_and_keeps_relaying() {
        let (input_send, input) = relay_input();
        let output = SharedOutput::default();
        let (completed, relay) = spawn_drainable_output(
            "microvm-boot-console-flush-test",
            input,
            io::Cursor::new(b"before\n".to_vec()),
            output.clone(),
        )
        .unwrap();
        block_on(async {
            let (drain, mut requests) = MicrovmOutputDrain::new(None, Some(relay));
            let (flushed, ()) = futures::join!(drain.flush(), async {
                let request = requests.recv().await.unwrap();
                assert_eq!(*request.input(), MicrovmPortbDrain::Flush);
                request.complete(Ok(()));
            });
            flushed.unwrap();
        });
        assert_eq!(*output.0.lock(), b"before\n");
        // The relay keeps copying the console's output after the flush.
        input_send.send(b"after\n".to_vec());
        drop(input_send);
        block_on(completed).unwrap().unwrap();
        assert_eq!(*output.0.lock(), b"before\nafter\n");
    }

    #[test]
    fn relay_reports_a_failed_write_of_the_pending_output() {
        struct FailingOutput;
        impl io::Write for FailingOutput {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (_input_send, input) = relay_input();
        let (_completed, relay) = spawn_drainable_output(
            "microvm-boot-console-error-test",
            input,
            io::Cursor::new(b"lost".to_vec()),
            FailingOutput,
        )
        .unwrap();
        let error = block_on(relay.write_pending()).unwrap_err();
        assert!(format!("{error:#}").contains("boot console's output"));
    }

    #[test]
    fn write_pending_empties_a_pair_without_waiting_for_more() {
        use std::io::Write as _;
        pal_async::DefaultPool::run_with(async |driver| {
            let (mut vm, _host, mut pending) = crate::serial_io::pending_pair(&driver).unwrap();
            vm.write_all(b"NVX-PAIR-OUTPUT").unwrap();
            let mut output = Vec::new();
            // A buffer smaller than the output takes several reads.
            write_pending(&mut pending, &mut [0; 4], &mut output).unwrap();
            assert_eq!(output, b"NVX-PAIR-OUTPUT");
            // An empty pair ends the write at once rather than waiting.
            write_pending(&mut pending, &mut [0; 4], &mut output).unwrap();
            assert_eq!(output, b"NVX-PAIR-OUTPUT");
        });
    }
}
