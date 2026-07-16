// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration test for OpenVMM's fd-passing protocol on the TTRPC endpoint.
//!
//! This boots a real `openvmm --rpc` process and drives the bespoke
//! fd-passing protocol (see `openvmm_ttrpc_vmservice/src/fd_passing.md`) over
//! the same Unix socket, then confirms that a descriptor registered on the
//! fd-passing connection is resolvable by name from a *separate* TTRPC
//! `CreateVm` connection via a `TapBackend { fd_name }` NIC.
//!
//! The feature is UNIX-only and tap `fd_name` resolution is Linux-only, so this
//! test is gated to `target_os = "linux"` where it is declared in the parent
//! `ttrpc` module.

use anyhow::Context;
use futures::AsyncReadExt as _;
use futures::AsyncWriteExt as _;
use guid::Guid;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultPool;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::AsSockRef;
use pal_async::socket::PolledSocket;
use petri::ResolvedArtifact;
use petri_artifacts_vmm_test::artifacts;
use std::future::poll_fn;
use std::io::IoSlice;
use std::os::fd::AsFd;
use std::os::fd::BorrowedFd;
use unix_socket::UnixStream;

/// The 4-byte fd-passing handshake magic: `0xFD 'F' 'D' 0x01`.
const HANDSHAKE_MAGIC: [u8; 4] = [0xFD, b'F', b'D', 0x01];

const OPCODE_REGISTER: u8 = 1;
const OPCODE_DEREGISTER: u8 = 2;

petri::test!(fd_passing_tap, |resolver| {
    // Only supported on x86_64 for now (matches the TTRPC interface test).
    if petri_artifacts_common::tags::MachineArch::host()
        != petri_artifacts_common::tags::MachineArch::X86_64
    {
        return None;
    }
    let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
    let kernel = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE);
    let initrd = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_NATIVE);
    Some([openvmm.erase(), kernel.erase(), initrd.erase()])
});

fn fd_passing_tap(
    params: petri::PetriTestParams<'_>,
    [openvmm, kernel_path, initrd_path]: [ResolvedArtifact; 3],
) -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let socket_path = tempdir.path().join("ttrpc.sock");
    let pidfile_path = tempdir.path().join("openvmm.pid");

    DefaultPool::run_with(async |driver| {
        let (mut child, client, _stderr_task) =
            super::launch_openvmm(&driver, &params, &openvmm, &socket_path, &pidfile_path).await?;

        // Open a dedicated fd-passing connection and exercise the wire protocol
        // against the live server. Async IO keeps the single-threaded executor
        // free to run co-scheduled tasks (e.g. the stderr pump).
        let mut fd_conn = PolledSocket::connect_unix(&driver, &socket_path)
            .await
            .context("connecting fd-passing socket")?;
        handshake(&mut fd_conn)
            .await
            .context("fd-passing handshake")?;

        // A real (but non-tap) descriptor to register: one end of a socket
        // pair. The other end is held open so the fd stays valid.
        let (registered_fd, _keepalive) = UnixStream::pair()?;
        let registered_fd = registered_fd.as_fd();

        // Register succeeds.
        let (status, msg) = register(&mut fd_conn, "t0", registered_fd).await?;
        anyhow::ensure!(status == 0, "register 't0' failed: {msg}");
        // Registering the same name again fails; the connection stays usable.
        let (status, _) = register(&mut fd_conn, "t0", registered_fd).await?;
        anyhow::ensure!(status != 0, "duplicate register unexpectedly succeeded");
        // Registering without an attached descriptor fails.
        let (status, _) = register_without_fd(&mut fd_conn, "t2").await?;
        anyhow::ensure!(status != 0, "register without fd unexpectedly succeeded");
        // Deregistering an unknown name fails.
        let (status, _) = deregister(&mut fd_conn, "nope").await?;
        anyhow::ensure!(
            status != 0,
            "deregister of unknown name unexpectedly succeeded"
        );
        // Deregister then re-register so 't0' is available for CreateVm below.
        let (status, _) = deregister(&mut fd_conn, "t0").await?;
        anyhow::ensure!(status == 0, "deregister 't0' failed");
        let (status, msg) = register(&mut fd_conn, "t0", registered_fd).await?;
        anyhow::ensure!(status == 0, "re-register 't0' failed: {msg}");
        // NOTE: `fd_conn` is intentionally kept open for the rest of the test:
        // names are dropped when the registering connection closes.

        // Negative case: an unregistered `fd_name` fails to resolve during
        // CreateVm, proving the proto -> registry resolution path is wired in.
        let err = client
            .call()
            .start(
                vmservice::Vm::CreateVm,
                create_vm_request(&kernel_path, &initrd_path, "does-not-exist"),
            )
            .await
            .unwrap_err();
        assert!(
            err.message.contains("failed to resolve tap fd"),
            "expected an fd resolution error, got: {}",
            err.message
        );

        // Positive case: a `fd_name` registered on the *fd-passing* connection
        // resolves from this *separate* TTRPC connection, proving both share one
        // global registry. The descriptor is a socket pair, not a real tap, so
        // VM bring-up still fails -- but past resolution, with a different
        // error (never the "failed to resolve" error above).
        let result = client
            .call()
            .start(
                vmservice::Vm::CreateVm,
                create_vm_request(&kernel_path, &initrd_path, "t0"),
            )
            .await;
        match result {
            // Resolution succeeded and the VM was created; tear it back down.
            Ok(()) => {
                let _ = client.call().start(vmservice::Vm::TeardownVm, ()).await;
            }
            Err(err) => assert!(
                !err.message.contains("failed to resolve tap fd"),
                "registered fd_name should have resolved, got: {}",
                err.message
            ),
        }

        // Shut down openvmm and confirm a clean exit.
        let _ = client.call().start(vmservice::Vm::Quit, ()).await;
        drop(fd_conn);

        let exit_status = child.wait().await?;
        tracing::info!(?exit_status, "openvmm exited");
        assert!(
            exit_status.success(),
            "openvmm exited abnormally: {exit_status:?}"
        );
        assert!(
            !pidfile_path.exists(),
            "pidfile should be removed after exit"
        );
        Ok(())
    })
}

