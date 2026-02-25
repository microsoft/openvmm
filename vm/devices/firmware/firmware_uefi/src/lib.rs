// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI helper device.
//!
//! A bespoke virtual device that works in-tandem with the custom Hyper-V UEFI
//! firmware running within the guest.
//!
//! This device is primarily concerned with implementing + exposing the various
//! runtime services the UEFI code interfaces with.
//!
//! NOTE: Unlike Hyper-V's implementation, this device is _not_ responsible for
//! injecting UEFI config blobs into guest memory (i.e: things like VM topology
//! information, device enablement info, etc...). That happens _outside_ this
//! device, as part of VM initialization, in tandem with loading the UEFI image
//! itself.
//!
//! # Crate Structure
//!
//! The idea behind this organization is that conceptually, the UEFI device
//! isn't so much a single unified device, rather, it's a hodge-podge of little
//! "micro-devices" that all happen to be dispatched via a single pair of ports.
//!
//! ### `mod service`:
//!
//! The individual UEFI device services themselves.
//!
//! What is a service? As a rule of thumb: a service is something that has
//! one/more [`UefiCommand`]s associated with it.
//!
//! Rather than having each service directly handle its own IO port routing, the
//! top-level `UefiDevice` code in `lib.rs` takes care of that in one central
//! location. That way, the only thing service implementations needs to expose
//! is are service-specific "handler" functions.
//!
//! e.g: there's no reason for, say, UEFI generation ID services to directly
//! share state with the UEFI watchdog service, or the event log service. As
//! such, each is modeled as a separate struct + impl.
//!
//! ### `pub mod platform`
//!
//! A centralized place to expose various service-specific interface traits that
//! must be implemented by the "platform" hosting the UEFI device.
//!
//! This layer of abstraction allows the re-using the same UEFI emulator between
//! multiple VMMs (OpenVMM, Underhill, etc...), without tying the emulator to any
//! VMM specific infrastructure (via some kind of compile-time feature flag
//! infrastructure).

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod platform;
#[cfg(feature = "fuzzing")]
pub mod service;
#[cfg(not(feature = "fuzzing"))]
mod service;

pub use crate::service::diagnostics::LogLevel;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use firmware_uefi_custom_vars::CustomVars;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use local_clock::InspectableLocalClock;
use pal_async::local::block_on;
use platform::logger::UefiLogger;
use platform::nvram::VsmConfig;
use service::diagnostics::DEFAULT_LOGS_PER_PERIOD;
use std::convert::TryInto;
use std::ops::RangeInclusive;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;
use uefi_nvram_storage::VmmNvramStorage;
use vmcore::device_state::ChangeDeviceState;
use vmcore::vmtime::VmTimeSource;
use watchdog_core::platform::WatchdogPlatform;

