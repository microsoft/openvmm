# Configuration and Management
As [acknowledged in the disclaimer](../../user_guide/openvmm.md#notable-features) management interfaces in OpenVMM are unstable and incomplete. You can do basic VM operations such as stop, restart, save,  restore, pause, resume, etc.. via the interactive console. You can deploy a VM with various configurations such as number of processors, RAM size, UEFI, graphic console, etc.. and devices such as virtual NIC, vTPM, etc.. via the CLI. gRPC / ttrpc support functionality similar to the interactive console.

## Missing Functionality
  * List all the VMs 
  * Create snapshots
  * Hibernate / resume a VM
