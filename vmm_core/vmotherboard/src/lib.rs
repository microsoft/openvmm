// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A declarative builder API to init and wire-up virtual devices onto a
//! "virtual motherboard".
//!
//! At a high level: Given a [`BaseChipsetBuilder`] + a list of
//! [`BaseChipsetDevices`](options::BaseChipsetDevices), return [`Chipset`].

#![forbid(unsafe_code)]

mod base_chipset;
mod chipset;

pub use self::base_chipset::BaseChipsetBuilder;
pub use self::base_chipset::BaseChipsetBuilderError;
pub use self::base_chipset::BaseChipsetBuilderOutput;
pub use self::base_chipset::BaseChipsetDeviceInterfaces;
pub use self::base_chipset::options;
pub use self::chipset::Chipset;
pub use self::chipset::ChipsetDevices;
pub use self::chipset::DynamicDeviceUnit;

// API wart: future changes should avoid exposing the `ChipsetBuilder`, and move
// _all_ device instantiation into `vmotherboard` itself.
pub use self::chipset::ChipsetBuilder;
pub use self::chipset::backing::arc_mutex::device::ArcMutexChipsetDeviceBuilder;

use chipset_device::ChipsetDevice;
use inspect::InspectMut;
use mesh::MeshPayload;
use std::marker::PhantomData;
use std::sync::Arc;
use vm_resource::Resource;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::ProtobufSaveRestore;

/// A supertrait of `ChipsetDevice` that requires devices to also support
/// InspectMut and SaveRestore.
///
/// We don't want to put these bounds on `ChipsetDevice` directly, as that would
/// tightly couple `ChipsetDevice` devices with OpenVMM-specific infrastructure,
/// making it difficult to share device implementations across VMMs.
pub trait VmmChipsetDevice:
    ChipsetDevice + InspectMut + ProtobufSaveRestore + ChangeDeviceState
{
}

impl<T> VmmChipsetDevice for T where
    T: ChipsetDevice + InspectMut + ProtobufSaveRestore + ChangeDeviceState
{
}

/// A device-triggered power event.
pub enum PowerEvent {
    /// Initiate Power Off
    PowerOff,
    /// Initiate Reset
    Reset,
    /// Initiate Hibernate
    Hibernate,
}

/// Handler for device-triggered power events.
pub trait PowerEventHandler: Send + Sync {
    /// Called when there is a device-triggered power event.
    fn on_power_event(&self, evt: PowerEvent);
}

/// Handler for device-triggered debug events.
pub trait DebugEventHandler: Send + Sync {
    /// Called when a device has requested a debug break.
    fn on_debug_break(&self, vp: Option<u32>);
}

/// Generic Bus Identifier. Used to describe VM bus topologies.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BusId<T> {
    name: Arc<str>,
    _kind: PhantomData<T>,
}

impl<T> BusId<T> {
    /// Create a new `BusId` with the given `name`.
    pub fn new(name: &str) -> Self {
        BusId {
            name: name.into(),
            _kind: PhantomData,
        }
    }
}

#[doc(hidden)]
pub mod bus_kind {
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum Pci {}
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum PcieEnumerator {}
    #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum PcieDownstreamPort {}
}

/// Type-safe PCI bus ID.
pub type BusIdPci = BusId<bus_kind::Pci>;

/// Type-safe ID for the internal "bus" of a PCIe root
/// complex or switch.
pub type BusIdPcieEnumerator = BusId<bus_kind::PcieEnumerator>;

/// Type-safe ID for a downstream PCIe port.
pub type BusIdPcieDownstreamPort = BusId<bus_kind::PcieDownstreamPort>;

/// A handle to instantiate a chipset device.
#[derive(MeshPayload, Debug)]
pub struct ChipsetDeviceHandle {
    /// The name of the device.
    pub name: String,
    /// The device resource handle.
    pub resource: Resource<ChipsetDeviceHandleKind>,
}
