// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Root-complex-level PCIe error injection (AER and DPC).
//!
//! These helpers model error reporting closer to real hardware: callers target
//! the *device* that generated the error (by its Bus/DevFn), and the handling
//! port is discovered automatically by walking the topology. For AER the
//! handler is the root port that decodes the device's bus; for DPC it is the
//! first DPC-capable port found walking upstream from the device.

use crate::port::PcieDownstreamPort;
use crate::root::GenericPcieRootComplex;
use chipset_device::pci::PciAerErrorKind;
use chipset_device::pci::PciAerInjection;
use chipset_device::pci::PcieDpcRoutingAction;

/// Runtime AER injection request.
///
/// The target device is supplied separately to the injection methods; this
/// payload carries only the error contents.
#[derive(Debug, Clone, Copy)]
pub struct PcieAerInjectRequest {
    /// Error kind.
    pub kind: PciAerErrorKind,
    /// Status bits to OR into the corresponding AER status register.
    pub status_bits: u32,
    /// Header log DWORDs.
    pub header_log: [u32; 4],
}

impl PcieAerInjectRequest {
    /// Build the routed AER injection payload for `target` (Bus<<8 | DevFn).
    fn to_injection(self, target: u16) -> PciAerInjection {
        PciAerInjection {
            kind: self.kind,
            status_bits: self.status_bits,
            header_log: self.header_log,
            source_id: target,
        }
    }
}

impl GenericPcieRootComplex {
    /// Inject an AER event reported by the device at `target` (Bus<<8 | DevFn).
    ///
    /// The device's own AER capability is updated (best effort), then the event
    /// is reported at the root port that decodes the device's bus (the error
    /// handler). When that root port has an AER capability, its registers are
    /// updated and an interrupt is fired if root-error reporting is enabled.
    pub fn inject_aer(&mut self, target: u16, request: PcieAerInjectRequest) -> anyhow::Result<()> {
        let target_bus = (target >> 8) as u8;
        let function = (target & 0xff) as u8;
        let injection = request.to_injection(target);

        let port = self.find_root_port_for_bus(target_bus).ok_or_else(|| {
            anyhow::anyhow!("no root port decodes bus {target_bus:#x} for target {target:#06x}")
        })?;

        // 1. Record the error in the source device's own AER capability.
        let _ = port.inject_child_aer(target_bus, function, injection);

        // 2. Report the error at the root port (the error handler).
        port.report_aer(injection);

        Ok(())
    }

    /// Begin DPC containment (phase 1) for the device at `target`.
    ///
    /// `request` is optional; when supplied, the source device's own AER
    /// capability is updated and the handler port's AER capability is recorded.
    /// DPC is triggered at the first DPC-capable port found walking upstream
    /// from the device (the port closest to it).
    pub fn inject_dpc_begin(
        &mut self,
        target: u16,
        request: Option<PcieAerInjectRequest>,
    ) -> anyhow::Result<()> {
        let target_bus = (target >> 8) as u8;
        let function = (target & 0xff) as u8;
        let aer = request.map(|r| r.to_injection(target));

        let port = self.find_root_port_for_bus(target_bus).ok_or_else(|| {
            anyhow::anyhow!("no root port decodes bus {target_bus:#x} for target {target:#06x}")
        })?;

        // 1. Optionally record the error in the source device's AER capability.
        if let Some(injection) = aer {
            let _ = port.inject_child_aer(target_bus, function, injection);
        }

        // 2. Trigger DPC at the first DPC-capable port upstream of the device.
        let action = PcieDpcRoutingAction::Begin { aer };
        if apply_dpc_upstream(port, target, target_bus, function, action) {
            Ok(())
        } else {
            anyhow::bail!("no DPC-capable port found upstream of target {target:#06x}")
        }
    }

    /// Complete DPC containment (phase 2) for the device at `target` by clearing
    /// RP busy on the handler port.
    pub fn inject_dpc_complete(&mut self, target: u16) -> anyhow::Result<()> {
        let target_bus = (target >> 8) as u8;
        let function = (target & 0xff) as u8;

        let port = self.find_root_port_for_bus(target_bus).ok_or_else(|| {
            anyhow::anyhow!("no root port decodes bus {target_bus:#x} for target {target:#06x}")
        })?;

        let action = PcieDpcRoutingAction::Complete;
        if apply_dpc_upstream(port, target, target_bus, function, action) {
            Ok(())
        } else {
            anyhow::bail!("no DPC-capable port found upstream of target {target:#06x}")
        }
    }
}

