# Suggested Dev Environment


# [WSL2] Cross Compiling from WSL2 to Windows

Setting up cross compilation is very useful, as it allows using the same repo
cloned in WSL2 to both develop OpenHCL, as well as launch it via OpenVMM via the
WHP backend.

## Required Dependencies

Note that this requires some additional dependencies, described below.

### Windows deps

Visual Studio build tools must be installed, along with the Windows SDK. This is
the same as what's required to build OpenVMM on windows.

### WSL deps

The msvc target `x86_64-pc-windows-msvc` must be installed for the toolchain
being used in WSL. This can be added by doing the following:

```bash
rustup target add x86_64-pc-windows-msvc
```

Note that today this is only supported with the external, public toolchain, not
msrustup.

Additional build tools must be installed as well. If your distro has LLVM 14
available (Ubuntu 22.04 or newer):
```bash
sudo apt install clang-tools-14 lld-14
```

Otherwise, follow the steps at https://apt.llvm.org/ to install a specific
version, by adding the correct apt repos. Note that you must install
`clang-tools-14` as default `clang-14` uses gcc style arguments, where
`clang-cl-14` uses msvc style arguments. You can use their helper script as
well:
```bash
wget https://apt.llvm.org/llvm.sh
chmod +x llvm.sh
sudo ./llvm.sh 14
sudo apt install clang-tools-14
```

## Setting up the terminal environment

Source the `build_support/setup_windows_cross.sh` script from your terminal
instance. For example, the following script will do this along with setting a
default cargo build target:

```bash
#!/bin/bash

# Setup environment and windows cross tooling.

export CARGO_BUILD_TARGET=x86_64-unknown-linux-gnu
cd ~/hvlite || exit
. build_support/setup_windows_cross.sh
exec "$SHELL"
```

For developers using shells other than bash, you may need to run the
`setup_windows_cross.sh` script in bash then launch your shell in order to get
the correct environment variables.

## Editing with vscode

You can have rust-analyzer target Windows, which will allow you to use the same
repo for OpenHCL, Linux, and Windows changes, but the vscode remote server
must be launched from the terminal window that sourced the setup script. You can
do this by closing all vscode windows then opening your workspace with
`code <path to workspace>` in your terminal.

Add the following to your workspace settings for a vscode workspace
dedicated to Windows:

```json
"settings": {
    "rust-analyzer.cargo.target": "x86_64-pc-windows-msvc"
}
```

## Running Windows OpenVMM from within WSL

You can build and run the windows version of OpenVMM by overriding the target
field of cargo commands, via `--target x86_64-pc-windows-msvc`. For example, the
following command will run OpenVMM with WHP:

```bash
cargo run --target x86_64-pc-windows-msvc
```

You can optionally set cargo aliases for this so that you don't have to type out
the full target every time. Add the following to your `~/.cargo/config.toml`:

```toml
[alias]
winbuild = "build --target x86_64-pc-windows-msvc"
wincheck = "check --target x86_64-pc-windows-msvc"
winclippy = "clippy --target x86_64-pc-windows-msvc"
windoc = "doc --target x86_64-pc-windows-msvc"
winrun = "run --target x86_64-pc-windows-msvc"
wintest = "test --target x86_64-pc-windows-msvc"
```

You can then run the windows version of OpenVMM by running:

```bash
cargo winrun
```

### Speeding up Windows OpenVMM launch

Due to filesystem limitations on WSL, launching OpenVMM directly will be somewhat
slow. Instead, you can copy the built binaries to a location on in the Windows
filesystem and then launch them via WSL.

Quite a few folks working on the OpenVMM project have hacked together personal
helper scripts to automate this process.

TODO: include a sample of such a script here
