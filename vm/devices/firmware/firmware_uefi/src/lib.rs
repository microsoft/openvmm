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

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;
#[cfg(feature = "fuzzing")]
pub mod service;
#[cfg(not(feature = "fuzzing"))]
mod service;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredToken;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_write;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use firmware_uefi_resources::LogLevel;
use firmware_uefi_resources::UefiCommandSet;
use firmware_uefi_resources::UefiConfig;
use firmware_uefi_resources::platform::UefiLogger;
use firmware_uefi_resources::platform::VsmConfig;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use local_clock::InspectableLocalClock;
use service::diagnostics::DEFAULT_LOGS_PER_PERIOD;
use service::diagnostics::WATCHDOG_LOGS_PER_PERIOD;
use service::nvram::NvramServices;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::future::Future;
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
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

#[derive(InspectMut)]
struct UefiDeviceServices {
    nvram: NvramState,
    event_log: service::event_log::EventLogServices,
    uefi_watchdog: service::uefi_watchdog::UefiWatchdogServices,
    #[inspect(mut)]
    generation_id: service::generation_id::GenerationIdServices,
    #[inspect(mut)]
    time: service::time::TimeServices,
    diagnostics: service::diagnostics::DiagnosticsServices,
}

/// The NVRAM services, or the command that is currently using them.
///
/// Commands are driven from [`PollDevice::poll_device`] with the I/O deferred,
/// because blocking on the store in the I/O handler holds the device lock and
/// deadlocks against an inspect on the thread that services the store.
enum NvramState {
    Idle(NvramServices),
    Running(RunningNvramCommand),
    /// Only seen while moving between the two states above.
    Invalid,
}

type NvramCommandFuture = Pin<Box<dyn Future<Output = NvramServices> + Send>>;

/// An NVRAM command in progress, which owns the NVRAM services until it
/// completes.
struct RunningNvramCommand {
    command: NvramCommandFuture,
    done: DeferredWrite,
    /// Commands issued by other VPs while this one runs, in arrival order. A
    /// VP waits for its own I/O, so this holds at most one entry per VP.
    queued: VecDeque<QueuedNvramCommand>,
}

/// An NVRAM command waiting for the running one to finish.
struct QueuedNvramCommand {
    desc_addr: u64,
    done: DeferredWrite,
}

impl Inspect for NvramState {
    fn inspect(&self, req: inspect::Request<'_>) {
        match self {
            NvramState::Idle(nvram) => nvram.inspect(req),
            NvramState::Running(_) => req.value("command in progress"),
            NvramState::Invalid => req.ignore(),
        }
    }
}

fn run_nvram_command(
    gm: GuestMemory,
    mut nvram: NvramServices,
    desc_addr: u64,
) -> NvramCommandFuture {
    Box::pin(async move {
        nvram.handle_command(&gm, desc_addr).await;
        nvram
    })
}

// Begin and end range are inclusive.
const IO_PORT_RANGE_BEGIN: u16 = 0x28;
// The device only decodes dword accesses at REGISTER_ADDRESS and REGISTER_DATA,
// so the top of the data dword (0x2e/0x2f) is left unclaimed for the
// "missing-superio" device to absorb guest probes of the legacy SuperIO ports.
const IO_PORT_RANGE_END: u16 = 0x2d;
const MMIO_RANGE_BEGIN: u64 = 0xeffed000;
const MMIO_RANGE_END: u64 = 0xeffedfff;

const REGISTER_ADDRESS: u16 = 0x0;
const REGISTER_DATA: u16 = 0x4;

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
    /// Overrides the per-period rate limit applied to EfiDiagnostics
    /// See [`UefiDevice::resolve_rate_limit`] for more information.
    diagnostics_rate_limit: Option<u32>,

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

    // Wakes the poll that drives a deferred NVRAM command.
    #[inspect(skip)]
    waker: Option<Waker>,
}

