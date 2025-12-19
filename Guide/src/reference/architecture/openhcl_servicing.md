# OpenHCL Servicing

## Overview

OpenHCL servicing (also known as VTL2 servicing) is a mechanism that allows the host to update or service the OpenHCL paravisor while minimizing downtime and maintaining device state. During a servicing operation, OpenHCL can save its state, shut down, be updated by the host, and then restore its previous state when restarted.

This capability is particularly important in production environments where maintaining continuous VM operation during paravisor updates is critical.

## Servicing Lifecycle

A servicing operation follows these steps:

### 1. Servicing Request

The host initiates a servicing operation by sending a request to OpenHCL with:
- **Correlation ID**: For tracing and diagnostics
- **Timeout Hint**: Deadline for completing the save operation
- **Capabilities Flags**: Indicates what the host supports

### 2. State Saving

OpenHCL pauses VM execution and saves state from multiple components:

#### State Units
- **VmBus Relay**: VMBus channel state and connections
- **Device Workers**: Individual device driver states
- **Chipset Devices**: Hardware emulation state
- **Firmware State**: UEFI/BIOS runtime state

#### Servicing Init State
- **Firmware Type**: How the VM booted (UEFI/PCAT/None)
- **VM Stop Reference Time**: Hypervisor reference time when state units stopped
- **Emuplat State**: RTC, PCI bridge, and network VF manager state
- **VMGS State**: Virtual machine guest state storage
- **Correlation ID**: For tracing across the servicing operation

#### Device-Specific State
- **NVMe State**: NVMe manager and driver state (when keepalive is enabled)
- **DMA Manager State**: Private and shared pool allocations
- **VMBus Client State**: Client-side VMBus connection state
- **MANA State**: Microsoft Azure Network Adapter state

### 3. Component Shutdown

After state is saved, components are shut down:

#### Without Keepalive
- NVMe devices are cleanly shut down
- Device handles are closed
- DMA allocations are released
- VFIO device handles are dropped (causing device reset)

#### With Keepalive (NVMe)
- NVMe devices remain connected (CC.EN=1)
- VFIO device handles are kept open (preventing reset)
- DMA buffers in the private pool are preserved
- Device maintains its operational state

### 4. Host Servicing

While OpenHCL is stopped:
- The host can update the OpenHCL binary
- The host can update the OpenHCL kernel
- The host can update the IGVM file
- VM guest state in VTL0 continues to be preserved by the host

### 5. OpenHCL Restart

The host restarts OpenHCL with the new version:
- New OpenHCL instance loads
- Saved state is provided as input
- Memory layout and resources are restored

### 6. State Restoration

OpenHCL restores components in order:

1. **DMA Manager Restoration**: Private and shared pools are restored first
2. **NVMe Manager Restoration**: NVMe devices reconnect to their saved state
3. **Device Restoration**: VFIO devices are opened and reconnected
4. **State Unit Restoration**: VM devices and emulation state is restored
5. **VM Resumption**: The guest VM resumes execution

## NVMe Keepalive

NVMe keepalive is a key feature that allows NVMe devices to remain operational during servicing:

### Requirements

NVMe keepalive requires all of the following:

1. **Private Pool Availability**: The DMA manager must have private pool ranges configured
2. **Host Support**: The host must support keepalive operations
3. **Configuration**: `nvme_keep_alive` must be enabled in OpenHCL configuration
4. **Save/Restore Support**: Must be enabled when creating the NVMe manager

### How It Works

When keepalive is enabled:

1. **Persistent DMA Allocations**: NVMe driver uses the private pool for all DMA buffers
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

When keepalive is not enabled or not available:

1. NVMe devices are cleanly shut down
2. VFIO device handles are closed (triggering FLR - Function Level Reset)
3. All device state is lost
4. On restore, devices must be fully reinitialized
5. Guest OS must handle device reappearance and potential I/O errors

## Compatibility and Versioning

### Saved State Format

The servicing state uses Protocol Buffers (`Protobuf`) for serialization:
- Forward and backward compatibility with field numbering
- Schema evolution support
- Cross-version restore capability

### Compatibility Handling

OpenHCL includes logic to handle state from different versions:

#### `fix_pre_save()`
- Updates state before saving to ensure compatibility with older versions
- Handles legacy field conversions
- Maintains VMBus relay compatibility with release branches

#### `fix_post_restore()`
- Updates state after loading from older versions
- Converts legacy formats to current format
- Ensures proper field population for current code

## Configuration

Servicing behavior is controlled by several configuration options:

### NVMe Keepalive Configuration

- `host,privatepool`: Enable keepalive if both host and private pool support it
- `nohost,privatepool`: Private pool available but host keepalive disabled
- `nohost,noprivatepool`: Keepalive fully disabled

### Test Scenarios

For testing servicing behavior:
- `SaveStuck`: Causes save operation to wait indefinitely
- `SaveFail`: Forces save operation to fail
- These help test timeout handling and failure recovery

## Error Handling

Servicing operations include robust error handling:

### Save Failures
- State units that fail to save are logged
- Overall servicing operation may still succeed if critical state is saved
- Non-critical failures are logged but don't block servicing

### Restore Failures
- Critical component failures prevent VM startup
- Error messages indicate which component failed
- Detailed logging helps diagnose issues

### Timeout Handling
- Host provides a deadline for save operation
- OpenHCL attempts to complete save before deadline
- If deadline is exceeded, host may force termination

## Servicing State Components

### State Units

State units represent saveable/restorable VM components:
- Each has a unique name identifier
- Saved state is stored as a blob
- Units can be saved/restored independently

### Correlation ID

The correlation ID:
- Tracks the servicing operation end-to-end
- Used in all logging and tracing during servicing
- Helps correlate events across host and guest
- Persists across the servicing operation

## Implementation Details

### Mesh RPC Architecture

Servicing uses mesh RPC for coordinating async operations:
- Request/response pattern for save operations
- Graceful shutdown coordination
- Timeout and cancellation support

### Memory Management

During servicing:
- Private pool pages remain allocated and mapped
- Shared pool may be repopulated on restore
- VTL permissions are preserved and reapplied
- Physical memory addresses may change but are remapped

### Device Ordering

Components are restored in dependency order:
1. DMA Manager (provides memory for other devices)
2. NVMe Manager (may be needed by storage devices)
3. VMBus Relay (communication infrastructure)
4. Device Workers (individual devices)
5. State Units (guest-visible state)

## Monitoring and Diagnostics

### Tracing

Servicing operations are heavily instrumented:
- `nvme_manager_restore`: NVMe manager restoration
- `shutdown_nvme_manager`: NVMe shutdown coordination
- All operations include correlation ID in spans

### Inspection

Runtime inspection is available via the `inspect` framework:
- Current servicing state
- Save/restore operation status
- Component-specific state details

### Logging

Key events are logged at appropriate levels:
- `info`: Normal servicing milestones
- `warn`: Non-critical failures or fallback behavior
- `error`: Critical failures that impact servicing

## See Also

- [NVMe Storage Backend](../../backends/storage/nvme.md) - How NVMe devices participate in servicing
- [DMA Manager](../../openhcl/dma_manager.md) - Memory pool save/restore
- [OpenHCL Boot Process](./openhcl_boot.md) - Initial startup and initialization
