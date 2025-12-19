# NVMe Storage Backend

## Overview

OpenHCL includes an NVMe driver that enables it to interact with NVMe storage devices assigned to VTL2. This is particularly important in Azure Boost environments where storage is exposed as NVMe devices.

The NVMe driver in Underhill (OpenHCL's userspace component) provides a safe, Rust-based implementation for managing NVMe storage devices through VFIO (Virtual Function I/O).

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

For more details on private pool configuration, see [DMA Manager](../../openhcl/dma_manager.md).

## See Also

- [DMA Manager](../../openhcl/dma_manager.md) - Memory management for device I/O
- [OpenHCL Servicing](../../architecture/openhcl_servicing.md) - How NVMe devices behave during servicing
- [NVMe Driver Rustdoc](https://openvmm.dev/rustdoc/linux/nvme_driver/index.html) - Detailed API documentation
