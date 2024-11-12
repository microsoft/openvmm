// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Power Management Device (as found on the PIIX4 chipset - kinda)
//!
//! # What's with all this PIIX4 stuff?
//!
//! The current implementation of [`PowerManagementDevice`] is based off code in
//! Hyper-V, which happens to emulate the _specific_ PM device as found on the
//! PIIX4 chipset.
//!
//! ...well, kinda.
//!
//! This current implementation is only a _partial_ port of the PM device found
//! on the PIIX4 chipset, with a good chunk of the PIIX4 functionality having
//! been lifted into a wrapper device found under `chipset_legacy/piix4_pm.rs`.
//!
//! # So, what's next?
//!
//! Eventually, this device should be swapped out for a minimal "generic" PM /
//! ACPI device, with all remaining PIIX4 specific functionality being lifted
//! into the legacy piix4_pm device (which may or may not end up reusing the
//! generic PM device as part of its implementation).
//!
//! Of course, there's always the tricky issue that the current implementation
//! _works fine_, so when this work is going to happen... well, your guess is as
//! good as mine.

#![warn(missing_docs)]

use chipset_device::interrupt::LineInterruptTarget;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device::ChipsetDevice;
use inspect::Inspect;
use inspect::InspectMut;
use open_enum::open_enum;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::vmtime::VmTimeAccess;

open_enum! {
    /// Power management I/O offsets from base port address
    pub enum DynReg: u8 {
        #![allow(missing_docs)] // self explanatory constants
        STATUS             = 0x00, // two-byte value
        RESUME_ENABLE      = 0x02, // two-byte value
        CONTROL            = 0x04, // two-byte value
        TIMER              = 0x08, // four-byte value (read only)
        GEN_PURPOSE_STATUS = 0x0C, // two-byte value
        GEN_PURPOSE_ENABLE = 0x0E, // two-byte value
        PROC_CONTROL       = 0x10, // four-byte value
        PROC_L2            = 0x14, // one-byte value
        PROC_L3            = 0x15, // one-byte value
        GLOBAL_STATUS      = 0x18, // two-byte value
        DEVICE_STATUS      = 0x1C, // four-byte value
        GLOBAL_ENABLE      = 0x20, // two-byte value
        GLOBAL_CONTROL     = 0x28, // four-byte value
        DEVICE_CONTROL     = 0x2C, // four-byte value
        GENERAL_INPUT1     = 0x30, // one-byte value (read only)
        GENERAL_INPUT2     = 0x31, // one-byte value (read only)
        GENERAL_INPUT3     = 0x32, // one-byte value (read only)
        RESET              = 0x33, // one-byte value
        GENERAL_OUTPUT0    = 0x34, // one-byte value
        GENERAL_OUTPUT2    = 0x35, // one-byte value
        GENERAL_OUTPUT3    = 0x36, // one-byte value
        GENERAL_OUTPUT4    = 0x37, // one-byte value
    }
}

const CONTROL_SCI_ENABLE_MASK: u16 = 0x0001; // Events should cause SCI =  not SMI
const CONTROL_SUSPEND_ENABLE_MASK: u16 = 0x2000; // Enable the specified suspend type
const CONTROL_SUSPEND_TYPE_MASK: u16 = 0x1C00; // Suspend type field
const ENABLE_TIMER_OVERFLOW_MASK: u16 = 0x0001; // Timer overflow should interrupt
const GLOBAL_CONTROL_BIOS_RLS_MASK: u32 = 0x00000002; // Generate SCI?
const STATUS_DEVICE_MASK: u16 = 0x0010; // One device event flags is set
const STATUS_GP_MASK: u16 = 0x0080; // One of the GP event flags is set
const STATUS_PM_MASK: u16 = 0x0040; // One of the PM event flags is set
const TIMER_OVERFLOW_MASK: u16 = 0x0001; // The PM timer overflowed

/// Value that initiates a system reset when written to [`DynReg::RESET`].
pub const RESET_VALUE: u8 = 0x01; // Reset the VM

#[derive(Clone, Debug, Inspect)]
struct PmState {
    #[inspect(hex)]
    general_purpose_output: u32,

