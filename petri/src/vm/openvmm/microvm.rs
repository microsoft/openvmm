// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM-specific microVM construction and runtime support.

use super::construct::SerialData;
use super::runtime::PetriVmInner;
use crate::Firmware;
use crate::PetriLogFile;
use anyhow::Context;
use chipset_resources::microvm::MicrovmPortbHandle;
use chipset_resources::microvm::MicrovmPortbTimeAbi;
use chipset_resources::microvm::MicrovmShutdownHandle;
use chipset_resources::microvm::MicrovmSnapshotRequestHandle;
use chipset_resources::pit::PitDeviceHandle;
use cpu_profile::HostCpuSignature;
use futures::AsyncWriteExt;
use openvmm_defs::config::Config;
use openvmm_defs::config::LinuxDirectBootMode;
use openvmm_defs::config::LinuxIsolationConfig;
use openvmm_defs::config::LoadMode;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::microvm::MicrovmConfig;
use openvmm_defs::time_abi::TimeAbiParameters;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use pal_async::socket::WriteHalf;
use pal_async::task::Spawn;
use petri_artifacts_common::tags::MachineArch;
use serial_core::resources::DisconnectedSerialBackendHandle;
use std::path::Path;
use std::sync::OnceLock;
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

/// Returns the microVM configuration of a VM: every microVM uses the NVX time
/// ABI, with the CPU profile of this process's microVM cold boots
/// ([`cold_boot_cpu_profile`]) for `firmware` and the OpenVMM binary
/// `openvmm`.
pub(super) async fn microvm_config(
    machine_profile: MachineProfile,
    openvmm: &Path,
    firmware: &Firmware,
) -> MicrovmConfig {
    let time_abi = if is_enabled(machine_profile) {
        Some(TimeAbiParameters {
            cpu_profile: cold_boot_cpu_profile(openvmm, firmware).await.to_owned(),
            generation: 0,
            hooks: Default::default(),
        })
    } else {
        None
    };
    MicrovmConfig {
        time_abi,
        ..Default::default()
    }
}

/// The CPU profile of this process's microVM cold boots, which the first one
/// decides ([`cold_boot_cpu_profile`]).
static COLD_BOOT_CPU_PROFILE: OnceLock<&'static str> = OnceLock::new();

/// Returns the CPU profile of this process's microVM cold boots: `auto`,
/// unless OpenVMM's time ABI verification of an `auto` cold boot of
/// `firmware` with the OpenVMM binary `openvmm` fails as
/// [`auto_cpu_profile_needs_host_fallback`] describes, in which case `host`,
/// with a warning that names the failure. Any other outcome keeps `auto`, so
/// that the cold boot itself fails as it would. The first microVM cold boot
/// of the process decides.
async fn cold_boot_cpu_profile(openvmm: &Path, firmware: &Firmware) -> &'static str {
    if let Some(profile) = COLD_BOOT_CPU_PROFILE.get() {
        return profile;
    }
    let host = host_cpu();
    let profile = if !host_profile_may_replace_auto(host.as_ref()) {
        cpu_profile::AUTO
    } else {
        match verify_auto_cold_boot(openvmm, firmware).await {
            Ok(Verification::Passed) => {
                tracing::info!(
                    "the time ABI verification of a microVM cold boot with --cpu-profile auto \
                     passed; keeping auto"
                );
                cpu_profile::AUTO
            }
            Ok(Verification::Failed { code, detail })
                if falls_back_to_host_profile(code.as_deref(), host.as_ref()) =>
            {
                tracing::warn!(
                    code = ?code,
                    detail = %detail,
                    "a microVM cold boot with --cpu-profile auto fails on this host because \
                     the backend cannot present the built-in CPU profile that auto selects; \
                     using --cpu-profile host for the microVM cold boots of this test"
                );
                cpu_profile::HOST
            }
            Ok(Verification::Failed { code, detail }) => {
                tracing::info!(
                    code = ?code,
                    detail = %detail,
                    "the time ABI verification of a microVM cold boot with --cpu-profile auto \
                     failed; keeping auto"
                );
                cpu_profile::AUTO
            }
            Err(err) => {
                tracing::warn!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "failed to verify a microVM cold boot with --cpu-profile auto; keeping auto"
                );
                cpu_profile::AUTO
            }
        }
    };
    COLD_BOOT_CPU_PROFILE.get_or_init(|| profile)
}

