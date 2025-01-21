// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCAT BIOS helper device.
//!
//! A bespoke virtual device that works in-tandem with the custom Hyper-V PCAT
//! BIOS running within the guest.
//!
//! Provides interfaces to fetch various bits of VM machine topology and
//! configuration, along with hooks into various VMM runtime services (e.g:
//! event logging, efficient busy-waiting, generation ID, etc...).

#![warn(missing_docs)]
#![forbid(unsafe_code)]

mod bios_boot_order;
mod default_cmos_values;
mod root_cpu_data;

pub use default_cmos_values::default_cmos_values;

use self::bios_boot_order::bios_boot_order;
use chipset_device::io::deferred::defer_write;
use chipset_device::io::deferred::DeferredToken;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device::ChipsetDevice;
use guestmem::GuestMemory;
use guestmem::MapRom;
use guestmem::UnmapRom;
use inspect::Inspect;
use inspect::InspectMut;
use std::fmt::Debug;
use std::ops::RangeInclusive;
use std::task::Context;
use std::time::Duration;
use thiserror::Error;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::VpIndex;
use vmcore::device_state::ChangeDeviceState;
use vmcore::vmtime::VmTimeAccess;
use vmcore::vmtime::VmTimeSource;
use zerocopy::IntoBytes;

/// Static config info which gets queried by the PCAT BIOS.
pub mod config {
    use guid::Guid;
    use inspect::Inspect;
    use vm_topology::memory::MemoryLayout;
    use vm_topology::processor::x86::X86Topology;
    use vm_topology::processor::ProcessorTopology;

    /// Subset of SMBIOS v2.4 CPU Information structure.
    #[derive(Debug, Inspect)]
    #[expect(missing_docs)] // self-explanatory fields
    pub struct SmbiosProcessorInfoBundle {
        pub processor_family: u8,
        pub voltage: u8,
        pub external_clock: u16,
        pub max_speed: u16,
        pub current_speed: u16,
    }

    /// A collection of SMBIOS constants that get reflected into the guest.
    ///
    /// There is a lot of info here, but empirically, it's not _super_ important
    /// to make these values 100% accurate...
    #[expect(missing_docs)] // self-explanatory fields
    #[derive(Debug, Inspect)]
    pub struct SmbiosConstants {
        pub bios_guid: Guid,
        #[inspect(with = "String::from_utf8_lossy")]
        pub system_serial_number: Vec<u8>,
        #[inspect(with = "String::from_utf8_lossy")]
        pub base_board_serial_number: Vec<u8>,
        #[inspect(with = "String::from_utf8_lossy")]
        pub chassis_serial_number: Vec<u8>,
        #[inspect(with = "String::from_utf8_lossy")]
        pub chassis_asset_tag: Vec<u8>,
        #[inspect(with = "String::from_utf8_lossy")]
        pub bios_lock_string: Vec<u8>,
        #[inspect(with = "String::from_utf8_lossy")]
        pub processor_manufacturer: Vec<u8>,
        #[inspect(with = "String::from_utf8_lossy")]
        pub processor_version: Vec<u8>,
        /// If set to `None`, default UNKNOWN values are used
        pub cpu_info_bundle: Option<SmbiosProcessorInfoBundle>,
    }

    /// A particular kind of boot device PCAT understands.
    #[derive(Debug, Clone, Copy, Inspect)]
    #[expect(missing_docs)] // self-explanatory variants
    pub enum BootDevice {
        Floppy = 0,
        Optical = 1,
        HardDrive = 2,
        Network = 3,
    }

    /// Determines if a boot device is connected or not.
    #[derive(Debug, Clone, Copy, Inspect)]
    pub struct BootDeviceStatus {
        /// Boot device
        pub kind: BootDevice,
        /// Whether it is physically attached to the system
        pub attached: bool,
    }

    /// PCAT device static configuration data.
    #[derive(Debug, Inspect)]
    pub struct PcatBiosConfig {
        /// Number of VCPUs
        pub processor_topology: ProcessorTopology<X86Topology>,
        /// The VM's memory layout
        pub mem_layout: MemoryLayout,
        /// The SRAT ACPI table reflected into the guest
        pub srat: Vec<u8>,
        /// Initial [Generation Id](generation_id) value
        pub initial_generation_id: [u8; 16],
        /// Hibernation support
        pub hibernation_enabled: bool,
        /// Boot device order
        #[inspect(iter_by_index)]
        pub boot_order: [BootDeviceStatus; 4],
        /// If num-lock is enabled at boot
        pub num_lock_enabled: bool,
        /// Bundle of SMBIOS constants
        pub smbios: SmbiosConstants,
    }
}