impl UefiDevice {
    pub(crate) async fn new(
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
            diagnostics_rate_limit: cfg.diagnostics_rate_limit,
            address: 0,
            gm,
            watchdog_recv,
            waker: None,
            service: UefiDeviceServices {
                nvram: NvramState::Idle(
                    NvramServices::new(
                        nvram_storage,
                        cfg.base_template,
                        cfg.custom_uefi_json,
                        cfg.secure_boot,
                        vsm_config,
                        is_restoring,
                    )
                    .await?,
                ),
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

    /// Resolves the effective per-period rate limit for diagnostics emission,
    /// given a built-in default and the device's optional override.
    ///
    /// - override is `None`: use the built-in default.
    /// - override is `Some(0)`: disable rate limiting entirely.
    /// - override is `Some(n)`: use `n` as the override limit.
    fn resolve_rate_limit(&self, default_limit: u32) -> Option<u32> {
        match self.diagnostics_rate_limit {
            None => Some(default_limit),
            Some(0) => None,
            Some(n) => Some(n),
        }
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

    /// Returns a token when the write completes later, from
    /// [`PollDevice::poll_device`].
    fn write_data(&mut self, addr: u32, data: u32) -> Option<DeferredToken> {
        match UefiCommand(addr) {
            UefiCommand::NVRAM => return Some(self.start_nvram_command(data.into())),
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
                        limit: self.resolve_rate_limit(DEFAULT_LOGS_PER_PERIOD),
                    },
                    None,
                );
            }
            _ => tracelimit::warn_ratelimited!(addr, data, "unknown uefi write"),
        }
        None
    }

    /// Starts the NVRAM command whose descriptor is at `desc_addr`, or queues
    /// it behind the one already running.
    fn start_nvram_command(&mut self, desc_addr: u64) -> DeferredToken {
        let (done, token) = defer_write();
        self.service.nvram = match std::mem::replace(&mut self.service.nvram, NvramState::Invalid) {
            NvramState::Idle(nvram) => {
                if let Some(waker) = &self.waker {
                    waker.wake_by_ref();
                }
                NvramState::Running(RunningNvramCommand {
                    command: run_nvram_command(self.gm.clone(), nvram, desc_addr),
                    done,
                    queued: VecDeque::new(),
                })
            }
            NvramState::Running(mut running) => {
                running
                    .queued
                    .push_back(QueuedNvramCommand { desc_addr, done });
                NvramState::Running(running)
            }
            NvramState::Invalid => unreachable!(),
        };
        token
    }

    /// Drives the running NVRAM command, completing its I/O and starting the
    /// next queued one when it finishes.
    fn poll_nvram(&mut self, cx: &mut Context<'_>) {
        while let NvramState::Running(running) = &mut self.service.nvram {
            let Poll::Ready(nvram) = running.command.as_mut().poll(cx) else {
                return;
            };
            let NvramState::Running(RunningNvramCommand {
                command: _,
                done,
                mut queued,
            }) = std::mem::replace(&mut self.service.nvram, NvramState::Invalid)
            else {
                unreachable!()
            };
            done.complete();
            self.service.nvram = match queued.pop_front() {
                Some(QueuedNvramCommand { desc_addr, done }) => {
                    NvramState::Running(RunningNvramCommand {
                        command: run_nvram_command(self.gm.clone(), nvram, desc_addr),
                        done,
                        queued,
                    })
                }
                None => NvramState::Idle(nvram),
            };
        }
    }

    /// Runs every started NVRAM command to completion.
    async fn finish_nvram_commands(&mut self) {
        std::future::poll_fn(|cx| {
            self.poll_nvram(cx);
            match self.service.nvram {
                NvramState::Running(_) => Poll::Pending,
                NvramState::Idle(_) | NvramState::Invalid => Poll::Ready(()),
            }
        })
        .await
    }

    fn idle_nvram(&mut self) -> &mut NvramServices {
        match &mut self.service.nvram {
            NvramState::Idle(nvram) => nvram,
            NvramState::Running(_) | NvramState::Invalid => {
                unreachable!("NVRAM commands are finished before the device stops")
            }
        }
    }

