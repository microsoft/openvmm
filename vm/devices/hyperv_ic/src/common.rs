// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common code for IC implementations.

use anyhow::Context as _;
use hyperv_ic_protocol::FRAMEWORK_VERSION_1;
use hyperv_ic_protocol::FRAMEWORK_VERSION_3;
use hyperv_ic_protocol::HeaderFlags;
use hyperv_ic_protocol::MessageType;
use hyperv_ic_protocol::Status;
use hyperv_ic_protocol::Version;
use inspect::Inspect;
use inspect::InspectMut;
use std::io::IoSlice;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_async::async_dgram::AsyncSendExt;
use vmbus_async::pipe::MessagePipe;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_ring::RingMem;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Supported framework versions.
const FRAMEWORK_VERSIONS: &[Version] = &[FRAMEWORK_VERSION_1, FRAMEWORK_VERSION_3];

#[derive(InspectMut)]
pub(crate) struct IcPipe<M: RingMem = GpadlRingMem> {
    #[inspect(mut)]
    pub pipe: MessagePipe<M>,
    #[inspect(skip)]
    buf: Vec<u8>,
    pub transaction_id: u8,
}

#[derive(Inspect, Default)]
pub(crate) enum NegotiateState {
    #[default]
    SendVersion,
    WaitVersion,
    Invalid,
}

#[derive(Copy, Clone, Debug, Inspect)]
pub(crate) struct Versions {
    #[inspect(display)]
    pub framework_version: Version,
    #[inspect(display)]
    pub message_version: Version,
}

impl<M: RingMem> IcPipe<M> {
    pub fn new(raw: RawAsyncChannel<M>) -> Result<Self, std::io::Error> {
        let pipe = MessagePipe::new(raw)?;
        let buf = vec![0; hyperv_ic_protocol::MAX_MESSAGE_SIZE];
        Ok(Self {
            pipe,
            buf,
            transaction_id: u8::MAX,
        })
    }

    pub async fn negotiate(
        &mut self,
        state: &mut NegotiateState,
        message_versions: &[Version],
    ) -> anyhow::Result<Option<Versions>> {
        match state {
            NegotiateState::SendVersion => {
                let transaction_id = self.transaction_id.wrapping_add(1);
                let message = hyperv_ic_protocol::NegotiateMessage {
                    framework_version_count: FRAMEWORK_VERSIONS.len() as u16,
                    message_version_count: message_versions.len() as u16,
                    ..FromZeros::new_zeroed()
                };

                let header = hyperv_ic_protocol::Header {
                    message_type: MessageType::VERSION_NEGOTIATION,
                    message_size: (size_of_val(&message)
                        + size_of_val(FRAMEWORK_VERSIONS)
                        + size_of_val(message_versions)) as u16,
                    status: Status::SUCCESS,
                    transaction_id,
                    flags: HeaderFlags::new().with_transaction(true).with_request(true),
                    ..FromZeros::new_zeroed()
                };

                self.pipe
                    .send_vectored(&[
                        IoSlice::new(header.as_bytes()),
                        IoSlice::new(message.as_bytes()),
                        IoSlice::new(FRAMEWORK_VERSIONS.as_bytes()),
                        IoSlice::new(message_versions.as_bytes()),
                    ])
                    .await
                    .context("ring buffer error")?;

                self.transaction_id = transaction_id;
                *state = NegotiateState::WaitVersion;
                Ok(None)
            }
            NegotiateState::WaitVersion => {
                let Some((message_type, status, buf)) = self.read_message(None).await? else {
                    return Ok(None);
                };
                // A response to a request issued before the channel was
                // reset (e.g. across save/restore) may still be in flight.
                // There is no one left to receive it, so drop it.
                if message_type != MessageType::VERSION_NEGOTIATION {
                    tracelimit::warn_ratelimited!(
                        ?message_type,
                        "dropping unexpected message while negotiating versions"
                    );
                    return Ok(None);
                }
                if status != Status::SUCCESS {
                    anyhow::bail!("version negotiation failed: {status:?}");
                }
                let (message, rest) = hyperv_ic_protocol::NegotiateMessage::read_from_prefix(buf)
                    .ok()
                    .context("missing negotiate message")?;
                if message.framework_version_count != 1 || message.message_version_count != 1 {
                    anyhow::bail!("no supported versions");
                }
                let ([framework_version, message_version], _) =
                    <[Version; 2]>::read_from_prefix(rest)
                        .ok()
                        .context("missing version table")?;

                if !FRAMEWORK_VERSIONS.contains(&framework_version) {
                    anyhow::bail!("unsupported framework version: {framework_version}");
                }
                if !message_versions.contains(&message_version) {
                    anyhow::bail!("unsupported message version: {message_version}");
                }

                let versions = Versions {
                    framework_version,
                    message_version,
                };

                *state = NegotiateState::Invalid;
                Ok(Some(versions))
            }
            NegotiateState::Invalid => {
                unreachable!()
            }
        }
    }