/// PCAT event
#[derive(Debug)]
pub enum PcatEvent {
    /// Failed to boot via any boot medium
    BootFailure,
    /// Attempted to boot (INT19) via BIOS
    BootAttempt,
}

/// Platform interface to emit PCAT events.
pub trait PcatLogger: Send {
    /// Emit a log corresponding to the provided event.
    fn log_event(&self, event: PcatEvent);
}

#[derive(Debug, Inspect)]
struct PcatBiosState {
    #[inspect(hex)]
    address: u32,
    #[inspect(hex)]
    read_count: u32,
    #[inspect(hex)]
    e820_entry: u8,
    #[inspect(hex)]
    srat_offset: u32,
    #[inspect(hex)]
    srat_size: u32,
    #[inspect(hex)]
    port80: u32,
    #[inspect(skip)]
    entropy: [u8; 64],
    entropy_placed: bool,
}

impl PcatBiosState {
    fn new() -> Self {
        let mut entropy = [0; 64];
        getrandom::getrandom(&mut entropy).expect("rng failure");
        Self {
            address: 0,
            read_count: 0,
            e820_entry: 0,
            srat_offset: 0,
            srat_size: 0,
            port80: 0,
            entropy,
            entropy_placed: false,
        }
    }
}

/// PCAT device runtime dependencies.
#[expect(missing_docs)] // self-explanatory fields
pub struct PcatBiosRuntimeDeps<'a> {
    pub gm: GuestMemory,
    pub logger: Box<dyn PcatLogger>,
    pub generation_id_deps: generation_id::GenerationIdRuntimeDeps,
    pub vmtime: &'a VmTimeSource,
    /// The BIOS ROM.
    ///
    /// If missing, then assume the ROM is already in memory.
    pub rom: Option<Box<dyn MapRom>>,
    pub register_pio: &'a mut dyn RegisterPortIoIntercept,
    /// Replays the initial MTRRs on all VPs.
    pub replay_mtrrs: Box<dyn Send + FnMut()>,
}

/// PCAT BIOS helper device.
#[derive(InspectMut)]
pub struct PcatBiosDevice {
    // Fixed configuration
    config: config::PcatBiosConfig,

    // Runtime glue
    vmtime_wait: VmTimeAccess,
    gm: GuestMemory,
    #[inspect(skip)]
    logger: Box<dyn PcatLogger>,
    #[inspect(skip)]
    _rom_mems: Vec<Box<dyn UnmapRom>>,
    pre_boot_pio: PreBootStubbedPio,
    #[inspect(skip)]
    replay_mtrrs: Box<dyn Send + FnMut()>,

    // Sub-emulators
    #[inspect(mut)]
    generation_id: generation_id::GenerationId,

    // Runtime book-keeping
    #[inspect(skip)]
    deferred_wait: Option<DeferredWrite>,

    // Volatile state
    state: PcatBiosState,
}

// Begin and end range are inclusive.
const IO_PORT_RANGE_BEGIN: u16 = 0x28;
const IO_PORT_RANGE_END: u16 = 0x2f;
const IO_PORT_ADDR_OFFSET: u16 = 0x0;
const IO_PORT_DATA_OFFSET: u16 = 0x4;

// Reports BIOS POST status.
const POST_IO_PORT: u16 = 0x80;

/// Errors which may occur during PCAT BIOS helper device initialization.
#[derive(Debug, Error)]
#[expect(missing_docs)] // self-explanatory variants
pub enum PcatBiosDeviceInitError {
    #[error("expected exactly 2 mmio holes, found {0}")]
    IncorrectMmioHoles(usize),
    #[error("invalid ROM size {0:x} bytes, expected 256KB")]
    InvalidRomSize(u64),
    #[error("error mapping ROM")]
    Rom(#[source] std::io::Error),
}

impl PcatBiosDevice {
    /// Create a new instance of the PCAT BIOS helper device.
    pub fn new(
        runtime_deps: PcatBiosRuntimeDeps<'_>,
        config: config::PcatBiosConfig,
    ) -> Result<PcatBiosDevice, PcatBiosDeviceInitError> {
        let PcatBiosRuntimeDeps {
            gm,
            logger,
            generation_id_deps,
            vmtime,
            rom,
            register_pio,
            replay_mtrrs,
        } = runtime_deps;

        let initial_generation_id = config.initial_generation_id;

        if config.mem_layout.mmio().len() != 2 {
            return Err(PcatBiosDeviceInitError::IncorrectMmioHoles(
                config.mem_layout.mmio().len(),
            ));
        }

        let mut rom_mems = Vec::new();
        if let Some(rom) = rom {
            let rom_size = rom.len();
            if rom_size != 0x40000 {
                return Err(PcatBiosDeviceInitError::InvalidRomSize(rom_size));
            }

            // Map the ROM at both high and low memory.
            for gpa in [0xfffc0000, 0xf0000] {
                let rom_offset = (gpa + rom_size) & 0xfffff;
                let len = rom_size - rom_offset;
                let mem = rom
                    .map_rom(gpa, rom_offset, len)
                    .map_err(PcatBiosDeviceInitError::Rom)?;
                rom_mems.push(mem);
            }
        }

        Ok(PcatBiosDevice {
            gm,
            logger,
            config,
            state: PcatBiosState::new(),
            generation_id: generation_id::GenerationId::new(
                initial_generation_id,
                generation_id_deps,
            ),
            vmtime_wait: vmtime.access("pcat-wait"),
            deferred_wait: None,
            _rom_mems: rom_mems,
            pre_boot_pio: PreBootStubbedPio::new(register_pio),
            replay_mtrrs,
        })
    }

