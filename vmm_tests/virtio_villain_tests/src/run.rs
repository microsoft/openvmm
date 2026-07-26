// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Runs a single villain test in one OpenVMM VM and reads its serial verdict.

use crate::villain::VerdictScan;
use crate::villain::scan_verdict;
use anyhow::Context as _;
use petri_artifacts_common::tags::MachineArch;
use std::path::PathBuf;

/// Guest kernel initcalls to blacklist so the in-guest virtio drivers don't
/// claim the devices villain wants to drive itself. Mirrors villain's own
/// runner (`run`, `skip_initcalls`).
const INITCALL_BLACKLIST: &str = concat!(
    "virtio_blk_init,",
    "virtio_net_driver_init,",
    "virtio_console_init,",
    "virtio_balloon_driver_init,",
    "virtio_rng_driver_init,",
    "virtio_mem_driver_init,",
    "virtio_pmem_driver_init,",
    "virtio_iommu_drv_init,",
    "virtio_fs_init,",
    "virtio_vsock_init,",
    "virtio_rtc_drv_init,",
    "virtio_watchdog_driver_init,",
    "virtio_input_driver_init,",
    "ext4_init_fs,",
    "fuse_init,",
    "audit_init,",
    "cpufreq_core_init",
);

/// Static, per-run VM parameters shared by every villain test.
#[derive(Clone)]
pub struct VmParams {
    /// Path to villain's `initramfs.cpio.gz` (bare PID-1 `init`).
    pub initramfs: PathBuf,
    /// Guest architecture (host arch for Phase 1).
    pub arch: MachineArch,
    /// Guest RAM in bytes.
    pub mem_bytes: u64,
}

/// Boot one VM for `test_name`, wait for it to halt, and scan the serial
/// console log for the verdict marker.
///
/// `log_source` must be rooted at this test's own directory so that its
/// `linux.log` (the teed serial0 console) is isolated from other tests.
pub fn run_one(
    params: &VmParams,
    artifacts: &petri::TestArtifacts,
    log_source: &petri::PetriLogSource,
    test_name: &str,
) -> anyhow::Result<VerdictScan> {
    // Backing resources for the kitchen-sink devices. These must live until
    // after the VM tears down, so they are owned here and only their paths
    // are moved into the config closure.
    let pmem_file = tempfile::NamedTempFile::new().context("failed to create pmem backing file")?;
    pmem_file
        .as_file()
        .set_len(128 * 1024 * 1024)
        .context("failed to size pmem backing file")?;
    let pmem_path = pmem_file.path().to_string_lossy().into_owned();

    let fs_dir = tempfile::tempdir().context("failed to create virtio-fs root")?;
    let fs_path = fs_dir.path().to_string_lossy().into_owned();

    #[cfg(unix)]
    let vsock_dir = tempfile::tempdir().context("failed to create vsock dir")?;
    #[cfg(unix)]
    let vsock_socket = vsock_dir.path().join("vsock");
    // Bind the vsock listener up front so a failure is a hard error rather
    // than a silently-omitted device (which would make vsock tests falsely
    // self-SKIP). The socket lives in a fresh unique tempdir, so binding is
    // expected to succeed on any functioning host.
    #[cfg(unix)]
    let vsock_listener = unix_socket::UnixListener::bind(&vsock_socket).with_context(|| {
        format!(
            "failed to bind vsock listener at {}",
            vsock_socket.display()
        )
    })?;

    let arch = params.arch;
    let console = match arch {
        MachineArch::X86_64 => "ttyS0",
        MachineArch::Aarch64 => "ttyAMA0",
    };
    let cmdline =
        format!("console={console} initcall_blacklist={INITCALL_BLACKLIST} vv.test={test_name}");

    let initramfs = params.initramfs.clone();
    let mem_bytes = params.mem_bytes;

    let log_path = log_source.output_dir().join("linux.log");

    pal_async::DefaultPool::run_with(async |driver| -> anyhow::Result<()> {
        let mut post_test_hooks = Vec::new();
        let petri_params = petri::PetriTestParams {
            test_name,
            logger: log_source,
            post_test_hooks: &mut post_test_hooks,
        };

        let resolver = petri::ArtifactResolver::resolver(artifacts);
        let firmware = petri::Firmware::linux_direct(&resolver, arch);
        let vm_artifacts = petri::PetriVmArtifacts::<petri::openvmm::OpenVmmPetriBackend>::new(
            &resolver, firmware, arch, false,
        )
        .context("firmware/arch not compatible with OpenVMM backend")?;

        let builder = petri::PetriVmBuilder::minimal(petri_params, vm_artifacts, &driver)?
            .with_serial_output()
            .with_processor_topology(petri::ProcessorTopology {
                vp_count: 1,
                ..Default::default()
            })
            .with_memory(petri::MemoryConfig {
                startup_bytes: mem_bytes,
                ..Default::default()
            })
            .with_prebuilt_initrd(initramfs)
            .modify_backend(move |b| {
                attach_kitchen_sink(
                    b,
                    cmdline,
                    pmem_path,
                    fs_path,
                    #[cfg(unix)]
                    vsock_socket,
                    #[cfg(unix)]
                    vsock_listener,
                )
            });

        let mut vm = builder
            .run_without_agent()
            .await
            .context("failed to boot villain VM")?;

        // Villain runs the selected test, prints `[TAG] <name>`, then powers
        // off (reboot(RB_POWER_OFF)). Wait for that halt.
        //
        // A genuinely wedged device model (e.g. a malformed descriptor chain
        // that sends OpenVMM's virtio worker into a non-terminating loop) will
        // never power off and cannot even be torn down. We deliberately do NOT
        // paper over that here: the nextest per-test `slow-timeout`
        // (.config/nextest.toml) terminates such a test and reports it as a
        // failure, which is the correct outcome for a real product bug.
        let halt = vm
            .wait_for_halt()
            .await
            .context("villain VM did not halt")?;
        tracing::info!(?halt, "villain VM halted");
        vm.teardown().await.context("failed to tear down VM")?;
        // This runner uses minimal() and attaches no device that registers a
        // post-test hook, so none should have accumulated. If that ever
        // changes, fail loudly rather than silently dropping the hook (e.g.
        // guest crash-dump extraction) — wire up hook execution here.
        assert!(
            post_test_hooks.is_empty(),
            "villain runner would silently drop {} post-test hook(s); \
             wire up hook execution before attaching hook-registering devices",
            post_test_hooks.len(),
        );
        Ok(())
    })?;

    let log = fs_err::read_to_string(&log_path)
        .with_context(|| format!("failed to read serial log {}", log_path.display()))?;

    // Keep backing resources alive until here.
    drop(pmem_file);
    drop(fs_dir);
    #[cfg(unix)]
    drop(vsock_dir);

    Ok(scan_verdict(&log, test_name))
}

