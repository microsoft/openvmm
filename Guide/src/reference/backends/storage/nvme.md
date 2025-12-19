# NVMe Storage Backend

## Overview

OpenHCL includes an NVMe driver that enables it to interact with NVMe storage devices assigned to VTL2. This is particularly important in Azure Boost environments where storage is exposed as NVMe devices.

The NVMe driver in Underhill (OpenHCL's userspace component) provides a safe, Rust-based implementation for managing NVMe storage devices through VFIO (Virtual Function I/O).

## Key Components

### NVMe Driver

The NVMe driver (`nvme_driver` crate) provides the core functionality for interacting with NVMe devices:

- **Device Initialization**: Handles NVMe controller initialization, queue setup, and namespace discovery
- **I/O Operations**: Provides asynchronous read/write operations through submission and completion queues
- **Save/Restore Support**: Enables device state preservation during servicing operations (when supported)

For detailed implementation information, see the [NVMe Driver API documentation](https://openvmm.dev/rustdoc/linux/nvme_driver/index.html).

### NVMe Manager

The NVMe manager (`underhill_core::nvme_manager`) coordinates multiple NVMe devices and provides:

- **Device Registry**: Tracks and manages multiple NVMe devices by PCI ID
- **Multi-threaded Architecture**: Uses mesh RPC for concurrent cross-device operations while serializing per-device requests
- **Namespace Resolution**: Resolves disk configurations to specific NVMe namespaces
- **Lifecycle Management**: Handles device initialization, operation, and shutdown

### DMA Manager Integration

NVMe devices work closely with the [DMA Manager](../../openhcl/dma_manager.md) for memory management:

- **Private Pool**: When available, NVMe devices use the private pool for persistent DMA allocations that survive servicing
- **VTL Permissions**: The DMA manager ensures proper VTL0 access permissions for memory buffers
- **Save/Restore Support**: Persistent allocations enable NVMe keepalive during servicing operations

## Relayed Storage

In OpenHCL, "relayed storage" refers to devices that are:

1. First assigned to VTL2 by the host
2. Then relayed by OpenHCL over VMBus to VTL0

While relayed storage can be any device type, NVMe devices are the most common use case, particularly for Azure Boost storage acceleration. OpenHCL translates VMBus storage requests from VTL0 into NVMe operations, providing compatibility with existing guest OS storage stacks while leveraging hardware-accelerated NVMe storage.

## Configuration

NVMe device support is enabled through OpenHCL configuration:

- **`nvme_vfio`**: Enables VFIO-based NVMe driver support
- **`nvme_keep_alive`**: Controls whether NVMe devices remain active during servicing operations
- **Private Pool**: Must be available for save/restore support

## See Also

- [DMA Manager](../../openhcl/dma_manager.md) - Memory management for device I/O
- [OpenHCL Servicing](../../architecture/openhcl_servicing.md) - How NVMe devices behave during servicing
- [NVMe Driver Rustdoc](https://openvmm.dev/rustdoc/linux/nvme_driver/index.html) - Detailed API documentation