/// Apply a DPC action at the first DPC-capable port encountered walking
/// upstream from the device toward `port` (the decoding root port).
fn apply_dpc_upstream(
    port: &mut PcieDownstreamPort,
    target: u16,
    target_bus: u8,
    function: u8,
    action: PcieDpcRoutingAction,
) -> bool {
    let secondary_bus = *port.cfg_space.assigned_bus_range().start();

    // When the device is behind a switch, deeper ports (closer to the device)
    // get the first chance to contain the error.
    if target_bus != secondary_bus && port.route_child_dpc(target_bus, function, action) {
        return true;
    }

    // Otherwise the decoding root port is the handler.
    port.apply_dpc_action(target, action)
}

#[cfg(test)]
mod tests {
    use super::PcieAerInjectRequest;
    use crate::GenericPciePortDefinition;
    use crate::PAGE_SHIFT;
    use crate::PcieAerSettings;
    use crate::PcieDpcSettings;
    use crate::PciePortSettings;
    use crate::port::PcieDownstreamPort;
    use crate::root::GenericPcieRootComplex;
    use crate::root::ecam_size_from_bus_numbers;
    use crate::switch::GenericPcieSwitch;
    use crate::switch::GenericPcieSwitchDefinition;
    use crate::test_helpers::*;
    use chipset_device::mmio::MmioIntercept;
    use chipset_device::pci::PciAerErrorKind;
    use chipset_device::pci::PciAerInjection;
    use chipset_device::pci::PcieDpcRoutingAction;
    use memory_range::MemoryRange;
    use pci_bus::GenericPciBusDevice;
    use pci_core::bus_range::AssignedBusRange;
    use pci_core::capabilities::extended::aer::AerExtendedCapability;
    use pci_core::msi::MsiTarget;
    use pci_core::spec::caps::CapabilityId;
    use pci_core::spec::caps::ExtendedCapabilityId;
    use pci_core::spec::caps::aer::AerExtendedCapabilityHeader;
    use pci_core::spec::caps::aer::CorrectableErrorStatus;
    use pci_core::spec::caps::aer::RootErrorCommand;
    use pci_core::spec::caps::aer::RootErrorStatus;
    use pci_core::spec::caps::aer::UncorrectableErrorStatus;
    use pci_core::spec::caps::dpc::DpcControl;
    use pci_core::spec::caps::dpc::DpcExtendedCapabilityHeader;
    use pci_core::spec::caps::dpc::DpcStatus;
    use pci_core::spec::caps::pci_express::DevicePortType;
    use pci_core::spec::hwid::ClassCode;
    use pci_core::spec::hwid::HardwareIds;
    use pci_core::spec::hwid::ProgrammingInterface;
    use pci_core::spec::hwid::Subclass;
    use std::sync::Arc;
    use std::sync::Mutex;
    use zerocopy::IntoBytes;

    fn bridge_hardware_ids() -> HardwareIds {
        HardwareIds {
            vendor_id: 0x1234,
            device_id: 0x5678,
            revision_id: 0,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::BRIDGE_PCI_TO_PCI,
            base_class: ClassCode::BRIDGE,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        }
    }