    fn index_using_read_count(&self, data: &[u8]) -> u32 {
        let index = (self.state.read_count % 8) as usize * 4;
        let mut buffer = [0u8; 4];
        for i in 0..4_usize {
            if index + i < data.len() {
                buffer[i] = data[index + i];
            } else {
                buffer[i] = b' ';
            }
        }
        u32::from_ne_bytes(buffer)
    }

    fn read_data(&mut self, addr: u32) -> u32 {
        let mut buffer = [0u8; 4];
        match PcatAddress(addr) {
            PcatAddress::FIRST_MEMORY_BLOCK_SIZE => {
                // Consumers: PCAT BIOS in source/bsp/OEM.ASM
                //
                // Report only the first memory block here as the BIOS really
                // isn't structured to deal with gaps between memory blocks.
                // This will bound where the BIOS puts things, including the
                // ACPI tables, answers to INT 15 E820, etc.
                self.config.mem_layout.ram()[0].range.len().to_kb()
            }
            PcatAddress::NUM_LOCK_ENABLED => self.config.num_lock_enabled as u32,
            PcatAddress::BIOS_GUID => {
                let index = (self.state.read_count % 4) as usize;
                buffer.copy_from_slice(&self.config.smbios.bios_guid.as_bytes()[index * 4..][..4]);
                u32::from_ne_bytes(buffer)
            }
            PcatAddress::BIOS_SYSTEM_SERIAL_NUMBER => {
                self.index_using_read_count(self.config.smbios.system_serial_number.as_bytes())
            }
            PcatAddress::BIOS_BASE_SERIAL_NUMBER => {
                self.index_using_read_count(self.config.smbios.base_board_serial_number.as_bytes())
            }
            PcatAddress::BIOS_CHASSIS_SERIAL_NUMBER => {
                self.index_using_read_count(self.config.smbios.chassis_serial_number.as_bytes())
            }
            PcatAddress::BIOS_CHASSIS_ASSET_TAG => {
                self.index_using_read_count(self.config.smbios.chassis_asset_tag.as_bytes())
            }
            PcatAddress::BOOT_DEVICE_ORDER => bios_boot_order(&self.config.boot_order),
            PcatAddress::BIOS_PROCESSOR_COUNT => self.config.processor_topology.vp_count(),
            PcatAddress::PROCESSOR_LOCAL_APIC_ID => {
                if self.state.read_count < self.config.processor_topology.vp_count() {
                    self.config
                        .processor_topology
                        .vp_arch(VpIndex::new(self.state.read_count))
                        .apic_id
                } else {
                    !0
                }
            }
            PcatAddress::SRAT_SIZE => self.config.srat.len() as u32,
            PcatAddress::SRAT_DATA => {
                let srat_chunk = (self.state.srat_offset + self.state.read_count * 4) as usize;
                if let Some(data) = self.config.srat.get(srat_chunk..).and_then(|c| c.get(..4)) {
                    u32::from_ne_bytes(data.try_into().unwrap())
                } else {
                    tracelimit::warn_ratelimited!(
                        "invalid SRAT offset: {} + {} * 4 < {} - 4",
                        self.state.srat_offset,
                        self.state.read_count,
                        self.config.srat.len()
                    );
                    0
                }
            }
            PcatAddress::MEMORY_AMOUNT_ABOVE_4GB => {
                // Consumers:
                // - vmbios/source/bsp/em/smbios/Smbport.asm,
                // - core/src/MEM.ASM.
                self.config.mem_layout.ram_above_4gb().to_mb()
            }
            PcatAddress::SLEEP_STATES => {
                // The AMI BIOS wants to read a byte value of flags to determine
                // what sleep states (S1...S4) are supported. In the original
                // AMI BIOS code, S4 was enabled as:
                //
                //              or      aml_buff.AMLDATA.dSx, 8
                //
                // Our data register is 4-bytes wide, we just fill in the low
                // byte (al) here with the S4 flag if it should be set
                if self.config.hibernation_enabled {
                    8
                } else {
                    0
                }
            }
            PcatAddress::PCI_IO_GAP_START => {
                self.config.mem_layout.mmio()[0].start().try_into().unwrap()
            }
            PcatAddress::PROCESSOR_STA_ENABLE => {
                // Read by the ACPI _STA (status) method in the Processor
                // objects in the PCAT BIOS DSDT. Return zero (not active) for
                // any processor whose index exceeds the current active
                // processor count.
                if self.state.read_count < self.config.processor_topology.vp_count() {
                    1
                } else {
                    0
                }
            }
            PcatAddress::BIOS_LOCK_STRING => {
                self.index_using_read_count(self.config.smbios.bios_lock_string.as_bytes())
            }
            PcatAddress::MEMORY_ABOVE_HIGH_MMIO => {
                // Consumers:
                // - vmbios/source/bsp/em/smbios/Smbport.asm,
                // - core/src/MEM.ASM.
                self.config
                    .mem_layout
                    .ram_above_high_mmio()
                    .expect("validated exactly 2 mmio ranges")
                    .to_mb()
            }
            PcatAddress::HIGH_MMIO_GAP_BASE_IN_MB => {
                // Consumers:
                // - vmbios/source/bsp/em/smbios/Smbport.asm,
                // - core/src/MEM.ASM.
                self.config.mem_layout.mmio()[1].start().to_mb()
            }
            PcatAddress::HIGH_MMIO_GAP_LENGTH_IN_MB => {
                // Consumers:
                // - vmbios/source/bsp/em/smbios/Smbport.asm,
                // - core/src/MEM.ASM.
                //
                // In a classic case of "two wrongs make a right", PCAT expects
                // to get _one less_ than the true MMIO region length , as when
                // this code was written in Hyper-V, the `end - start`
                // calculation used an _inclusive_ `start..=end` range from the
                // MMIO gaps API, which wasn't properly compensated for here.
                self.config.mem_layout.mmio()[1].len().to_mb() - 1
            }
            PcatAddress::E820_ENTRY => handle_int15_e820_query(
                &self.config.mem_layout,
                self.state.e820_entry,
                self.state.read_count,
            ),
            PcatAddress::INITIAL_MEGABYTES_BELOW_GAP => {
                // Consumers: vmbios/source/bsp/em/smbios/smbios/Smbport.asm
                self.config.mem_layout.ram_below_4gb().to_mb()
            }
            _ => {
                tracelimit::warn_ratelimited!(?addr, "unknown bios read");
                0xffffffff
            }
        }
    }