    // Power Management Dynamic I/O state
    #[inspect(hex)]
    status: u16,
    #[inspect(hex)]
    resume_enable: u16,
    #[inspect(hex)]
    control: u16,
    #[inspect(hex)]
    general_purpose_status: u16,
    #[inspect(hex)]
    general_purpose_enable: u16,
    #[inspect(hex)]
    processor_control: u32,
    #[inspect(hex)]
    device_status: u32,
    #[inspect(hex)]
    global_status: u16,
    #[inspect(hex)]
    global_enable: u16,
    #[inspect(hex)]
    global_control: u32,
    #[inspect(hex)]
    device_control: u32,
}

impl PmState {
    fn new() -> Self {
        Self {
            general_purpose_output: 0x7FFFBFFF,
            status: 0,
            resume_enable: 0,
            control: 0,
            general_purpose_status: 0,
            general_purpose_enable: 0,
            processor_control: 0,
            device_status: 0,
            global_status: 0,
            global_enable: 0,
            global_control: 0,
            device_control: 0,
        }
    }

    fn read_dynamic(&mut self, vmtime: &VmTimeAccess, offset: u8) -> u32 {
        match DynReg(offset) {
            // 0x00 - two-byte value
            // Indicate that no events have triggered a sticky flag.
            DynReg::STATUS => self.status.into(),
            // 0x02 - two-byte value
            DynReg::RESUME_ENABLE => (self.resume_enable & 0x0521).into(),
            // 0x04 - two-byte value
            DynReg::CONTROL => self.control.into(),
            // 0x08 - four-byte value (read only)
            // If the pmtimer_assist is set then the hypervisor will intercept
            // accesses to this port and return its own reference time.
            // Hypervisor reference time is different from our reference time,
            // but that's ok because nothing else needs to match. This is faster
            // than us doing this work, but not always available.
            DynReg::TIMER => {
                let now = vmtime.now();
                // Convert the 100ns-period VM time to the 3.579545MHz PM timer time.
                (now.as_100ns() as u128 * 3_579_545 / 10_000_000) as u32
            }
            // 0x0C - two-byte value
            DynReg::GEN_PURPOSE_STATUS => self.general_purpose_status.into(),
            // 0x0E - two-byte value
            DynReg::GEN_PURPOSE_ENABLE => self.general_purpose_enable.into(),
            // 0x10 - four-byte value
            DynReg::PROC_CONTROL => self.processor_control,
            // 0x14 - one-byte value
            DynReg::PROC_L2 => 0,
            // 0x15 - one-byte value
            DynReg::PROC_L3 => 0,
            // 0x18 - two-byte value
            DynReg::GLOBAL_STATUS => {
                let mut value = self.global_status;

                // Incorporate the summary status bits. It doesn't appear that
                // the timer overflow status is paid attention to in this case.
                if (self.status & !TIMER_OVERFLOW_MASK) != 0 {
                    value |= STATUS_PM_MASK;
                }

                if self.general_purpose_status != 0 {
                    value |= STATUS_GP_MASK;
                }

                if self.device_status != 0 {
                    value |= STATUS_DEVICE_MASK;
                }

                value.into()
            }
            // 0x1C - four-byte value
            DynReg::DEVICE_STATUS => self.device_status,
            // 0x20 - two-byte value
            DynReg::GLOBAL_ENABLE => self.global_enable.into(),
            // 0x28 - four-byte value
            DynReg::GLOBAL_CONTROL => self.global_control,
            // 0x2C - four-byte value
            DynReg::DEVICE_CONTROL => self.device_control,
            // 0x30 - one-byte value (read only)
            DynReg::GENERAL_INPUT1 => 0,
            // 0x31 - one-byte value (read only)
            DynReg::GENERAL_INPUT2 => 0,
            // 0x32 - one-byte value (read only)
            DynReg::GENERAL_INPUT3 => 0,
            // 0x34 - one-byte value
            DynReg::GENERAL_OUTPUT0 => self.general_purpose_output,
            // 0x35 - one-byte value
            DynReg::GENERAL_OUTPUT2 => self.general_purpose_output >> 8,
            // 0x36 - one-byte value
            DynReg::GENERAL_OUTPUT3 => self.general_purpose_output >> 16,
            // 0x37 - one-byte value
            DynReg::GENERAL_OUTPUT4 => self.general_purpose_output >> 24,
            _ => {
                tracelimit::warn_ratelimited!(?offset, "unhandled register read");
                !0
            }
        }
    }

