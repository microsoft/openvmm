// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Nested-launch helper for running an L2 OpenVMM guest inside an L1 OpenVMM
//! guest controlled by petri.
//!
//! The helper is split into two halves:
//!
//! * [`PetriVmBuilder::with_nested_l2`] (build-time) stages the L2 artifacts
//!   (an `x86_64-unknown-linux-musl` `openvmm` binary, an L2 kernel, and an
//!   L2 initrd) into a temporary directory and attaches that directory to
//!   the L1 VM as a virtio-fs share. It returns a [`NestedL2Builder`] handle
//!   that captures the runtime knobs needed to actually start the L2.
//! * [`NestedL2Builder::launch`] (runtime, after L1 has booted and the L1
//!   pipette is online) mounts the share inside L1, spawns the in-L1
//!   `openvmm` process in ttrpc server mode, issues `CreateVm` and
//!   `ResumeVm` RPCs to configure and start the L2, and brings up a second
//!   [`PipetteClient`] connected to the L2 pipette.

use crate::PetriLogSource;
use crate::PetriVmBuilder;
use crate::vm::openvmm::OpenVmmPetriBackend;
use anyhow::Context as _;
use mesh_rpc::client::ExistingConnection;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultDriver;
use pal_async::task::Spawn as _;
use pal_async::task::Task;
use pipette_client::PIPETTE_VSOCK_PORT;
use pipette_client::PipetteClient;
use pipette_client::process::Child;
use pipette_client::process::ExitStatus;
use pipette_client::process::Stdio;
use std::path::PathBuf;
use tempfile::TempDir;
use vm_resource::IntoResource as _;

/// Virtio-fs tag used to expose the staged L2 artifacts from the host to L1.
const NESTED_VFS_TAG: &str = "petri_nested_vfs";

/// In-L1 mount point for the staging share.
const L1_MOUNT_POINT: &str = "/mnt/vfs/nested";

/// File name (inside the staging share) of the in-guest `openvmm` binary.
const STAGED_OPENVMM_NAME: &str = "openvmm";
/// File name (inside the staging share) of the L2 kernel image.
const STAGED_KERNEL_NAME: &str = "kernel";
/// File name (inside the staging share) of the L2 initrd image.
const STAGED_INITRD_NAME: &str = "initrd";

/// L1-side base path for the L2's hybrid-vsock listener. The in-L1 openvmm
/// connects out to `<L1_BIND_PREFIX>_<port>` when the guest opens an
/// AF_VSOCK connection on `<port>`, so the L1 pipette will bind
/// `<L1_BIND_PREFIX>_PIPETTE_VSOCK_PORT` to catch the L2 pipette's outbound
/// connection.
const L1_BIND_PREFIX: &str = "/tmp/petri-l2-vsock";

/// L1-side path for the in-L1 openvmm's ttrpc control socket.
const L1_TTRPC_SOCKET: &str = "/tmp/petri-l2-ttrpc";

/// L1-side path where openvmm connects for L2 COM1 serial output.
const L1_SERIAL_SOCKET: &str = "/tmp/petri-l2-com1";

/// Build-time configuration for an L2 nested guest.
///
/// All three file paths must point at host-side files; they are copied
/// into a staging directory at [`PetriVmBuilder::with_nested_l2`] time so
/// the helper does not need to keep the originals alive for the lifetime
/// of the L1 VM.
pub struct NestedL2Config {
    /// Host-side path to the `x86_64-unknown-linux-musl` `openvmm` binary
    /// to run inside L1 as the L2 hypervisor.
    pub openvmm_musl: PathBuf,
    /// Host-side path to the L2 kernel image (Linux direct boot).
    pub kernel: PathBuf,
    /// Host-side path to the L2 initrd image.
    pub initrd: PathBuf,
    /// Number of virtual processors to give the L2.
    pub vp_count: u32,
    /// Memory size in bytes to give the L2.
    pub memory_bytes: u64,
    /// Additional kernel command-line tokens, appended after the
    /// helper's defaults.
    pub extra_cmdline: Vec<String>,
}

impl NestedL2Config {
    /// Create a config from the three required artifact paths, with
    /// reasonable defaults (1 vp, 256 MiB, no extra cmdline).
    pub fn new(openvmm_musl: PathBuf, kernel: PathBuf, initrd: PathBuf) -> Self {
        Self {
            openvmm_musl,
            kernel,
            initrd,
            vp_count: 1,
            memory_bytes: 256 * 1024 * 1024,
            extra_cmdline: Vec::new(),
        }
    }
}

