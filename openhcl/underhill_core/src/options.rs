// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CLI argument parsing for the underhill core process.

#![warn(missing_docs)]

use anyhow::Context;
use anyhow::bail;
use inspect::Inspect;
use mesh::MeshPayload;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Clone, Debug, MeshPayload)]
pub enum TestScenarioConfig {
    SaveFail,
    RestoreStuck,
    SaveStuck,
}

impl FromStr for TestScenarioConfig {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<TestScenarioConfig, anyhow::Error> {
        match s {
            "SERVICING_SAVE_FAIL" => Ok(TestScenarioConfig::SaveFail),
            "SERVICING_RESTORE_STUCK" => Ok(TestScenarioConfig::RestoreStuck),
            "SERVICING_SAVE_STUCK" => Ok(TestScenarioConfig::SaveStuck),
            _ => Err(anyhow::anyhow!("Invalid test config: {}", s)),
        }
    }
}

#[derive(Clone, Debug, MeshPayload)]
pub enum GuestStateLifetimeCli {
    Default,
    ReprovisionOnFailure,
    Reprovision,
    Ephemeral,
}

impl FromStr for GuestStateLifetimeCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<GuestStateLifetimeCli, anyhow::Error> {
        match s {
            "DEFAULT" | "0" => Ok(GuestStateLifetimeCli::Default),
            "REPROVISION_ON_FAILURE" | "1" => Ok(GuestStateLifetimeCli::ReprovisionOnFailure),
            "REPROVISION" | "2" => Ok(GuestStateLifetimeCli::Reprovision),
            "EPHEMERAL" | "3" => Ok(GuestStateLifetimeCli::Ephemeral),
            _ => Err(anyhow::anyhow!("Invalid lifetime: {}", s)),
        }
    }
}

#[derive(Clone, Debug, MeshPayload)]
pub enum GuestStateEncryptionPolicyCli {
    Auto,
    None,
    GspById,
    GspKey,
}

impl FromStr for GuestStateEncryptionPolicyCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<GuestStateEncryptionPolicyCli, anyhow::Error> {
        match s {
            "AUTO" | "0" => Ok(GuestStateEncryptionPolicyCli::Auto),
            "NONE" | "1" => Ok(GuestStateEncryptionPolicyCli::None),
            "GSP_BY_ID" | "2" => Ok(GuestStateEncryptionPolicyCli::GspById),
            "GSP_KEY" | "3" => Ok(GuestStateEncryptionPolicyCli::GspKey),
            _ => Err(anyhow::anyhow!("Invalid encryption policy: {}", s)),
        }
    }
}

#[derive(Clone, Debug, MeshPayload, Inspect)]
pub enum KeepAliveConfig {
    EnabledHostAndPrivatePoolPresent,
    DisabledHostAndPrivatePoolPresent,
    Disabled,
}

impl FromStr for KeepAliveConfig {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<KeepAliveConfig, anyhow::Error> {
        match s.to_lowercase().as_str() {
            "host,privatepool" => Ok(KeepAliveConfig::EnabledHostAndPrivatePoolPresent),
            "nohost,privatepool" => Ok(KeepAliveConfig::DisabledHostAndPrivatePoolPresent),
            "nohost,noprivatepool" => Ok(KeepAliveConfig::Disabled),
            x if x.starts_with("disabled,") => Ok(KeepAliveConfig::Disabled),
            _ => Err(anyhow::anyhow!("Invalid keepalive config: {}", s)),
        }
    }
}

impl KeepAliveConfig {
    pub fn is_enabled(&self) -> bool {
        matches!(self, KeepAliveConfig::EnabledHostAndPrivatePoolPresent)
    }
}

// We've made our own parser here instead of using something like clap in order
// to save on compiled file size. We don't need all the features a crate can provide.
/// underhill core command-line and environment variable options.
pub struct Options {
    /// (OPENHCL_WAIT_FOR_START=1 | --wait-for-start)
    ///  wait for a diagnostics start request before initializing and starting the VM
    pub wait_for_start: bool,