    fn write_dynamic(&mut self, action: &mut PowerActionFn, offset: u8, value: u32, mask: u32) {
        match DynReg(offset) {
            // 0x00 - two-byte value
            DynReg::STATUS => self.status &= !value as u16,
            // 0x02 - two-byte value
            DynReg::RESUME_ENABLE => {
                // 0x0521 represents the bits that are not marked as reserved in the PIIX4 manual.
                self.resume_enable &= !mask as u16;
                self.resume_enable |= value as u16 & 0x0521;
            }
            // 0x04 - two-byte value
            DynReg::CONTROL => {
                let value = value as u16;
                if (value & CONTROL_SUSPEND_ENABLE_MASK) != 0 {
                    // Get the suspend type, which is Bits[12:10] of the control register.
                    // Our platform defines a suspend type of 0 as power off(S5) and a suspend
                    // type of 1 as hibernate(S4); no other types are supported.The BIOS/firmware
                    // ACPI tables must reflect these values to the guest.
                    //
                    // Any other values will be ignored.
                    let suspend_type = (value & CONTROL_SUSPEND_TYPE_MASK) >> 10;
                    match suspend_type {
                        0 => (action)(PowerAction::PowerOff),
                        1 => (action)(PowerAction::Hibernate),
                        _ => {}
                    }
                }

                self.control &= !mask as u16;
                self.control |= value;
            }
            DynReg::TIMER => {
                // Ignore writes.
            }
            // 0x0C - two-byte value
            DynReg::GEN_PURPOSE_STATUS => {
                self.general_purpose_status &= !value as u16;
            }
            // 0x0E - two-byte value
            DynReg::GEN_PURPOSE_ENABLE => {
                self.general_purpose_enable &= !mask as u16;
                self.general_purpose_enable |= value as u16 & 0x0f01;
            }
            // 0x10 - four-byte value
            DynReg::PROC_CONTROL => {
                self.processor_control &= !mask;
                self.processor_control |= value & 0x00023E1E;
            }
            // 0x14 - one-byte value
            DynReg::PROC_L2 => {} // Writes to this address do nothing.
            // 0x15 - one-byte value
            DynReg::PROC_L3 => {} // Writes to this address do nothing.
            // 0x18 - two-byte value
            DynReg::GLOBAL_STATUS => {
                // Writes of 1 clear the corresponding status bits. Some of
                // these bits can only be cleared when other registers are
                // cleared (i.e. they are "summary" status bits for other registers.
                self.global_status &= !(value & 0x0D25) as u16;
            }

            // 0x1C - four-byte value
            DynReg::DEVICE_STATUS => self.device_status = !value,
            // 0x20 - two-byte value
            DynReg::GLOBAL_ENABLE => {
                self.global_enable &= !mask as u16;
                self.global_enable |= (value & 0x8D13) as u16;
            }
            // 0x28 - four-byte value
            DynReg::GLOBAL_CONTROL => {
                // We don't support the BIOS release bit.
                let value = value & !GLOBAL_CONTROL_BIOS_RLS_MASK;
                self.global_control &= !mask;
                self.global_control |= value & 0x0701FFE7;
            }
            // 0x2C - four-byte value
            DynReg::DEVICE_CONTROL => {
                self.device_control &= !mask;
                self.device_control |= value;
            }
            // 0x33 - one-byte value
            DynReg::RESET => {
                if value as u8 == RESET_VALUE {
                    (action)(PowerAction::Reboot);
                }
            }
            // 0x34 - one-byte value
            DynReg::GENERAL_OUTPUT0 => {
                let mask = mask & 0xffff;
                self.general_purpose_output &= !mask;
                self.general_purpose_output |= value & mask;
            }
            // 0x35 - one-byte value
            DynReg::GENERAL_OUTPUT2 => {
                self.general_purpose_output &= !0xFF00;
                self.general_purpose_output |= (value << 8) & 0xFF00;
            }
            // 0x36 - one-byte value
            DynReg::GENERAL_OUTPUT3 => {
                let mask = mask & 0xffff;
                self.general_purpose_output &= !(mask << 16);
                self.general_purpose_output |= (value << 16) & mask;
            }
            // 0x37 - one-byte value
            DynReg::GENERAL_OUTPUT4 => {
                self.general_purpose_output &= !0xFF000000;
                self.general_purpose_output |= (value << 24) & 0x7F000000;
            }
            _ => tracelimit::warn_ratelimited!(?offset, ?value, "unhandled register write"),
        }
    }
}