    /// Extra inspection fields for the UEFI device.
    fn inspect_extra(&mut self, resp: &mut inspect::Response<'_>) {
        const USAGE: &str =
            "Use: inspect -u <default|info|full>,<stdout|tracing> vm/uefi/process_diagnostics";

        resp.field_mut_with("process_diagnostics", |v| {
            let output = (|| {
                let value = v?;
                let (level_str, dest_str) = value.split_once(',').unwrap_or((value, "stdout"));

                let log_level_override = match level_str {
                    "default" => Some(LogLevel::make_default()),
                    "info" => Some(LogLevel::make_info()),
                    "full" => Some(LogLevel::make_full()),
                    _ => return None,
                };

                Some(match dest_str {
                    "stdout" => match self.process_diagnostics(
                        true,
                        service::diagnostics::DiagnosticsEmitter::String,
                        log_level_override,
                    ) {
                        Ok(Some(output)) if output.is_empty() => {
                            "(no diagnostics entries found)".to_string()
                        }
                        Ok(Some(output)) => output,
                        Ok(None) => unreachable!("String emitter should return output"),
                        Err(error) => format!("error processing diagnostics: {error}"),
                    },
                    "tracing" => {
                        match self.process_diagnostics(
                            true,
                            service::diagnostics::DiagnosticsEmitter::Tracing { limit: None },
                            log_level_override,
                        ) {
                            Ok(_) => format!(
                                "processed diagnostics via tracing \
                                 (log_level_override: {level_str})"
                            ),
                            Err(error) => {
                                format!("error processing diagnostics: {error}")
                            }
                        }
                    }
                    _ => return None,
                })
            })();

            Result::<_, std::convert::Infallible>::Ok(output.unwrap_or_else(|| USAGE.to_string()))
        });
    }
}