    /// (OPENHCL_SIGNAL_VTL0_STARTED=1)
    /// immediately signal that VTL0 has started, before doing any
    /// initialization. This allows VM boot to proceed even if initialization
    /// may hang (e.g., because you specified OPENHCL_WAIT_FOR_START=1).
    pub signal_vtl0_started: bool,

    /// (OPENHCL_REFORMAT_VMGS=1 | --reformat-vmgs)
    /// reformat the VMGS file on boot. useful for running potentially destructive VMGS tests.
    pub reformat_vmgs: bool,

    /// (OPENHCL_PID_FILE_PATH=/path/to/file | --pid /path/to/file)
    /// write the PID to the specified path
    pub pid: Option<PathBuf>,

    /// (OPENHCL_VMBUS_MAX_VERSION=\<number\>)
    /// limit the maximum protocol version allowed by vmbus; used for testing purposes
    pub vmbus_max_version: Option<u32>,

    /// (OPENHCL_VMBUS_ENABLE_MNF=1)
    /// Enable handling of MNF in the Underhill vmbus server, instead of the host.
    pub vmbus_enable_mnf: Option<bool>,

    /// (OPENHCL_VMBUS_FORCE_CONFIDENTIAL_EXTERNAL_MEMORY=1)
    /// Force the use of confidential external memory for all non-relay vmbus channels. For testing
    /// purposes only.
    ///
    /// N.B.: Not all vmbus devices support this feature, so enabling it may cause failures.
    pub vmbus_force_confidential_external_memory: bool,

    /// (OPENHCL_VMBUS_CHANNEL_UNSTICK_DELAY_MS=\<number\>) (default: 100)
    /// Delay before unsticking a vmbus channel after it has been opened, in milliseconds. Set to
    /// zero to disable unsticking.
    pub vmbus_channel_unstick_delay_ms: u64,

    /// (OPENHCL_CMDLINE_APPEND=\<string\>)
    /// Command line to append to VTL0, only used with direct boot.
    pub cmdline_append: Option<String>,

    /// (OPENHCL_VNC_PORT=\<number\> | --vnc-port \<number\>) (default: 3)
    /// VNC (vsock) port number
    pub vnc_port: u32,

    /// (OPENHCL_GDBSTUB=1)
    /// Enables the GDB stub for debugging the guest.
    pub gdbstub: bool,

    /// (OPENHCL_GDBSTUB_PORT=\<number\>) (default: 4)
    /// GDB stub (vsock) port number.
    pub gdbstub_port: u32,

    /// (OPENHCL_VTL0_STARTS_PAUSED=1)
    /// Start with VTL0 paused
    pub vtl0_starts_paused: bool,

    /// (OPENHCL_FRAMEBUFFER_GPA_BASE=\<number\>)
    /// Base GPA of the fixed framebuffer mapping for underhill to read.
    /// If a value is provided, a graphics device is exposed.
    // TODO: send this value as an IGVM device tree parameter instead
    pub framebuffer_gpa_base: Option<u64>,

    /// (OPENHCL_SERIAL_WAIT_FOR_RTS=\<bool\>)
    /// Whether the emulated 16550 waits for guest DTR+RTS before pulling data
    /// from the host.
    pub serial_wait_for_rts: bool,

    /// (OPENHCL_FORCE_LOAD_VTL0_IMAGE=\<string\>)
    /// Force load the specified image in VTL0. The image must support the
    /// option specified.
    ///
    /// Valid options are "pcat, uefi, linux".
    pub force_load_vtl0_image: Option<String>,

    /// (OPENHCL_NVME_VFIO=1)
    /// Use the user-mode VFIO NVMe driver instead of the Linux driver.
    pub nvme_vfio: bool,

    /// (OPENHCL_MCR_DEVICE=1)
    /// MCR Device Enable
    pub mcr: bool, // TODO MCR: support closed-source ENV vars

    /// (OPENHCL_HIDE_ISOLATION=1)
    /// Hide the isolation mode from the guest.
    pub hide_isolation: bool,

    /// (OPENHCL_HALT_ON_GUEST_HALT=1) When receiving a halt request from a
    /// lower VTL, halt underhill instead of forwarding the halt request to the
    /// host. This allows for debugging state without the partition state
    /// changing from the host.
    pub halt_on_guest_halt: bool,

