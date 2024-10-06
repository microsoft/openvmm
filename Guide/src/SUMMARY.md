# Summary

[Introduction](./index.md)

# User Guide

- [OpenVMM](./user_guide/openvmm.md)
  - [Running OpenVMM](./user_guide/openvmm/run.md)
    - [Examples](./user_guide/openvmm/run/examples.md)
    - [CLI](./user_guide/openvmm/run/cli.md)
    - [Interactive Console](./user_guide/openvmm/run/interactive_console.md)
    - [Graphical Console](./user_guide/openvmm/run/graphical_console.md)
    - [Logging](./user_guide/openvmm/run/logging.md)
  - [Troubleshooting](./user_guide/openvmm/troubleshooting.md)
- [OpenHCL](./user_guide/openhcl.md)
  - [Running OpenHCL](./user_guide/openhcl/run.md)
    - [On Windows - Hyper-V](./user_guide/openhcl/run/hyperv.md)
    - [On Windows - OpenVMM](./user_guide/openhcl/run/openvmm.md)
    - [On Linux - OpenVMM]()
  - [Troubleshooting](./user_guide/openhcl/troubleshooting.md)
  - [Diagnostics](./user_guide/openhcl/diag.md)
    - [Preface: CVM restrictions](./user_guide/openhcl/diag/cvm_restrictions.md)
    - [ohcldiag-dev](./user_guide/openhcl/diag/ohcldiag_dev.md)
    - [Network packet capture (PCAP)](./user_guide/openhcl/diag/pcap.md)
    - [Performance analysis](./user_guide/openhcl/diag/perf.md)

# Developer Guide

- [Getting Started](./dev_guide/getting_started.md)
  - [On Linux / WSL2](./dev_guide/getting_started/linux.md)
  - [On Windows](./dev_guide/getting_started/windows.md)
  - [Building OpenVMM](./dev_guide/getting_started/build_openvmm.md)
  - [Building OpenHCL](./dev_guide/getting_started/build_openhcl.md)
  - [Suggested Dev Environment](./dev_guide/getting_started/suggested_dev_env.md)
- [Testing](./dev_guide/tests.md)
  - [Unit Tests](./dev_guide/tests/unit.md)
  - [VMM Tests](./dev_guide/tests/vmm.md)
    - [Azure-hosted Test Images](./dev_guide/tests/vmm/azure_images.md)
  - [Fuzzing](./dev_guide/tests/fuzzing.md)
    - [Running Fuzzers](./dev_guide/tests/fuzzing/running.md)
    - [Writing Fuzzers](./dev_guide/tests/fuzzing/writing.md)
- [Developer Features](./dev_guide/dev_feats.md)
  - [Hardware Debugging (gdbstub)](./dev_guide/dev_feats/gdbstub.md)
  - [Kernel Debugging (KDNET)](./dev_guide/dev_feats/kdnet.md)
- [Developer Tools / Utilities](./dev_guide/dev_tools.md)
  - [`cargo xtask`](./dev_guide/dev_tools/xtask.md)
  - [`cargo xflowey`](./dev_guide/dev_tools/xflowey.md)
  - [VmgsTool](./dev_guide/dev_tools/vmgstool.md)
  - [update-rootfs.py]()
  - [igvmfilegen]()
  - [guest_test_uefi](./dev_guide/dev_tools/guest_test_uefi.md)
- [Contributing](./dev_guide/contrib.md)
  - [Coding Conventions](./dev_guide/contrib/code.md)
  - [Submitting Changes](./dev_guide/contrib/pr.md)
  - [Guide Updates](./dev_guide/contrib/guide.md)

# Developer Reference

- [OpenVMM Architecture]()
- [OpenHCL Architecture]()
- [Devices]()

---

[OpenVMM Crate Docs]()

<!-- # TEMP - pages to delete / fold into other pages

- [Serial Access](./openhcl/dev_features.md)
- [Scripts](./process/scripts.md)
- [Building the OHCL Linux Kernel](./openhcl/kernel.md)
- [Windows - OpenVMM (via WSL2)](./openhcl/cross_compile.md) -->