impl ChangeDeviceState for UefiDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {
        self.finish_nvram_commands().await;
    }

    async fn reset(&mut self) {
        self.address = 0;

        self.finish_nvram_commands().await;
        self.idle_nvram().reset();
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
        self.waker = Some(cx.waker().clone());
        self.poll_nvram(cx);

        // Poll services
        self.service.uefi_watchdog.watchdog.poll(cx);
        self.service.generation_id.poll(cx);

        // Poll watchdog timeout events
        if let Poll::Ready(Ok(())) = self.watchdog_recv.poll_recv(cx) {
            // NOTE: Do not allow reprocessing diagnostics here.
            // UEFI programs the watchdog's configuration, so we should assume that
            // this path could trigger multiple times.
            let _ = self.process_diagnostics(
                false,
                service::diagnostics::DiagnosticsEmitter::Tracing {
                    limit: self.resolve_rate_limit(WATCHDOG_LOGS_PER_PERIOD),
                },
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
            REGISTER_DATA => {
                if let Some(token) = self.write_data(self.address, v) {
                    return IoResult::Defer(token);
                }
            }
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
            REGISTER_DATA => {
                if let Some(token) = self.write_data(self.address, v) {
                    return IoResult::Defer(token);
                }
            }
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
                waker: _,
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
                diagnostics_rate_limit: _,
            } = self;

            Ok(state::SavedState {
                address: *address,

                // The device is stopped before it is saved, and stopping it
                // finishes every NVRAM command.
                nvram: match nvram {
                    NvramState::Idle(nvram) => nvram.save()?,
                    NvramState::Running(_) | NvramState::Invalid => {
                        unreachable!("NVRAM command in progress at save")
                    }
                },
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

            self.idle_nvram().restore(nvram)?;
            self.service.event_log.restore(event_log)?;
            self.service.uefi_watchdog.restore(watchdog)?;
            self.service.generation_id.restore(generation_id)?;
            self.service.time.restore(time)?;
            self.service.diagnostics.restore(diagnostics)?;

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use firmware_uefi_resources::platform::UefiEvent;
    use guid::Guid;
    use inspect::Inspect;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Wake;
    use ucs2::Ucs2LeSlice;
    use uefi_nvram_storage::EFI_TIME;
    use uefi_nvram_storage::NextVariable;
    use uefi_nvram_storage::NvramStorage;
    use uefi_nvram_storage::NvramStorageError;
    use uefi_nvram_storage::in_memory::InMemoryNvram;
    use uefi_specs::hyperv::nvram::NvramCommand;
    use uefi_specs::hyperv::nvram::NvramCommandDescriptor;
    use uefi_specs::hyperv::nvram::NvramVariableCommand;
    use uefi_specs::uefi::common::EfiStatus;
    use vmcore::save_restore::SaveRestore;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;
    use watchdog_core::platform::WatchdogCallback;

    /// NVRAM storage whose reads wait for a permit, standing in for a backing
    /// store that is serviced by another thread.
    #[derive(Inspect)]
    struct GatedNvram {
        #[inspect(flatten)]
        inner: InMemoryNvram,
        #[inspect(skip)]
        permits: mesh::Receiver<()>,
    }

    #[async_trait::async_trait]
    impl NvramStorage for GatedNvram {
        async fn get_variable(
            &mut self,
            name: &Ucs2LeSlice,
            vendor: Guid,
        ) -> Result<Option<(u32, Vec<u8>, EFI_TIME)>, NvramStorageError> {
            self.permits.recv().await.unwrap();
            self.inner.get_variable(name, vendor).await
        }

        async fn set_variable(
            &mut self,
            name: &Ucs2LeSlice,
            vendor: Guid,
            attr: u32,
            data: Vec<u8>,
            timestamp: EFI_TIME,
        ) -> Result<(), NvramStorageError> {
            self.inner
                .set_variable(name, vendor, attr, data, timestamp)
                .await
        }

        async fn append_variable(
            &mut self,
            name: &Ucs2LeSlice,
            vendor: Guid,
            data: Vec<u8>,
            timestamp: EFI_TIME,
        ) -> Result<bool, NvramStorageError> {
            self.inner
                .append_variable(name, vendor, data, timestamp)
                .await
        }

        async fn remove_variable(
            &mut self,
            name: &Ucs2LeSlice,
            vendor: Guid,
        ) -> Result<bool, NvramStorageError> {
            self.inner.remove_variable(name, vendor).await
        }

        async fn next_variable(
            &mut self,
            name_vendor: Option<(&Ucs2LeSlice, Guid)>,
        ) -> Result<NextVariable, NvramStorageError> {
            self.inner.next_variable(name_vendor).await
        }
    }

    impl SaveRestore for GatedNvram {
        type SavedState = <InMemoryNvram as SaveRestore>::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, vmcore::save_restore::SaveError> {
            self.inner.save()
        }

        fn restore(
            &mut self,
            state: Self::SavedState,
        ) -> Result<(), vmcore::save_restore::RestoreError> {
            self.inner.restore(state)
        }
    }

    struct TestLogger;
    impl UefiLogger for TestLogger {
        fn log_event(&self, _event: UefiEvent) {}
    }

    struct TestWatchdog;
    #[async_trait::async_trait]
    impl WatchdogPlatform for TestWatchdog {
        async fn on_timeout(&mut self) {}
        async fn read_and_clear_boot_status(&mut self) -> bool {
            false
        }
        fn add_callback(&mut self, _callback: Box<dyn WatchdogCallback>) {}
    }

    #[derive(Default)]
    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct TestDevice {
        dev: UefiDevice,
        gm: GuestMemory,
        permits: mesh::Sender<()>,
        _keeper: VmTimeKeeper,
    }

    async fn test_device(driver: &DefaultDriver) -> TestDevice {
        let gm = GuestMemory::allocate(64 * 1024);
        let keeper = VmTimeKeeper::new(driver, VmTime::from_100ns(0));
        let vmtime = keeper.builder().build(driver).await.unwrap();
        let (_genid_send, generation_id_recv) = mesh::channel();
        let (_watchdog_send, watchdog_recv) = mesh::channel();
        let (permits, permits_recv) = mesh::channel();
        let storage: Box<dyn VmmNvramStorage> = Box::new(GatedNvram {
            inner: InMemoryNvram::new(),
            permits: permits_recv,
        });

        let dev = UefiDevice {
            use_mmio: false,
            command_set: UefiCommandSet::X64,
            diagnostics_rate_limit: None,
            gm: gm.clone(),
            address: 0,
            watchdog_recv,
            waker: None,
            service: UefiDeviceServices {
                nvram: NvramState::Idle(
                    NvramServices::new(storage, None, None, false, None, true)
                        .await
                        .unwrap(),
                ),
                event_log: service::event_log::EventLogServices::new(Box::new(TestLogger)),
                uefi_watchdog: service::uefi_watchdog::UefiWatchdogServices::new(
                    vmtime.access("uefi-watchdog"),
                    Box::new(TestWatchdog),
                    false,
                )
                .await,
                generation_id: service::generation_id::GenerationIdServices::new(
                    [0; 16],
                    generation_id::GenerationIdRuntimeDeps {
                        gm: gm.clone(),
                        generation_id_recv,
                        notify_interrupt: vmcore::line_interrupt::LineInterrupt::detached(),
                    },
                ),
                time: service::time::TimeServices::new(
                    Box::new(local_clock::MockLocalClock::new()),
                ),
                diagnostics: service::diagnostics::DiagnosticsServices::new(
                    LogLevel::make_default(),
                ),
            },
        };

        TestDevice {
            dev,
            gm,
            permits,
            _keeper: keeper,
        }
    }

    /// Writes a GET_VARIABLE command for a variable that does not exist, and
    /// returns its descriptor address.
    fn write_get_variable(gm: &GuestMemory, desc_addr: u64) -> u64 {
        let name_addr = desc_addr + 0x100;
        gm.write_at(name_addr, &[b'A', 0, 0, 0]).unwrap();
        gm.write_plain(
            desc_addr,
            &NvramCommandDescriptor {
                command: NvramCommand::GET_VARIABLE,
                // Sentinel: a command that never ran must not look finished.
                status: EfiStatus::DEVICE_ERROR.into(),
            },
        )
        .unwrap();
        gm.write_plain(
            desc_addr + size_of::<NvramCommandDescriptor>() as u64,
            &NvramVariableCommand {
                attributes: 0,
                name_address: name_addr.into(),
                name_bytes: 4,
                vendor_guid: Guid::default(),
                data_address: (desc_addr + 0x200).into(),
                data_bytes: 16,
            },
        )
        .unwrap();
        desc_addr
    }

    fn status(gm: &GuestMemory, desc_addr: u64) -> EfiStatus {
        gm.read_plain::<NvramCommandDescriptor>(desc_addr)
            .unwrap()
            .status
            .into()
    }

    /// Issues the NVRAM command at `desc_addr` through the I/O port and
    /// returns the deferred I/O token.
    fn issue(dev: &mut UefiDevice, desc_addr: u64) -> DeferredToken {
        let port = IO_PORT_RANGE_BEGIN;
        assert!(matches!(
            dev.io_write(port + REGISTER_ADDRESS, &UefiCommand::NVRAM.0.to_ne_bytes()),
            IoResult::Ok
        ));
        match dev.io_write(port + REGISTER_DATA, &(desc_addr as u32).to_ne_bytes()) {
            IoResult::Defer(token) => token,
            _ => panic!("an NVRAM command must defer its I/O"),
        }
    }

    /// Drives the device until `token` completes.
    async fn complete(dev: &mut UefiDevice, token: &mut DeferredToken) {
        std::future::poll_fn(|cx| {
            dev.poll_device(cx);
            token.poll_write(cx)
        })
        .await
        .unwrap();
    }

    /// The I/O handler must not wait for the backing store: the store can
    /// depend on a thread that is itself waiting for this device's lock.
    #[async_test]
    async fn nvram_command_defers_instead_of_blocking(driver: DefaultDriver) {
        let TestDevice {
            mut dev,
            gm,
            permits,
            _keeper,
        } = test_device(&driver).await;
        let waker = Arc::new(CountingWaker::default());
        dev.poll_device(&mut Context::from_waker(&waker.clone().into()));

        let desc_addr = write_get_variable(&gm, 0x1000);
        let mut token = issue(&mut dev, desc_addr);
        assert_eq!(
            waker.0.load(Ordering::SeqCst),
            1,
            "the command must be polled"
        );

        // The store has not answered, so the command is still running.
        dev.poll_device(&mut Context::from_waker(&waker.clone().into()));
        assert!(
            token
                .poll_write(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(status(&gm, desc_addr), EfiStatus::DEVICE_ERROR);

        permits.send(());
        complete(&mut dev, &mut token).await;
        assert_eq!(status(&gm, desc_addr), EfiStatus::NOT_FOUND);
    }

    /// A command issued while another runs is queued, not dropped, and both
    /// complete in order.
    #[async_test]
    async fn concurrent_nvram_commands_complete_in_order(driver: DefaultDriver) {
        let TestDevice {
            mut dev,
            gm,
            permits,
            _keeper,
        } = test_device(&driver).await;
        dev.poll_device(&mut Context::from_waker(Waker::noop()));

        let first = write_get_variable(&gm, 0x1000);
        let second = write_get_variable(&gm, 0x4000);
        let mut first_token = issue(&mut dev, first);
        let mut second_token = issue(&mut dev, second);

        permits.send(());
        complete(&mut dev, &mut first_token).await;
        assert_eq!(status(&gm, first), EfiStatus::NOT_FOUND);
        assert_eq!(status(&gm, second), EfiStatus::DEVICE_ERROR);

        permits.send(());
        complete(&mut dev, &mut second_token).await;
        assert_eq!(status(&gm, second), EfiStatus::NOT_FOUND);
    }

    /// The MMIO register pair (used on aarch64) defers the same way.
    #[async_test]
    async fn nvram_command_over_mmio_defers(driver: DefaultDriver) {
        let TestDevice {
            mut dev,
            gm,
            permits,
            _keeper,
        } = test_device(&driver).await;
        dev.use_mmio = true;
        dev.poll_device(&mut Context::from_waker(Waker::noop()));

        let desc_addr = write_get_variable(&gm, 0x1000);
        let base = MMIO_RANGE_BEGIN;
        assert!(matches!(
            dev.mmio_write(
                base + u64::from(REGISTER_ADDRESS),
                &UefiCommand::NVRAM.0.to_ne_bytes()
            ),
            IoResult::Ok
        ));
        let mut token = match dev.mmio_write(
            base + u64::from(REGISTER_DATA),
            &(desc_addr as u32).to_ne_bytes(),
        ) {
            IoResult::Defer(token) => token,
            _ => panic!("an NVRAM command must defer its I/O"),
        };

        permits.send(());
        complete(&mut dev, &mut token).await;
        assert_eq!(status(&gm, desc_addr), EfiStatus::NOT_FOUND);
    }

    /// Stopping the device finishes a running command, so saved state never
    /// has one in flight.
    #[async_test]
    async fn stop_finishes_a_running_nvram_command(driver: DefaultDriver) {
        let TestDevice {
            mut dev,
            gm,
            permits,
            _keeper,
        } = test_device(&driver).await;
        dev.poll_device(&mut Context::from_waker(Waker::noop()));

        let desc_addr = write_get_variable(&gm, 0x1000);
        let token = issue(&mut dev, desc_addr);
        permits.send(());
        dev.stop().await;

        token.write_future().await.unwrap();
        assert_eq!(status(&gm, desc_addr), EfiStatus::NOT_FOUND);
        dev.save().unwrap();
    }
}