/// The prefix of the line that OpenVMM's time ABI verification
/// (`--x-time-abi-verify`) prints.
const VERIFY_PREFIX: &str = "NVX-TIME-ABI-VERIFY:";

/// The outcome of OpenVMM's time ABI verification of a microVM cold boot.
#[derive(Debug, PartialEq, Eq)]
enum Verification {
    /// The cold boot's time ABI checks passed.
    Passed,
    /// The cold boot failed with the time ABI code `code`, if any, and the
    /// error chain `detail`.
    Failed {
        code: Option<String>,
        detail: String,
    },
}

/// Runs OpenVMM's time ABI verification of an `auto` cold boot of the Linux
/// direct `firmware` with the OpenVMM binary `openvmm`, which builds the
/// microVM partition, as a cold boot does, without running the guest.
async fn verify_auto_cold_boot(
    openvmm: &Path,
    firmware: &Firmware,
) -> anyhow::Result<Verification> {
    let Firmware::LinuxDirect { kernel, initrd } = firmware else {
        anyhow::bail!("microVM requires an uncompressed Linux direct artifact");
    };
    let mut command = std::process::Command::new(openvmm);
    command
        .args(["--machine", "microvm", "--x-time-abi-verify", "--kernel"])
        .arg(kernel.get());
    if let Some(initrd) = initrd {
        command.arg("--initrd").arg(initrd.get());
    }
    command.stdin(std::process::Stdio::null());
    let output = blocking::unblock(move || command.output())
        .await
        .context("failed to run the time ABI verification")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_verification(&stdout).with_context(|| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim_end();
        let tail = stderr
            .char_indices()
            .rev()
            .nth(1023)
            .map_or(stderr, |(start, _)| &stderr[start..]);
        format!(
            "the time ABI verification exited with {} without a verification line; \
             stderr ends with: {tail}",
            output.status
        )
    })
}

/// Parses the verification line of `stdout`, the output of OpenVMM's time
/// ABI verification.
fn parse_verification(stdout: &str) -> Option<Verification> {
    let line = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(VERIFY_PREFIX))?;
    // The detail, an escaped error chain, is the line's last field.
    let (fields, detail) = match line.split_once(" detail=\"") {
        Some((fields, detail)) => (fields, Some(detail)),
        None => (line, None),
    };
    let field = |name: &str| {
        fields
            .split_whitespace()
            .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))
    };
    match field("status")? {
        "ok" => Some(Verification::Passed),
        "fail" => Some(Verification::Failed {
            code: field("code")
                .filter(|code| *code != "none")
                .map(str::to_owned),
            detail: detail
                .map(|detail| {
                    detail
                        .strip_suffix('"')
                        .unwrap_or(detail)
                        .replace("\\\"", "\"")
                        .replace("\\\\", "\\")
                })
                .unwrap_or_default(),
        }),
        _ => None,
    }
}

/// Returns whether a microVM cold boot that failed with the error chain
/// `error` under `--cpu-profile auto` should be retried with
/// `--cpu-profile host`.
///
/// That is so only where OpenVMM's own error suggests `--cpu-profile host`:
/// `auto` selected the built-in CPU profile of the host CPU's generation, the
/// backend cannot present that profile (`E_PROFILE_UNSUPPORTED`, the leading
/// time ABI code of the chain), and host profiles serve the CPU. Any other
/// failure, including that of a host profile that `auto` fell back to,
/// should fail as it is.
pub fn auto_cpu_profile_needs_host_fallback(error: &str) -> bool {
    falls_back_to_host_profile(leading_time_abi_code(error), host_cpu().as_ref())
}

