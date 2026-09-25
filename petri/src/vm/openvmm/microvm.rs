// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM-specific microVM construction and runtime support.

use super::construct::SerialData;
use super::runtime::PetriVmInner;
use crate::Firmware;
use crate::PetriLogFile;
use anyhow::Context;
use chipset_resources::microvm::MicrovmPortbHandle;
use chipset_resources::microvm::MicrovmShutdownHandle;
use chipset_resources::microvm::MicrovmSnapshotRequestHandle;
use futures::AsyncWriteExt;
use openvmm_defs::config::Config;
use openvmm_defs::config::LinuxDirectBootMode;
use openvmm_defs::config::LinuxIsolationConfig;
use openvmm_defs::config::LoadMode;
use openvmm_defs::microvm::MachineProfile;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use pal_async::socket::WriteHalf;
use pal_async::task::Spawn;
use petri_artifacts_common::tags::MachineArch;
use serial_core::resources::DisconnectedSerialBackendHandle;
use unix_socket::UnixStream;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::SerialBackendHandle;
use vmotherboard::ChipsetDeviceHandle;

pub(super) struct RuntimeResources {
    input: WriteHalf<UnixStream>,
    output: mesh::Receiver<Vec<u8>>,
}

pub(super) struct PortbSerial {
    io: Resource<SerialBackendHandle>,
    runtime: RuntimeResources,
}

pub(super) fn is_enabled(machine_profile: MachineProfile) -> bool {
    machine_profile == MachineProfile::Microvm
}

pub(super) fn validate_profile(
    machine_profile: MachineProfile,
    arch: MachineArch,
    firmware: &Firmware,
) -> anyhow::Result<()> {
    if is_enabled(machine_profile) {
        anyhow::ensure!(
            matches!(arch, MachineArch::X86_64),
            "microVM requires an x86-64 guest"
        );
        anyhow::ensure!(
            matches!(firmware, Firmware::LinuxDirect { .. }),
            "microVM requires an uncompressed Linux direct artifact"
        );
    }
    Ok(())
}

pub(super) fn linux_command_line(
    machine_profile: MachineProfile,
    standard: String,
    init: &str,
    vsock_blacklist: &str,
) -> String {
    if is_enabled(machine_profile) {
        format!("rdinit={init} {vsock_blacklist}")
    } else {
        standard
    }
}

pub(super) fn configure_load_mode(
    machine_profile: MachineProfile,
    load_mode: &mut LoadMode,
    processor_count: u32,
) -> anyhow::Result<()> {
    if !is_enabled(machine_profile) {
        return Ok(());
    }

    let LoadMode::Linux {
        cmdline,
        enable_serial,
        isolation,
        boot_mode,
        smbios,
        ..
    } = load_mode
    else {
        unreachable!("microVM firmware was validated as LinuxDirect");
    };
    let mut configured =
        openvmm_defs::microvm::build_microvm_command_line(&[std::mem::take(cmdline)], false)?;
    openvmm_defs::microvm::append_microvm_processor_limit(&mut configured, processor_count)?;
    *cmdline = configured;
    *enable_serial = false;
    *isolation = LinuxIsolationConfig::None;
    *boot_mode = LinuxDirectBootMode::MpTable;
    **smbios = Default::default();
    Ok(())
}

pub(super) fn configure_serial(
    driver: &DefaultDriver,
    log_file: PetriLogFile,
    host: PolledSocket<UnixStream>,
    io: Option<Resource<SerialBackendHandle>>,
) -> anyhow::Result<SerialData> {
    let (read, input) = host.split();
    let (output_send, output) = mesh::channel();
    let task = driver.spawn(
        "microvm-portb-console",
        crate::log_task_with_output(log_file, read, "microvm-portb-console", output_send),
    );
    Ok(SerialData {
        emulated_serial_config: [None, None, None, None],
        serial_tasks: vec![task],
        linux_direct_serial_agent: None,
        microvm: Some(PortbSerial {
            io: io.unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource()),
            runtime: RuntimeResources { input, output },
        }),
    })
}