/// Sends the client handshake and validates the server's reply.
async fn handshake(sock: &mut PolledSocket<UnixStream>) -> anyhow::Result<()> {
    let mut client_handshake = [0u8; 8];
    client_handshake[..4].copy_from_slice(&HANDSHAKE_MAGIC);
    sock.write_all(&client_handshake).await?;
    let mut server_handshake = [0u8; 8];
    sock.read_exact(&mut server_handshake).await?;
    anyhow::ensure!(
        server_handshake[..4] == HANDSHAKE_MAGIC,
        "bad server handshake magic: {:x?}",
        &server_handshake[..4]
    );
    Ok(())
}

/// Sends a `Register` request carrying `fd` and returns the response.
async fn register(
    sock: &mut PolledSocket<UnixStream>,
    name: &str,
    fd: BorrowedFd<'_>,
) -> anyhow::Result<(u8, String)> {
    let mut frame = vec![OPCODE_REGISTER, name.len() as u8];
    frame.extend_from_slice(name.as_bytes());
    send_frame(sock, &frame, &[fd]).await?;
    read_response(sock).await
}

/// Sends a `Register` request with no attached descriptor (an error case).
async fn register_without_fd(
    sock: &mut PolledSocket<UnixStream>,
    name: &str,
) -> anyhow::Result<(u8, String)> {
    let mut frame = vec![OPCODE_REGISTER, name.len() as u8];
    frame.extend_from_slice(name.as_bytes());
    sock.write_all(&frame).await?;
    read_response(sock).await
}

/// Sends a `Deregister` request and returns the response.
async fn deregister(
    sock: &mut PolledSocket<UnixStream>,
    name: &str,
) -> anyhow::Result<(u8, String)> {
    let mut frame = vec![OPCODE_DEREGISTER, name.len() as u8];
    frame.extend_from_slice(name.as_bytes());
    sock.write_all(&frame).await?;
    read_response(sock).await
}

/// Sends `frame` with `fds` attached, awaiting write readiness so the executor
/// thread is never blocked.
///
/// The descriptors ride on the first `sendmsg` only: once any bytes are
/// accepted the kernel has taken the `SCM_RIGHTS`, so the remainder of a short
/// write is sent with no ancillary data. This both guarantees the whole frame
/// is written and that the fds are delivered exactly once.
async fn send_frame(
    sock: &mut PolledSocket<UnixStream>,
    frame: &[u8],
    fds: &[BorrowedFd<'_>],
) -> anyhow::Result<()> {
    let mut pos = 0;
    while pos < frame.len() {
        // Attach the fds only while nothing has been sent yet.
        let fds: &[BorrowedFd<'_>] = if pos == 0 { fds } else { &[] };
        let n = poll_fn(|cx| {
            sock.poll_io(cx, InterestSlot::Write, PollEvents::OUT, |this| {
                unix_socket::send_with_fds(
                    this.get().as_sock_ref().as_fd(),
                    &[IoSlice::new(&frame[pos..])],
                    fds.iter().copied(),
                )
            })
        })
        .await?;
        anyhow::ensure!(n > 0, "sendmsg accepted 0 bytes");
        pos += n;
    }
    Ok(())
}

/// Reads one response frame: `status: u8`, `msg_len: u16` (LE), `msg`.
async fn read_response(sock: &mut PolledSocket<UnixStream>) -> anyhow::Result<(u8, String)> {
    let mut hdr = [0u8; 3];
    sock.read_exact(&mut hdr).await?;
    let status = hdr[0];
    let msg_len = u16::from_le_bytes([hdr[1], hdr[2]]) as usize;
    let mut msg = vec![0u8; msg_len];
    sock.read_exact(&mut msg).await?;
    Ok((status, String::from_utf8_lossy(&msg).into_owned()))
}

/// Builds a minimal direct-boot `CreateVm` request with a single tap NIC whose
/// backing descriptor is resolved by `fd_name`.
fn create_vm_request(
    kernel_path: &ResolvedArtifact,
    initrd_path: &ResolvedArtifact,
    fd_name: &str,
) -> vmservice::CreateVmRequest {
    vmservice::CreateVmRequest {
        config: Some(vmservice::VmConfig {
            memory_config: Some(vmservice::MemoryConfig {
                memory_mb: 256,
                ..Default::default()
            }),
            processor_config: Some(vmservice::ProcessorConfig {
                processor_count: 1,
                ..Default::default()
            }),
            boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                vmservice::DirectBoot {
                    kernel_path: kernel_path.get().to_string_lossy().to_string(),
                    initrd_path: initrd_path.get().to_string_lossy().to_string(),
                    kernel_cmdline: "console=ttyS0 rdinit=/bin/busybox panic=-1 -- poweroff -f"
                        .to_string(),
                },
            )),
            devices_config: Some(vmservice::DevicesConfig {
                nic_config: vec![vmservice::NicConfig {
                    nic_id: Guid::new_random().to_string(),
                    mac_address: "00-15-5D-12-12-14".to_string(),
                    backend: Some(vmservice::nic_config::Backend::Tap(vmservice::TapBackend {
                        source: Some(vmservice::tap_backend::Source::FdName(fd_name.to_string())),
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }),
        log_id: String::new(),
    }
}