/// Returns whether a microVM cold boot whose `--cpu-profile auto` failed with
/// the time ABI code `code` on the host CPU `host` falls back to
/// `--cpu-profile host`, as [`auto_cpu_profile_needs_host_fallback`]
/// describes.
fn falls_back_to_host_profile(code: Option<&str>, host: Option<&HostCpuSignature>) -> bool {
    code == Some(cpu_profile::ProfileErrorCode::ProfileUnsupported.as_str())
        && host_profile_may_replace_auto(host)
}

/// Returns whether `--cpu-profile auto` selects a built-in CPU profile for the
/// host CPU `host` that a host profile could replace: a built-in profile
/// serves the CPU, and so do host profiles. Elsewhere, `auto` itself uses a
/// host profile or fails.
fn host_profile_may_replace_auto(host: Option<&HostCpuSignature>) -> bool {
    host.is_some_and(|host| {
        cpu_profile::pinned_profile_serves(host) && cpu_profile::supports_host_profiles(host)
    })
}

/// Returns the leading time ABI code of the OpenVMM error chain `error`: its
/// first bracketed code, such as `E_PROFILE_UNSUPPORTED`, which OpenVMM
/// keeps across process boundaries.
fn leading_time_abi_code(error: &str) -> Option<&str> {
    error.match_indices("[E_").find_map(|(start, _)| {
        let code = &error[start + 1..];
        let code = &code[..code.find(']')?];
        code.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            .then_some(code)
    })
}