    #[test]
    fn test_apply_dpc_action_two_phase_updates_aer_and_rp_busy() {
        let msi_target = MsiTarget::disconnected();
        let mut port = PcieDownstreamPort::new(
            "root",
            bridge_hardware_ids(),
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );

        let injected_unc_status = UncorrectableErrorStatus::new()
            .with_data_link_protocol_error_status(true)
            .into_bits();

        let injection = PciAerInjection {
            kind: PciAerErrorKind::Uncorrectable,
            status_bits: injected_unc_status,
            header_log: [1, 2, 3, 4],
            source_id: 0x0100,
        };
        assert!(port.apply_dpc_action(
            0x0100,
            PcieDpcRoutingAction::Begin {
                aer: Some(injection)
            }
        ));

        let aer_off = find_ext_cap_offset_type1(&port.cfg_space, ExtendedCapabilityId::AER);
        let dpc_off = find_ext_cap_offset_type1(&port.cfg_space, ExtendedCapabilityId::DPC);

        let mut v = 0u32;
        port.cfg_space
            .read_u32(
                aer_off + AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_STATUS.0,
                &mut v,
            )
            .unwrap();
        // The handler port's own Uncorrectable Error Status is not set; only its
        // Root Error Status aggregates the received message.
        assert!(!UncorrectableErrorStatus::from_bits(v).data_link_protocol_error_status());

        port.cfg_space
            .read_u32(
                aer_off + AerExtendedCapabilityHeader::ROOT_ERROR_STATUS.0,
                &mut v,
            )
            .unwrap();
        assert!(RootErrorStatus::from_bits(v).err_fatal_nonfatal_received());

        port.cfg_space
            .read_u32(
                dpc_off + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
                &mut v,
            )
            .unwrap();
        let status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(status.dpc_trigger_status());
        assert!(status.dpc_rp_busy());

        assert!(port.apply_dpc_action(0x0100, PcieDpcRoutingAction::Complete));
        port.cfg_space
            .read_u32(
                dpc_off + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
                &mut v,
            )
            .unwrap();
        let status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(status.dpc_trigger_status());
        assert!(!status.dpc_rp_busy());
    }