/// Builder-side handle returned by [`PetriVmBuilder::with_nested_l2`]. Owns
/// the staging tempdir (which must live for the lifetime of the L1 VM)
/// and the parameters needed to spawn the in-L1 openvmm.
pub struct NestedL2Builder {
    staging_dir: TempDir,
    driver: DefaultDriver,
    log_source: PetriLogSource,
    vp_count: u32,
    memory_bytes: u64,
    extra_cmdline: Vec<String>,
}

/// Runtime handle returned by [`NestedL2Builder::launch`]. Owns the L2
/// pipette client, the in-L1 openvmm child process, and the staging
/// tempdir (transferred from the builder). The L2 serial console output
/// is logged through petri's standard log-file plumbing to
/// `nested-l2-console.log`; the in-L1 openvmm's diagnostic output is
/// logged to `nested-l2-openvmm.log`.
pub struct NestedL2 {
    l2_agent: PipetteClient,
    child: Child,
    _serial_task: Task<anyhow::Result<()>>,
    _stderr_task: Task<anyhow::Result<()>>,
    _staging_dir: TempDir,
}

impl NestedL2 {
    /// The L2 pipette client.
    pub fn l2_agent(&self) -> &PipetteClient {
        &self.l2_agent
    }

    /// Wait for the in-L1 openvmm process to exit and return its status.
    pub async fn wait_for_exit(&mut self) -> Result<ExitStatus, mesh::RecvError> {
        self.child.wait().await
    }
}

impl PetriVmBuilder<OpenVmmPetriBackend> {
    /// Configure the L1 VM to host a nested L2 OpenVMM guest.
    ///
    /// This stages the L2 artifacts (musl `openvmm` binary, kernel, initrd)
    /// into a temporary directory on the host and attaches that directory
    /// to the L1 VM as a read-only virtio-fs share. The returned
    /// [`NestedL2Builder`] must be retained until after the L1 VM has
    /// booted and its pipette is available; calling
    /// [`NestedL2Builder::launch`] then mounts the share inside L1 and
    /// spawns the in-L1 openvmm.
    ///
    /// Internally calls [`PetriVmBuilder::modify_backend`] (which composes
    /// across calls) and `with_nested_virt`, so the L1 is configured to
    /// expose nested-virtualization extensions to the guest.
    pub fn with_nested_l2(self, cfg: NestedL2Config) -> anyhow::Result<(Self, NestedL2Builder)> {
        let staging_dir = tempfile::Builder::new()
            .prefix("petri-nested-l2-")
            .tempdir()
            .context("failed to create nested-L2 staging tempdir")?;

        fs_err::copy(
            &cfg.openvmm_musl,
            staging_dir.path().join(STAGED_OPENVMM_NAME),
        )
        .context("failed to stage in-guest openvmm binary")?;
        fs_err::copy(&cfg.kernel, staging_dir.path().join(STAGED_KERNEL_NAME))
            .context("failed to stage L2 kernel")?;

        // Inject pipette into the L2 initrd so the L2 guest can run
        // pipette as PID 1 (via rdinit=/pipette on the kernel cmdline).
        // This is the same mechanism petri uses for L1 linux-direct boots.
        let pipette_path = self
            .pipette_binary
            .as_ref()
            .context("nested L2 requires a pipette binary on the L1 builder")?;
        let initrd_gz = fs_err::read(&cfg.initrd).context("failed to read L2 initrd")?;
        let pipette_data = fs_err::read(pipette_path.get())
            .context("failed to read pipette binary for L2 initrd injection")?;
        let merged_gz =
            crate::cpio::inject_into_initrd(&initrd_gz, "pipette", &pipette_data, 0o100755)
                .context("failed to inject pipette into L2 initrd")?;
        fs_err::write(staging_dir.path().join(STAGED_INITRD_NAME), &merged_gz)
            .context("failed to write L2 initrd with pipette")?;

        let driver = self.resources.driver.clone();
        let log_source = self.resources.log_source.clone();
        let staging_root_path = staging_dir.path().to_string_lossy().into_owned();

        let builder = self.modify_backend(move |b| {
            // virtio-fs holds host-side filesystem state that cannot be
            // round-tripped through save/restore, so opt out of the
            // framework's default save/restore smoke check. See
            // `PetriVmConfigOpenVmm::without_save_restore_check`.
            b.with_nested_virt()
                .without_save_restore_check()
                .with_pcie_root_topology(1, 1, 1)
                .with_custom_config(move |c| {
                    c.pcie_devices.push(PcieDeviceConfig {
                        port_name: "s0rc0rp0".into(),
                        resource: virtio_resources::VirtioPciDeviceHandle(
                            virtio_resources::fs::VirtioFsHandle {
                                tag: NESTED_VFS_TAG.into(),
                                fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                                    root_path: staging_root_path,
                                    mount_options: String::new(),
                                },
                            }
                            .into_resource(),
                        )
                        .into_resource(),
                    });
                })
        });