#[derive(Debug, Error)]
pub enum UefiInitError {
    #[error("nvram setup error")]
    NvramSetup(#[from] service::nvram::NvramSetupError),
    #[error("nvram error")]
    Nvram(#[from] service::nvram::NvramError),
    #[error("event log error")]
    EventLog(#[from] service::event_log::EventLogError),
}

#[derive(Inspect, PartialEq, Clone)]
pub enum UefiCommandSet {
    X64,
    Aarch64,
}

#[derive(InspectMut)]
struct UefiDeviceServices {
    nvram: service::nvram::NvramServices,
    event_log: service::event_log::EventLogServices,
    uefi_watchdog: service::uefi_watchdog::UefiWatchdogServices,
    #[inspect(mut)]
    generation_id: service::generation_id::GenerationIdServices,
    #[inspect(mut)]
    time: service::time::TimeServices,
    diagnostics: service::diagnostics::DiagnosticsServices,
}

// Begin and end range are inclusive.
const IO_PORT_RANGE_BEGIN: u16 = 0x28;
const IO_PORT_RANGE_END: u16 = 0x2f;
const MMIO_RANGE_BEGIN: u64 = 0xeffed000;
const MMIO_RANGE_END: u64 = 0xeffedfff;

const REGISTER_ADDRESS: u16 = 0x0;
const REGISTER_DATA: u16 = 0x4;

/// Various bits of static configuration data.
#[derive(Clone)]
pub struct UefiConfig {
    pub custom_uefi_vars: CustomVars,
    pub secure_boot: bool,
    pub initial_generation_id: [u8; 16],
    pub use_mmio: bool,
    pub command_set: UefiCommandSet,
    pub diagnostics_log_level: LogLevel,
}

/// Various runtime objects used by the UEFI device + underlying services.
pub struct UefiRuntimeDeps<'a> {
    pub gm: GuestMemory,
    pub nvram_storage: Box<dyn VmmNvramStorage>,
    pub logger: Box<dyn UefiLogger>,
    pub vmtime: &'a VmTimeSource,
    pub watchdog_platform: Box<dyn WatchdogPlatform>,
    pub watchdog_recv: mesh::Receiver<()>,
    pub generation_id_deps: generation_id::GenerationIdRuntimeDeps,
    pub vsm_config: Option<Box<dyn VsmConfig>>,
    pub time_source: Box<dyn InspectableLocalClock>,
}

/// The Hyper-V UEFI services chipset device.
#[derive(InspectMut)]
#[inspect(extra = "UefiDevice::inspect_extra")]
pub struct UefiDevice {
    // Fixed configuration
    use_mmio: bool,
    command_set: UefiCommandSet,

    // Runtime glue
    gm: GuestMemory,

    // Sub-emulators
    #[inspect(mut)]
    service: UefiDeviceServices,

    // Volatile state
    #[inspect(hex)]
    address: u32,

    // Receiver for watchdog timeout events
    #[inspect(skip)]
    watchdog_recv: mesh::Receiver<()>,
}

impl UefiDevice {
    pub async fn new(
        runtime_deps: UefiRuntimeDeps<'_>,
        cfg: UefiConfig,
        is_restoring: bool,
    ) -> Result<Self, UefiInitError> {
        let UefiRuntimeDeps {
            gm,
            nvram_storage,
            logger,
            vmtime,
            watchdog_platform,
            watchdog_recv,
            generation_id_deps,
            vsm_config,
            time_source,
        } = runtime_deps;

        // Create the UEFI device with the rest of the services.
        let uefi = UefiDevice {
            use_mmio: cfg.use_mmio,
            command_set: cfg.command_set,
            address: 0,
            gm,
            watchdog_recv,
            service: UefiDeviceServices {
                nvram: service::nvram::NvramServices::new(
                    nvram_storage,
                    cfg.custom_uefi_vars,
                    cfg.secure_boot,
                    vsm_config,
                    is_restoring,
                )
                .await?,
                event_log: service::event_log::EventLogServices::new(logger),
                uefi_watchdog: service::uefi_watchdog::UefiWatchdogServices::new(
                    vmtime.access("uefi-watchdog"),
                    watchdog_platform,
                    is_restoring,
                )
                .await,
                generation_id: service::generation_id::GenerationIdServices::new(
                    cfg.initial_generation_id,
                    generation_id_deps,
                ),
                time: service::time::TimeServices::new(time_source),
                diagnostics: service::diagnostics::DiagnosticsServices::new(
                    cfg.diagnostics_log_level,
                ),
            },
        };

        Ok(uefi)
    }

    fn read_data(&mut self, addr: u32) -> u32 {
        match UefiCommand(addr) {
            UefiCommand::WATCHDOG_RESOLUTION
            | UefiCommand::WATCHDOG_CONFIG
            | UefiCommand::WATCHDOG_COUNT => {
                let reg = bios_cmd_to_watchdog_register(UefiCommand(addr)).unwrap();
                self.handle_watchdog_read(reg)
            }
            UefiCommand::NFIT_SIZE => 0, // no NFIT
            _ => {
                tracelimit::warn_ratelimited!(?addr, "unknown uefi read");
                !0
            }
        }
    }

    fn write_data(&mut self, addr: u32, data: u32) {
        match UefiCommand(addr) {
            UefiCommand::NVRAM => block_on(self.nvram_handle_command(data.into())),
            UefiCommand::EVENT_LOG_FLUSH => self.event_log_flush(data),
            UefiCommand::WATCHDOG_RESOLUTION
            | UefiCommand::WATCHDOG_CONFIG
            | UefiCommand::WATCHDOG_COUNT => {
                let reg = bios_cmd_to_watchdog_register(UefiCommand(addr)).unwrap();
                self.handle_watchdog_write(reg, data)
            }
            UefiCommand::GENERATION_ID_PTR_LOW => self.write_generation_id_low(data),
            UefiCommand::GENERATION_ID_PTR_HIGH => self.write_generation_id_high(data),
            UefiCommand::CRYPTO => self.crypto_handle_command(data.into()),
            UefiCommand::BOOT_FINALIZE if self.command_set == UefiCommandSet::X64 => {
                // We set MTRRs across all processors at load time, so we don't need to do anything here.
            }
            UefiCommand::GET_TIME if self.command_set == UefiCommandSet::Aarch64 => {
                if let Err(err) = self.get_time(data as u64) {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "failed to access memory for GET_TIME"
                    );
                }
            }
            UefiCommand::SET_TIME if self.command_set == UefiCommandSet::Aarch64 => {
                if let Err(err) = self.set_time(data as u64) {
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "failed to access memory for SET_TIME"
                    );
                }
            }
            UefiCommand::SET_EFI_DIAGNOSTICS_GPA => {
                tracelimit::info_ratelimited!(?addr, data, "set gpa for diagnostics");
                self.service.diagnostics.set_gpa(data)
            }
            UefiCommand::PROCESS_EFI_DIAGNOSTICS => {
                let _ = self.process_diagnostics(
                    false,
                    service::diagnostics::DiagnosticsEmitter::Tracing {
                        limit: Some(DEFAULT_LOGS_PER_PERIOD),
                    },
                    None,
                );
            }
            _ => tracelimit::warn_ratelimited!(addr, data, "unknown uefi write"),
        }
    }