/// Power action being requested
#[allow(missing_docs)] // self explanatory variants
#[derive(Debug, Copy, Clone)]
pub enum PowerAction {
    PowerOff,
    Hibernate,
    Reboot,
}

/// Callback invoked whenever a power action is requested
pub type PowerActionFn = Box<dyn FnMut(PowerAction) + Send + Sync>;

#[derive(Inspect)]
struct PowerManagementDeviceRt {
    /// 0x37-byte IO port corresponding to the dynamic register range
    pio_dynamic: Box<dyn ControlPortIoIntercept>,
    /// ACPI interrupt line
    acpi_interrupt: LineInterrupt,
    /// VM time access for the PM timer.
    vmtime: VmTimeAccess,
    /// Callback invoked whenever a power action is requested
    #[inspect(skip)]
    action: PowerActionFn,
    /// Enable / Disable hypervisor PM timer assist (when available)
    #[inspect(skip)]
    pm_timer_assist: Option<Box<dyn PmTimerAssist>>,
}

/// This is used when running the UEFI BIOS. When passed via
/// `PowerManagementDevice::new`, the device will pre-populate various register
/// values + automatically map the dynamic memory regions at the specified port
/// io address.
///
/// NOTE: at some point, this device should be refactored into a "generic" power
/// management device, which will do away with all the PIIX4 cruft that
/// necessitates this extra "enable ACPI mode" call.
#[derive(Debug, Clone, Copy)]
pub struct EnableAcpiMode {
    /// Default base address for the dynamic register region.
    pub default_pio_dynamic: u16,
}

/// Interface to enable/disable hypervisor PM timer assist.
pub trait PmTimerAssist: Send + Sync {
    /// Sets the port of the PM timer assist.
    fn set(&self, port: Option<u16>);
}

/// A power management + ACPI device.
///
/// See the module level docs for more details.
#[derive(InspectMut)]
pub struct PowerManagementDevice {
    // Static configuration
    #[inspect(skip)]
    enable_acpi_mode: Option<EnableAcpiMode>,

    // Runtime glue
    #[inspect(flatten)]
    rt: PowerManagementDeviceRt,

    // Volatile state
    #[inspect(flatten)]
    state: PmState,
}

impl PowerManagementDevice {
    /// Create a new [`PowerManagementDevice`].
    ///
    /// Most arguments to this constructor are self describing, though there are
    /// some that merit additional explanation:
    ///
    /// - `action`: a callback invoked whenever the PM initiates a power event
    /// - `pio_control` and `pio_status`: define where in the port IO space the
    ///   control/status registers get mapped to.
    /// - `enable_acpi_mode`: see the docs for [`EnableAcpiMode`]
    pub fn new(
        action: PowerActionFn,
        acpi_interrupt: LineInterrupt,
        register_pio: &mut dyn RegisterPortIoIntercept,
        vmtime: VmTimeAccess,
        enable_acpi_mode: Option<EnableAcpiMode>,
        pm_timer_assist: Option<Box<dyn PmTimerAssist>>,
    ) -> Self {
        let pio_dynamic = register_pio.new_io_region("dynamic", 0x37);

        let mut this = PowerManagementDevice {
            enable_acpi_mode,
            rt: PowerManagementDeviceRt {
                pio_dynamic,
                action,
                acpi_interrupt,
                vmtime,
                pm_timer_assist,
            },
            state: PmState::new(),
        };

        // ensure timer assist is disabled
        if let Some(pm_timer_assist) = &this.rt.pm_timer_assist {
            pm_timer_assist.set(None)
        }

        if let Some(acpi_mode) = enable_acpi_mode {
            this.enable_acpi_mode(acpi_mode.default_pio_dynamic)
        }

        this
    }

    fn enable_acpi_mode(&mut self, default_pio_dynamic: u16) {
        tracing::debug!("ACPI mode enabled");
        self.rt.pio_dynamic.map(default_pio_dynamic);
        self.state.control = CONTROL_SCI_ENABLE_MASK;
    }

    /// (used by the PIIX4 wrapper device)
    // DEVNOTE: also used internally, but the PIIX4 wrapper device also uses it
    ///
    /// Evaluates whether the power management (ACPI) interrupt should be
    /// asserted or de-asserted
    ///
    /// If the state is out of sync with what it should be, this function will
    /// either assert or de-assert the interrupt. The logic for whether an ACPI
    /// interrupt should be sent is covered in various parts of Chapter 4 of any
    /// version of the ACPI spec.
    ///
    /// reSearch query: `CheckInterruptAssertion`
    pub fn check_interrupt_assertion(&self) {
        // Check if any power events should cause an interrupt to be asserted.
        let level = (self.state.resume_enable > 0 && self.state.status > 0)
            || (self.state.general_purpose_status > 0 && self.state.general_purpose_enable > 0);

        self.rt.acpi_interrupt.set_level(level)
    }

