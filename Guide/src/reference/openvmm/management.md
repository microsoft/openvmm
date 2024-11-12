# Configuration and Management
As [acknowledged in the disclaimer] management interfaces in OpenVMM are
unstable and incomplete. You can do basic VM operations such as stop, restart,
save, restore, pause, resume, etc.. via the interactive console. You can deploy
a VM with various configurations such as number of processors, RAM size, UEFI,
graphic console, etc.. and devices such as virtual NIC, vTPM, etc.. via the CLI.
gRPC / ttrpc support functionality similar to the interactive console.

## Missing Functionality
* List all the VMs

```admonish note title="Disclaimer"
In the current design, we only support one VM per OpenVMM process. Hence we would
need additional tools to keep track of all the launched VM instances. It is
possible to redesign OpenVMM to manage multiple VMs. It is also possible to
design a separate management tool that keeps track of all the OpenVMM processes
using gRPC / ttrpc interfaces. It is unclear which design we will end up with.
```

* Create snapshots
* Hibernate / Resume a VM

[acknowledged in the disclaimer]: ../../user_guide/openvmm.md#notable-features
