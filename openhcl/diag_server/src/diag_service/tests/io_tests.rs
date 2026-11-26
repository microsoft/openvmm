// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use test_with_tracing::test;

// Test: Execute a child with pipe-backed and raw-socket standard IO and a controlled environment.
// Expected: Input, separate output/error bytes and exit status survive; waiting consumes the PID once.
#[async_test]
async fn exec_round_trips_standard_io_and_environment(driver: DefaultDriver) {
    let f = Fixture::new(&driver);
    for raw_socket_io in [false, true] {
        let (stdin_id, mut stdin) = f.connect(&driver).await;
        let (stdout_id, mut stdout) = f.connect(&driver).await;
        let (stderr_id, mut stderr) = f.connect(&driver).await;
        let request = ExecRequest {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "IFS= read -r line; printf 'out:%s:%s' \"$line\" \"$VALUE\"; printf err >&2; exit 7".into()],
            stdin: stdin_id, stdout: stdout_id, stderr: stderr_id,
            clear_env: true, raw_socket_io,
            env: vec![
                diag_proto::EnvPair { name: "VALUE".into(), value: Some("kept".into()) },
                diag_proto::EnvPair { name: "REMOVE_ME".into(), value: None },
            ],
            ..Default::default()
        };
        let child = call(&f.handler, &driver, |send| {
            UnderhillDiag::Exec(request, send)
        })
        .await
        .unwrap();
        assert!(child.pid > 0);
        stdin.write_all(b"hello world\n").await.unwrap();
        stdin.close().await.unwrap();
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"out:hello world:kept");
        output.clear();
        stderr.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, b"err");
        drop(stdout);
        drop(stderr);
        let request = || WaitRequest { pid: child.pid };
        let waited = call(&f.handler, &driver, |send| {
            UnderhillDiag::Wait(request(), send)
        })
        .await
        .unwrap();
        assert_eq!(waited.exit_code, 7);
        let error = call(&f.handler, &driver, |send| {
            UnderhillDiag::Wait(request(), send)
        })
        .await
        .unwrap_err();
        assert!(error.message.contains("pid not found"));
    }
}

