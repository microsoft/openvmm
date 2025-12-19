# NVMe Storage Backend

## Overview

OpenHCL includes a usermode VFIO NVMe driver that enables it to interact with NVMe storage devices assigned to VTL2. This is particularly important in Azure Boost environments where storage is exposed as NVMe devices.

The NVMe driver in OpenHCL provides a safe, Rust-based implementation for managing NVMe storage devices through VFIO (Virtual Function I/O).

## Key Components

### NVMe Driver

The NVMe driver (`nvme_driver` crate) provides the core functionality for interacting with NVMe devices. For detailed implementation information, see the [NVMe Driver API documentation](https://openvmm.dev/rustdoc/linux/nvme_driver/index.html).

### NVMe Manager

The NVMe manager coordinates multiple NVMe devices with a multi-threaded architecture. For implementation details, see the [underhill_core::nvme_manager rustdocs](https://openvmm.dev/rustdoc/linux/underhill_core/nvme_manager/index.html).

## Configuration

NVMe device support is controlled through environment variables:

### `OPENHCL_NVME_KEEP_ALIVE`

Controls whether NVMe devices remain active during servicing operations:

- `host,privatepool`: Enable keepalive when both host support and private pool are available
- `nohost,privatepool`: Private pool available but host keepalive disabled  
- `nohost,noprivatepool`: Keepalive fully disabled

The boot shim infers this configuration based on the detected environment unless explicitly overridden.

### Additional Flags

- **`nvme_vfio`**: Enables VFIO-based NVMe driver support
- **Private Pool**: Must be configured via `OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG` for keepalive support

```admonish note
The boot shim sets the `OPENHCL_NVME_KEEP_ALIVE` configuration and passes it to the kernel, init, and OpenHCL processes based on detected environment capabilities.
```

For more details on private pool configuration, see [DMA Manager](../../openhcl/dma_manager.md).

## NVMe Keepalive During Servicing

NVMe keepalive is a key feature that allows NVMe devices to remain operational during OpenHCL servicing operations, minimizing downtime and avoiding device reinitialization.

### Requirements

NVMe keepalive requires:

1. **Private Pool Availability**: The DMA manager must have private pool ranges configured
2. **Host Support**: The host must support keepalive operations (communicated via capabilities flags)
3. **Configuration**: `OPENHCL_NVME_KEEP_ALIVE` environment variable must be set appropriately

When all requirements are met, NVMe devices use the private pool for DMA allocations that persist across servicing.

### How It Works

When keepalive is enabled:

1. **Persistent DMA Allocations**: NVMe driver uses the private pool for all DMA buffers (when keepalive is enabled; otherwise uses ephemeral allocations)
2. **State Preservation**: 
   - NVMe driver saves queue states, registers, and namespace information
   - DMA manager saves private pool allocation metadata
   - VFIO keeps device handles open
3. **Device Stays Connected**: The NVMe controller remains enabled (CC.EN=1)
4. **Restoration**:
   - Private pool allocations are restored
   - VFIO device is reconnected with persistent DMA clients
   - NVMe driver restores queue state and resumes I/O operations

### Benefits

- **Minimal Downtime**: No device reset or reinitialization required
- **No I/O Interruption**: Pending I/O operations can complete
- **Faster Recovery**: Device is immediately operational after restore
- **Data Integrity**: No loss of in-flight operations

### Without Keepalive

When keepalive is not enabled or not available, OpenHCL (running in VTL2) handles the device shutdown and reinitialization transparently, hiding these details from the VTL0 guest:

1. NVMe devices are cleanly shut down by OpenHCL
2. VFIO device handles are closed (triggering FLR - Function Level Reset)
3. All device state is lost
4. On restore, OpenHCL reinitializes the devices
5. The VTL0 guest continues operation without needing to handle device reappearance

## See Also

- [DMA Manager](../../openhcl/dma_manager.md) - Memory management for device I/O
- [OpenHCL Servicing](../../architecture/openhcl_servicing.md) - How NVMe devices behave during servicing
- [NVMe Driver Rustdoc](https://openvmm.dev/rustdoc/linux/nvme_driver/index.html) - Detailed API documentation
