# OpenHCL

OpenHCL is an execution environment which runs OpenVMM as a **paravisor**.

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

* * *

To learn more about OpenHCL's architecture, please refer to
[OpenHCL Architecture](../reference/architecture/openhcl.md).

> _Note:_ As you explore the OpenVMM repo, you may find references to the term
> **Underhill**.
>
> Underhill was the former codename for OpenHCL, so whenever you see the term
> "Underhill", you can treat it as synonymous to "OpenHCL".
>
> We are actively migrating existing code and docs away from using the term
> "Underhill".

[VSM]: https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/vsm
[Virtual Trust Levels]: https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/vsm
[Azure Boost]: https://learn.microsoft.com/en-us/azure/azure-boost/overview
[Confidential VMs]: https://azure.microsoft.com/en-us/solutions/confidential-compute
[Trusted Launch VMs]: https://learn.microsoft.com/en-us/azure/virtual-machines/trusted-launch
[TDX]: https://www.intel.com/content/www/us/en/developer/tools/trust-domain-extensions/overview.html
[SEV-SNP]: https://www.amd.com/content/dam/amd/en/documents/epyc-business-docs/white-papers/SEV-SNP-strengthening-vm-isolation-with-integrity-protection-and-more.pdf