    pub async fn write_message(
        &mut self,
        versions: &Versions,
        message_type: MessageType,
        flags: HeaderFlags,
        message: &[u8],
    ) -> anyhow::Result<()> {
        let transaction_id = self.transaction_id.wrapping_add(1);
        let header = hyperv_ic_protocol::Header {
            framework_version: versions.framework_version,
            message_type,
            message_size: message.len() as u16,
            message_version: versions.message_version,
            status: Status::SUCCESS,
            transaction_id,
            flags,
            ..FromZeros::new_zeroed()
        };

        self.pipe
            .send_vectored(&[IoSlice::new(header.as_bytes()), IoSlice::new(message)])
            .await
            .context("ring buffer error")?;
        self.transaction_id = transaction_id;
        Ok(())
    }

    pub async fn read_response(
        &mut self,
        versions: &Versions,
    ) -> anyhow::Result<Option<(Status, &[u8])>> {
        Ok(self
            .read_message(Some(versions))
            .await?
            .map(|(_, status, buf)| (status, buf)))
    }

    pub async fn read_message(
        &mut self,
        versions: Option<&Versions>,
    ) -> anyhow::Result<Option<(MessageType, Status, &[u8])>> {
        let n = self
            .pipe
            .recv(&mut self.buf)
            .await
            .context("ring buffer error")?;
        let buf = &self.buf[..n];
        let (header, rest) = hyperv_ic_protocol::Header::read_from_prefix(buf)
            .ok()
            .context("missing header")?;

        if !header.flags.transaction() || !header.flags.response() {
            anyhow::bail!("invalid transaction response");
        }

        if header.transaction_id != self.transaction_id {
            tracelimit::warn_ratelimited!(
                transaction_id = header.transaction_id,
                expected_transaction_id = self.transaction_id,
                "dropping response for an unexpected transaction"
            );
            return Ok(None);
        }

        let rest = rest
            .get(..header.message_size as usize)
            .context("missing message body")?;

        let valid_versions = if header.message_type == MessageType::VERSION_NEGOTIATION {
            header.framework_version == Version::new(0, 0)
                && header.message_version == Version::new(0, 0)
        } else if let Some(versions) = versions {
            header.framework_version == versions.framework_version
                && header.message_version == versions.message_version
        } else {
            true
        };
        if !valid_versions {
            tracelimit::warn_ratelimited!(
                framework_version = %header.framework_version,
                message_version = %header.message_version,
                "dropping response with incorrect protocol versions"
            );
            return Ok(None);
        }

        Ok(Some((header.message_type, header.status, rest)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperv_ic_protocol::Header;
    use hyperv_ic_protocol::NegotiateMessage;
    use pal_async::async_test;
    use test_with_tracing::test;
    use vmbus_channel::connected_async_channels;
    use vmbus_ring::FlatRingMem;

    const TEST_MESSAGE_VERSION: Version = Version::new(5, 0);
    const TEST_MESSAGE_VERSIONS: &[Version] = &[TEST_MESSAGE_VERSION];

    fn response_header(message_type: MessageType, message_size: usize) -> Header {
        Header {
            message_type,
            message_size: message_size as u16,
            status: Status::SUCCESS,
            transaction_id: 0,
            flags: HeaderFlags::new()
                .with_transaction(true)
                .with_response(true),
            ..FromZeros::new_zeroed()
        }
    }

    fn version_response(framework_version: Version, message_version: Version) -> Vec<u8> {
        let message = NegotiateMessage {
            framework_version_count: 1,
            message_version_count: 1,
            reserved: 0,
        };
        let versions = [framework_version, message_version];
        let header = response_header(
            MessageType::VERSION_NEGOTIATION,
            size_of_val(&message) + size_of_val(&versions),
        );
        [header.as_bytes(), message.as_bytes(), versions.as_bytes()].concat()
    }

    /// A transaction completion for some earlier, unrelated request.
    fn stale_response(message_type: MessageType) -> Vec<u8> {
        let mut header = response_header(message_type, 0);
        header.status = Status::FAIL;
        header.as_bytes().to_vec()
    }

    fn new_pipes() -> (IcPipe<FlatRingMem>, MessagePipe<FlatRingMem>) {
        let (host, guest) = connected_async_channels(16384);
        (IcPipe::new(host).unwrap(), MessagePipe::new(guest).unwrap())
    }

    /// Runs the send half of negotiation and consumes the request from the
    /// guest side of the channel.
    async fn send_version_request(
        pipe: &mut IcPipe<FlatRingMem>,
        state: &mut NegotiateState,
        guest: &mut MessagePipe<FlatRingMem>,
    ) {
        assert!(
            pipe.negotiate(state, TEST_MESSAGE_VERSIONS)
                .await
                .unwrap()
                .is_none()
        );
        let mut buf = [0; hyperv_ic_protocol::MAX_MESSAGE_SIZE];
        let n = guest.recv(&mut buf).await.unwrap();
        let (header, _) = Header::read_from_prefix(&buf[..n]).unwrap();
        assert_eq!(header.message_type, MessageType::VERSION_NEGOTIATION);
        assert!(header.flags.request());
    }

    #[async_test]
    async fn transactions_advance_and_wrap_ids() {
        let (mut pipe, mut guest) = new_pipes();
        let mut state = NegotiateState::default();
        send_version_request(&mut pipe, &mut state, &mut guest).await;
        guest
            .send(&version_response(FRAMEWORK_VERSION_3, TEST_MESSAGE_VERSION))
            .await
            .unwrap();
        let versions = pipe
            .negotiate(&mut state, TEST_MESSAGE_VERSIONS)
            .await
            .unwrap()
            .unwrap();
        for expected_id in 1..=257u16 {
            pipe.write_message(
                &versions,
                MessageType::SHUTDOWN,
                HeaderFlags::new().with_transaction(true).with_request(true),
                &[],
            )
            .await
            .unwrap();
            let mut buffer = [0; hyperv_ic_protocol::MAX_MESSAGE_SIZE];
            let size = guest.recv(&mut buffer).await.unwrap();
            let (mut header, _) = Header::read_from_prefix(&buffer[..size]).unwrap();
            assert_eq!(header.transaction_id, expected_id as u8);
            header.flags = HeaderFlags::new()
                .with_transaction(true)
                .with_response(true);
            guest.send(header.as_bytes()).await.unwrap();
            let (status, body) = pipe.read_response(&versions).await.unwrap().unwrap();
            assert_eq!(status, Status::SUCCESS);
            assert!(body.is_empty());
        }
    }

    #[async_test]
    async fn responses_match_transaction_and_versions() {
        let versions = Versions {
            framework_version: FRAMEWORK_VERSION_3,
            message_version: TEST_MESSAGE_VERSION,
        };
        for message_type in [
            MessageType::SHUTDOWN,
            MessageType::TIME_SYNC,
            MessageType::KVP_EXCHANGE,
        ] {
            let (mut pipe, mut guest) = new_pipes();
            let mut buffer = [0; hyperv_ic_protocol::MAX_MESSAGE_SIZE];
            for expected_id in 0..2 {
                pipe.write_message(
                    &versions,
                    message_type,
                    HeaderFlags::new().with_transaction(true).with_request(true),
                    &[],
                )
                .await
                .unwrap();
                let size = guest.recv(&mut buffer).await.unwrap();
                let (mut response, _) = Header::read_from_prefix(&buffer[..size]).unwrap();
                assert_eq!(response.transaction_id, expected_id);
                response.flags = HeaderFlags::new()
                    .with_transaction(true)
                    .with_response(true);

                if expected_id == 1 {
                    for (transaction_id, framework_version, message_version) in [
                        (0, versions.framework_version, versions.message_version),
                        (2, versions.framework_version, versions.message_version),
                        (1, Version::new(0, 0), versions.message_version),
                        (1, FRAMEWORK_VERSION_1, versions.message_version),
                        (1, Version::new(3, 1), versions.message_version),
                        (1, versions.framework_version, Version::new(0, 0)),
                        (1, versions.framework_version, Version::new(5, 1)),
                    ] {
                        let invalid = Header {
                            transaction_id,
                            framework_version,
                            message_version,
                            status: Status::NOT_SUPPORTED,
                            ..response
                        };
                        guest.send(invalid.as_bytes()).await.unwrap();
                        assert!(pipe.read_response(&versions).await.unwrap().is_none());
                        assert_eq!(pipe.transaction_id, expected_id);
                    }
                    response.status = Status::FAIL;
                }

                let body = b"ack";
                response.message_size = body.len() as u16;
                guest
                    .send(&[response.as_bytes(), body].concat())
                    .await
                    .unwrap();
                let (status, received_body) = pipe.read_response(&versions).await.unwrap().unwrap();
                assert_eq!(status, response.status);
                assert_eq!(received_body, body);
            }
        }
    }

    #[async_test]
    async fn negotiate_drops_unmatched_ids_and_invalid_header_versions() {
        let (mut pipe, mut guest) = new_pipes();
        let mut state = NegotiateState::default();
        send_version_request(&mut pipe, &mut state, &mut guest).await;
        for (transaction_id, framework_version, message_version) in [
            (1, Version::new(0, 0), Version::new(0, 0)),
            (0, FRAMEWORK_VERSION_3, Version::new(0, 0)),
            (0, Version::new(0, 0), TEST_MESSAGE_VERSION),
        ] {
            let mut response = version_response(FRAMEWORK_VERSION_3, TEST_MESSAGE_VERSION);
            let (header, _) = Header::mut_from_prefix(response.as_mut_slice()).unwrap();
            header.transaction_id = transaction_id;
            header.framework_version = framework_version;
            header.message_version = message_version;
            guest.send(&response).await.unwrap();
            assert!(
                pipe.negotiate(&mut state, TEST_MESSAGE_VERSIONS)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(matches!(state, NegotiateState::WaitVersion));
        }
        guest
            .send(&version_response(FRAMEWORK_VERSION_3, TEST_MESSAGE_VERSION))
            .await
            .unwrap();
        assert!(
            pipe.negotiate(&mut state, TEST_MESSAGE_VERSIONS)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[async_test]
    async fn negotiate() {
        for &framework_version in FRAMEWORK_VERSIONS {
            let (mut pipe, mut guest) = new_pipes();
            let mut state = NegotiateState::default();
            send_version_request(&mut pipe, &mut state, &mut guest).await;
            guest
                .send(&version_response(framework_version, TEST_MESSAGE_VERSION))
                .await
                .unwrap();
            let versions = pipe
                .negotiate(&mut state, TEST_MESSAGE_VERSIONS)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(versions.framework_version, framework_version);
            assert_eq!(versions.message_version, TEST_MESSAGE_VERSION);
        }
    }

    #[async_test]
    async fn negotiate_rejects_failed_response() {
        for status in [Status::FAIL, Status::NOT_SUPPORTED] {
            let (mut pipe, mut guest) = new_pipes();
            let mut state = NegotiateState::default();
            send_version_request(&mut pipe, &mut state, &mut guest).await;
            let mut response = version_response(FRAMEWORK_VERSION_3, TEST_MESSAGE_VERSION);
            let (header, _) = Header::mut_from_prefix(response.as_mut_slice()).unwrap();
            header.status = status;
            guest.send(&response).await.unwrap();
            pipe.negotiate(&mut state, TEST_MESSAGE_VERSIONS)
                .await
                .unwrap_err();
            assert!(matches!(state, NegotiateState::WaitVersion));
        }
    }

    #[async_test]
    async fn negotiate_rejects_unadvertised_versions() {
        for (framework_version, message_version) in [
            (Version::new(2, 0), TEST_MESSAGE_VERSION),
            (Version::new(3, 1), TEST_MESSAGE_VERSION),
            (FRAMEWORK_VERSION_3, Version::new(4, 0)),
            (FRAMEWORK_VERSION_3, Version::new(5, 1)),
            (FRAMEWORK_VERSION_3, Version::new(6, 0)),
        ] {
            let (mut pipe, mut guest) = new_pipes();
            let mut state = NegotiateState::default();
            send_version_request(&mut pipe, &mut state, &mut guest).await;
            guest
                .send(&version_response(framework_version, message_version))
                .await
                .unwrap();
            pipe.negotiate(&mut state, TEST_MESSAGE_VERSIONS)
                .await
                .unwrap_err();
            assert!(matches!(state, NegotiateState::WaitVersion));
        }
    }

    #[async_test]
    async fn negotiate_drops_stale_responses() {
        let (mut pipe, mut guest) = new_pipes();
        let mut state = NegotiateState::default();
        send_version_request(&mut pipe, &mut state, &mut guest).await;
        guest
            .send(&stale_response(MessageType::KVP_EXCHANGE))
            .await
            .unwrap();
        guest
            .send(&stale_response(MessageType::TIME_SYNC))
            .await
            .unwrap();
        guest
            .send(&version_response(FRAMEWORK_VERSION_3, TEST_MESSAGE_VERSION))
            .await
            .unwrap();
        for _ in 0..2 {
            assert!(
                pipe.negotiate(&mut state, TEST_MESSAGE_VERSIONS)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(matches!(state, NegotiateState::WaitVersion));
        }
        let versions = pipe
            .negotiate(&mut state, TEST_MESSAGE_VERSIONS)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(versions.framework_version, FRAMEWORK_VERSION_3);
        assert_eq!(versions.message_version, TEST_MESSAGE_VERSION);
    }

    #[async_test]
    async fn negotiate_rejects_non_transaction_response() {
        let (mut pipe, mut guest) = new_pipes();
        let mut state = NegotiateState::default();
        send_version_request(&mut pipe, &mut state, &mut guest).await;
        let mut response = version_response(FRAMEWORK_VERSION_3, TEST_MESSAGE_VERSION);
        let (header, _) = Header::mut_from_prefix(response.as_mut_slice()).unwrap();
        header.flags = HeaderFlags::new().with_request(true);
        guest.send(&response).await.unwrap();
        pipe.negotiate(&mut state, TEST_MESSAGE_VERSIONS)
            .await
            .unwrap_err();
    }
}
