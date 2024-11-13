Unlike in traditional virtualization, where a VMM runs in a privileged host/root
partition and provides virtualization services to a unprivileged guest
partition, the "paravisor" model enables a VMM to provide virtualization
services from _within_ the guest partition itself.

It can be considered a form of "virtual firmware", running at a higher privilege
level than the primary guest OS.

Paravisors are quite exciting, as they enable a wide variety of useful and novel
virtualization scenarios! For example: at Microsoft, OpenHCL plays a key role in
enabling several important Azure scenarios:

- Enabling existing workloads to seamlessly leverage [Azure Boost] (Azure's
  next-generation hardware accelerator), without requiring any modifications to
  the guest VM image.

- Enabling existing guest operating systems to run inside [Confidential VMs].

- Powering [Trusted Launch VMs] - VMs that support Secure Boot, and include a
  vTPM.

[Confidential VMs]:
    https://azure.microsoft.com/en-us/solutions/confidential-compute
[Trusted Launch VMs]:
    https://learn.microsoft.com/en-us/azure/virtual-machines/trusted-launch
[Azure Boost]: https://learn.microsoft.com/en-us/azure/azure-boost/overview