/// Returns the signature of this host's CPU, which OpenVMM's CPU profiles
/// identify on x86-64 hosts only.
fn host_cpu() -> Option<HostCpuSignature> {
    // The host CPU is identified with the CPUID instruction of the host.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    let host = Some(HostCpuSignature::current());
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(not(target_arch = "x86_64"))]
    let host = None;
    host
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
    // The time ABI puts the PIT in strict mode.
    for device in chipset_devices.iter_mut() {
        if device.name == PitDeviceHandle::ID {
            device.resource = PitDeviceHandle { time_abi: true }.into_resource();
        }
    }
    chipset_devices.extend([
        ChipsetDeviceHandle {
            name: MicrovmPortbHandle::ID.to_owned(),
            resource: MicrovmPortbHandle {
                io,
                generation_id: [0x5a; 16],
                output_drain: None,
                time_abi: MicrovmPortbTimeAbi {
                    generation: 0,
                    utc_offset_ms: 0,
                    sample_delay_us: 0,
                    test_hooks: false,
                    restore: None,
                },
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

#[cfg(test)]
mod tests {
    use super::*;

    const GENOA: HostCpuSignature = HostCpuSignature::new(*b"AuthenticAMD", 0x00a1_0f11);
    const SKYLAKE_SP: HostCpuSignature = HostCpuSignature::new(*b"GenuineIntel", 0x0005_0654);
    /// A CPU that no built-in profile serves, so `auto` falls back to a host
    /// profile.
    const GRANITE_RAPIDS: HostCpuSignature = HostCpuSignature::new(*b"GenuineIntel", 0x000a_06d1);
    /// A CPU of a vendor that host profiles do not serve.
    const HYGON: HostCpuSignature = HostCpuSignature::new(*b"HygonGenuine", 0x0090_0f01);

    /// The error of a cold boot whose `--cpu-profile auto` selected a
    /// built-in profile that the backend cannot present, as OpenVMM reports
    /// it on an AMD EPYC 9V74 host with WHP.
    const AUTO_UNSUPPORTED: &str = "failed to launch vm worker: failed to launch worker: \
        this host cannot boot the built-in CPU profile that --cpu-profile auto selected; \
        --cpu-profile host boots on a profile derived from this host, for development: \
        failed to create the prototype partition: [E_PROFILE_UNSUPPORTED] the hypervisor's \
        processor features cannot present CPU profile amd.genoa.v1: they lack \
        tsa_l1_no_support, tsa_sq_no_support";

    #[test]
    fn host_fallback_needs_an_unsupported_built_in_profile() {
        let unsupported = Some("E_PROFILE_UNSUPPORTED");
        assert!(falls_back_to_host_profile(unsupported, Some(&GENOA)));
        assert!(falls_back_to_host_profile(unsupported, Some(&SKYLAKE_SP)));
        // `auto` already used a host profile, or none could serve the CPU.
        assert!(!falls_back_to_host_profile(
            unsupported,
            Some(&GRANITE_RAPIDS)
        ));
        assert!(!falls_back_to_host_profile(unsupported, Some(&HYGON)));
        // The host CPU is not identified.
        assert!(!falls_back_to_host_profile(unsupported, None));
        for code in [
            None,
            Some("E_CPU_GENERATION"),
            Some("E_CPU_SURFACE"),
            Some("E_PROFILE_HOST_UNKNOWN"),
            Some("E_TSC_SYNC_UNSUPPORTED"),
        ] {
            assert!(!falls_back_to_host_profile(code, Some(&GENOA)), "{code:?}");
        }
    }

    #[test]
    fn leading_time_abi_code_is_the_first_bracketed_code() {
        assert_eq!(
            leading_time_abi_code(AUTO_UNSUPPORTED),
            Some("E_PROFILE_UNSUPPORTED")
        );
        assert_eq!(
            leading_time_abi_code("[E_CPU_GENERATION] first: [E_PROFILE_UNSUPPORTED] second"),
            Some("E_CPU_GENERATION")
        );
        assert_eq!(
            leading_time_abi_code("[E_not a code] then [E_PROFILE_UNSUPPORTED] x"),
            Some("E_PROFILE_UNSUPPORTED")
        );
        assert_eq!(leading_time_abi_code("[E_PROFILE_UNSUPPORTED"), None);
        assert_eq!(leading_time_abi_code("failed to launch vm worker"), None);
        // A failure without the code never falls back, whatever the host.
        assert!(!auto_cpu_profile_needs_host_fallback(
            "failed to launch worker: the hypervisor is unavailable"
        ));
        assert!(!auto_cpu_profile_needs_host_fallback(
            "failed to launch worker: [E_CPU_GENERATION] the host CPU is not in generation genoa"
        ));
    }

    #[test]
    fn verification_line_reports_the_code_and_the_detail() {
        assert_eq!(
            parse_verification(
                "booting\nNVX-TIME-ABI-VERIFY: v=1 status=ok backend=whp \
                 cpu_profile=intel.alderlake.v1 tsc_hz=2918400675\n"
            ),
            Some(Verification::Passed)
        );
        assert_eq!(
            parse_verification(
                "NVX-TIME-ABI-VERIFY: v=1 status=fail backend=whp code=E_PROFILE_UNSUPPORTED \
                 detail=\"failed: [E_PROFILE_UNSUPPORTED] a \\\"quoted\\\" status=ok\""
            ),
            Some(Verification::Failed {
                code: Some("E_PROFILE_UNSUPPORTED".to_owned()),
                detail: "failed: [E_PROFILE_UNSUPPORTED] a \"quoted\" status=ok".to_owned(),
            })
        );
        assert_eq!(
            parse_verification(
                "NVX-TIME-ABI-VERIFY: v=1 status=fail backend=kvm code=none detail=\"failed\""
            ),
            Some(Verification::Failed {
                code: None,
                detail: "failed".to_owned(),
            })
        );
        assert_eq!(parse_verification("fatal error: no hypervisor"), None);
        assert_eq!(
            parse_verification("NVX-TIME-ABI-VERIFY: v=1 status=unknown"),
            None
        );
    }
}
