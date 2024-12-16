// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CLI argument parsing for the underhill core process.

#![warn(missing_docs)]

use anyhow::bail;
use anyhow::Context;
use std::path::PathBuf;

// We've made our own parser here instead of using something like clap in order
// to save on compiled file size. We don't need all the features a crate can provide.
/// underhill core command-line and environment variable options.
pub struct Options {
    /// (OPENHCL_WAIT_FOR_START=1 | --wait-for-start)
    ///  wait for a diagnostics start request before initializing and starting the VM
    pub wait_for_start: bool,

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

    /// (OPENHCL_EMULATE_APIC=1)
    /// Enable an APIC emulator.
    pub emulate_apic: bool,

    /// (OPENHCL_ENABLE_SHARED_VISIBILITY_POOL=1)
    /// Enable the shared visibility pool. This is enabled by default on
    /// hardware isolated platforms, but can be enabled for testing. Hardware
    /// devices will use the shared pool for DMA if enabled.
    pub enable_shared_visibility_pool: bool,

    /// (OPENHCL_CVM_GUEST_VSM=1)
    /// Enable support for guest vsm in CVMs. This is disabled by default.
    pub cvm_guest_vsm: bool,

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

    /// (OPENHCL_NVME_KEEP_ALIVE=1) Enable nvme keep alive when servicing.
    pub nvme_keep_alive: bool,
}

impl Options {
    pub(crate) fn parse(extra_args: Vec<String>) -> anyhow::Result<Self> {
        /// Reads an environment variable, falling back to a legacy variable (replacing
        /// "OPENHCL_" with "UNDERHILL_") if the original is not set.
        fn legacy_openhcl_env(name: &str) -> Option<std::ffi::OsString> {
            std::env::var_os(name).or_else(|| {
                std::env::var_os(format!(
                    "UNDERHILL_{}",
                    name.strip_prefix("OPENHCL_").unwrap_or(name)
                ))
            })
        }

        fn parse_bool(value: Option<std::ffi::OsString>) -> bool {
            value
                .map(|v| v.to_ascii_lowercase() == "true" || v == "1")
                .unwrap_or_default()
        }

        let parse_legacy_env_bool = |name| parse_bool(legacy_openhcl_env(name));
        let parse_env_bool = |name| parse_bool(std::env::var_os(name));

        let parse_legacy_env_number = |name| {
            legacy_openhcl_env(name)
                .map(|v| {
                    v.to_string_lossy().parse().context(format!(
                        "Error parsing numeric environment variable {} {:?}",
                        name, v
                    ))
                })
                .transpose()
        };

        let mut wait_for_start = parse_legacy_env_bool("OPENHCL_WAIT_FOR_START");
        let mut reformat_vmgs = parse_legacy_env_bool("OPENHCL_REFORMAT_VMGS");
        let mut pid = legacy_openhcl_env("OPENHCL_PID_FILE_PATH")
            .map(|x| x.to_string_lossy().into_owned().into());
        let vmbus_max_version = legacy_openhcl_env("OPENHCL_VMBUS_MAX_VERSION")
            .map(|x| {
                vmbus_core::parse_vmbus_version(&(x.to_string_lossy()))
                    .map_err(|x| anyhow::anyhow!("Error parsing vmbus max version: {}", x))
            })
            .transpose()?;
        let vmbus_enable_mnf =
            legacy_openhcl_env("OPENHCL_VMBUS_ENABLE_MNF").map(|v| parse_bool(Some(v)));
        let vmbus_force_confidential_external_memory =
            parse_env_bool("OPENHCL_VMBUS_FORCE_CONFIDENTIAL_EXTERNAL_MEMORY");
        let cmdline_append =
            legacy_openhcl_env("OPENHCL_CMDLINE_APPEND").map(|x| x.to_string_lossy().into_owned());
        let force_load_vtl0_image = legacy_openhcl_env("OPENHCL_FORCE_LOAD_VTL0_IMAGE")
            .map(|x| x.to_string_lossy().into_owned());
        let mut vnc_port = parse_legacy_env_number("OPENHCL_VNC_PORT")?.map(|x| x as u32);
        let framebuffer_gpa_base = parse_legacy_env_number("OPENHCL_FRAMEBUFFER_GPA_BASE")?;
        let vtl0_starts_paused = parse_legacy_env_bool("OPENHCL_VTL0_STARTS_PAUSED");
        let serial_wait_for_rts = parse_legacy_env_bool("OPENHCL_SERIAL_WAIT_FOR_RTS");
        let nvme_vfio = parse_legacy_env_bool("OPENHCL_NVME_VFIO");
        let emulate_apic = parse_legacy_env_bool("OPENHCL_EMULATE_APIC");
        let mcr = parse_legacy_env_bool("OPENHCL_MCR_DEVICE");
        let enable_shared_visibility_pool =
            parse_legacy_env_bool("OPENHCL_ENABLE_SHARED_VISIBILITY_POOL");
        let cvm_guest_vsm = parse_legacy_env_bool("OPENHCL_CVM_GUEST_VSM");
        let hide_isolation = parse_env_bool("OPENHCL_HIDE_ISOLATION");
        let halt_on_guest_halt = parse_legacy_env_bool("OPENHCL_HALT_ON_GUEST_HALT");
        let no_sidecar_hotplug = parse_legacy_env_bool("OPENHCL_NO_SIDECAR_HOTPLUG");
        let gdbstub = parse_legacy_env_bool("OPENHCL_GDBSTUB");
        let gdbstub_port = parse_legacy_env_number("OPENHCL_GDBSTUB_PORT")?.map(|x| x as u32);
        let nvme_keep_alive = parse_env_bool("OPENHCL_NVME_KEEP_ALIVE");

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
            reformat_vmgs,
            pid,
            vmbus_max_version,
            vmbus_enable_mnf,
            vmbus_force_confidential_external_memory,
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
            emulate_apic,
            enable_shared_visibility_pool,
            cvm_guest_vsm,
            hide_isolation,
            halt_on_guest_halt,
            no_sidecar_hotplug,
            nvme_keep_alive,
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