    fn write_data(
        &mut self,
        addr: u32,
        data: u32,
    ) -> Result<Option<DeferredToken>, guestmem::GuestMemoryError> {
        match PcatAddress(addr) {
            PcatAddress::BIOS_PROCESSOR_COUNT => {
                // gets poked by the bios for some reason...
            }
            PcatAddress::SRAT_SIZE => {
                if self.config.srat.len() > (data as usize) {
                    tracelimit::warn_ratelimited!(
                        data,
                        len = self.config.srat.len(),
                        "improper SRAT_SIZE write",
                    );
                }

                self.state.srat_size = data;
            }
            PcatAddress::SRAT_OFFSET => {
                if (data as usize) >= self.config.srat.len() || data >= self.state.srat_size {
                    tracelimit::warn_ratelimited!(
                        data,
                        len = self.config.srat.len(),
                        "improper SRAT_OFFSET write",
                    );
                }

                self.state.srat_offset = data;
            }
            PcatAddress::SRAT_DATA => {
                if data == 0 || data == 0xffffffff {
                    tracelimit::warn_ratelimited!(data, "improper SRAT_DATA write");
                }

                self.gm.write_at(data as u64, &self.config.srat)?;
            }
            PcatAddress::BOOT_FINALIZE => {
                // The BIOS trashes the originally set MTRRs. Reset them.
                (self.replay_mtrrs)();
            }
            PcatAddress::ENTROPY_TABLE => {
                if data == 0 || data == 0xffffffff {
                    tracelimit::warn_ratelimited!(data, "improper ENTROPY_TABLE write");
                }

                if !self.state.entropy_placed {
                    self.gm.write_plain(data as u64, &self.state.entropy)?;
                    self.state.entropy_placed = true;
                }
            }
            PcatAddress::PROCESSOR_DMTF_TABLE => {
                if data == 0 || data == 0xffffffff {
                    tracelimit::warn_ratelimited!(data, "improper PROCESSOR_DMTF_TABLE write");
                }

                let cpu_info_legacy = root_cpu_data::get_vp_dmi_info(
                    self.config.smbios.cpu_info_bundle.as_ref(),
                    &self.config.smbios.processor_manufacturer,
                    &self.config.smbios.processor_version,
                );

                self.gm.write_plain(data as u64, &cpu_info_legacy)?;
            }
            PcatAddress::PROCESSOR_STA_ENABLE => {
                // NOTE: doesn't make a whole lot of sense, but that's what our
                // old impl did, so better safe than sorry...
                self.state.read_count = data;
            }
            PcatAddress::WAIT_NANO100 => {
                return Ok(Some(
                    self.defer_wait(Duration::from_nanos(data as u64 * 100)),
                ))
            }
            PcatAddress::GENERATION_ID_PTR_LOW => self.generation_id.write_generation_id_low(data),
            PcatAddress::GENERATION_ID_PTR_HIGH => {
                self.generation_id.write_generation_id_high(data)
            }
            PcatAddress::E820_ENTRY => {
                self.state.e820_entry = data as u8;
            }
            _ => tracelimit::warn_ratelimited!(addr, data, "unknown bios write"),
        }

        Ok(None)
    }

