extern crate alloc;
use alloc::rc::Rc;
use core::cell::RefCell;

use super::*;
use crate::comms::{SerialCommsServer, test::MockSerialIo};
use inv_packet::{COMMS_PACKET_FOOTER_MAGIC, COMMS_PACKET_HEADER_MAGIC};

#[derive(Clone)]
struct TestDeserializerState {
    execute_result: Result<u64, ExecutorError>,
    mappings_result: Result<(), ExecutorError>,
    received_mappings: Vec<u8>,
    execute_calls: usize,
}

struct TestDeserializer {
    state: Rc<RefCell<TestDeserializerState>>,
}

impl TestDeserializer {
    fn new(state: Rc<RefCell<TestDeserializerState>>) -> Self {
        Self { state }
    }
}

impl Deserializer for TestDeserializer {
    fn deserialize_and_execute(
        &mut self,
        _testcase: &mut OpenTMKFuzzTest,
    ) -> Result<u64, ExecutorError> {
        let mut state = self.state.borrow_mut();
        state.execute_calls += 1;
        state.execute_result.clone()
    }

    fn set_function_registry(&mut self, _registry: Arc<Mutex<FunctionRegistry>>) {}

    fn set_mappings(&mut self, mappings: Vec<u8>) -> Result<(), ExecutorError> {
        let mut state = self.state.borrow_mut();
        state.received_mappings = mappings;
        state.mappings_result.clone()
    }
}

fn frame_packet(packet: &OpenTMKPacket) -> Vec<u8> {
    let payload = postcard::to_allocvec(packet).expect("packet serialization should succeed");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&COMMS_PACKET_HEADER_MAGIC.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&COMMS_PACKET_FOOTER_MAGIC.to_le_bytes());
    bytes
}

fn fuzz_packet() -> OpenTMKFuzzTest {
    OpenTMKFuzzTest {
        timeout_usec: 123,
        testcase_vcpu0: vec![1, 2, 3, 4],
    }
}

fn test_state(
    execute_result: Result<u64, ExecutorError>,
    mappings_result: Result<(), ExecutorError>,
) -> Rc<RefCell<TestDeserializerState>> {
    Rc::new(RefCell::new(TestDeserializerState {
        execute_result,
        mappings_result,
        received_mappings: Vec::new(),
        execute_calls: 0,
    }))
}

#[test]
fn no_deserializer_returns_error() {
    // Case: fuzz packets are rejected until a deserializer has been configured.
    let mut executor = Executor::new_with_comms(SerialCommsServer::new_with_transport(
        MockSerialIo::default(),
    ));
    let mut packet = fuzz_packet();

    let err = executor
        .on_receive_fuzz_test_packet(&mut packet)
        .expect_err("missing deserializer should fail");

    assert_eq!(err, ExecutorError::NoDeserializerEnabled);
}

#[test]
fn successful_fuzz_packet_returns_ack() {
    // Case: successful testcase execution returns an Ack packet carrying the deserializer code.
    let state = test_state(Ok(77), Ok(()));
    let mut executor = Executor::new_with_comms(SerialCommsServer::new_with_transport(
        MockSerialIo::default(),
    ));
    executor.deserializer = Some(Box::new(TestDeserializer::new(state.clone())));

    let response = executor
        .on_receive_fuzz_test_packet(&mut fuzz_packet())
        .expect("fuzz packet should succeed");

    assert_eq!(
        response,
        Some(OpenTMKPacket::Ack(OpenTMKAckPacket { code: 77 }))
    );
    assert_eq!(state.borrow().execute_calls, 1);
}

#[test]
fn deserializer_failure_returns_error_packet() {
    // Case: deserializer execution failures are surfaced as Error response packets.
    let state = test_state(
        Err(ExecutorError::SyzlangDeserializerFailed(String::from(
            "decode failed",
        ))),
        Ok(()),
    );
    let mut executor = Executor::new_with_comms(SerialCommsServer::new_with_transport(
        MockSerialIo::default(),
    ));
    executor.deserializer = Some(Box::new(TestDeserializer::new(state)));

    let response = executor
        .on_receive_fuzz_test_packet(&mut fuzz_packet())
        .expect("fuzz packet should still produce a response");

    assert_eq!(
        response,
        Some(OpenTMKPacket::Error(OpenTMKErrorPacket {
            message: String::from("SyzlangDeserializerFailed(\"decode failed\")"),
            fatal: false,
        }))
    );
}

#[test]
fn invalid_config_deserializer_returns_error() {
    // Case: configuration packets that select no deserializer fail immediately.
    let mut executor = Executor::new_with_comms(SerialCommsServer::new_with_transport(
        MockSerialIo::default(),
    ));
    let config = OpenTMKConfigurationPacket {
        deserializer: OpenTMKGrammarDeserializer::None,
        mapping: Vec::new(),
    };

    let err = executor
        .on_receive_configuration_packet(&config)
        .expect_err("invalid configuration should fail");

    assert_eq!(err, ExecutorError::NoDeserializerEnabled);
}

#[test]
fn unexpected_ack_and_error_packets_are_rejected() {
    // Case: executor-side Ack and Error packets are treated as protocol violations.
    let mut executor = Executor::new_with_comms(SerialCommsServer::new_with_transport(
        MockSerialIo::default(),
    ));

    assert_eq!(
        executor
            .on_recieve_ack_packet(&OpenTMKAckPacket { code: 0 })
            .expect_err("ack packets should be rejected"),
        ExecutorError::UnexpectedPacketReceived
    );
    assert_eq!(
        executor
            .on_recieve_error_packet(&OpenTMKErrorPacket {
                message: String::from("boom"),
                fatal: false,
            })
            .expect_err("error packets should be rejected"),
        ExecutorError::UnexpectedPacketReceived
    );
}

#[test]
fn framed_fuzz_packet_writes_framed_ack() {
    // Case: processing one framed fuzz packet from mocked comms writes a framed Ack response.
    let state = test_state(Ok(11), Ok(()));
    let packet = OpenTMKPacket::FuzzTest(fuzz_packet());
    let mut executor = Executor::new_with_comms(SerialCommsServer::new_with_transport(
        MockSerialIo::with_reads(frame_packet(&packet)),
    ));
    executor.deserializer = Some(Box::new(TestDeserializer::new(state)));

    executor
        .process_next_packet()
        .expect("packet processing should succeed");

    assert_eq!(
        executor.comms.handle.written_bytes(),
        frame_packet(&OpenTMKPacket::Ack(OpenTMKAckPacket { code: 11 }))
    );
}

#[test]
fn framed_fuzz_packet_writes_framed_error() {
    // Case: processing one framed fuzz packet writes a framed Error packet when execution fails.
    let state = test_state(
        Err(ExecutorError::SyzlangDeserializerFailed(String::from(
            "test failure",
        ))),
        Ok(()),
    );
    let packet = OpenTMKPacket::FuzzTest(fuzz_packet());
    let mut executor = Executor::new_with_comms(SerialCommsServer::new_with_transport(
        MockSerialIo::with_reads(frame_packet(&packet)),
    ));
    executor.deserializer = Some(Box::new(TestDeserializer::new(state)));

    executor
        .process_next_packet()
        .expect("packet processing should succeed");

    assert_eq!(
        executor.comms.handle.written_bytes(),
        frame_packet(&OpenTMKPacket::Error(OpenTMKErrorPacket {
            message: String::from("SyzlangDeserializerFailed(\"test failure\")"),
            fatal: false,
        }))
    );
}
