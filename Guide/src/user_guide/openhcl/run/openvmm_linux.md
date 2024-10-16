
# Linux - OpenVMM

Currently, OpenHCL cannot be used on Linux hosts, primarily due to limitations
in KVM (or our imagination). We would love to improve this, and we would accept
contributions that get this working.

The main challenge is that OpenHCL needs to run in an environment where it can
trap and emulate privileged instructions from the guest OS. It also benefits
from the host being able to target interrupts directly into the guest OS,
without relaying them through OpenHCL.

On Windows, this is achieved via Hyper-V's VTL support, even when leveraging SNP
or TDX. As of writing this, KVM does not yet support the required primitives for
this.

Here are some approaches we can take to close the gap:

* Use KVM's nested virtualization support. Launch an ordinary VM to run OpenHCL,
  modified to launch the guest OS in a nested VM. This won't be as fast as
  OpenHCL in Hyper-V, but it will allow a simple development environment on
  existing Linux kernels.

* Extend KVM to support Hyper-V-style VTLs, to reach parity with Hyper-V, even
  in non-confidential VMs.

* Extend KVM to fully multiple VMPLs on SNP machines, and update OpenHCL to
  support using architectural GHCB calls to switch VMPLs, rather than
  Hyper-V-specific hypercalls.

* Update OpenHCL to support TDX without Hyper-V-specific hypercalls. Optionally,
  extend KVM to model TDX L2s as VTLs so that the host can target interrupts
  to the guest directly.

If you are interested in helping with any of this, please let us know.
