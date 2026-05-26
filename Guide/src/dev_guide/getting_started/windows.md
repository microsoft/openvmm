# Getting started on Windows

This page provides instructions for installing the necessary dependencies to
build OpenVMM on Windows.

```admonish tip
We _strongly_ suggest using [WSL2](./linux.md) for OpenVMM development, rather
than developing on Windows directly.

Developing in WSL2 offers a smoother development experience, while still
allowing you to build and run OpenVMM on Windows through the use of
[cross compilation](./cross_compile.md).

Additionally, it allows you to have a single clone of the OpenVMM repo
suitable for both OpenVMM and OpenHCL development.
```

You must be running a recent version of Windows 11. Windows 10 is no longer
supported as a development platform, due to needed WHP APIs.

```admonish note title="Recommended: Windows 11 26H1"
For the best OpenHCL development experience, we recommend running a
**Windows 11 26H1** Insider flight (build 28000+) if it is available for your
device. This enables COM3 serial output for OpenHCL kernel logs and matches
the OS used on the project's CI runners.

See [What to know about Windows 11 version 26H1](https://techcommunity.microsoft.com/blog/windows-itpro-blog/what-to-know-about-windows-11-version-26h1/4491941)
and the [Windows Insider Flight Hub](https://learn.microsoft.com/en-us/windows-insider/flight-hub/)
for availability. If 26H1 is not available for your device, Windows 11
24H2/25H2 works — see [Debugging OpenHCL](../../reference/openhcl/debugging.md#recommended-host-os-for-openhcl-development)
for details on the tradeoffs.
```

**NOTE: OpenHCL does NOT build on Windows.**

If you are interested in building OpenHCL, please follow the getting started
guide for [Linux / WSL2](./linux.md).

## Installing Rust

To build OpenVMM, you first need to install Rust.

The OpenVMM project actively tracks the latest stable release of Rust, though it
may take a week or two after a new stable is released until OpenVMM switches
over to it.

Please follow the [official instructions](https://www.rust-lang.org/tools/install) to do so.

### Visual Studio C++ Build Tools and Windows SDK

If you don't already have it, you will need to install
[Visual Studio C++ Build tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
or [Visual Studio](https://visualstudio.microsoft.com/vs/) with the components
"Desktop Development for C++" and the Windows SDK. Windows 11 SDK version 26100 is the latest
version as of this update, but you should install the newest version if there is a newer
version.

The C++ build tools can be installed via `Visual Studio Installer` -> `Modify` -> `Individual Components`
-> `MSVC v143 - VS 2022 C++ x64/x86 build tools (latest)`.

The Windows SDK can be installed via `Visual Studio Installer` -> `Modify` -> `Individual Components`
-> `Windows 11 SDK (10.0.26100.0)`.

The C++ build tools and Windows SDK can be installed at the same time.

Or, you can install the tool via the powershell command below.

```powershell
PS> winget install Microsoft.VisualStudio.2022.Community --override "--quiet --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --add Microsoft.VisualStudio.Component.Windows11SDK.26100"
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

You are now ready to build [OpenVMM](./build_openvmm.md)!
