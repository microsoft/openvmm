# Getting started on WSL

This page provides instructions for installing the necessary dependencies to
build OpenVMM or OpenHCL in WSL2.

You must be running a recent version of Windows 11. Windows 10 is no longer
supported as a development platform, due to needed WHP APIs.

## [Host] Installing WSL

To install Windows Subsystem for Linux, run the following command in an
elevated Powershell window:

```powershell
PS> wsl --install
```

This should install WSL2 using the default Ubuntu linux distribution.
You can check that the installation completed successfully by running the
following command in a Powershell window.
```powershell
PS> wsl -l -v
  NAME            STATE           VERSION
* Ubuntu          Running         2
```
Once that command has completed, you will need to open WSL to complete the
installation and set your password. You can open WSL by typing `wsl` or `bash`
into Command Prompt or Powershell, or by opening the "Ubuntu" Windows Terminal
profile that should have been created.

## [WSL] Installing Rust

To build OpenVMM or OpenHCL, you first need Rust. You can follow
[these download instructions](https://www.rust-lang.org/tools/install).
Run the following command from that page inside WSL.

```bash
WSL> curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## [WSL] Cloning the OpenVMM source

**NOTE: If you are developing in OpenHCL or using OpenVMM in WSL, clone the**
**repo into WSL's filesystem. Do not clone the repo into Windows then try to**
**access said clone from Linux. This will cause issues.**

```bash
WSL> git clone https://github.com/microsoft/openvmm.git
```

## [WSL] Other dependencies

On Linux, there are various other dependencies you will need depending on what
you're working on. On Debian-based distros such as Ubuntu, running the following
command within WSL will install these dependencies.

```bash
WSL> sudo apt install \
  binutils              \
  build-essential       \
  gcc-aarch64-linux-gnu \
  libssl-dev
```

## Next Steps

You should now be ready to build [OpenVMM](./openvmm/build.md) or
[OpenHCL](./openhcl/build.md)!

For those interested in actively iterating on OpenVMM/OpenHCL code, you should
also [configure your editor / IDE](./ide_setup.md).
