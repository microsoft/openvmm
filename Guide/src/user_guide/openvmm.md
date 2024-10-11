# OpenVMM

OpenVMM can be configured to run as a conventional [hosted, or
"type-2"](https://en.wikipedia.org/wiki/Hypervisor#Classification) Virtual
Machine Monitor (VMM).

At the moment, OpenVMM can be built and run on the following host platforms:

| Host OS | Architecture  | Virtualization API                |
| ------- | ------------- | --------------------------------- |
| Windows | x64 / Aarch64 | WHP (Windows Hypervisor Platform) |
| Linux   | x64           | KVM                               |
|         | x64           | MSHV (Microsoft Hypervisor)       |
| macOS   | Aarch64       | Hypervisor.framework              |

When compiled, OpenVMM consists of a single standalone `openvmm` / `openvmm.exe`
executable.[^note]


> **DISCLAIMER**
>
> In recent years, development efforts in the OpenVMM project have primarily
> focused on [OpenHCL](./openhcl.md) (AKA: OpenVMM as a paravisor).
>
> As a result, not a lot of "polish" has gone into making the experience of
> running OpenVMM in traditional host contexts particularly "pleasant".
>
> This lack of polish manifests in several ways, including but not limited to:
>
> - Unorganized and minimally documented management interfaces (e.g: CLI, ttrpc/grpc)
> - Unoptimized device backend performance (e.g: for storage, networking, graphics)
> - Unexpectedly missing device features (e.g: legacy IDE drive, PS/2 mouse features)
> - **No API or feature-set stability guarantees whatsoever.**
>
> Suffice to say: At this time, OpenVMM _on the host_ is not yet ready to run
> end-user workloads. amd should be treated more akin to a useful development
> platform for implementing new OpenVMM features, rather than a ready-to-deploy
> application.

Assuming you've read the disclaimer above, and are prepared to deal with any
potential "rough edges" you may encounter when using OpenVMM, proceed to
[Running OpenVMM](./openvmm/run.md) to try OpenVMM out for yourself!

[^note]: though, depending on the platform and compiled-in feature-set, some
    additional DLLs and/or system libraries may need to be installed (notably:
    `lxutil.dll` on Windows).