// Test: Execute commands with merged stderr, a pseudo-terminal, and a signal termination.
// Expected: Merged/terminal output is delivered and a signalled child uses the documented 255 fallback.
#[async_test]
async fn exec_merged_terminal_and_signalled_exit(driver: DefaultDriver) {
    let f = Fixture::new(&driver);
    for mode in ["pipes", "raw", "tty"] {
        let (id, mut output) = f.connect(&driver).await;
        let child = f
            .handler
            .handle_exec(
                &driver,
                &ExecRequest {
                    command: "/bin/sh".into(),
                    args: vec!["-c".into(), "printf out; printf err >&2".into()],
                    stdout: id,
                    combine_stderr: true,
                    tty: mode == "tty",
                    raw_socket_io: mode == "raw",
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut data = Vec::new();
        output.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, b"outerr");
        drop(output);
        assert_eq!(
            f.handler
                .handle_wait(&WaitRequest { pid: child.pid })
                .await
                .unwrap()
                .exit_code,
            0
        );
    }
    let child = f
        .handler
        .handle_exec(
            &driver,
            &ExecRequest {
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "kill -TERM $$".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        f.handler
            .handle_wait(&WaitRequest { pid: child.pid })
            .await
            .unwrap()
            .exit_code,
        255
    );
}

// Test: Reject unknown IO connections and a nonexistent executable.
// Expected: Each error identifies the failed operation instead of spawning or waiting on a child.
#[async_test]
async fn exec_invalid_connections_and_missing_command_report_context(driver: DefaultDriver) {
    let f = Fixture::new(&driver);
    for (stdin, stdout, stderr, expected) in [
        (999, 0, 0, "stdin"),
        (0, 999, 0, "stdout"),
        (0, 0, 999, "stderr"),
    ] {
        let error = f
            .handler
            .handle_exec(
                &driver,
                &ExecRequest {
                    command: "/no-such-diag-test-command".into(),
                    stdin,
                    stdout,
                    stderr,
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(&format!("failed to get {expected} conn")));
    }
    let error = f
        .handler
        .handle_exec(
            &driver,
            &ExecRequest {
                command: "/no-such-diag-test-command".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("failed to launch /no-such-diag-test-command"));
}

// Test: Stream a regular file and reject directories, missing paths and unpollable character devices.
// Expected: File bytes are exact; errors retain context and close the consumed data connection.
#[async_test]
async fn file_streams_preserve_contents_and_report_path_errors(driver: DefaultDriver) {
    let f = Fixture::new(&driver);
    let path = f.directory.path().join("payload");
    let expected = vec![0x5a; 4097];
    std::fs::write(&path, &expected).unwrap();
    let (id, mut stream) = f.connect(&driver).await;
    call(&f.handler, &driver, |send| {
        UnderhillDiag::ReadFile(
            FileRequest {
                conn: id,
                follow: false,
                file_path: path.to_str().unwrap().into(),
            },
            send,
        )
    })
    .await
    .unwrap();
    let mut data = Vec::new();
    stream.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, expected);

    for (path, message) in [
        (f.directory.path().to_path_buf(), "cannot read directory"),
        (f.directory.path().join("missing"), "failed to open file"),
    ] {
        let (id, mut stream) = f.connect(&driver).await;
        let error = f
            .handler
            .handle_read_file(
                &driver,
                &FileRequest {
                    conn: id,
                    follow: false,
                    file_path: path.to_str().unwrap().into(),
                },
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(message));
        assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
    }
    for follow in [false, true] {
        let (id, mut stream) = f.connect(&driver).await;
        let error = f
            .handler
            .handle_read_file(
                &driver,
                &FileRequest {
                    conn: id,
                    follow,
                    file_path: "/dev/null".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("failed to create polled pipe"));
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
    }
}

// Test: Relay bounded pipe records in snapshot and follow modes.
// Expected: Each record gains one NUL terminator and EOF closes the stream without losing bytes.
#[async_test]
async fn file_relay_nul_terminates_each_record(driver: DefaultDriver) {
    for follow in [false, true] {
        let (read_pipe, mut write_pipe) = pal::unix::pipe::pair().unwrap();
        let (server, client) = unix_socket::UnixStream::pair().unwrap();
        let file = PolledPipe::new(&driver, read_pipe).unwrap();
        let server = PolledSocket::new(&driver, Socket::from(OwnedFd::from(server))).unwrap();
        let mut client = PolledSocket::new(&driver, client).unwrap();
        io::Write::write_all(&mut write_pipe, b"record\n").unwrap();
        drop(write_pipe);
        let (result, data) = futures::join!(relay_read_file(file, server, follow), async {
            let mut data = Vec::new();
            client.read_to_end(&mut data).await.unwrap();
            data
        });
        result.unwrap();
        assert_eq!(data, b"record\n\0");
    }
}

// Test: Forward profiler parameters and the selected data socket without running the profiler.
// Expected: The backend sees the exact arguments/duration and its bytes reach the requested stream.
#[cfg(feature = "profiler")]
#[async_test]
async fn profiler_preserves_parameters_and_stream(driver: DefaultDriver) {
    let mut f = Fixture::new(&driver);
    let (id, mut client) = f.connect(&driver).await;
    let handler = f.handler.clone();
    let request = ProfileRequest {
        conn: id,
        duration: 17,
        profiler_args: vec!["--sample".into()],
    };
    let (send, receive) = mesh::oneshot();
    let ((), ()) = futures::join!(
        handler.handle_profile_request(AzureProfiler::Profile(request, send), CancelContext::new()),
        async {
            let DiagRequest::Profile(rpc) = f.requests.recv().await.unwrap() else {
                panic!("expected profile")
            };
            rpc.handle(async |mut params| {
                assert_eq!(params.duration, 17);
                assert_eq!(params.profiler_args, ["--sample"]);
                io::Write::write_all(&mut params.conn, b"profile").unwrap();
                Ok(())
            })
            .await;
        },
    );
    receive.await.unwrap().unwrap();
    let mut data = Vec::new();
    client.read_to_end(&mut data).await.unwrap();
    assert_eq!(data, b"profile");
}

// Test: Half-close an unclaimed data connection and wait for the server to close it.
// Expected: The peer reaches EOF and its former connection ID cannot be claimed later.
#[async_test]
async fn disconnected_data_connections_are_rejected(driver: DefaultDriver) {
    let f = Fixture::new(&driver);
    let (id, mut client) = f.connect(&driver).await;
    client.close().await.unwrap();
    assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
    assert!(f.handler.take_connection(id).await.is_err());
}

// Test: Read a real local terminal as a character file in snapshot and follow modes.
// Expected: Records gain one NUL byte; snapshot EOF and a following client's close both stop relaying.
#[async_test]
async fn character_file_snapshot_and_follow_have_bounded_lifetimes(driver: DefaultDriver) {
    let f = Fixture::new(&driver);
    for follow in [false, true] {
        let (mut primary, secondary) = term::open_pty().unwrap();
        let path = std::fs::read_link(format!("/proc/self/fd/{}", secondary.as_raw_fd())).unwrap();
        let (id, mut client) = f.connect(&driver).await;
        io::Write::write_all(&mut primary, b"record\n").unwrap();
        f.handler
            .handle_read_file(
                &driver,
                &FileRequest {
                    conn: id,
                    follow,
                    file_path: path.to_str().unwrap().into(),
                },
            )
            .await
            .unwrap();
        let mut data = [0; 8];
        client.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"record\n\0");
        if follow {
            client.close().await.unwrap();
        }
        assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
    }
}

// Test: Disconnect before the data listener can send its initial connection ID.
// Expected: The abandoned connection cannot be claimed and the next connection remains usable.
#[async_test]
async fn failed_data_handshake_does_not_stop_the_listener(driver: DefaultDriver) {
    let directory = tempfile::tempdir().unwrap();
    let data_path = directory.path().join("data.sock");
    let server =
        crate::DiagServer::new_unix(&directory.path().join("control.sock"), &data_path).unwrap();
    let inner = server.inner.clone();
    // Close the first client before starting the server, so its handshake must fail.
    let abandoned = unix_socket::UnixStream::connect(&data_path).unwrap();
    abandoned
        .as_sock_ref()
        .shutdown(std::net::Shutdown::Both)
        .unwrap();
    drop(abandoned);
    let (cancel, cancelled) = mesh::oneshot();
    let (requests, _requests) = mesh::channel();
    let server_driver = driver.clone();
    let task = driver.spawn("test-handshake-server", async move {
        server.serve(&server_driver, cancelled, requests).await
    });
    let mut client = PolledSocket::new(
        &driver,
        unix_socket::UnixStream::connect(&data_path).unwrap(),
    )
    .unwrap();
    let mut id = [0; 8];
    client.read_exact(&mut id).await.unwrap();
    assert_eq!(u64::from_ne_bytes(id), 2);
    assert!(inner.take_connection(1).await.is_err());
    let mut connection = inner.take_connection(2).await.unwrap();
    connection.write_all(b"still serving").await.unwrap();
    let mut bytes = [0; 13];
    client.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"still serving");
    cancel.send(());
    task.await.unwrap();
}

// Test: Forward a terminal's input stream to a real child and relay its resulting output.
// Expected: The child receives the full input line, emits its expected reply and exits successfully.
#[async_test]
async fn terminal_input_reaches_the_child(driver: DefaultDriver) {
    let f = Fixture::new(&driver);
    let (stdin_id, mut stdin) = f.connect(&driver).await;
    let (stdout_id, mut stdout) = f.connect(&driver).await;
    let child = f
        .handler
        .handle_exec(
            &driver,
            &ExecRequest {
                command: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "IFS= read -r line; printf 'tty:%s' \"$line\"".into(),
                ],
                tty: true,
                stdin: stdin_id,
                stdout: stdout_id,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    stdin.write_all(b"input\n").await.unwrap();
    let mut output = Vec::new();
    stdout.read_to_end(&mut output).await.unwrap();
    // A terminal can echo the input before the child's reply.
    assert!(
        output.ends_with(b"tty:input"),
        "unexpected terminal output: {output:?}"
    );
    drop(stdout);
    assert_eq!(
        f.handler
            .handle_wait(&WaitRequest { pid: child.pid })
            .await
            .unwrap()
            .exit_code,
        0
    );
}

// Test: Echo a multi-buffer binary stream through pipe-backed and raw child IO,
//       then close stdin while receiving stdout, stderr and the exit status.
// Expected: Every byte survives, EOF lets the child finish, and its final stderr
//       marker and nonzero exit status arrive without waiting for client close.
#[async_test]
async fn exec_drains_binary_streams_after_stdin_eof(driver: DefaultDriver) {
    let f = Fixture::new(&driver);
    let expected: Vec<_> = (0..131_073).map(|i| (i % 256) as u8).collect();
    for raw_socket_io in [false, true] {
        let (stdin_id, mut stdin) = f.connect(&driver).await;
        let (stdout_id, mut stdout) = f.connect(&driver).await;
        let (stderr_id, mut stderr) = f.connect(&driver).await;
        let child = f
            .handler
            .handle_exec(
                &driver,
                &ExecRequest {
                    command: "/bin/sh".into(),
                    args: vec!["-c".into(), "cat; printf finished >&2; exit 23".into()],
                    stdin: stdin_id,
                    stdout: stdout_id,
                    stderr: stderr_id,
                    raw_socket_io,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut output = Vec::new();
        let mut error = Vec::new();
        let wait = WaitRequest { pid: child.pid };
        let ((), read_output, read_error, status) = futures::join!(
            async {
                stdin.write_all(&expected).await.unwrap();
                stdin.close().await.unwrap();
            },
            stdout.read_to_end(&mut output),
            stderr.read_to_end(&mut error),
            f.handler.handle_wait(&wait),
        );
        read_output.unwrap();
        read_error.unwrap();
        assert_eq!(output, expected);
        assert_eq!(error, b"finished");
        assert_eq!(status.unwrap().exit_code, 23);
    }
}
