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
//!   `openvmm` process, bridges its hybrid-vsock listener back to the host
//!   via [`PipetteClient::relay_unix_socket`], and brings up a second
//!   [`PipetteClient`] connected to the L2 pipette.

use crate::PetriLogSource;
use crate::PetriVmBuilder;
use crate::vm::openvmm::OpenVmmPetriBackend;
use anyhow::Context as _;
use openvmm_defs::config::PcieDeviceConfig;
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
        fs_err::copy(&cfg.initrd, staging_dir.path().join(STAGED_INITRD_NAME))
            .context("failed to stage L2 initrd")?;

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
    /// Mount the staging share inside L1, spawn the in-L1 openvmm, and
    /// bring up an L2 pipette client.
    ///
    /// `l1_agent` must be the [`PetriVmBuilder`] run's pipette client for
    /// the L1 VM.
    pub async fn launch(self, l1_agent: &PipetteClient) -> anyhow::Result<NestedL2> {
        // 1. Mount the staging share inside L1.
        l1_agent
            .mount(NESTED_VFS_TAG, L1_MOUNT_POINT, "virtiofs", 0, true)
            .await
            .context("failed to mount nested virtio-fs share inside L1")?;

        // 2. Make the staged openvmm binary executable (fs::copy preserves
        //    mode, but virtio-fs default mounts may strip the +x bit, and
        //    we can't easily set host-side perms with fs_err::copy on all
        //    platforms — so be explicit).
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

        // 3. Bind the L1-side hybrid-vsock listener path that the L2's
        //    in-guest pipette will connect to. The in-L1 openvmm's
        //    --vmbus-vsock-path argument tells it to compute outbound
        //    paths as <prefix>_<port>, so we bind the prefix + port
        //    string ourselves.
        let bind_path = format!("{L1_BIND_PREFIX}_{}", PIPETTE_VSOCK_PORT);
        let duplex = l1_agent
            .relay_unix_socket(&bind_path)
            .await
            .context("failed to start RelayUnixSocket on L1 pipette")?;

        // 4. Spawn the in-L1 openvmm. Note: `--com1 console` routes the L2
        //    serial console to the openvmm process's stdout, which we then
        //    capture below.
        let cmdline = {
            let mut s = String::from("console=ttyS0");
            for token in &self.extra_cmdline {
                s.push(' ');
                s.push_str(token);
            }
            s
        };

        let vp_count_str = self.vp_count.to_string();
        // Use private, THP-eligible RAM rather than the default shared
        // file-backed RAM: the shared backing creates a tmpfs file per
        // VM and faults pages in one at a time, which is dramatically
        // slower than anonymous-private + THP for a short-lived L2
        // boot. Documented under `--memory` in openvmm.
        let memory_arg = format!("size={},shared=off,thp=on", self.memory_bytes);
        let mut child = l1_agent
            .command(&staged_openvmm)
            .args([
                "--hypervisor",
                "kvm",
                "--hv",
                "--processors",
                &vp_count_str,
                "--memory",
                &memory_arg,
                "--kernel",
                &staged_kernel,
                "--initrd",
                &staged_initrd,
                "--cmdline",
                &cmdline,
                "--vmbus-vsock-path",
                L1_BIND_PREFIX,
                "--com1",
                "console",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .await
            .context("failed to spawn in-L1 openvmm")?;

        // 5. Mirror the L2 serial console (the in-L1 openvmm's stdout,
        //    which is routed to com1 by `--com1 console`) and the in-L1
        //    openvmm's diagnostic output (its stderr) into petri log
        //    files so that failures - especially early L2 launch
        //    failures, where the L2 pipette never comes up - leave a
        //    diagnosable trace.
        let stdout = child
            .stdout
            .take()
            .context("in-L1 openvmm child had no stdout pipe")?;
        let stderr = child
            .stderr
            .take()
            .context("in-L1 openvmm child had no stderr pipe")?;
        let serial_task = self.driver.spawn(
            "nested-l2-serial",
            crate::log_task(
                self.log_source.log_file("nested-l2-console")?,
                stdout,
                "nested-l2-console",
            ),
        );
        let stderr_task = self.driver.spawn(
            "nested-l2-openvmm-stderr",
            crate::log_task(
                self.log_source.log_file("nested-l2-openvmm")?,
                stderr,
                "nested-l2-openvmm",
            ),
        );

        // 6. Bring up the L2 pipette client over the relayed unix socket.
        //    Race the pipette handshake against the in-L1 openvmm exiting
        //    so that if the L2 fails to launch (or crashes early before
        //    its pipette can connect) we fail fast with the child's exit
        //    status instead of hanging.
        let l2_agent = {
            let pipette_fut =
                PipetteClient::new(&self.driver, duplex, self.log_source.output_dir());
            let wait_fut = child.wait();
            futures::pin_mut!(pipette_fut, wait_fut);
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

        // 7. Confirm the L2 pipette is responsive.
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