    /// Extra inspection fields for the UEFI device.
    fn inspect_extra(&mut self, resp: &mut inspect::Response<'_>) {
        resp.field_mut_with("dump_efi_diagnostics", |v| {
            let Some(value) = v else {
                return Result::<_, std::convert::Infallible>::Ok(
                    "Use: inspect -u <default|info|full>,<stdout|tracing> vm/uefi/dump_efi_diagnostics".to_string(),
                );
            };

            let (level_str, dest_str) = value.split_once(',').unwrap_or((value, "stdout"));

            let log_level_override = match level_str {
                "default" => Some(LogLevel::make_default()),
                "info" => Some(LogLevel::make_info()),
                "full" => Some(LogLevel::make_full()),
                _ => return Result::<_, std::convert::Infallible>::Ok(
                    "Invalid log level. Use: inspect -u <default|info|full>,<stdout|tracing> vm/uefi/dump_efi_diagnostics".to_string(),
                ),
            };

            match dest_str {
                "stdout" => {
                    let result = self.process_diagnostics(
                        true,
                        service::diagnostics::DiagnosticsEmitter::String,
                        log_level_override,
                    );

                    Result::<_, std::convert::Infallible>::Ok(match result {
                        Ok(Some(output)) if output.is_empty() => {
                            "(no diagnostics entries found)".to_string()
                        }
                        Ok(Some(output)) => output,
                        Err(error) => format!("error processing diagnostics: {error}"),
                        Ok(None) => unreachable!("String emitter should return output"),
                    })
                }
                "tracing" => {
                    let _ = self.process_diagnostics(
                        true,
                        service::diagnostics::DiagnosticsEmitter::Tracing { limit: None },
                        log_level_override,
                    );
                    Result::<_, std::convert::Infallible>::Ok(format!(
                        "attempted to process diagnostics through inspect (log_level_override: {})",
                        level_str
                    ))
                }
                _ => Result::<_, std::convert::Infallible>::Ok(
                    "Invalid destination. Use: inspect -u <default|info|full>,<stdout|tracing> vm/uefi/dump_efi_diagnostics".to_string(),
                ),
            }
        });
    }
}