/// Attach every virtio device villain can probe, plus the required cmdline.
/// Devices OpenVMM does not model are simply omitted; villain self-SKIPs them.
fn attach_kitchen_sink(
    b: petri::openvmm::PetriVmConfigOpenVmm,
    cmdline: String,
    pmem_path: String,
    fs_path: String,
    #[cfg(unix)] vsock_socket: PathBuf,
    #[cfg(unix)] vsock_listener: unix_socket::UnixListener,
) -> petri::openvmm::PetriVmConfigOpenVmm {
    use openvmm_defs::config::LoadMode;
    use openvmm_defs::config::PcieDeviceConfig;
    use vm_resource::IntoResource;
    use vm_resource::Resource;
    use vm_resource::kind::VirtioDeviceHandle;

    // Collect every virtio device villain can probe as an inner
    // `Resource<VirtioDeviceHandle>`. Each is later wrapped in a
    // `VirtioPciDeviceHandle` and attached to its own PCIe root port. Devices
    // OpenVMM does not model are simply omitted; villain self-SKIPs them.
    let mut inner: Vec<Resource<VirtioDeviceHandle>> = vec![
        // rng
        virtio_resources::rng::VirtioRngHandle.into_resource(),
        // block (RAM-backed, writable)
        virtio_resources::blk::VirtioBlkHandle {
            disk: disk_backend_resources::LayeredDiskHandle::single_layer(
                disk_backend_resources::layer::RamDiskLayerHandle {
                    len: Some(64 * 1024 * 1024),
                    sector_size: None,
                },
            )
            .into_resource(),
            read_only: false,
        }
        .into_resource(),
        // console (serial backend goes nowhere)
        virtio_resources::console::VirtioConsoleHandle {
            backend: serial_core::resources::DisconnectedSerialBackendHandle.into_resource(),
        }
        .into_resource(),
        // net (null endpoint: drops tx, never rx)
        virtio_resources::net::VirtioNetHandle {
            max_queues: None,
            mac_address: net_backend_resources::mac_address::MacAddress::new([
                0x00, 0x15, 0x5d, 0x00, 0x00, 0x01,
            ]),
            endpoint: net_backend_resources::null::NullHandle.into_resource(),
        }
        .into_resource(),
        // virtio-fs (built-in HostFs backend, no external virtiofsd)
        virtio_resources::fs::VirtioFsHandle {
            tag: "villainfs".into(),
            fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                root_path: fs_path,
                mount_options: String::new(),
            },
        }
        .into_resource(),
        // pmem (file-backed)
        virtio_resources::pmem::VirtioPmemHandle { path: pmem_path }.into_resource(),
    ];

    // vsock (unix only: the listener was bound by the caller so a bind
    // failure is a hard error, not a silently-dropped device).
    #[cfg(unix)]
    inner.push(
        virtio_resources::vsock::VirtioVsockHandle {
            guest_cid: 3,
            base_path: vsock_socket.to_string_lossy().into_owned(),
            listener: vsock_listener,
        }
        .into_resource(),
    );

    // One PCIe root port per device (segment 0, root complex 0).
    b.with_pcie_root_topology(1, 1, inner.len() as u64)
        .with_custom_config(move |c| {
            // Replace the kernel command line wholesale.
            if let LoadMode::Linux { cmdline: cl, .. } = &mut c.load_mode {
                *cl = cmdline;
            }

            for (i, handle) in inner.into_iter().enumerate() {
                c.pcie_devices.push(PcieDeviceConfig {
                    port_name: format!("s0rc0rp{i}"),
                    resource: virtio_resources::VirtioPciDeviceHandle(handle).into_resource(),
                });
            }
        })
}