    /// (used by the PIIX4 wrapper device)
    ///
    /// Remap dynamic registers based on config in the PCI config space
    #[inline(always)]
    pub fn update_dynamic_pio_mappings(&mut self, pio_dynamic_addr: Option<u16>) {
        match pio_dynamic_addr {
            Some(addr) => {
                self.rt.pio_dynamic.map(addr);
                if let Some(assist) = &self.rt.pm_timer_assist {
                    assist.set(Some(addr + DynReg::TIMER.0 as u16))
                }
            }
            None => {
                self.rt.pio_dynamic.unmap();
                if let Some(assist) = &self.rt.pm_timer_assist {
                    assist.set(None);
                }
            }
        }
    }

    /// (used by the PIIX4 wrapper device)
    ///
    /// See calling code in piix4_pm.rs for details on what this does.
    #[inline(always)]
    pub fn pcat_facp_acpi_enable(&mut self, enable: bool) {
        if enable {
            self.state.control |= CONTROL_SCI_ENABLE_MASK;
            self.state.resume_enable |= ENABLE_TIMER_OVERFLOW_MASK;
        } else {
            self.state.control &= !CONTROL_SCI_ENABLE_MASK;
            self.state.resume_enable &= !ENABLE_TIMER_OVERFLOW_MASK;
        }
    }

    /// (used by the PIIX4 wrapper device)
    ///
    /// Get a mutable reference to the provided [`PowerActionFn`]
    pub fn power_action(&mut self) -> &mut PowerActionFn {
        &mut self.rt.action
    }
}

impl ChangeDeviceState for PowerManagementDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.rt.pio_dynamic.unmap();
        self.rt.acpi_interrupt.set_level(false);
        self.state = PmState::new();
        if let Some(acpi_mode) = self.enable_acpi_mode {
            self.enable_acpi_mode(acpi_mode.default_pio_dynamic)
        }
    }
}

impl ChipsetDevice for PowerManagementDevice {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_line_interrupt_target(&mut self) -> Option<&mut dyn LineInterruptTarget> {
        Some(self)
    }
}

fn aligned_offset(offset: u8) -> Option<u8> {
    const TABLE: &[(DynReg, u8)] = &[
        (DynReg::STATUS, 2),             // 0x00 - two-byte value
        (DynReg::RESUME_ENABLE, 2),      // 0x02 - two-byte value
        (DynReg::CONTROL, 2),            // 0x04 - two-byte value
        (DynReg::TIMER, 4),              // 0x08 - four-byte value
        (DynReg::GEN_PURPOSE_STATUS, 2), // 0x0C - two-byte value
        (DynReg::GEN_PURPOSE_ENABLE, 2), // 0x0E - two-byte value
        (DynReg::PROC_CONTROL, 4),       // 0x10 - four-byte value
        (DynReg::PROC_L2, 1),            // 0x14 - one-byte value
        (DynReg::PROC_L3, 1),            // 0x15 - one-byte value
        (DynReg::GLOBAL_STATUS, 2),      // 0x18 - two-byte value
        (DynReg::DEVICE_STATUS, 4),      // 0x1C - four-byte value
        (DynReg::GLOBAL_ENABLE, 2),      // 0x20 - two-byte value
        (DynReg::GLOBAL_CONTROL, 4),     // 0x28 - four-byte value
        (DynReg::DEVICE_CONTROL, 4),     // 0x2C - four-byte value
        (DynReg::GENERAL_INPUT1, 1),     // 0x30 - one-byte value (read only)
        (DynReg::GENERAL_INPUT2, 1),     // 0x31 - one-byte value (read only)
        (DynReg::GENERAL_INPUT3, 1),     // 0x32 - one-byte value (read only)
        (DynReg::GENERAL_OUTPUT0, 1),    // 0x34 - one-byte value
        (DynReg::GENERAL_OUTPUT2, 1),    // 0x35 - one-byte value
        (DynReg::GENERAL_OUTPUT3, 1),    // 0x36 - one-byte value
        (DynReg::GENERAL_OUTPUT4, 1),    // 0x37 - one-byte value
        (DynReg::RESET, 1),              // 0x38 - one-byte value
    ];

    for (start, len) in TABLE.iter().copied() {
        if offset >= start.0 && offset < start.0 + len {
            return Some(start.0);
        }
    }
    None
}

