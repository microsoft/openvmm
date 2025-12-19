# DMA Manager

## Overview

The DMA Manager (`openhcl_dma_manager`) is a critical component in OpenHCL that manages memory pools for Direct Memory Access (DMA) operations used by device drivers. It provides a centralized system for allocating and managing DMA buffers with appropriate memory visibility and VTL permissions.

## Architecture

The DMA Manager maintains two types of memory pools:

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
3. **Private Non-Persistent Allocations**: Using locked memory, doesn't persist across servicing
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

### NVMe Driver

The NVMe driver is a primary user of the DMA Manager's private pool:

1. NVMe drivers request persistent, private allocations with VTL0 permissions
2. The DMA Manager allocates from the private pool and adjusts VTL permissions
3. During servicing, these allocations are preserved, allowing NVMe devices to remain operational
4. After servicing, the restored private pool state reconnects the device to its buffers

### Other Device Drivers

Device drivers use the DMA Manager through the client spawner API:

```rust
let dma_client = dma_manager.client_spawner().create_client(
    DmaClientParameters {
        device_name: "my-device",
        lower_vtl_policy: LowerVtlPermissionPolicy::Vtl0,
        allocation_visibility: AllocationVisibility::Private,
        persistent_allocations: true,
    }
)?;
```

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

The DMA Manager is initialized during OpenHCL startup with:

- **Shared Pool Ranges**: Memory ranges from the host for shared visibility
- **Private Pool Ranges**: Memory ranges reserved for private persistent allocations
- **VTOM Offset**: Bit position for shared/private memory distinction on CVMs
- **Isolation Type**: Whether running on hardware-isolated or software-isolated platform

The availability of the private pool directly impacts:
- NVMe keepalive support (requires private pool)
- Device save/restore capabilities
- Overall servicing functionality

## See Also

- [NVMe Storage Backend](../../backends/storage/nvme.md) - Primary user of the private pool
- [OpenHCL Servicing](../../architecture/openhcl_servicing.md) - How DMA Manager state is preserved during servicing