pub(super) fn attach_chipset_devices(
    machine_profile: MachineProfile,
    chipset_devices: &mut Vec<ChipsetDeviceHandle>,
    serial: Option<PortbSerial>,
) -> Option<RuntimeResources> {
    if !is_enabled(machine_profile) {
        assert!(serial.is_none());
        return None;
    }

    let (io, runtime) = match serial {
        Some(PortbSerial { io, runtime }) => (io, Some(runtime)),
        None => (DisconnectedSerialBackendHandle.into_resource(), None),
    };
    chipset_devices.extend([
        ChipsetDeviceHandle {
            name: MicrovmPortbHandle::ID.to_owned(),
            resource: MicrovmPortbHandle {
                io,
                generation_id: [0x5a; 16],
                restore_entropy: Vec::new(),
            }
            .into_resource(),
        },
        ChipsetDeviceHandle {
            name: MicrovmShutdownHandle::ID.to_owned(),
            resource: MicrovmShutdownHandle.into_resource(),
        },
        ChipsetDeviceHandle {
            name: MicrovmSnapshotRequestHandle::ID.to_owned(),
            resource: MicrovmSnapshotRequestHandle {
                notify: None,
                input_gate_timeout: std::time::Duration::from_secs(5),
            }
            .into_resource(),
        },
    ]);
    runtime
}

pub(super) fn configures_standard_serial(machine_profile: MachineProfile) -> bool {
    !is_enabled(machine_profile)
}

pub(super) fn has_vmgs(machine_profile: MachineProfile) -> bool {
    !is_enabled(machine_profile)
}

pub(super) fn firmware_event_send<T>(machine_profile: MachineProfile, sender: T) -> Option<T> {
    (!is_enabled(machine_profile)).then_some(sender)
}

pub(super) fn hypervisor_enabled(machine_profile: MachineProfile, default: bool) -> bool {
    default && !is_enabled(machine_profile)
}

pub(super) fn vmbus_disabled(machine_profile: MachineProfile) -> bool {
    is_enabled(machine_profile)
}

pub(super) fn validate_config(config: &Config) -> anyhow::Result<()> {
    openvmm_defs::microvm::validate_machine_config(config, None)
}

pub(crate) fn choose_hypervisor<T>(
    machine_profile: MachineProfile,
    standard: impl FnOnce() -> anyhow::Result<T>,
    microvm: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if is_enabled(machine_profile) {
        microvm()
    } else {
        standard()
    }
}

impl PetriVmInner {
    pub(super) async fn wait_for_microvm_portb_output(
        &mut self,
        marker: &str,
    ) -> anyhow::Result<()> {
        self.wait_for_microvm_portb_bytes(marker.as_bytes()).await
    }

    pub(super) async fn wait_for_microvm_portb_bytes(
        &mut self,
        marker: &[u8],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!marker.is_empty(), "microVM portb marker cannot be empty");
        let output = &mut self
            .resources
            .microvm
            .as_mut()
            .context("microVM portb output is not configured")?
            .output;
        let mut buffered = Vec::new();
        loop {
            let chunk = output.recv().await.map_err(|error| {
                anyhow::anyhow!(
                    "microVM portb output disconnected ({error}) after bytes {:?}",
                    String::from_utf8_lossy(&buffered[..buffered.len().min(256)])
                )
            })?;
            buffered.extend_from_slice(&chunk);
            if buffered
                .windows(marker.len())
                .any(|window| window == marker)
            {
                return Ok(());
            }
        }
    }

    pub(super) async fn write_microvm_portb_input(&mut self, input: &[u8]) -> anyhow::Result<()> {
        self.resources
            .microvm
            .as_mut()
            .context("microVM portb input is not configured")?
            .input
            .write_all(input)
            .await
            .context("writing microVM portb input")
    }

    pub(super) async fn pulse_save_restore(&self) -> anyhow::Result<()> {
        self.worker
            .pulse_save_restore()
            .await
            .map_err(anyhow::Error::from)
    }
}