    /// (OPENHCL_NO_SIDECAR_HOTPLUG=1) Leave sidecar VPs remote even if they
    /// hit exits.
    pub no_sidecar_hotplug: bool,

    /// (OPENHCL_NVME_KEEP_ALIVE=\<KeepaliveConfig\>)
    /// Configure NVMe keep alive behavior when servicing.
    /// Options are:
    ///  - "host,privatepool" - Enable keep alive if both host and private pool support it.
    ///  - "nohost,privatepool" - Used when the host does not support keepalive, but a private pool is present. Keepalive is disabled.
    ///  - "nohost,noprivatepool" - Keepalive is disabled.
    ///  - "disabled, X, X" - Keepalive is disabled due to manual
    ///    override. Host and private pool options are ignored.
    pub nvme_keep_alive: KeepAliveConfig,

    /// (OPENHCL_MANA_KEEP_ALIVE=\<KeepAliveConfig\>)
    /// Configure MANA keep alive behavior when servicing.
    /// Options are:
    ///  - "host,privatepool" - Enable keep alive if both host and private pool support it.
    ///  - "nohost,privatepool" - Used when the host does not support keepalive, but a private pool is present. Keepalive is disabled.
    ///  - "nohost,noprivatepool" - Keepalive is disabled.
    ///  - "disabled, X, X" - TODO: This needs to be implemented for mana.
    pub mana_keep_alive: KeepAliveConfig,

    /// (OPENHCL_NVME_ALWAYS_FLR=1)
    /// Always use the FLR (Function Level Reset) path for NVMe devices,
    /// even if we would otherwise attempt to use VFIO's NoReset support.
    pub nvme_always_flr: bool,

    /// (OPENHCL_TEST_CONFIG=\<TestScenarioConfig\>)
    /// Test configurations are designed to replicate specific behaviors and
    /// conditions in order to simulate various test scenarios.
    pub test_configuration: Option<TestScenarioConfig>,

    /// (OPENHCL_DISABLE_UEFI_FRONTPAGE=1) Disable the frontpage in UEFI which
    /// will result in UEFI terminating, shutting down the guest instead of
    /// showing the frontpage.
    pub disable_uefi_frontpage: Option<bool>,

    /// (HCL_DEFAULT_BOOT_ALWAYS_ATTEMPT=1) Instruct UEFI to always attempt a
    /// default boot, even if existing boot entries fail.
    pub default_boot_always_attempt: Option<bool>,

    /// (HCL_GUEST_STATE_LIFETIME=\<GuestStateLifetimeCli\>)
    /// Specify which guest state lifetime to use.
    pub guest_state_lifetime: Option<GuestStateLifetimeCli>,

    /// (HCL_GUEST_STATE_ENCRYPTION_POLICY=\<GuestStateEncryptionPolicyCli\>)
    /// Specify which guest state encryption policy to use.
    pub guest_state_encryption_policy: Option<GuestStateEncryptionPolicyCli>,

    /// (HCL_STRICT_ENCRYPTION_POLICY=1) Strict guest state encryption policy.
    pub strict_encryption_policy: Option<bool>,

    /// (HCL_ATTEMPT_AK_CERT_CALLBACK=1) Attempt to renew the AK cert.
    /// If not specified, use the configuration in DPSv2 ManagementVtlFeatures.
    pub attempt_ak_cert_callback: Option<bool>,

    /// (OPENHCL_ENABLE_VPCI_RELAY=1) Enable the VPCI relay.
    pub enable_vpci_relay: Option<bool>,

    /// (OPENHCL_DISABLE_PROXY_REDIRECT=1) Disable proxy interrupt redirection.
    pub disable_proxy_redirect: bool,

    /// (OPENHCL_DISABLE_LOWER_VTL_TIMER_VIRT=1) Disable lower VTL timer virtualization.
    pub disable_lower_vtl_timer_virt: bool,

    /// (OPENHCL_CONFIG_TIMEOUT_IN_SECONDS=\<number\>) (default: 5)
    /// Timeout in seconds for VM configuration operations, both initial
    /// configuration and subsequent modifications.
    pub config_timeout_in_seconds: u64,

