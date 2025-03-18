// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

mod bidir;
pub mod cancel;
pub mod cell;
mod deadline;
pub mod error;
mod lazy;
pub mod pipe;
pub mod rpc;

pub use mesh_channel_core::ChannelError;
pub use mesh_channel_core::ChannelErrorKind;
pub use mesh_channel_core::OneshotReceiver;
pub use mesh_channel_core::OneshotSender;
pub use mesh_channel_core::Receiver;
pub use mesh_channel_core::Receiver as MpscReceiver;
pub use mesh_channel_core::RecvError;
pub use mesh_channel_core::Sender;
pub use mesh_channel_core::Sender as MpscSender;
pub use mesh_channel_core::TryRecvError;
pub use mesh_channel_core::channel;
pub use mesh_channel_core::channel as mpsc_channel;
pub use mesh_channel_core::oneshot;

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_node::message::MeshPayload;
    use mesh_protobuf::SerializedMessage;
    use pal_async::async_test;
    use pal_event::Event;
    use test_with_tracing::test;

    #[test]
    fn test() {
        let (send, mut recv) = channel::<(String, String)>();
        send.send(("abc".to_string(), "def".to_string()));
        assert_eq!(
            recv.try_recv().unwrap(),
            ("abc".to_string(), "def".to_string())
        );
    }

    #[test]
    fn test_send_port() {
        let (send, mut recv) = channel::<Receiver<u32>>();
        let (sendi, recvi) = channel::<u32>();
        send.send(recvi);
        let mut recvi = recv.try_recv().unwrap();
        sendi.send(0xf00d);
        assert_eq!(recvi.try_recv().unwrap(), 0xf00d);
    }

    #[test]
    fn test_send_resource() {
        let (send, mut recv) = channel::<Event>();
        let event = Event::new();
        send.send(event.clone());
        let event2 = recv.try_recv().unwrap();
        event2.signal();
        event.wait();
    }

    #[async_test]
    async fn test_mpsc() {
        let (send, mut recv) = mpsc_channel::<u32>();
        send.send(5);
        roundtrip((send.clone(),)).0.send(6);
        drop(send);
        let a = recv.recv().await.unwrap();
        let b = recv.recv().await.unwrap();
        assert!(matches!(recv.recv().await.unwrap_err(), RecvError::Closed));
        let mut s = [a, b];
        s.sort_unstable();
        assert_eq!(&s, &[5, 6]);
    }

    #[async_test]
    async fn test_mpsc_again() {
        let (send, recv) = mpsc_channel::<u32>();
        drop(recv);
        send.send(5);
    }

    /// Serializes and deserializes a mesh message. Used to force an MpscSender
    /// to clone its underlying port.
    fn roundtrip<T: MeshPayload>(t: T) -> T {
        SerializedMessage::from_message(t).into_message().unwrap()
    }
}