    fn write_address(&mut self, addr: u32) -> Option<DeferredToken> {
        // As a side effect of setting the address register, we also reset the
        // data register read counter.
        self.state.address = addr;
        self.state.read_count = 0;

        // Some commands do not write to the data register, only the address
        // register (so as to save an additional VMEXIT).
        match PcatAddress(addr) {
            PcatAddress::WAIT1_MILLISECOND => {
                return Some(self.defer_wait(Duration::from_millis(1)))
            }
            PcatAddress::WAIT10_MILLISECONDS => {
                return Some(self.defer_wait(Duration::from_millis(10)))
            }
            PcatAddress::WAIT2_MILLISECOND => {
                return Some(self.defer_wait(Duration::from_millis(2)))
            }
            PcatAddress::REPORT_BOOT_FAILURE => {
                tracelimit::info_ratelimited!("pcat boot: failure");
                self.stop_pre_boot_pio();
                self.logger.log_event(PcatEvent::BootFailure)
            }
            PcatAddress::REPORT_BOOT_ATTEMPT => {
                tracelimit::info_ratelimited!("pcat boot: attempt");
                self.stop_pre_boot_pio();
                self.logger.log_event(PcatEvent::BootAttempt)
            }
            _ => {}
        }
        None
    }

    fn defer_wait(&mut self, duration: Duration) -> DeferredToken {
        tracing::trace!(?duration, "deferring wait request");
        self.vmtime_wait
            .set_timeout(self.vmtime_wait.now().wrapping_add(duration));
        let (write, token) = defer_write();
        self.deferred_wait = Some(write);
        token
    }

