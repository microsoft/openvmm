# DMA Manager

## Overview

The DMA Manager ([`OpenhclDmaManager`](https://openvmm.dev/rustdoc/linux/openhcl_dma_manager/struct.OpenhclDmaManager.html) in the `openhcl_dma_manager` crate) is a critical component in OpenHCL that manages memory pools for Direct Memory Access (DMA) operations used by device drivers. It provides a centralized system for allocating and managing DMA buffers with appropriate memory visibility and VTL permissions.

## Architecture

The DMA Manager maintains multiple types of memory pools (see [`OpenhclDmaManager`](https://openvmm.dev/rustdoc/linux/openhcl_dma_manager/struct.OpenhclDmaManager.html) for complete details):

### Shared Pool

The shared pool contains pages that are:
- Mapped with **shared visibility** on Confidential VMs (CVMs)
- Accessible by the host
- Visible to all VTLs (VTL0 and VTL2)
- Used by devices that need host-visible memory

### Private Pool

The private pool contains pages that are:
- Mapped with **private visibility** on CVMs
- Hidden from the host on hardware-isolated platforms
- Can be made accessible to VTL0 through permission modifications
- Used for **persistent allocations** that survive save/restore operations
- Critical for NVMe keepalive support during servicing

## Key Features

### Memory Allocation

The DMA Manager provides clients with different allocation strategies based on their requirements:

1. **Shared Allocations**: From the shared pool, automatically accessible to all VTLs
2. **Private Persistent Allocations**: From the private pool, survives servicing operations
3. **Private Non-Persistent Allocations (Locked Memory)**: Uses locked memory from normal VTL2 RAM. This memory is locked (pinned) in physical memory to prevent swapping and ensure stable addresses for DMA operations. Locked memory allocations do not persist across servicing operations.
4. **VTL Permission Management**: Automatically adjusts VTL0 permissions when required

### Client Parameters

When creating a DMA client, devices specify:

- **Device Name**: Identifier for the client
- **Lower VTL Policy**: Whether allocations must be accessible to VTL0
- **Allocation Visibility**: Whether to use shared or private memory
- **Persistent Allocations**: Whether allocations should survive save/restore

### Save/Restore Support

The DMA Manager supports save/restore operations, which is essential for servicing:

- **Shared Pool State**: Saved and restored to maintain host-visible allocations
- **Private Pool State**: Saved and restored to maintain persistent device buffers
- **NVMe Keepalive**: Depends on private pool availability to maintain device state

## Integration with Device Drivers

Device drivers use the DMA Manager to allocate memory for device I/O operations. Major users include:

- **NVMe Driver**: Uses private pool for persistent allocations when keepalive is enabled
- **MANA Driver**: Uses DMA allocations for network operations

For implementation details, see the device driver rustdocs and the `DmaClientParameters` API documentation.

## Memory Visibility on CVMs

On Confidential VMs:

- **Shared Memory**: Uses the host-visible memory region (pages marked shared with the host)
- **Private Memory**: Uses guest-private memory (hidden from host on hardware-isolated platforms)
- **VTOM Offset**: The DMA Manager is configured with the Virtual TOM (Top Of Memory) offset bit to distinguish between shared and private memory

## VTL Permission Management

For software-isolated VMs (non-hardware isolated):

- The DMA Manager can modify VTL page permissions via `HvCallModifyVtlProtectionMask`
- Private pool allocations can be made accessible to VTL0 when required
- This is necessary because private pool pages start as VTL2-only accessible

On hardware-isolated VMs:

- VTL permission modification is not available (host is untrusted)
- Only shared pool or locked memory can be used for VTL0-accessible allocations

## Configuration

The DMA Manager is initialized during OpenHCL startup based on configuration determined by the boot shim.

### Private Pool Configuration

The `OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG` parameter controls the VTL2 GPA pool size used for the private pool:

- `debug`: Use debug version of lookup table or device tree
- `off`: Disable the VTL2 GPA pool
- `<num_pages>`: Explicitly specify pool size in pages

The boot shim (see `openhcl_boot`) determines pool sizes using heuristics based on the system configuration (memory size, device requirements, etc.) unless explicitly overridden by this parameter.

### Initialization Parameters

- **Shared Pool Ranges**: Memory ranges from the host for shared visibility
- **Private Pool Ranges**: Memory ranges reserved for private persistent allocations (determined by VTL2 GPA pool config)
- **VTOM Offset**: Bit position for shared/private memory distinction on CVMs
- **Isolation Type**: Whether running on hardware-isolated or software-isolated platform

The availability of the private pool directly impacts:
- NVMe keepalive support (requires private pool)
- Device save/restore capabilities
- Overall servicing functionality

## See Also

- [NVMe Storage Backend](../../backends/storage/nvme.md) - Primary user of the private pool
- [OpenHCL Servicing](../../architecture/openhcl_servicing.md) - How DMA Manager state is preserved during servicing