impl PortIoIntercept for PowerManagementDevice {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if let Some(offset) = self.rt.pio_dynamic.offset_of(io_port) {
            let offset = offset as u8;
            let value = if let Some(aligned_offset) = aligned_offset(offset) {
                let value: u64 = self
                    .state
                    .read_dynamic(&self.rt.vmtime, aligned_offset)
                    .into();
                value >> ((offset - aligned_offset) * 8)
            } else {
                tracelimit::warn_ratelimited!(offset, "unknown read from relative offset");
                0
            };

            data.copy_from_slice(&value.to_ne_bytes()[..data.len()]);
            return IoResult::Ok;
        }

        IoResult::Err(IoError::InvalidRegister)
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if let Some(offset) = self.rt.pio_dynamic.offset_of(io_port) {
            let offset = offset as u8;
            let mut value = [0; 8];
            value[..data.len()].copy_from_slice(data);
            let value = u32::from_ne_bytes(value[..4].try_into().unwrap());
            if let Some(aligned_offset) = aligned_offset(offset) {
                let mask = !0u64 >> ((8 - data.len()) * 8) << ((offset - aligned_offset) * 8);
                let value = value << ((offset - aligned_offset) * 8);
                self.state
                    .write_dynamic(&mut self.rt.action, aligned_offset, value, mask as u32)
            } else {
                tracelimit::warn_ratelimited!(offset, value, "unknown write to relative offset");
            }

            self.check_interrupt_assertion();
            return IoResult::Ok;
        }

        IoResult::Err(IoError::InvalidRegister)
    }
}

/// Target for lines corresponding to bits in General Purpose Event Block 0.
///
/// For a full general description of this register, see the ACPI Spec. See
/// section 4.7.1 in the ACPI 2.0 spec.
impl LineInterruptTarget for PowerManagementDevice {
    fn set_irq(&mut self, vector: u32, high: bool) {
        // Latch the bit; it can only be cleared by the guest.
        self.state.general_purpose_status |= (high as u16) << vector;
        self.check_interrupt_assertion();
    }

    fn valid_lines(&self) -> &[std::ops::RangeInclusive<u32>] {
        &[0..=15]
    }
}

mod saved_state {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.pm")]
        pub struct SavedState {
            #[mesh(1)]
            pub general_purpose_output: u32,
            #[mesh(2)]
            pub status: u16,
            #[mesh(3)]
            pub resume_enable: u16,
            #[mesh(4)]
            pub control: u16,
            #[mesh(5)]
            pub general_purpose_status: u16,
            #[mesh(6)]
            pub general_purpose_enable: u16,
            #[mesh(7)]
            pub processor_control: u32,
            #[mesh(8)]
            pub device_status: u32,
            #[mesh(9)]
            pub global_status: u16,
            #[mesh(10)]
            pub global_enable: u16,
            #[mesh(11)]
            pub global_control: u32,
            #[mesh(12)]
            pub device_control: u32,
        }
    }

    impl SaveRestore for PowerManagementDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let PmState {
                general_purpose_output,
                status,
                resume_enable,
                control,
                general_purpose_status,
                general_purpose_enable,
                processor_control,
                device_status,
                global_status,
                global_enable,
                global_control,
                device_control,
            } = self.state;

            let saved_state = state::SavedState {
                general_purpose_output,
                status,
                resume_enable,
                control,
                general_purpose_status,
                general_purpose_enable,
                processor_control,
                device_status,
                global_status,
                global_enable,
                global_control,
                device_control,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                general_purpose_output,
                status,
                resume_enable,
                control,
                general_purpose_status,
                general_purpose_enable,
                processor_control,
                device_status,
                global_status,
                global_enable,
                global_control,
                device_control,
            } = state;

            self.state = PmState {
                general_purpose_output,
                status,
                resume_enable,
                control,
                general_purpose_status,
                general_purpose_enable,
                processor_control,
                device_status,
                global_status,
                global_enable,
                global_control,
                device_control,
            };

            self.check_interrupt_assertion();

            Ok(())
        }
    }
}