    /// Unmap the pre-boot PIO stubs if they are active.
    /// This should be called before booting into an OS, since
    /// the BIOS should no longer try to access these ports.
    fn stop_pre_boot_pio(&mut self) {
        if self.pre_boot_pio.is_active() {
            tracing::info!("disabling pre-boot legacy port-io stubs");
            self.pre_boot_pio.unmap();
        }
    }
}

open_enum::open_enum! {
    /// Must match constants in VMCONFIG.EQU
    enum PcatAddress: u32 {
        FIRST_MEMORY_BLOCK_SIZE      = 0x00,
        NUM_LOCK_ENABLED             = 0x01,
        BIOS_GUID                    = 0x02,
        BIOS_SYSTEM_SERIAL_NUMBER    = 0x03,
        BIOS_BASE_SERIAL_NUMBER      = 0x04,
        BIOS_CHASSIS_SERIAL_NUMBER   = 0x05,
        BIOS_CHASSIS_ASSET_TAG       = 0x06,
        BOOT_DEVICE_ORDER            = 0x07,
        BIOS_PROCESSOR_COUNT         = 0x08,
        PROCESSOR_LOCAL_APIC_ID      = 0x09,
        SRAT_SIZE                    = 0x0A,
        SRAT_OFFSET                  = 0x0B,
        SRAT_DATA                    = 0x0C,
        MEMORY_AMOUNT_ABOVE_4GB      = 0x0D,
        GENERATION_ID_PTR_LOW        = 0x0E,
        GENERATION_ID_PTR_HIGH       = 0x0F,
        SLEEP_STATES                 = 0x10,

        PCI_IO_GAP_START             = 0x12,

        PROCESSOR_STA_ENABLE         = 0x16,
        WAIT_NANO100                 = 0x17,
        WAIT1_MILLISECOND            = 0x18,
        WAIT10_MILLISECONDS          = 0x19,
        BOOT_FINALIZE                = 0x1A,
        WAIT2_MILLISECOND            = 0x1B,
        BIOS_LOCK_STRING             = 0x1C,
        PROCESSOR_DMTF_TABLE         = 0x1D,
        ENTROPY_TABLE                = 0x1E,
        MEMORY_ABOVE_HIGH_MMIO       = 0x1F,
        HIGH_MMIO_GAP_BASE_IN_MB     = 0x20,
        HIGH_MMIO_GAP_LENGTH_IN_MB   = 0x21,
        E820_ENTRY                   = 0x22,
        INITIAL_MEGABYTES_BELOW_GAP  = 0x23,

        REPORT_BOOT_FAILURE          = 0x3A,
        REPORT_BOOT_ATTEMPT          = 0x3B,
    }
}

/// Handler for PCAT BIOS e820 Enlightenment
///
/// The following documentation is copied wholesale from the OS repo.
///
/// * * *
///
/// The guest OS will discover the parts of GPA space that are populated with
/// usable RAM by using the INT 15 E820 interface. This interface returns one
/// entry of the table per invocation, with an iterator value passed back and
/// forth through EBX.
///
/// Our virtual AMI BIOS is constructed in a way that's difficult to change
/// without odd side effects, as many things look at the E820 table entries
/// internally, and it's not always clear which parts are switched on or off,
/// making changes hard to validate.
///
/// Extending the AMI BIOS to understand an unbounded number of memory blocks,
/// each with a small gap between them is more difficult than just calling out
/// to the worker process and handing it here. On the other hand, some
/// parameters, such as the location of the Extended BIOS Data Area (EBDA) are
/// really BIOS-internal things and moving them to the worker process would be
/// fragile. So the algorithm here is that the BIOS responds to queries about
/// everything involving the first memory block. The BIOS sets itself up within
/// that. Any subsequent memory block is handled here within the worker process.
///
/// From the ACPI spec:
///
/// ```text
/// Input:
///
///     Register    |   Parameter   |   Description
///                 |               |
///       EAX       | Function Code |   E820
///                 |               |
///       EBX       | Continuation  |   Contains the loop counter.
///                 |               |
///       ES:DI     | Buffer Ptr    |   Pointer to a buffer with the table entry.
///                 |               |
///       ECX       | Buffer Size   |   Size of passed in struct.
///                 |               |
///       EDX       | Signature     |   'SMAP'
///
/// Output:
///
///       EAX       | Signature     |   'SMAP'
///                 |               |
///       ES:DI     | Buffer Ptr    |   same as input
///                 |               |
///       ECX       | Size          |   20 bytes
///                 |               |
///       EBX       | Continuation  |   Value that the caller should use to get
///                 |               |   the next entry.
///```
///
/// In order to avoid opening an aperture to the guest here, the BIOS takes
/// register contents modified by this function and unpacks them into the
/// caller's buffer.
///
/// The AMI BIOS will subtract the number of entries that it wants to handle
/// internally from EBX before writing it to the BIOS port, so that this
/// function will see indices starting with 0.
///
/// So we return to the guest using this port as a FIFO. Each successive read
/// returns a different part of the data:
///
/// ```text
///       0 (b:0)       | 1 == "entry exists"
///       0 (b:1)       | 0 == "memory",      1 == "reserved"
///       0 (b:2)       | 0 == "last entry",    1 == "there's more data"
///       0 (31:3)      | Length in megabytes low (48:20)
///       1             | Base Address Low
///       2             | Base Address High
/// ```
fn handle_int15_e820_query(mem_layout: &MemoryLayout, e820_entry: u8, read_count: u32) -> u32 {
    // The first memory range is the one that the BIOS itself knows about, and
    // the one for which the BIOS will answer the guest OS's questions. This is
    // done because the BIOS places various tables (EBDA, ACPI "reclaim", ACPI
    // NVS, etc.) in this memory block, carving things out of it.
    //
    // The BIOS, on the other hand, has no idea, at least in the core BIOS code,
    // that the other memory blocks exist. This is necessary because there can
    // be a series of gaps between memory blocks that are hard to accommodate
    // within the BIOS. For reporting things above the gaps, this function looks
    // at the upper memory blocks.
    let index = (e820_entry + 1) as usize;

    // Special case: if there is only a single RAM range, no error should be
    // logged + zero should be returned, indicating that there are no further
    // RAM regions.
    if e820_entry == 0 && mem_layout.ram().len() == 1 {
        return 0;
    }

    let Some(ram) = mem_layout.ram().get(index) else {
        tracelimit::warn_ratelimited!(?e820_entry, "unexpected e820 entry");
        return 0;
    };

    match read_count {
        0 => {
            let mut data = 1; // entry exists
            data |= if index + 1 != mem_layout.ram().len() {
                0b100 // more data
            } else {
                0 // last entry
            };
            data |= ram.range.len().to_mb() << 3; // clamp reported RAM to the nearest megabyte
            data
        }
        1 => ram.range.start() as u32,
        2 => (ram.range.start() >> 32) as u32,
        _ => {
            tracelimit::warn_ratelimited!(?read_count, "invalid E820 read count");
            0
        }
    }
}

impl ChangeDeviceState for PcatBiosDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.generation_id.reset();
        self.state = PcatBiosState::new();
    }
}

