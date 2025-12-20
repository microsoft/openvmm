# Storage Backends

## Overview

Storage backends in OpenVMM and OpenHCL provide the underlying implementation for storage devices presented to guest VMs. Different backend types serve different use cases and deployment scenarios.

## Relayed Storage

In OpenHCL, "relayed storage" refers to storage devices that are:

1. **First assigned to VTL2 by the host**: The host assigns a physical storage device (or a virtual representation of one) to OpenHCL running in VTL2
2. **Then relayed by OpenHCL over VMBus to VTL0**: OpenHCL exposes the device to the guest OS in VTL0 using VMBus synthetic storage interfaces

This architecture provides several benefits:

- **Compatibility**: Guest OSes continue to use standard VMBus storage drivers without modification
- **Security**: OpenHCL mediates all storage access, maintaining security boundaries between VTLs
- **Flexibility**: The host can assign different types of storage devices transparently

### Common Use Cases

**Azure Boost Storage**: In Azure Boost-enabled systems, storage is exposed as NVMe devices to VTL2. OpenHCL translates VMBus storage requests from VTL0 into NVMe operations, providing compatibility with existing guest OS storage stacks while leveraging hardware-accelerated NVMe storage.

While relayed storage can use any device type that the host can assign to VTL2, NVMe devices are the most common and well-optimized case.

## Storage Backend Types

- **[NVMe](./nvme.md)**: NVMe storage backend using VFIO

## See Also

- [OpenHCL Architecture](../../architecture/openhcl.md) - High-level overview of OpenHCL
- [DMA Manager](../../openhcl/dma_manager.md) - Memory management for device I/O
- [OpenHCL Servicing](../../architecture/openhcl_servicing.md) - How storage devices behave during servicing