impl ChangeDeviceState for UefiDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.address = 0;

        self.service.nvram.reset();
        self.service.event_log.reset();
        self.service.uefi_watchdog.watchdog.reset();
        self.service.generation_id.reset();
        self.service.diagnostics.reset();
    }
}

impl ChipsetDevice for UefiDevice {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        (!self.use_mmio).then_some(self)
    }

    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        self.use_mmio.then_some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for UefiDevice {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        // Poll services
        self.service.uefi_watchdog.watchdog.poll(cx);
        self.service.generation_id.poll(cx);

        // Poll watchdog timeout events
        if let Poll::Ready(Ok(())) = self.watchdog_recv.poll_recv(cx) {
            // NOTE: Do not allow reprocessing diagnostics here.
            // UEFI programs the watchdog's configuration, so we should assume that
            // this path could trigger multiple times.
            //
            // Here, we emit diagnostics to tracing with INFO level and no limit
            let _ = self.process_diagnostics(
                false,
                service::diagnostics::DiagnosticsEmitter::Tracing { limit: None },
                Some(LogLevel::make_info()),
            );
        }
    }
}

impl PortIoIntercept for UefiDevice {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.len() != 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        let offset = io_port - IO_PORT_RANGE_BEGIN;

        let v = match offset {
            REGISTER_ADDRESS => self.address,
            REGISTER_DATA => self.read_data(self.address),
            _ => return IoResult::Err(IoError::InvalidRegister),
        };

        data.copy_from_slice(&v.to_ne_bytes());
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if data.len() != 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        let offset = io_port - IO_PORT_RANGE_BEGIN;

        let v = u32::from_ne_bytes(data.try_into().unwrap());
        match offset {
            REGISTER_ADDRESS => {
                self.address = v;
            }
            REGISTER_DATA => self.write_data(self.address, v),
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        &[("uefi", IO_PORT_RANGE_BEGIN..=IO_PORT_RANGE_END)]
    }
}

impl MmioIntercept for UefiDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        if data.len() != 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        let v = match (addr - MMIO_RANGE_BEGIN) as u16 {
            REGISTER_ADDRESS => self.address,
            REGISTER_DATA => self.read_data(self.address),
            _ => return IoResult::Err(IoError::InvalidRegister),
        };

        data.copy_from_slice(&v.to_ne_bytes());
        IoResult::Ok
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        let Ok(data) = data.try_into() else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };

        let v = u32::from_ne_bytes(data);
        match (addr - MMIO_RANGE_BEGIN) as u16 {
            REGISTER_ADDRESS => {
                self.address = v;
            }
            REGISTER_DATA => self.write_data(self.address, v),
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        &[("uefi", MMIO_RANGE_BEGIN..=MMIO_RANGE_END)]
    }
}

fn bios_cmd_to_watchdog_register(cmd: UefiCommand) -> Option<watchdog_core::Register> {
    let res = match cmd {
        UefiCommand::WATCHDOG_RESOLUTION => watchdog_core::Register::Resolution,
        UefiCommand::WATCHDOG_CONFIG => watchdog_core::Register::Config,
        UefiCommand::WATCHDOG_COUNT => watchdog_core::Register::Count,
        _ => return None,
    };
    Some(res)
}