        Ok((
            builder,
            NestedL2Builder {
                staging_dir,
                driver,
                log_source,
                vp_count: cfg.vp_count,
                memory_bytes: cfg.memory_bytes,
                extra_cmdline: cfg.extra_cmdline,
            },
        ))
    }
}

impl NestedL2Builder {
    /// Mount the staging share inside L1, spawn the in-L1 openvmm in ttrpc
    /// server mode, issue `CreateVm`/`ResumeVm` RPCs, and bring up an L2
    /// pipette client.
    ///
    /// `l1_agent` must be the [`PetriVmBuilder`] run's pipette client for
    /// the L1 VM.
    pub async fn launch(self, l1_agent: &PipetteClient) -> anyhow::Result<NestedL2> {
        // 1. Mount the staging share inside L1.
        l1_agent
            .mount(NESTED_VFS_TAG, L1_MOUNT_POINT, "virtiofs", 0, true)
            .await
            .context("failed to mount nested virtio-fs share inside L1")?;

        // 2. Make the staged openvmm binary executable.
        let staged_openvmm = format!("{L1_MOUNT_POINT}/{STAGED_OPENVMM_NAME}");
        let staged_kernel = format!("{L1_MOUNT_POINT}/{STAGED_KERNEL_NAME}");
        let staged_initrd = format!("{L1_MOUNT_POINT}/{STAGED_INITRD_NAME}");
        let chmod_out = l1_agent
            .command("chmod")
            .args(["+x", &staged_openvmm])
            .output()
            .await
            .context("failed to invoke chmod inside L1")?;
        if !chmod_out.status.success() {
            anyhow::bail!(
                "chmod +x of staged openvmm failed: status={:?}, stderr={}",
                chmod_out.status,
                String::from_utf8_lossy(&chmod_out.stderr),
            );
        }

        // 3. Set up relays for the L2 pipette vsock and L2 serial console.
        //    Both are bound by pipette inside L1 *before* openvmm starts,
        //    so the paths exist when openvmm attempts to connect.
        let vsock_bind_path = format!("{L1_BIND_PREFIX}_{}", PIPETTE_VSOCK_PORT);
        let pipette_duplex = l1_agent
            .relay_unix_socket(&vsock_bind_path)
            .await
            .context("failed to start vsock RelayUnixSocket on L1 pipette")?;

        let serial_duplex = l1_agent
            .relay_unix_socket(L1_SERIAL_SOCKET)
            .await
            .context("failed to start serial RelayUnixSocket on L1 pipette")?;

        // 4. Spawn the in-L1 openvmm in ttrpc server mode. In this mode,
        //    openvmm binds a ttrpc socket and waits for RPCs; it closes
        //    stdout to signal readiness.
        let mut child = l1_agent
            .command(&staged_openvmm)
            .args(["--ttrpc", L1_TTRPC_SOCKET])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .await
            .context("failed to spawn in-L1 openvmm")?;

        // 5. Capture stderr for diagnostics.
        let stderr = child
            .stderr
            .take()
            .context("in-L1 openvmm child had no stderr pipe")?;
        let stderr_task = self.driver.spawn(
            "nested-l2-openvmm-stderr",
            crate::log_task(
                self.log_source.log_file("nested-l2-openvmm")?,
                stderr,
                "nested-l2-openvmm",
            ),
        );

        // 6. Wait for openvmm to signal readiness by closing stdout.
        //    Race against the child exiting so we fail fast on startup
        //    errors.
        {
            let mut stdout = child
                .stdout
                .take()
                .context("in-L1 openvmm child had no stdout pipe")?;
            let read_fut = async {
                use futures::AsyncReadExt;
                let mut buf = [0u8; 1];
                stdout
                    .read(&mut buf)
                    .await
                    .context("reading from in-L1 openvmm stdout")
            };
            let wait_fut = child.wait();
            let read_fut = std::pin::pin!(read_fut);
            let wait_fut = std::pin::pin!(wait_fut);
            match futures::future::select(read_fut, wait_fut).await {
                futures::future::Either::Left((Ok(0), _)) => {
                    // stdout closed — openvmm is ready.
                }
                futures::future::Either::Left((Ok(_), _)) => {
                    anyhow::bail!(
                        "in-L1 openvmm wrote unexpected data to stdout \
                         (expected stdout close as readiness signal)"
                    );
                }
                futures::future::Either::Left((Err(e), _)) => {
                    return Err(e);
                }
                futures::future::Either::Right((status, _)) => {
                    let status =
                        status.context("waiting for in-L1 openvmm to report exit status")?;
                    anyhow::bail!(
                        "in-L1 openvmm exited before becoming ready: {status:?} \
                         (see nested-l2-openvmm.log)"
                    );
                }
            }
        }

        // 7. Relay the ttrpc socket from L1 to the host, then issue RPCs.
        let ttrpc_duplex = l1_agent
            .relay_connect_unix_socket(L1_TTRPC_SOCKET)
            .await
            .context("failed to relay ttrpc socket from L1")?;

        let client = mesh_rpc::Client::new(&self.driver, ExistingConnection::new(ttrpc_duplex));

        // 8. Build and send CreateVm request.
        // Build the L2 kernel command line.
        let cmdline = {
            let mut s = String::from(
                "console=ttyS0 panic=-1 rdinit=/pipette \
                 initcall_blacklist=virtio_vsock_init",
            );
            for token in &self.extra_cmdline {
                s.push(' ');
                s.push_str(token);
            }
            s
        };

        client
            .call()
            .start(
                vmservice::Vm::CreateVm,
                vmservice::CreateVmRequest {
                    config: Some(vmservice::VmConfig {
                        memory_config: Some(vmservice::MemoryConfig {
                            memory_mb: self.memory_bytes / (1024 * 1024),
                            private_memory: true,
                            transparent_hugepages: true,
                            ..Default::default()
                        }),
                        processor_config: Some(vmservice::ProcessorConfig {
                            processor_count: self.vp_count,
                            ..Default::default()
                        }),
                        boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                            vmservice::DirectBoot {
                                kernel_path: staged_kernel,
                                initrd_path: staged_initrd,
                                kernel_cmdline: cmdline,
                            },
                        )),
                        serial_config: Some(vmservice::SerialConfig {
                            ports: vec![vmservice::serial_config::Config {
                                port: 0,
                                socket_path: L1_SERIAL_SOCKET.to_string(),
                                connect: true,
                            }],
                        }),
                        hvsocket_config: Some(vmservice::HvSocketConfig {
                            path: L1_BIND_PREFIX.to_string(),
                        }),
                        ..Default::default()
                    }),
                    log_id: "nested-l2".to_string(),
                },
            )
            .await
            .map_err(|s| anyhow::anyhow!("CreateVm RPC failed: {} (code {})", s.message, s.code))?;

        // 9. Log the L2 serial console output (relayed from the L1 serial
        //    socket).
        let serial_task = self.driver.spawn(
            "nested-l2-serial",
            crate::log_task(
                self.log_source.log_file("nested-l2-console")?,
                serial_duplex,
                "nested-l2-console",
            ),
        );

        // 10. Resume the VM (ttrpc creates VMs in paused state).
        client
            .call()
            .start(vmservice::Vm::ResumeVm, ())
            .await
            .map_err(|s| anyhow::anyhow!("ResumeVm RPC failed: {} (code {})", s.message, s.code))?;

        // 11. Bring up the L2 pipette client over the relayed vsock.
        //     Race the pipette handshake against the in-L1 openvmm exiting
        //     so that if the L2 fails to launch we fail fast.
        let l2_agent = {
            let pipette_fut =
                PipetteClient::new(&self.driver, pipette_duplex, self.log_source.output_dir());
            let wait_fut = child.wait();
            let pipette_fut = std::pin::pin!(pipette_fut);
            let wait_fut = std::pin::pin!(wait_fut);
            match futures::future::select(pipette_fut, wait_fut).await {
                futures::future::Either::Left((res, _)) => {
                    res.context("failed to set up L2 PipetteClient")?
                }
                futures::future::Either::Right((status, _)) => {
                    let status =
                        status.context("waiting for in-L1 openvmm to report exit status")?;
                    anyhow::bail!(
                        "in-L1 openvmm exited before L2 pipette connected: {status:?} \
                         (see nested-l2-console.log and nested-l2-openvmm.log)"
                    );
                }
            }
        };

        // 12. Confirm the L2 pipette is responsive.
        l2_agent.ping().await.context("L2 pipette ping failed")?;

        Ok(NestedL2 {
            l2_agent,
            child,
            _serial_task: serial_task,
            _stderr_task: stderr_task,
            _staging_dir: self.staging_dir,
        })
    }
}
