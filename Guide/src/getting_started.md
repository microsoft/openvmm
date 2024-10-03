# Getting started on Windows

This page provides instructions for installing the necessary dependencies to
build OpenVMM in Windows.

You must be running a recent version of Windows 11. Windows 10 is no longer
supported as a development platform, due to needed WHP APIs.

**Refer to the instructions for [WSL](./getting_started_wsl.md)
if you need to build OpenHCL.**

It is strongly suggested that you use [WSL2](./getting_started_wsl.md)
for OpenVMM development and [cross compile](./openhcl/cross_compile.md)
for Windows when necessary, as OpenVMM development can be very slow on Windows.
Additionally, it allows you to use one enlistment for both OpenVMM and OpenHCL.

## Installing Rust

To build OpenVMM, you first need Rust. You can follow
[these download instructions](https://www.rust-lang.org/tools/install).

If you don't already have it, you will need to install
[Visual Studio C++ Build tools ](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
or [Visual Studio](https://visualstudio.microsoft.com/vs/) with the component
"Desktop Development for C++".

This can be installed via `Visual Studio Installer` -> `Modify` -> `Individual Components`
-> `MSVC v143 - VS 2022 C++ x64/x86 build tools (latest)`.

Or, you can install the tool via the powershell command below.

```powershell
PS> winget install Microsoft.VisualStudio.2022.Community --override "--quiet --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64"
```

### Aarch64 support

To build ARM64, you need an additional dependency.
This can be installed via `Visual Studio Installer` -> `Modify` -> `Individual Components`
-> `MSVC v143 - VS 2022 C++ ARM64/ARM64EC build tools (latest)`.

Or, you can install the tool via the powershell command below.

```powershell
PS> winget install Microsoft.VisualStudio.2022.Community --override "--quiet --add Microsoft.VisualStudio.Component.VC.Tools.ARM64"
```

## Cloning the OpenVMM source

If you haven't already installed `git`, you can download it
[here](https://git-scm.com/downloads).

```powershell
PS> git clone https://github.com/microsoft/openvmm.git
```

## Next Steps

For those interested in actively iterating on OpenVMM code, you should
[configure your editor / IDE](./ide_setup.md). Either way, you should now be
ready to [build OpenVMM](./openvmm/build.md).

You should now be ready to build [OpenVMM](./openvmm/build.md)!

For those interested in actively iterating on OpenVMM code, you should also
[configure your editor / IDE](./ide_setup.md).
