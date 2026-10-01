// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Delivers microVM portb output to the host before a guest-requested snapshot
//! capture and drains it, with its host output relay, before process exit.

use anyhow::Context;
use chipset_resources::microvm::MicrovmPortbDrain;
use futures::AsyncRead;
use futures::AsyncWriteExt;
use futures::executor::block_on;
use futures::io::AllowStdIo;
use mesh::CancelContext;
use mesh::OneshotReceiver;
use mesh::oneshot;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend;
use std::io;
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

pub(crate) struct MicrovmOutputDrain {
    portb: mesh::Sender<FailableRpc<MicrovmPortbDrain, ()>>,
    output: Option<OutputCompletion>,
}

impl MicrovmOutputDrain {
    pub(crate) fn new(
        output: Option<OutputCompletion>,
    ) -> (Self, mesh::Receiver<FailableRpc<MicrovmPortbDrain, ()>>) {
        let (portb, requests) = mesh::channel();
        (Self { portb, output }, requests)
    }

    /// Writes and flushes the output accepted by portb to its endpoint, which
    /// stays open, and fails after `timeout`.
    ///
    /// The host relay is not awaited because it completes only once the
    /// endpoint closes.
    pub(crate) async fn flush(&self, timeout: Duration) -> anyhow::Result<()> {
        CancelContext::new()
            .with_timeout(timeout)
            .until_cancelled(
                self.portb
                    .call_failable(std::convert::identity, MicrovmPortbDrain::Flush),
            )
            .await
            .context("microVM console output flush timed out")?
            .context("failed to flush the microVM portb endpoint")
    }

    pub(crate) async fn drain(self) -> anyhow::Result<()> {
        self.drain_with_timeout(Duration::from_secs(5)).await
    }

    async fn drain_with_timeout(self, timeout: Duration) -> anyhow::Result<()> {
        CancelContext::new()
            .with_timeout(timeout)
            .until_cancelled(async {
                self.portb
                    .call_failable(std::convert::identity, MicrovmPortbDrain::Close)
                    .await
                    .context("failed to drain the microVM portb endpoint")?;
                if let Some(output) = self.output {
                    output
                        .await
                        .context("microVM console output relay stopped without completing")?
                        .context("microVM console output relay failed")?;
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
            let (drain, mut requests) = MicrovmOutputDrain::new(Some(completed));
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
            let (drain, mut requests) = MicrovmOutputDrain::new(Some(completed));
            {
                let mut flush = pin!(drain.flush(Duration::from_secs(5)));
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
            let (drain, mut requests) = MicrovmOutputDrain::new(None);
            let mut flush = pin!(drain.flush(Duration::from_secs(5)));
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
    fn flush_has_a_bounded_deadline() {
        block_on(async {
            let (drain, _requests) = MicrovmOutputDrain::new(None);
            let error = drain.flush(Duration::from_millis(10)).await.unwrap_err();
            assert!(format!("{error:#}").contains("timed out"));
        });
    }

    #[test]
    fn drain_reports_endpoint_failure() {
        block_on(async {
            let (drain, mut requests) = MicrovmOutputDrain::new(None);
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
            let (drain, mut requests) = MicrovmOutputDrain::new(Some(completed));
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
            let (drain, _requests) = MicrovmOutputDrain::new(None);
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
        let (drain, mut requests) = MicrovmOutputDrain::new(None);
        let mut drain = Box::pin(drain.drain());
        assert!(drain.as_mut().now_or_never().is_none());
        requests.try_recv().unwrap().complete(Ok(()));
        block_on(drain).unwrap();
    }
}