    /// (OPENHCL_SERVICING_TIMEOUT_DUMP_COLLECTION_IN_MS=\<number\>) (default: 500)
    /// The default time to wait in milliseconds for dump collection during a
    /// panic in servicing.
    pub servicing_timeout_dump_collection_in_ms: u64,
}

impl Options {
    pub(crate) fn parse(
        extra_args: Vec<String>,
        extra_env: Vec<(String, Option<String>)>,
    ) -> anyhow::Result<Self> {
        // Pull the entire environment into a BTreeMap for manipulation through extra_env.
        let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        for (key, value) in extra_env {
            match value {
                Some(value) => env.insert(key.into(), value.into()),
                None => env.remove::<OsStr>(key.as_ref()),
            };
        }

        // Reads an environment variable, falling back to a legacy variable (replacing
        // "OPENHCL_" with "UNDERHILL_") if the original is not set.
        let read_legacy_openhcl_env = |name: &str| -> Option<&OsString> {
            env.get::<OsStr>(name.as_ref()).or_else(|| {
                env.get::<OsStr>(
                    format!(
                        "UNDERHILL_{}",
                        name.strip_prefix("OPENHCL_").unwrap_or(name)
                    )
                    .as_ref(),
                )
            })
        };

        // Reads an environment variable strings.
        let read_env = |name: &str| -> Option<&OsString> { env.get::<OsStr>(name.as_ref()) };

        fn parse_bool_opt(value: Option<&OsString>) -> anyhow::Result<Option<bool>> {
            value
                .map(|v| {
                    if v.eq_ignore_ascii_case("true") || v == "1" {
                        Ok(true)
                    } else if v.eq_ignore_ascii_case("false") || v == "0" {
                        Ok(false)
                    } else {
                        Err(anyhow::anyhow!(
                            "invalid boolean environment variable: {}",
                            v.to_string_lossy()
                        ))
                    }
                })
                .transpose()
        }

        fn parse_bool(value: Option<&OsString>) -> bool {
            parse_bool_opt(value).ok().flatten().unwrap_or_default()
        }

        let parse_legacy_env_bool = |name| parse_bool(read_legacy_openhcl_env(name));
        let parse_env_bool = |name: &str| parse_bool(read_env(name));
        let parse_env_bool_opt = |name: &str| {
            parse_bool_opt(read_env(name))
                .map_err(|e| tracing::warn!("failed to parse {name}: {e:#}"))
                .ok()
                .flatten()
        };

        fn parse_number(value: Option<&OsString>) -> anyhow::Result<Option<u64>> {
            value
                .map(|v| {
                    let v = v.to_string_lossy();
                    v.parse()
                        .context(format!("invalid numeric environment variable: {v}"))
                })
                .transpose()
        }

        let parse_legacy_env_number = |name| {
            parse_number(read_legacy_openhcl_env(name))
                .context(format!("parsing legacy env number: {name}"))
        };
        let parse_env_number = |name: &str| {
            parse_number(read_env(name)).context(format!("parsing env number: {name}"))
        };

        let mut wait_for_start = parse_legacy_env_bool("OPENHCL_WAIT_FOR_START");
        let mut reformat_vmgs = parse_legacy_env_bool("OPENHCL_REFORMAT_VMGS");
        let mut pid = read_legacy_openhcl_env("OPENHCL_PID_FILE_PATH")
            .map(|x| x.to_string_lossy().into_owned().into());
        let vmbus_max_version = read_legacy_openhcl_env("OPENHCL_VMBUS_MAX_VERSION")
            .map(|x| {
                vmbus_core::parse_vmbus_version(&(x.to_string_lossy()))
                    .map_err(|x| anyhow::anyhow!("Error parsing vmbus max version: {}", x))
            })
            .transpose()?;
        let vmbus_enable_mnf =
            read_legacy_openhcl_env("OPENHCL_VMBUS_ENABLE_MNF").map(|v| parse_bool(Some(v)));
        let vmbus_force_confidential_external_memory =
            parse_env_bool("OPENHCL_VMBUS_FORCE_CONFIDENTIAL_EXTERNAL_MEMORY");
        let vmbus_channel_unstick_delay_ms =
            parse_legacy_env_number("OPENHCL_VMBUS_CHANNEL_UNSTICK_DELAY_MS")?;
        let cmdline_append = read_legacy_openhcl_env("OPENHCL_CMDLINE_APPEND")
            .map(|x| x.to_string_lossy().into_owned());
        let force_load_vtl0_image = read_legacy_openhcl_env("OPENHCL_FORCE_LOAD_VTL0_IMAGE")
            .map(|x| x.to_string_lossy().into_owned());
        let mut vnc_port = parse_legacy_env_number("OPENHCL_VNC_PORT")?.map(|x| x as u32);
        let framebuffer_gpa_base = parse_legacy_env_number("OPENHCL_FRAMEBUFFER_GPA_BASE")?;
        let vtl0_starts_paused = parse_legacy_env_bool("OPENHCL_VTL0_STARTS_PAUSED");
        let serial_wait_for_rts = parse_legacy_env_bool("OPENHCL_SERIAL_WAIT_FOR_RTS");
        let nvme_vfio = parse_legacy_env_bool("OPENHCL_NVME_VFIO");
        let mcr = parse_legacy_env_bool("OPENHCL_MCR_DEVICE");
        let hide_isolation = parse_env_bool("OPENHCL_HIDE_ISOLATION");
        let halt_on_guest_halt = parse_legacy_env_bool("OPENHCL_HALT_ON_GUEST_HALT");
        let no_sidecar_hotplug = parse_legacy_env_bool("OPENHCL_NO_SIDECAR_HOTPLUG");
        let gdbstub = parse_legacy_env_bool("OPENHCL_GDBSTUB");
        let gdbstub_port = parse_legacy_env_number("OPENHCL_GDBSTUB_PORT")?.map(|x| x as u32);
        let nvme_keep_alive = read_env("OPENHCL_NVME_KEEP_ALIVE")
                    .map(|x| {
                        let s = x.to_string_lossy();
                        match s.parse::<KeepAliveConfig>() {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    "failed to parse OPENHCL_NVME_KEEP_ALIVE ('{s}'): {e}. Nvme keepalive will be disabled."
                                );
                                KeepAliveConfig::Disabled
                            }
                        }
                    })
                    .unwrap_or(KeepAliveConfig::Disabled);
        let mana_keep_alive = read_env("OPENHCL_MANA_KEEP_ALIVE")
                    .map(|x| {
                        let s = x.to_string_lossy();
                        match s.parse::<KeepAliveConfig>() {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    "failed to parse OPENHCL_MANA_KEEP_ALIVE ('{s}'): {e}. Mana keepalive will be disabled."
                                );
                                KeepAliveConfig::Disabled
                            }
                        }
                    })
                    .unwrap_or(KeepAliveConfig::Disabled);
        let nvme_always_flr = parse_env_bool("OPENHCL_NVME_ALWAYS_FLR");
        let test_configuration = read_env("OPENHCL_TEST_CONFIG").and_then(|x| {
            x.to_string_lossy()
                .parse::<TestScenarioConfig>()
                .map_err(|e| {
                    tracing::warn!(
                        "failed to parse OPENHCL_TEST_CONFIG: {}. No test will be simulated.",
                        e
                    )
                })
                .ok()
        });
        let disable_uefi_frontpage = parse_env_bool_opt("OPENHCL_DISABLE_UEFI_FRONTPAGE");
        let signal_vtl0_started = parse_env_bool("OPENHCL_SIGNAL_VTL0_STARTED");
        let default_boot_always_attempt = parse_env_bool_opt("HCL_DEFAULT_BOOT_ALWAYS_ATTEMPT");
        let guest_state_lifetime = read_env("HCL_GUEST_STATE_LIFETIME").and_then(|x| {
            x.to_string_lossy()
                .parse::<GuestStateLifetimeCli>()
                .map_err(|e| tracing::warn!("failed to parse HCL_GUEST_STATE_LIFETIME: {:#}", e))
                .ok()
        });
        let guest_state_encryption_policy =
            read_env("HCL_GUEST_STATE_ENCRYPTION_POLICY").and_then(|x| {
                x.to_string_lossy()
                    .parse::<GuestStateEncryptionPolicyCli>()
                    .map_err(|e| {
                        tracing::warn!("failed to parse HCL_GUEST_STATE_ENCRYPTION_POLICY: {:#}", e)
                    })
                    .ok()
            });
        let strict_encryption_policy = parse_env_bool_opt("HCL_STRICT_ENCRYPTION_POLICY");
        let attempt_ak_cert_callback = parse_env_bool_opt("HCL_ATTEMPT_AK_CERT_CALLBACK");
        let enable_vpci_relay = parse_env_bool_opt("OPENHCL_ENABLE_VPCI_RELAY");
        let disable_proxy_redirect = parse_env_bool("OPENHCL_DISABLE_PROXY_REDIRECT");
        let disable_lower_vtl_timer_virt = parse_env_bool("OPENHCL_DISABLE_LOWER_VTL_TIMER_VIRT");
        let config_timeout_in_seconds =
            parse_legacy_env_number("OPENHCL_CONFIG_TIMEOUT_IN_SECONDS")?.unwrap_or(5);
        let servicing_timeout_dump_collection_in_ms =
            parse_env_number("OPENHCL_SERVICING_TIMEOUT_DUMP_COLLECTION_IN_MS")?.unwrap_or(500);

        let mut args = std::env::args().chain(extra_args);
        // Skip our own filename.
        args.next();

        while let Some(next) = args.next() {
            let arg = next;

            match &*arg {
                "--wait-for-start" => wait_for_start = true,
                "--reformat-vmgs" => reformat_vmgs = true,

                x if x.starts_with("--") && x.len() > 2 => {
                    if let Some(eq) = arg.find('=') {
                        let (name, value) = arg.split_at(eq);
                        // Don't forget to exclude the '=' itself.
                        let value = &value[1..];
                        Self::parse_value_arg(name, value, &mut pid, &mut vnc_port)?;
                    } else {
                        if let Some(value) = args.next() {
                            Self::parse_value_arg(&arg, &value, &mut pid, &mut vnc_port)?;
                        } else {
                            bail!("Expected a value after argument {}", arg);
                        }
                    }
                }
                x => bail!("Unrecognized argument {}", x),
            }
        }

        Ok(Self {
            wait_for_start,
            signal_vtl0_started,
            reformat_vmgs,
            pid,
            vmbus_max_version,
            vmbus_enable_mnf,
            vmbus_force_confidential_external_memory,
            vmbus_channel_unstick_delay_ms: vmbus_channel_unstick_delay_ms.unwrap_or(100),
            cmdline_append,
            vnc_port: vnc_port.unwrap_or(3),
            framebuffer_gpa_base,
            gdbstub,
            gdbstub_port: gdbstub_port.unwrap_or(4),
            vtl0_starts_paused,
            serial_wait_for_rts,
            force_load_vtl0_image,
            nvme_vfio,
            mcr,
            hide_isolation,
            halt_on_guest_halt,
            no_sidecar_hotplug,
            nvme_keep_alive,
            mana_keep_alive,
            nvme_always_flr,
            test_configuration,
            disable_uefi_frontpage,
            default_boot_always_attempt,
            guest_state_lifetime,
            guest_state_encryption_policy,
            strict_encryption_policy,
            attempt_ak_cert_callback,
            enable_vpci_relay,
            disable_proxy_redirect,
            disable_lower_vtl_timer_virt,
            config_timeout_in_seconds,
            servicing_timeout_dump_collection_in_ms,
        })
    }

    fn parse_value_arg(
        name: &str,
        value: &str,
        pid: &mut Option<PathBuf>,
        vnc_port: &mut Option<u32>,
    ) -> anyhow::Result<()> {
        match name {
            "--pid" => *pid = Some(value.into()),
            "--vnc-port" => {
                *vnc_port = Some(
                    value
                        .parse()
                        .context(format!("Error parsing VNC port {}", value))?,
                )
            }
            x => bail!("Unrecognized argument {}", x),
        }

        Ok(())
    }
}
