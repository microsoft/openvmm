# Device Tree

OpenVMM builds a flattened device tree (FDT) for two boot paths:

- **x86 IGVM files** that request a device-tree parameter, such as OpenHCL.
- **ARM64 Linux direct boot** without ACPI.

Both paths use one writer, in
`openvmm/openvmm_core/src/worker/vm_loaders/device_tree.rs`. The processor
architecture selects the processor, interrupt, and PCIe interrupt nodes. The
boot path selects the memory, bus, and boot data nodes. ARM64 Linux
direct boot with ACPI uses a small stub tree instead. The stub tree has no
hardware descriptions. It tells the Linux EFI stub where to find the EFI
system table, and the kernel then finds the hardware through ACPI.

## Contents

| Hardware   | x86 IGVM                    | ARM64 Linux direct           |
| ---------- | --------------------------- | ---------------------------- |
| Processors | `/cpus`, with APIC IDs      | `/cpus`, with VP indexes     |
| Memory     | `memory@...`, with types    | `memory@...`                 |
| Interrupts | None                        | GIC, ITS or v2m, timer, PMU  |
| PCIe       | ECAM node per host bridge   | The same, with MSI and SMMU  |
| VMBus      | VTL0 and VTL2 under `/bus`  | `/openvmm/vmbus`             |
| UARTs      | `/pio-bus/serial@...`       | `/openvmm/uart@...` (PL011)  |
| Boot data  | `/chosen` and `/openhcl`    | `/chosen`, with initrd range |

The `bootargs` property in `/chosen` holds the command line. When a console is
selected, the `stdout-path` property in `/chosen` points to its UART node.

On x86, the memory nodes tell OpenHCL which RAM is VTL2-protectable.

## UARTs

The device tree describes only the UARTs that the VM's chipset capabilities
mark for the device tree. These flags are separate from device attachment and
from console selection.

On x86, the IGVM device tree describes COM1 and COM2 when any serial backend
is configured. It describes COM3 and COM4 only when each port has its own
backend. A listening backend counts as configured before a client connects.
OpenHCL selects COM3 as its console when the device tree describes COM3, so
the device tree does not describe COM3 without its own backend. COM1 defaults
to a console backend; use `--com1 none` to disable it.

These rules do not remove disconnected emulated UARTs. Native x86
Linux-direct ACPI still describes all four ports when serial is enabled.
ARM64 device trees and ACPI still describe both PL011 UARTs.