impl ChipsetDevice for PcatBiosDevice {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for PcatBiosDevice {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        self.generation_id.poll(cx);
        while self.vmtime_wait.poll_timeout(cx).is_ready() {
            if let Some(deferred) = self.deferred_wait.take() {
                tracing::trace!("releasing deferred wait");
                deferred.complete();
            }
        }
    }
}

impl MmioIntercept for PcatBiosDevice {
    fn mmio_read(&mut self, _addr: u64, _data: &mut [u8]) -> IoResult {
        tracelimit::error_ratelimited!("firmware should be mapped, should not be visible as MMIO");
        IoResult::Ok
    }

    fn mmio_write(&mut self, addr: u64, _data: &[u8]) -> IoResult {
        match addr {
            0xf5bea | 0xf5bfa => {
                // There is a bug in the firmware's throttle_getchar_FAR
                // enlightenment: it expects to write to a value in the ROM
                // segment, but this is not writable after POST. Just ignore
                // this, it means that getchar is not actually throttled after
                // POST (e.g. in DOS).
            }
            _ => tracelimit::warn_ratelimited!(addr, "unexpected firmware write"),
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        &[
            ("rom-low", 0xf0000..=0xfffff),
            ("rom-high", 0xfffc_0000..=0xffff_ffff),
        ]
    }
}

impl PortIoIntercept for PcatBiosDevice {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if io_port == POST_IO_PORT {
            data.copy_from_slice(&self.state.port80.to_ne_bytes()[..data.len()]);
            return IoResult::Ok;
        }

        if self.pre_boot_pio.contains_port(io_port) {
            tracing::trace!(?io_port, "stubbed pre-boot pio read");
            data.fill(!0);
            return IoResult::Ok;
        }

        // Some OSes probe for an 8-bit superio device at this location,
        // silence the logs generated by this.
        if io_port == 0x2f && data.len() == 1 {
            tracing::trace!(?io_port, "stubbed superio pio read");
            data.fill(!0);
            return IoResult::Ok;
        }

        if data.len() != 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        let offset = io_port - IO_PORT_RANGE_BEGIN;
        let v = match offset {
            IO_PORT_ADDR_OFFSET => self.state.address,
            IO_PORT_DATA_OFFSET => self.read_data(self.state.address),
            _ => return IoResult::Err(IoError::InvalidRegister),
        };
        data.copy_from_slice(&v.to_ne_bytes());

        tracing::trace!(
            offset,
            address = self.state.address,
            read_count = self.state.read_count,
            value = v,
            "bios read",
        );

        if offset == IO_PORT_DATA_OFFSET {
            self.state.read_count += 1;
        }

        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if io_port == POST_IO_PORT {
            let mut v = [0; 4];
            v[..data.len()].copy_from_slice(data);
            let data = u32::from_ne_bytes(v);

            tracing::debug!(data, "pcat boot: checkpoint");

            // magic number specific to PCAT BIOS
            const AT_END_POST_CHECKPOINT: u32 = 0x50ac;
            if data == AT_END_POST_CHECKPOINT {
                self.stop_pre_boot_pio();
            }

            // Store the port 80 data. Consider keeping a ring of
            // these for inspect in the future.
            self.state.port80 = data;
            return IoResult::Ok;
        }

        if self.pre_boot_pio.contains_port(io_port) {
            tracing::trace!(?io_port, ?data, "stubbed pre-boot pio write");
            return IoResult::Ok;
        }

        // Some OSes probe for an 8-bit superio device at this location,
        // silence the logs generated by this.
        if io_port == 0x2e && data.len() == 1 {
            tracing::trace!(?io_port, ?data, "stubbed superio pio write");
            return IoResult::Ok;
        }

        if data.len() != 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        let offset = io_port - IO_PORT_RANGE_BEGIN;
        let v = u32::from_ne_bytes(data.try_into().unwrap());
        let r = match offset {
            IO_PORT_ADDR_OFFSET => Ok(self.write_address(v)),
            IO_PORT_DATA_OFFSET => self.write_data(self.state.address, v),
            _ => return IoResult::Err(IoError::InvalidRegister),
        };

