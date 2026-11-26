// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use pal_async::DefaultDriver;
use pal_async::async_test;
use std::path::PathBuf;
use std::time::Duration;
use test_with_tracing::test;

mod io_tests;

struct Fixture {
    handler: Arc<DiagServiceHandler>,
    requests: mesh::Receiver<DiagRequest>,
    directory: tempfile::TempDir,
    data: PathBuf,
    _listener: Task<anyhow::Result<()>>,
}

impl Fixture {
    fn new(driver: &DefaultDriver) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data.sock");
        let listener = unix_socket::UnixListener::bind(&data).unwrap();
        let inner = Arc::new(crate::Inner {
            connections: Mutex::new(crate::DataConnections {
                next_id: 1,
                active: HashMap::new(),
            }),
        });
        let (sender, requests) = mesh::channel();
        let handler = Arc::new(DiagServiceHandler::new(sender, inner.clone()));
        let driver_copy = driver.clone();
        let task = driver.spawn("test-data-listener", async move {
            inner
                .process_data_connections(&driver_copy, listener.into())
                .await
        });
        Self {
            handler,
            requests,
            directory,
            data,
            _listener: task,
        }
    }

    async fn connect(
        &self,
        driver: &DefaultDriver,
    ) -> (u64, PolledSocket<unix_socket::UnixStream>) {
        let mut connection = PolledSocket::new(
            driver,
            unix_socket::UnixStream::connect(&self.data).unwrap(),
        )
        .unwrap();
        let mut id = [0; 8];
        connection.read_exact(&mut id).await.unwrap();
        let id = u64::from_ne_bytes(id);
        assert_ne!(id, 0);
        (id, connection)
    }
}

async fn call<T: mesh::MeshPayload>(
    handler: &DiagServiceHandler,
    driver: &DefaultDriver,
    request: impl FnOnce(mesh::OneshotSender<Result<T, mesh_rpc::service::Status>>) -> UnderhillDiag,
) -> Result<T, mesh_rpc::service::Status> {
    let (send, receive) = mesh::oneshot();
    handler
        .handle_diag_request(
            driver,
            request(send),
            CancelContext::new().with_timeout(Duration::from_secs(5)),
        )
        .await;
    receive.await.unwrap()
}

// Test: Forward delayed-start arguments and environment set/remove requests to the VM.
// Expected: Ordering, empty values and removals are preserved and backend failures reach RPC callers.
#[async_test]
async fn start_preserves_parameters_and_backend_errors(driver: DefaultDriver) {
    let mut f = Fixture::new(&driver);
    for fail in [false, true] {
        let handler = f.handler.clone();
        let request = StartRequest {
            args: vec!["--flag".into(), "value with spaces".into()],
            env: vec![
                diag_proto::EnvPair {
                    name: "SET".into(),
                    value: Some("".into()),
                },
                diag_proto::EnvPair {
                    name: "REMOVE".into(),
                    value: None,
                },
            ],
        };
        let (result, ()) = futures::join!(
            call(&handler, &driver, |send| UnderhillDiag::Start(
                request, send
            )),
            async {
                let DiagRequest::Start(rpc) = f.requests.recv().await.unwrap() else {
                    panic!("expected start")
                };
                rpc.handle(async |params| {
                    assert_eq!(params.args, ["--flag", "value with spaces"]);
                    assert_eq!(
                        params.env,
                        [("SET".into(), Some("".into())), ("REMOVE".into(), None)]
                    );
                    if fail {
                        Err(mesh::error::RemoteError::new(anyhow::anyhow!(
                            "start denied"
                        )))
                    } else {
                        Ok(())
                    }
                })
                .await;
            },
        );
        if fail {
            let error = result.unwrap_err();
            assert_eq!(error.code, mesh_rpc::service::Code::Unknown as i32);
            assert!(error.message.contains("start denied"));
        } else {
            result.unwrap();
        }
    }
}

// Test: Route pause, resume, restart, saved-state and crash requests without invoking a real VM.
// Expected: Each RPC reaches the matching backend operation and saved-state bytes are unchanged.
#[async_test]
async fn control_requests_route_to_the_matching_backend(driver: DefaultDriver) {
    let mut f = Fixture::new(&driver);
    for operation in ["pause", "resume", "restart"] {
        let handler = f.handler.clone();
        let (result, ()) = futures::join!(
            call(&handler, &driver, |send| match operation {
                "pause" => UnderhillDiag::Pause((), send),
                "resume" => UnderhillDiag::Resume((), send),
                "restart" => UnderhillDiag::Restart((), send),
                _ => unreachable!(),
            }),
            async {
                let rpc = match (operation, f.requests.recv().await.unwrap()) {
                    ("pause", DiagRequest::Pause(rpc))
                    | ("resume", DiagRequest::Resume(rpc))
                    | ("restart", DiagRequest::Restart(rpc)) => rpc,
                    (_, request) => panic!("incorrect backend request: {request:?}"),
                };
                rpc.complete(Ok(()));
            },
        );
        result.unwrap();
    }
    let handler = f.handler.clone();
    let (saved, ()) = futures::join!(
        call(&handler, &driver, |send| UnderhillDiag::DumpSavedState(
            (),
            send
        )),
        async {
            let DiagRequest::Save(rpc) = f.requests.recv().await.unwrap() else {
                panic!("expected save")
            };
            rpc.complete(Ok(vec![0, 0xff, 3]));
        },
    );
    assert_eq!(saved.unwrap().data, [0, 0xff, 3]);
    call(&handler, &driver, |send| {
        UnderhillDiag::Crash(diag_proto::CrashRequest { pid: 123 }, send)
    })
    .await
    .unwrap();
    assert!(matches!(
        f.requests.recv().await.unwrap(),
        DiagRequest::Crash(123)
    ));
}