    #[test]
    fn test_apply_dpc_action_supported_on_root_and_downstream_only() {
        let msi_target = MsiTarget::disconnected();
        let injection = PciAerInjection {
            kind: PciAerErrorKind::Uncorrectable,
            status_bits: UncorrectableErrorStatus::new()
                .with_data_link_protocol_error_status(true)
                .into_bits(),
            header_log: [0; 4],
            source_id: 0x0100,
        };
        let action = PcieDpcRoutingAction::Begin {
            aer: Some(injection),
        };

        let mut root = PcieDownstreamPort::new(
            "root",
            bridge_hardware_ids(),
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );
        assert!(root.apply_dpc_action(0x0100, action));

        let mut downstream = PcieDownstreamPort::new(
            "dsp",
            bridge_hardware_ids(),
            DevicePortType::DownstreamSwitchPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );
        assert!(downstream.apply_dpc_action(0x0100, action));

        let mut upstream = PcieDownstreamPort::new(
            "usp",
            bridge_hardware_ids(),
            DevicePortType::UpstreamSwitchPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );
        assert!(!upstream.apply_dpc_action(0x0100, action));

        let mut no_dpc = PcieDownstreamPort::new(
            "root-no-dpc",
            bridge_hardware_ids(),
            DevicePortType::RootPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: None,
                ..Default::default()
            },
            None,
            None,
        );
        assert!(!no_dpc.apply_dpc_action(0x0100, action));
    }

    #[test]
    fn test_apply_dpc_action_downstream_port_updates_endpoint_and_clears_busy() {
        let msi_target = MsiTarget::disconnected();

        let endpoint_aer = Arc::new(Mutex::new(AerExtendedCapability::new(
            &DevicePortType::Endpoint,
        )));
        let endpoint = TestAerEndpoint::new(0, endpoint_aer.clone());

        let mut port = PcieDownstreamPort::new(
            "dsp",
            bridge_hardware_ids(),
            DevicePortType::DownstreamSwitchPort,
            false,
            None,
            &msi_target,
            PciePortSettings {
                aer: Some(PcieAerSettings::default()),
                dpc: Some(PcieDpcSettings::default()),
                ..Default::default()
            },
            None,
            None,
        );
        port.add_pcie_device("dsp", "ep0", Box::new(endpoint))
            .unwrap();

        // Enable DPC trigger + interrupt.
        let dpc_off = find_ext_cap_offset_type1(&port.cfg_space, ExtendedCapabilityId::DPC);
        let dpc_control = DpcControl::new()
            .with_dpc_trigger_enable(1)
            .with_dpc_interrupt_enable(true);
        port.cfg_space
            .write_u32(
                dpc_off + DpcExtendedCapabilityHeader::CAPABILITY_CONTROL.0,
                (dpc_control.into_bits() as u32) << 16,
            )
            .unwrap();

        let header_log = [0xdead_0001, 0xbeef_0002, 0xcafe_0003, 0xfeed_0004];
        let injected_unc_status = UncorrectableErrorStatus::new()
            .with_data_link_protocol_error_status(true)
            .into_bits();
        let injection = PciAerInjection {
            kind: PciAerErrorKind::Uncorrectable,
            status_bits: injected_unc_status,
            header_log,
            source_id: 0,
        };
        // Update the source endpoint's AER, then contain at this downstream port.
        assert!(port.inject_child_aer(0, 0, injection).unwrap());
        assert!(port.apply_dpc_action(
            0,
            PcieDpcRoutingAction::Begin {
                aer: Some(injection)
            }
        ));

        let mut v = 0u32;
        port.cfg_space
            .read_u32(
                dpc_off + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
                &mut v,
            )
            .unwrap();
        assert_eq!((v & 0xffff) as u16, 0);
        let status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(status.dpc_trigger_status());
        assert!(status.dpc_rp_busy());

        let endpoint_aer = endpoint_aer.lock().expect("endpoint AER mutex poisoned");
        let endpoint_unc_status = UncorrectableErrorStatus::from_bits(read_aer_dword(
            &endpoint_aer,
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_STATUS,
        ));
        assert!(endpoint_unc_status.data_link_protocol_error_status());
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_0),
            header_log[0]
        );
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_1),
            header_log[1]
        );
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_2),
            header_log[2]
        );
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_3),
            header_log[3]
        );
        drop(endpoint_aer);

        assert!(port.apply_dpc_action(0, PcieDpcRoutingAction::Complete));
        port.cfg_space
            .read_u32(
                dpc_off + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
                &mut v,
            )
            .unwrap();
        let status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(status.dpc_trigger_status());
        assert!(!status.dpc_rp_busy());
    }

    #[test]
    fn test_inject_aer_root_switch_endpoint_propagates_state_and_interrupt() {
        let recorder = Arc::new(RecordingSignalMsi::default());
        let mut register_mmio = TestPcieMmioRegistration {};
        let rc_bus_range = AssignedBusRange::new();
        rc_bus_range.set_bus_range(0, 5);

        let msi_conn = pci_core::msi::MsiConnection::new();
        msi_conn.connect(recorder.clone());

        let mut rc = GenericPcieRootComplex::builder(
            &mut register_mmio,
            0..=5,
            MemoryRange::new(0..ecam_size_from_bus_numbers(0, 5)),
        )
        .root_ports(
            vec![GenericPciePortDefinition {
                name: "root-port".into(),
                devfn: Some(0),
                hotplug: false,
                settings: PciePortSettings {
                    aer: Some(PcieAerSettings::default()),
                    ..Default::default()
                },
            }],
            &msi_conn.msi_target(rc_bus_range, 0),
        )
        .build()
        .unwrap();

        let root_aer_off;
        {
            let root_port = rc.test_root_port_mut("root-port");

            // Root Port decodes bus 1..=5.
            root_port
                .cfg_space
                .write_u32(0x18, (5u32 << 16) | (1u32 << 8))
                .unwrap();

            // Enable MSI on the root port.
            let msi_off = find_cap_offset_type1(&root_port.cfg_space, CapabilityId::MSI.0);
            root_port
                .cfg_space
                .write_u32(msi_off + 0x04, 0xfee0_0000)
                .unwrap();
            root_port.cfg_space.write_u32(msi_off + 0x08, 0).unwrap();
            root_port
                .cfg_space
                .write_u32(msi_off + 0x0c, 0x0040)
                .unwrap();
            root_port.cfg_space.write_u32(msi_off, 1u32 << 16).unwrap();

            // Enable root correctable-error reporting in AER Root Error Command.
            root_aer_off =
                find_ext_cap_offset_type1(&root_port.cfg_space, ExtendedCapabilityId::AER);
            let root_error_command = RootErrorCommand::new()
                .with_correctable_error_reporting_enable(true)
                .into_bits();
            root_port
                .cfg_space
                .write_u32(
                    root_aer_off + AerExtendedCapabilityHeader::ROOT_ERROR_COMMAND.0,
                    root_error_command,
                )
                .unwrap();
        }

        // Build switch and program bus topology:
        // root bus1 -> switch upstream bus2 -> switch downstream bus3 -> endpoint fn0.
        let mut switch = SwitchAdapter(
            GenericPcieSwitch::new(GenericPcieSwitchDefinition {
                name: "sw".into(),
                downstream_ports: vec![GenericPciePortDefinition {
                    name: "sw-downstream-0".into(),
                    devfn: None,
                    hotplug: false,
                    settings: PciePortSettings {
                        aer: Some(PcieAerSettings::default()),
                        ..Default::default()
                    },
                }],
                msi_target: MsiTarget::disconnected(),
            })
            .unwrap(),
        );

        // Program switch upstream (type-1) bus numbers.
        chipset_device::pci::PciConfigSpace::pci_cfg_write(
            &mut switch.0,
            0x18,
            (4u32 << 16) | (2u32 << 8) | 1,
        )
        .unwrap();
        // Program downstream port 0 bus numbers via routed type-1 config write.
        // `secondary_bus` is the parent (root port) secondary bus.
        let _ = GenericPciBusDevice::pci_cfg_write_with_routing(
            &mut switch,
            1,
            2,
            0,
            0x18,
            (3u32 << 16) | (3u32 << 8) | 2,
        )
        .unwrap();

        let endpoint_aer = Arc::new(Mutex::new(AerExtendedCapability::new(
            &DevicePortType::Endpoint,
        )));
        switch
            .0
            .add_pcie_device(
                0,
                "ep0",
                Box::new(TestAerEndpoint::new(0, endpoint_aer.clone())),
            )
            .unwrap();

        rc.add_pcie_device(0, "switch", Box::new(switch)).unwrap();

        let source_id = 3u16 << 8;
        let header_log = [0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444];

        let injected_cor_status = CorrectableErrorStatus::new()
            .with_receiver_error_status(true)
            .into_bits();
        rc.inject_aer(
            source_id,
            PcieAerInjectRequest {
                kind: PciAerErrorKind::Correctable,
                status_bits: injected_cor_status,
                header_log,
            },
        )
        .unwrap();

        // The Root Port aggregates the received error message: its Root Error
        // Status records ERR_COR received and Error Source Identification
        // records the source. Its own Correctable Error Status / Header Log are
        // NOT set — that per-error state lives on the source device.
        let mut v = 0u32;
        let root_port = rc.test_root_port("root-port");

        root_port
            .cfg_space
            .read_u32(
                root_aer_off + AerExtendedCapabilityHeader::CORRECTABLE_ERROR_STATUS.0,
                &mut v,
            )
            .unwrap();
        assert!(!CorrectableErrorStatus::from_bits(v).receiver_error_status());

        root_port
            .cfg_space
            .read_u32(
                root_aer_off + AerExtendedCapabilityHeader::HEADER_LOG_0.0,
                &mut v,
            )
            .unwrap();
        assert_eq!(v, 0);

        root_port
            .cfg_space
            .read_u32(
                root_aer_off + AerExtendedCapabilityHeader::ROOT_ERROR_STATUS.0,
                &mut v,
            )
            .unwrap();
        assert!(RootErrorStatus::from_bits(v).err_cor_received());

        root_port
            .cfg_space
            .read_u32(
                root_aer_off + AerExtendedCapabilityHeader::ERROR_SOURCE_IDENTIFICATION.0,
                &mut v,
            )
            .unwrap();
        assert_eq!((v & 0xffff) as u16, source_id);

        // Endpoint local AER state should be updated with the same payload.
        let endpoint_aer = endpoint_aer.lock().expect("endpoint AER mutex poisoned");
        let endpoint_cor_status = CorrectableErrorStatus::from_bits(read_aer_dword(
            &endpoint_aer,
            AerExtendedCapabilityHeader::CORRECTABLE_ERROR_STATUS,
        ));
        assert!(endpoint_cor_status.receiver_error_status());
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_0),
            header_log[0]
        );
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_1),
            header_log[1]
        );
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_2),
            header_log[2]
        );
        assert_eq!(
            read_aer_dword(&endpoint_aer, AerExtendedCapabilityHeader::HEADER_LOG_3),
            header_log[3]
        );

        // Correctable root reporting was enabled, so an MSI should be signaled.
        let msi = recorder.pop().expect("expected one MSI for root AER event");
        assert_eq!(msi.1, 0xfee0_0000);
        assert_eq!(msi.2 & 0xffff, 0x0040);
    }

    #[test]
    fn test_inject_dpc_root_switch_endpoint_containment_at_root_port() {
        let mut register_mmio = TestPcieMmioRegistration {};
        let rc_bus_range = AssignedBusRange::new();
        rc_bus_range.set_bus_range(0, 5);

        let msi_conn = pci_core::msi::MsiConnection::new();
        let mut rc = GenericPcieRootComplex::builder(
            &mut register_mmio,
            0..=5,
            MemoryRange::new(0..ecam_size_from_bus_numbers(0, 5)),
        )
        .root_ports(
            vec![GenericPciePortDefinition {
                name: "root-port".into(),
                devfn: Some(0),
                hotplug: false,
                settings: PciePortSettings {
                    aer: Some(PcieAerSettings::default()),
                    dpc: Some(PcieDpcSettings::default()),
                    ..Default::default()
                },
            }],
            &msi_conn.msi_target(rc_bus_range, 0),
        )
        .build()
        .unwrap();

        let root_aer_off;
        let root_dpc_off;
        {
            let root_port = rc.test_root_port_mut("root-port");

            // Root Port decodes bus 1..=5.
            root_port
                .cfg_space
                .write_u32(0x18, (5u32 << 16) | (1u32 << 8))
                .unwrap();

            root_aer_off =
                find_ext_cap_offset_type1(&root_port.cfg_space, ExtendedCapabilityId::AER);
            root_dpc_off =
                find_ext_cap_offset_type1(&root_port.cfg_space, ExtendedCapabilityId::DPC);

            // Enable DPC trigger + interrupt.
            let dpc_control = DpcControl::new()
                .with_dpc_trigger_enable(1)
                .with_dpc_interrupt_enable(true);
            root_port
                .cfg_space
                .write_u32(
                    root_dpc_off + DpcExtendedCapabilityHeader::CAPABILITY_CONTROL.0,
                    (dpc_control.into_bits() as u32) << 16,
                )
                .unwrap();
        }

        // root bus1 -> switch upstream bus2 -> switch downstream bus3 -> endpoint fn0.
        let mut switch = SwitchAdapter(
            GenericPcieSwitch::new(GenericPcieSwitchDefinition {
                name: "sw".into(),
                downstream_ports: vec![GenericPciePortDefinition {
                    name: "sw-downstream-0".into(),
                    devfn: None,
                    hotplug: false,
                    settings: PciePortSettings {
                        aer: Some(PcieAerSettings::default()),
                        // No DPC on the switch downstream port, so containment
                        // walks upstream to the root port.
                        dpc: None,
                        ..Default::default()
                    },
                }],
                msi_target: MsiTarget::disconnected(),
            })
            .unwrap(),
        );

        chipset_device::pci::PciConfigSpace::pci_cfg_write(
            &mut switch.0,
            0x18,
            (4u32 << 16) | (2u32 << 8) | 1,
        )
        .unwrap();
        // `secondary_bus` is the parent (root port) secondary bus.
        let _ = GenericPciBusDevice::pci_cfg_write_with_routing(
            &mut switch,
            1,
            2,
            0,
            0x18,
            (3u32 << 16) | (3u32 << 8) | 2,
        )
        .unwrap();

        let endpoint_aer = Arc::new(Mutex::new(AerExtendedCapability::new(
            &DevicePortType::Endpoint,
        )));
        switch
            .0
            .add_pcie_device(
                0,
                "ep0",
                Box::new(TestAerEndpoint::new(0, endpoint_aer.clone())),
            )
            .unwrap();

        rc.add_pcie_device(0, "switch", Box::new(switch)).unwrap();

        let source_id = 3u16 << 8;
        let header_log = [0xaaaa_0001, 0xbbbb_0002, 0xcccc_0003, 0xdddd_0004];

        let injected_unc_status = UncorrectableErrorStatus::new()
            .with_data_link_protocol_error_status(true)
            .into_bits();
        rc.inject_dpc_begin(
            source_id,
            Some(PcieAerInjectRequest {
                kind: PciAerErrorKind::Uncorrectable,
                status_bits: injected_unc_status,
                header_log,
            }),
        )
        .unwrap();

        let mut v = 0u32;
        let root_port = rc.test_root_port("root-port");
        root_port
            .cfg_space
            .read_u32(
                root_aer_off + AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_STATUS.0,
                &mut v,
            )
            .unwrap();
        // The containing Root Port does not set its own Uncorrectable Error
        // Status for a downstream-sourced error; that lives on the endpoint. Its
        // Root Error Status does aggregate the received message.
        assert!(!UncorrectableErrorStatus::from_bits(v).data_link_protocol_error_status());

        root_port
            .cfg_space
            .read_u32(
                root_aer_off + AerExtendedCapabilityHeader::ROOT_ERROR_STATUS.0,
                &mut v,
            )
            .unwrap();
        assert!(RootErrorStatus::from_bits(v).err_fatal_nonfatal_received());

        root_port
            .cfg_space
            .read_u32(
                root_dpc_off + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
                &mut v,
            )
            .unwrap();
        assert_eq!((v & 0xffff) as u16, source_id);
        let dpc_status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(dpc_status.dpc_trigger_status());
        assert!(dpc_status.dpc_rp_busy());

        let endpoint_aer_guard = endpoint_aer.lock().expect("endpoint AER mutex poisoned");
        let endpoint_unc_status = UncorrectableErrorStatus::from_bits(read_aer_dword(
            &endpoint_aer_guard,
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_STATUS,
        ));
        assert!(endpoint_unc_status.data_link_protocol_error_status());
        assert_eq!(
            read_aer_dword(
                &endpoint_aer_guard,
                AerExtendedCapabilityHeader::HEADER_LOG_0
            ),
            header_log[0]
        );
        assert_eq!(
            read_aer_dword(
                &endpoint_aer_guard,
                AerExtendedCapabilityHeader::HEADER_LOG_1
            ),
            header_log[1]
        );
        assert_eq!(
            read_aer_dword(
                &endpoint_aer_guard,
                AerExtendedCapabilityHeader::HEADER_LOG_2
            ),
            header_log[2]
        );
        assert_eq!(
            read_aer_dword(
                &endpoint_aer_guard,
                AerExtendedCapabilityHeader::HEADER_LOG_3
            ),
            header_log[3]
        );
        drop(endpoint_aer_guard);

        rc.inject_dpc_complete(source_id).unwrap();

        let root_port = rc.test_root_port("root-port");
        root_port
            .cfg_space
            .read_u32(
                root_dpc_off + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
                &mut v,
            )
            .unwrap();
        let dpc_status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(dpc_status.dpc_trigger_status());
        assert!(!dpc_status.dpc_rp_busy());
    }

    #[test]
    fn test_inject_dpc_contains_at_closest_dsp() {
        // When a downstream switch port closer to the device also has a DPC
        // capability, containment happens there (not at the root port).
        let mut register_mmio = TestPcieMmioRegistration {};
        let rc_bus_range = AssignedBusRange::new();
        rc_bus_range.set_bus_range(0, 5);

        let msi_conn = pci_core::msi::MsiConnection::new();
        let mut rc = GenericPcieRootComplex::builder(
            &mut register_mmio,
            0..=5,
            MemoryRange::new(0..ecam_size_from_bus_numbers(0, 5)),
        )
        .root_ports(
            vec![GenericPciePortDefinition {
                name: "root-port".into(),
                devfn: Some(0),
                hotplug: false,
                settings: PciePortSettings {
                    aer: Some(PcieAerSettings::default()),
                    dpc: Some(PcieDpcSettings::default()),
                    ..Default::default()
                },
            }],
            &msi_conn.msi_target(rc_bus_range, 0),
        )
        .build()
        .unwrap();

        let root_dpc_off;
        {
            let root_port = rc.test_root_port_mut("root-port");
            root_port
                .cfg_space
                .write_u32(0x18, (5u32 << 16) | (1u32 << 8))
                .unwrap();
            root_dpc_off =
                find_ext_cap_offset_type1(&root_port.cfg_space, ExtendedCapabilityId::DPC);
        }

        // root bus1 -> switch upstream bus2 -> switch downstream bus3 -> endpoint.
        // Both the root port and the downstream switch port expose DPC.
        let mut switch = SwitchAdapter(
            GenericPcieSwitch::new(GenericPcieSwitchDefinition {
                name: "sw".into(),
                downstream_ports: vec![GenericPciePortDefinition {
                    name: "sw-downstream-0".into(),
                    devfn: None,
                    hotplug: false,
                    settings: PciePortSettings {
                        aer: Some(PcieAerSettings::default()),
                        dpc: Some(PcieDpcSettings::default()),
                        ..Default::default()
                    },
                }],
                msi_target: MsiTarget::disconnected(),
            })
            .unwrap(),
        );

        chipset_device::pci::PciConfigSpace::pci_cfg_write(
            &mut switch.0,
            0x18,
            (4u32 << 16) | (2u32 << 8) | 1,
        )
        .unwrap();
        // `secondary_bus` is the parent (root port) secondary bus.
        let _ = GenericPciBusDevice::pci_cfg_write_with_routing(
            &mut switch,
            1,
            2,
            0,
            0x18,
            (3u32 << 16) | (3u32 << 8) | 2,
        )
        .unwrap();

        let endpoint_aer = Arc::new(Mutex::new(AerExtendedCapability::new(
            &DevicePortType::Endpoint,
        )));
        switch
            .0
            .add_pcie_device(
                0,
                "ep0",
                Box::new(TestAerEndpoint::new(0, endpoint_aer.clone())),
            )
            .unwrap();

        rc.add_pcie_device(0, "switch", Box::new(switch)).unwrap();

        let source_id = 3u16 << 8;
        let header_log = [0x0a0a_0001, 0x0b0b_0002, 0x0c0c_0003, 0x0d0d_0004];
        let injected_unc_status = UncorrectableErrorStatus::new()
            .with_data_link_protocol_error_status(true)
            .into_bits();

        rc.inject_dpc_begin(
            source_id,
            Some(PcieAerInjectRequest {
                kind: PciAerErrorKind::Uncorrectable,
                status_bits: injected_unc_status,
                header_log,
            }),
        )
        .unwrap();

        // The downstream switch port (bus 2, dev 0, fn 0) contained the error.
        let dsp_dpc_status_addr = ((2u64 << 8) << PAGE_SHIFT)
            + root_dpc_off as u64
            + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0 as u64;
        let mut v = 0u32;
        rc.mmio_read(dsp_dpc_status_addr, v.as_mut_bytes()).unwrap();
        assert_eq!((v & 0xffff) as u16, source_id);
        let dsp_status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(dsp_status.dpc_trigger_status());
        assert!(dsp_status.dpc_rp_busy());

        // The root port did not contain the error (the closer DSP did).
        let mut rv = 0u32;
        let root_port = rc.test_root_port("root-port");
        root_port
            .cfg_space
            .read_u32(
                root_dpc_off + DpcExtendedCapabilityHeader::STATUS_SOURCE_ID.0,
                &mut rv,
            )
            .unwrap();
        assert!(!DpcStatus::from_bits((rv >> 16) as u16).dpc_trigger_status());

        // The source endpoint's AER state was updated.
        let endpoint_aer_guard = endpoint_aer.lock().expect("endpoint AER mutex poisoned");
        let endpoint_unc_status = UncorrectableErrorStatus::from_bits(read_aer_dword(
            &endpoint_aer_guard,
            AerExtendedCapabilityHeader::UNCORRECTABLE_ERROR_STATUS,
        ));
        assert!(endpoint_unc_status.data_link_protocol_error_status());
        drop(endpoint_aer_guard);

        // Completion clears RP busy on the downstream switch port.
        rc.inject_dpc_complete(source_id).unwrap();
        rc.mmio_read(dsp_dpc_status_addr, v.as_mut_bytes()).unwrap();
        let dsp_status = DpcStatus::from_bits((v >> 16) as u16);
        assert!(dsp_status.dpc_trigger_status());
        assert!(!dsp_status.dpc_rp_busy());
    }
}
