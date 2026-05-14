# PCIe Emulation

OpenVMM emulates a PCIe topology that presents root complexes,
root ports, and optional switches to the guest. Endpoint devices
(NVMe, virtio, etc.) attach to root ports via a generic bus
interface.

## Topology

```text
Root Complex (GenericPcieRootComplex)
├── Root Port 0  (PcieDownstreamPort)  → endpoint device
├── Root Port 1  (PcieDownstreamPort)  → endpoint device or switch
└── Root Port N  (PcieDownstreamPort)  → ...
```

The root complex owns the ECAM MMIO region. When the guest reads
or writes a config space address, the root complex decodes the
bus/device/function from the ECAM offset and routes the access
to the correct port. Each port has a Type 1 (bridge)
configuration space with PCIe Express and MSI capabilities.

Ports may optionally be hotplug-capable. Devices behind
non-hotplug ports are attached at VM construction time.
Hotplug-capable ports start empty and devices can be added or
removed at runtime.

## PCIe Hotplug

Native PCIe hotplug follows the same interrupt-driven model as
real hardware (PCIe Base Spec §6.7). No ACPI GPE, SCI, or custom
protocol is needed — the guest's `pciehp` driver handles
everything via config space registers and MSI.

### How it works

1. **VMM sets port state**: When a device is hot-added, the VMM
   atomically updates the port's Slot Status
   (`presence_detect_state`, `presence_detect_changed`,
   `data_link_layer_state_changed`) and Link Status
   (`data_link_layer_link_active`).

2. **MSI fires**: If the guest has enabled
   `hot_plug_interrupt_enable` in the port's Slot Control
   register, the VMM fires the port's MSI.

3. **Guest handles the event**: The guest's `pciehp` driver
   receives the MSI, reads Slot Status to see what changed,
   programs the bridge's bus numbers, scans the secondary bus
   for new devices, and clears the RW1C status bits.

4. **Device removal** follows the same flow in reverse —
   presence and link active are cleared, changed bits are set,
   and MSI fires.

### Runtime API

Hot-add and hot-remove are triggered via
`VmRpc::AddPcieDevice` and `VmRpc::RemovePcieDevice` messages.
These resolve a device resource, create the device with MMIO
registration, attach it to the named port, and fire the hotplug
notification.

See the
[`PetriVmRuntime`](https://openvmm.dev/rustdoc/linux/petri/vm/trait.PetriVmRuntime.html)
trait for the test API (`add_pcie_device` / `remove_pcie_device`)
and the `pcie_hotplug` test in
`vmm_tests/vmm_tests/tests/tests/multiarch/pcie.rs` for a
working example.

### ACPI _OSC

The SSDT includes an `_OSC` method on each PCIe root complex
that grants native PCIe control to the OS (ACPI spec §6.2.11,
PCI Firmware Spec §4.5.1). This tells the OS it can use native
hotplug, PME, AER, and other PCIe features rather than
ACPI-based fallbacks. Linux assumes native control regardless,
but Windows requires `_OSC` to enable native hotplug.

### MSI Interrupt Routing (aarch64)

On aarch64, PCIe MSI/MSI-X interrupts are routed through either
a GICv3 ITS or a GICv2m MSI frame, depending on the platform:

- **GICv3 ITS** (default on KVM with GICv3): The VMM creates a
  KVM in-kernel ITS device. Each PCIe device gets a 32-bit
  device ID composed as `(segment << 16) | BDF`, injected
  transparently by per-device wrappers in the interrupt path.
  ACPI boots emit an IORT with an ITS Group node and per-root-
  complex ID mappings. The device tree includes an `its` child
  node under the GIC with `msi-controller`.

- **GICv2m**: MSI writes map to a fixed pool of 64 SPIs via
  a v2m doorbell register. The MADT includes a GICv2m MSI
  frame entry.

The MSI controller can be overridden with the `--gic-msi`
CLI option (`auto`, `its`, or `v2m`).

### Implementation notes

```admonish note title="No Command Completed support"
Hotplug ports advertise `no_command_completed_support` in Slot
Capabilities. Our emulation applies Slot Control changes
instantly, so the guest does not need to wait for command
completion. This avoids an interrupt storm that would occur if
`command_completed` were set on every Slot Control write — see
the comment in `with_hotplug_support()` in `pci_express.rs`
for details.
```

```admonish note title="Spurious PME interrupts"
The `pcieport` driver's PME service shares the same MSI vector
as `pciehp`. When a hotplug MSI fires, the PME handler also
runs and may log "Spurious native interrupt!" since there is no
PME event. These warnings are cosmetic and do not affect
functionality. A future improvement could use MSI-X (multiple
vectors) to give each service its own interrupt.
```