// Test: Reject malformed packet-capture operations and forward query/start/stop parameters.
// Expected: Invalid requests do not reach the VM; stream counts, snaplen and socket identities survive.
#[async_test]
async fn packet_capture_validates_and_transfers_streams(driver: DefaultDriver) {
    let mut f = Fixture::new(&driver);
    for request in [
        NetworkPacketCaptureRequest {
            operation: -1,
            ..Default::default()
        },
        NetworkPacketCaptureRequest {
            operation: Operation::Start as i32,
            op_data: None,
        },
        NetworkPacketCaptureRequest {
            operation: Operation::Start as i32,
            op_data: Some(
                diag_proto::network_packet_capture_request::OpData::StartData(
                    diag_proto::StartPacketCaptureData {
                        snaplen: 97,
                        conns: vec![999],
                    },
                ),
            ),
        },
    ] {
        assert!(f.handler.handle_packet_capture(&request).await.is_err());
        assert!(f.requests.try_recv().is_err());
    }
    let (first_id, mut first) = f.connect(&driver).await;
    let (second_id, mut second) = f.connect(&driver).await;
    assert_ne!(first_id, second_id);
    for operation in [Operation::Query, Operation::Start, Operation::Stop] {
        let handler = f.handler.clone();
        let request = NetworkPacketCaptureRequest {
            operation: operation as i32,
            op_data: matches!(operation, Operation::Start).then(|| {
                diag_proto::network_packet_capture_request::OpData::StartData(
                    diag_proto::StartPacketCaptureData {
                        snaplen: 97,
                        conns: vec![first_id, second_id],
                    },
                )
            }),
        };
        let (result, ()) = futures::join!(
            call(&handler, &driver, |send| UnderhillDiag::PacketCapture(
                request, send
            )),
            async {
                let DiagRequest::PacketCapture(rpc) = f.requests.recv().await.unwrap() else {
                    panic!("expected capture")
                };
                rpc.handle(async |params| {
                    let op_data = match (operation, params.operation, params.op_data) {
                        (
                            Operation::Query,
                            PacketCaptureOperation::Query,
                            Some(OperationData::OpQueryData(0)),
                        ) => Some(OperationData::OpQueryData(2)),
                        (
                            Operation::Start,
                            PacketCaptureOperation::Start,
                            Some(OperationData::OpStartData(data)),
                        ) => {
                            assert_eq!(data.snaplen, 97);
                            assert_eq!(data.writers.len(), 2);
                            for (index, mut writer) in data.writers.into_iter().enumerate() {
                                io::Write::write_all(&mut writer, &[index as u8, 0xca]).unwrap();
                            }
                            None
                        }
                        (Operation::Stop, PacketCaptureOperation::Stop, None) => None,
                        params => panic!("incorrect capture parameters: {params:?}"),
                    };
                    Ok(PacketCaptureParams {
                        operation: PacketCaptureOperation::Query,
                        op_data,
                    })
                })
                .await;
            },
        );
        assert_eq!(
            result.unwrap().num_streams,
            if matches!(operation, Operation::Query) {
                2
            } else {
                0
            }
        );
    }
    let mut data = Vec::new();
    first.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, [0, 0xca]);
    data.clear();
    second.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, [1, 0xca]);
    assert!(f.handler.take_connection(first_id).await.is_err());
}

// Test: Issue OpenHCL ping and feature-gated memory profiling without inspecting VM state.
// Expected: Ping succeeds immediately; profiling preserves its PID and exact returned trace bytes.
#[async_test]
async fn openhcl_ping_and_memory_trace_dispatch(driver: DefaultDriver) {
    let mut f = Fixture::new(&driver);
    let (send, receive) = mesh::oneshot();
    f.handler
        .handle_diag2_request(&driver, OpenhclDiag::Ping((), send), CancelContext::new())
        .await;
    receive.await.unwrap().unwrap();
    let (send, receive) = mesh::oneshot();
    let handler = f.handler.clone();
    let process = handler.handle_diag2_request(
        &driver,
        OpenhclDiag::MemoryProfileTrace(MemoryProfileTraceRequest { pid: 42 }, send),
        CancelContext::new(),
    );
    #[cfg(feature = "mem-profile-tracing")]
    {
        let ((), ()) = futures::join!(process, async {
            let DiagRequest::MemoryProfileTrace(rpc) = f.requests.recv().await.unwrap() else {
                panic!("expected memory trace")
            };
            rpc.handle(async |pid| {
                assert_eq!(pid, 42);
                Ok(vec![0, 0xaa, 7])
            })
            .await;
        });
        assert_eq!(receive.await.unwrap().unwrap().data, [0, 0xaa, 7]);
    }
    #[cfg(not(feature = "mem-profile-tracing"))]
    {
        process.await;
        assert!(
            receive
                .await
                .unwrap()
                .unwrap_err()
                .message
                .contains("feature disabled")
        );
    }
}

// Test: Finish request processing after all service providers have disconnected.
// Expected: Processing returns successfully without forwarding any VM request or waiting forever.
#[async_test]
async fn disconnected_services_finish_request_processing(driver: DefaultDriver) {
    let mut f = Fixture::new(&driver);
    f.handler
        .process_requests(
            &driver,
            RpcReceiver::disconnected(),
            RpcReceiver::disconnected(),
            RpcReceiver::disconnected(),
            RpcReceiver::disconnected(),
        )
        .await
        .unwrap();
    assert!(f.requests.try_recv().is_err());
}