        match r {
            Ok(Some(token)) => return IoResult::Defer(token),
            Ok(None) => {}
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "bios command error"
                );
            }
        }

        tracing::trace!(
            offset,
            address = self.state.address,
            read_count = self.state.read_count,
            data = v,
            "bios write",
        );

        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        &[
            ("pcat_bios", IO_PORT_RANGE_BEGIN..=IO_PORT_RANGE_END),
            // NOTE: POST port 0x80 might overlap with a an ISA DMA page register.
            ("post", POST_IO_PORT..=POST_IO_PORT),
        ]
    }
}

/// Helper trait to convert bytes to various other units
trait ConvertBytes {
    /// Convert from bytes to megabytes
    fn to_mb(self) -> u32;
    /// Convert from bytes to kiloytes
    fn to_kb(self) -> u32;
}

impl ConvertBytes for u64 {
    fn to_mb(self) -> u32 {
        (self >> 20).try_into().unwrap()
    }

    fn to_kb(self) -> u32 {
        (self >> 10).try_into().unwrap()
    }
}

/// Encapsulates ownership over various legacy port io locations that the PCAT
/// BIOS attempts to access during init.
///
/// We don't implement any of the devices backing these ports, so in order to
/// cut down on the large amount of "unknown device" logging, we claim these
/// ports for the PCAT BIOS helper device during pre-boot, and then release
/// ownership post-boot.
#[derive(Inspect)]
struct PreBootStubbedPio {
    #[inspect(iter_by_index)]
    ranges: Vec<Box<dyn ControlPortIoIntercept>>,
}

impl PreBootStubbedPio {
    const LEN_PORT: &'static [(u16, u16)] = &[
        // ISA PnP
        (1, 0x279), // index
        (1, 0xa79), // write data port
        (1, 0x20b), // initial value for read data port
        (1, 0x20f), // ...which PCAT will increment by 4
        (1, 0x213),
        (1, 0x217),
        (1, 0x21b),
        (1, 0x21f),
        (1, 0x223),
        (1, 0x227), // ...until it gives up (after 8x tries)
        // something to do with archaic dual VGA init?
        (2, 0x102),
        (2, 0x46e8),
        // something to do with piix4 "routing ports"?
        (1, 0xeb),
        // (1, 0xed), // gets claimed as part of the 0xED IO port delay device
        (1, 0xee),
        // no idea ¯\_(ツ)_/¯
        (1, 0x6f0),
    ];

    fn new(register_pio: &mut dyn RegisterPortIoIntercept) -> PreBootStubbedPio {
        let mut ranges = Vec::new();
        for &(len, port) in Self::LEN_PORT {
            let mut control = register_pio.new_io_region("legacy-port-stub", len);
            control.map(port);
            ranges.push(control)
        }
        PreBootStubbedPio { ranges }
    }

    fn is_active(&self) -> bool {
        !self.ranges.is_empty()
    }

    fn unmap(&mut self) {
        for mut range in self.ranges.drain(..) {
            range.unmap()
        }
    }

    fn contains_port(&self, port: u16) -> bool {
        if !self.is_active() {
            return false;
        }

        Self::LEN_PORT
            .iter()
            .any(|&(len, p)| (p..p + len).contains(&port))
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use generation_id::GenerationId;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "firmware.pcat")]
        pub struct SavedState {
            #[mesh(1)]
            pub address: u32,
            #[mesh(2)]
            pub read_count: u32,
            #[mesh(3)]
            pub e820_entry: u8,
            #[mesh(4)]
            pub srat_offset: u32,
            #[mesh(5)]
            pub srat_size: u32,
            #[mesh(6)]
            pub port80: u32,
            #[mesh(7)]
            pub entropy: [u8; 64],
            #[mesh(8)]
            pub entropy_placed: bool,

            #[mesh(9)]
            pub genid: <GenerationId as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for PcatBiosDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let PcatBiosState {
                address,
                read_count,
                e820_entry,
                srat_offset,
                srat_size,
                port80,
                entropy,
                entropy_placed,
            } = self.state;

            let saved_state = state::SavedState {
                address,
                read_count,
                e820_entry,
                srat_offset,
                srat_size,
                port80,
                entropy,
                entropy_placed,
                genid: self.generation_id.save()?,
            };

            // sanity check that there aren't any outstanding deferred IOs
            assert!(self.deferred_wait.is_none());

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                address,
                read_count,
                e820_entry,
                srat_offset,
                srat_size,
                port80,
                entropy,
                entropy_placed,
                genid,
            } = state;

            self.state = PcatBiosState {
                address,
                read_count,
                e820_entry,
                srat_offset,
                srat_size,
                port80,
                entropy,
                entropy_placed,
            };

            self.generation_id.restore(genid)?;

            Ok(())
        }
    }
}