open_enum::open_enum! {
    pub enum UefiCommand: u32 {
        GENERATION_ID_PTR_LOW        = 0x0E,
        GENERATION_ID_PTR_HIGH       = 0x0F,
        BOOT_FINALIZE                = 0x1A,

        PROCESSOR_REPLY_STATUS_INDEX = 0x13,
        PROCESSOR_REPLY_STATUS       = 0x14,
        PROCESSOR_MAT_ENABLE         = 0x15,

        // Values added in Windows Blue
        NVRAM                        = 0x24,
        CRYPTO                       = 0x26,

        // Watchdog device (Windows 8.1 MQ)
        WATCHDOG_CONFIG              = 0x27,
        WATCHDOG_RESOLUTION          = 0x28,
        WATCHDOG_COUNT               = 0x29,

        // EFI Diagnostics
        SET_EFI_DIAGNOSTICS_GPA      = 0x2B,
        PROCESS_EFI_DIAGNOSTICS      = 0x2C,

        // Event Logging (Windows 8.1 MQ/M0)
        EVENT_LOG_FLUSH              = 0x30,

        // Set MOR bit variable. Triggered by TPM _DSM Memory Clear Interface.
        // In real hardware, _DSM triggers CPU SMM. UEFI SMM driver sets the
        // MOR state via variable service. Hypervisor does not support virtual SMM,
        // so _DSM is not able to trigger SMI in Hyper-V virtualization. The
        // alternative is to send an IO port command to BIOS device and persist the
        // MOR state in UEFI NVRAM via variable service on host.
        MOR_SET_VARIABLE             = 0x31,

        // ARM64 RTC GetTime SetTime (RS2)
        GET_TIME                     = 0x34,
        SET_TIME                     = 0x35,

        // Debugger output
        DEBUG_OUTPUT_STRING          = 0x36,

        // vPMem NFIT (RS3)
        NFIT_SIZE                    = 0x37,
        NFIT_POPULATE                = 0x38,
        VPMEM_SET_ACPI_BUFFER        = 0x39,
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use crate::service::diagnostics::DiagnosticsServices;
        use crate::service::event_log::EventLogServices;
        use crate::service::generation_id::GenerationIdServices;
        use crate::service::nvram::NvramServices;
        use crate::service::time::TimeServices;
        use crate::service::uefi_watchdog::UefiWatchdogServices;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "firmware.uefi")]
        pub struct SavedState {
            #[mesh(1)]
            pub address: u32,

            #[mesh(2)]
            pub nvram: <NvramServices as SaveRestore>::SavedState,
            #[mesh(3)]
            pub event_log: <EventLogServices as SaveRestore>::SavedState,
            #[mesh(4)]
            pub watchdog: <UefiWatchdogServices as SaveRestore>::SavedState,
            #[mesh(5)]
            pub generation_id: <GenerationIdServices as SaveRestore>::SavedState,
            #[mesh(6)]
            pub time: <TimeServices as SaveRestore>::SavedState,
            #[mesh(7)]
            pub diagnostics: <DiagnosticsServices as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for UefiDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let Self {
                use_mmio: _,
                command_set: _,
                gm: _,
                watchdog_recv: _,
                service:
                    UefiDeviceServices {
                        nvram,
                        event_log,
                        uefi_watchdog,
                        generation_id,
                        time,
                        diagnostics,
                    },
                address,
            } = self;

            Ok(state::SavedState {
                address: *address,

                nvram: nvram.save()?,
                event_log: event_log.save()?,
                watchdog: uefi_watchdog.save()?,
                generation_id: generation_id.save()?,
                time: time.save()?,
                diagnostics: diagnostics.save()?,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                address,

                nvram,
                event_log,
                watchdog,
                generation_id,
                time,
                diagnostics,
            } = state;

            self.address = address;

            self.service.nvram.restore(nvram)?;
            self.service.event_log.restore(event_log)?;
            self.service.uefi_watchdog.restore(watchdog)?;
            self.service.generation_id.restore(generation_id)?;
            self.service.time.restore(time)?;
            self.service.diagnostics.restore(diagnostics)?;

            Ok(())
        }
    }
}
