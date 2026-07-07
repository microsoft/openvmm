// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI Express definitions and emulators.

#![forbid(unsafe_code)]

pub mod its;
pub(crate) mod port;
pub use port::GenericPciePortDefinition;
pub use port::PciePortSettings;
pub use port::PortBarDefinition;
pub use port::PortBarSubregionDefinition;
pub use port::PortBarSubregionKind;
pub mod root;
pub mod switch;

#[cfg(any(test, feature = "fuzz"))]
#[expect(missing_docs)]
pub mod test_helpers;

const PAGE_SIZE: usize = 4096;
const PAGE_SIZE64: u64 = 4096;
const PAGE_OFFSET_MASK: u64 = PAGE_SIZE64 - 1;
const PAGE_SHIFT: u32 = PAGE_SIZE.trailing_zeros();

const VENDOR_ID: u16 = pci_core::microsoft::VENDOR_ID;

// Microsoft Device IDs assigned to OpenVMM virtual bridges and switch ports.
const ROOT_PORT_DEVICE_ID: u16 = pci_core::microsoft::DeviceId::PCIE_ROOT_PORT.0;
const UPSTREAM_SWITCH_PORT_DEVICE_ID: u16 =
    pci_core::microsoft::DeviceId::PCIE_UPSTREAM_SWITCH_PORT.0;
const DOWNSTREAM_SWITCH_PORT_DEVICE_ID: u16 =
    pci_core::microsoft::DeviceId::PCIE_DOWNSTREAM_SWITCH_PORT.0;

const MAX_FUNCTIONS_PER_BUS: usize = 256;

const BDF_BUS_SHIFT: u16 = 8;
const BDF_DEVICE_SHIFT: u16 = 3;
const BDF_DEVICE_FUNCTION_MASK: u16 = 0x00FF;

/// Error assigning devfns to a set of PCIe ports on a bus.
#[derive(Debug, thiserror::Error)]
pub enum PortDevfnError {
    /// An explicitly-requested devfn collided with an already-assigned port.
    #[error("port '{name}' devfn {devfn:#x} is already in use")]
    DevfnInUse {
        /// Name of the port whose devfn collided.
        name: std::sync::Arc<str>,
        /// The conflicting devfn value.
        devfn: u8,
    },
    /// No free devfn slot was available for an automatically-placed port.
    #[error("no available devfn slot for port '{name}'")]
    NoFreeDevfn {
        /// Name of the port that could not be placed.
        name: std::sync::Arc<str>,
    },
    /// A device has a port at a non-zero function but no function 0.
    ///
    /// Such a port is undiscoverable: the guest reads the multi-function bit
    /// from function 0's header before probing functions 1-7, so a device with
    /// no function 0 is skipped entirely.
    #[error(
        "device {device} has a port at a non-zero function but no function 0; \
         the device would be undiscoverable"
    )]
    MissingFunctionZero {
        /// The device number (0-31) missing function 0.
        device: u8,
    },
}

/// The placement of a PCIe port on its bus, as computed by
/// [`assign_port_devfns`].
pub(crate) struct PortPlacement {
    /// The assigned devfn (`device << 3 | function`).
    pub devfn: u8,
    /// Whether this port's device hosts more than one function, and thus must
    /// advertise the multi-function header bit. Computed from the final
    /// assignment, so a port that is the sole function of its device is *not*
    /// marked multi-function even when other ports exist on other devices.
    pub multi_function: bool,
}

/// Assigns a devfn to each of `ports`, in order, returning their placements.
///
/// A port with an explicit [`devfn`](GenericPciePortDefinition::devfn) is placed
/// there; a port without one takes the lowest free devfn at or above
/// `first_device`'s devfn. Assignment happens in order, so an explicit devfn
/// that collides with an already-assigned port (including one assigned
/// automatically) is an error.
///
/// Also validates that every device with a port at a non-zero function has a
/// function 0; otherwise the device would be undiscoverable (the guest reads
/// the multi-function bit from function 0 before probing functions 1-7).
pub(crate) fn assign_port_devfns(
    ports: &[GenericPciePortDefinition],
    first_device: u8,
) -> Result<Vec<PortPlacement>, PortDevfnError> {
    let start_devfn = first_device << BDF_DEVICE_SHIFT;
    // 256-bit bitmap of devfns already assigned to a port, indexed so that the
    // device number selects the byte and the function selects the bit.
    let mut used = [0u8; 32];
    let is_used = |used: &[u8; 32], devfn: u8| {
        used[(devfn >> BDF_DEVICE_SHIFT) as usize] & (1 << (devfn & 7)) != 0
    };
    let set_used = |used: &mut [u8; 32], devfn: u8| {
        used[(devfn >> BDF_DEVICE_SHIFT) as usize] |= 1 << (devfn & 7)
    };

    let mut placements = Vec::with_capacity(ports.len());
    for def in ports {
        let devfn = match def.devfn {
            Some(devfn) => {
                if is_used(&used, devfn) {
                    return Err(PortDevfnError::DevfnInUse {
                        name: def.name.clone(),
                        devfn,
                    });
                }
                devfn
            }
            None => (start_devfn..=u8::MAX)
                .find(|d| !is_used(&used, *d))
                .ok_or_else(|| PortDevfnError::NoFreeDevfn {
                    name: def.name.clone(),
                })?,
        };
        set_used(&mut used, devfn);
        placements.push(PortPlacement {
            devfn,
            // Filled in below, once all devfns are known.
            multi_function: false,
        });
    }

    for (device, &functions) in used.iter().enumerate() {
        if functions != 0 && functions & 1 == 0 {
            return Err(PortDevfnError::MissingFunctionZero {
                device: device as u8,
            });
        }
    }

    // The multi-function bit is per-device: a device must advertise it only
    // when it hosts more than one function. The function-0 check above
    // guarantees bit 0 is set for any populated device, so the bitmap byte is
    // exactly 1 iff function 0 is the device's only function.
    for placement in &mut placements {
        let device_functions = used[(placement.devfn >> BDF_DEVICE_SHIFT) as usize];
        placement.multi_function = device_functions > 1;
    }

    Ok(placements)
}
