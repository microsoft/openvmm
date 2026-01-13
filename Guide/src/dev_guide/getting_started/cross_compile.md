# \[WSL2] Cross Compiling from WSL2 to Windows

Setting up cross compilation is very useful, as it allows using the same repo
cloned in WSL2 to both develop OpenHCL, as well as launch it via OpenVMM via the
WHP backend.

## Required Dependencies

Note that this requires some additional dependencies, described below.

### Windows deps

Visual Studio build tools must be installed, along with the Windows SDK.
[This is the same as what's required to build OpenVMM on windows.](./windows.md#installing-rust)

### WSL deps

The msvc target `x86_64-pc-windows-msvc` must be installed for the toolchain
being used in WSL. This can be added by doing the following:

```bash
rustup target add x86_64-pc-windows-msvc
```

Additional build tools must be installed as well. If your distro has LLVM 14
available (Ubuntu 22.04 or newer):

```bash
sudo apt install clang-tools-14 lld-14 llvm-dev
```

Otherwise, follow the steps at <https://apt.llvm.org/> to install a specific
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
cd path/to/openvmm || exit
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

OpenVMM configures some environment variables that specify the default Linux kernel,
initrd, and UEFI firmware. To make those variables available in Windows, run the following:

```bash
export WSLENV=$WSLENV:X86_64_OPENVMM_LINUX_DIRECT_KERNEL:X86_64_OPENVMM_LINUX_DIRECT_INITRD:AARCH64_OPENVMM_LINUX_DIRECT_KERNEL:AARCH64_OPENVMM_LINUX_DIRECT_INITRD:X86_64_OPENVMM_UEFI_FIRMWARE:AARCH64_OPENVMM_UEFI_FIRMWARE:RUST_BACKTRACE
```

### Speeding up Windows OpenVMM launch

Due to filesystem limitations on WSL, launching OpenVMM directly will be somewhat
slow. Instead, you can copy the built binaries to a location on in the Windows
filesystem and then launch them via WSL.

Quite a few folks working on the OpenVMM project have hacked together personal
helper scripts to automate this process. Here is one example: (update the
variables at the top of the file as necessary.)

```bash
#!/bin/bash

# build & run script for openhcl testing with openvmm

set -e

args="-m 4GB -p 4"
copy_symbols=true
copy_remote=false
windows_temp="/mnt/e/cross"
windows_temp_win="E:\\cross"
remote_temp="\\\\<remote_computer>\\cross"
windows_enlistment="/mnt/e/openvmm"

disk_path="$windows_temp_win\\disk.vhdx"
uefi_firmware="$windows_temp_win\\MSVM.fd"

if [[ $# -lt 2 ]]; then
    echo "Usage: $0 <build|run|ohcldiag-dev> <x64|aarch64>..."
    exit 1
fi

if [[ $2 == "aarch64" ]]; then
    arch="aarch64"
    short_arch="aarch64"
elif [[ $2 == "x64" ]]; then
    arch="x86_64"
    short_arch="x64"
else
    echo "Unknown arch: $2"
    echo "Usage: $0 $1 <x64|aarch64>"
    exit 2
fi

uhdiag_path="$windows_temp_win\\uhdiag"
openvmm_path="$windows_temp/openvmm.exe"
windows_openvmm="$windows_temp/openvmm"
base_igvm="flowey-out/artifacts/build-igvm"
win_target="$arch-pc-windows-msvc"
base_win="target/$win_target/debug"

if [[ $1 == "build" || $1 == "run" ]]; then

    build_args="--target $arch-pc-windows-msvc"

    if [[ $3 == "vmm" ]]; then
        

        if [[ $4 == "uefi" ]]; then
            args+=" --uefi --uefi-firmware $uefi_firmware --disk memdiff:$disk_path"
        elif [[ $4 == "linux" ]]; then
            args+=""
        else
            echo "Unknown load mode: $4"
            echo "Usage: $0 $1 $2 $3 <uefi|linux>"
            exit 2
        fi

    elif [[ $3 == "hcl" ]]; then

        if [[ $4 == "uefi" ]]; then
            recipe="$short_arch"
            args+=" --disk memdiff:$disk_path --gfx --vmbus-com1-serial term --uefi-console-mode com1 --uefi"
        elif [[ $4 == "linux" ]]; then
            recipe="$short_arch-test-linux-direct"
            args+=" --vmbus-com1-serial term --vmbus-com2-serial term"
        else
            echo "Unknown load mode: $4"
            echo "Usage: $0 $1 $2 $3 <uefi|linux>"
            exit 2
        fi

        ohcl_name="openhcl-$recipe.bin"
        ohcl_path="$base_igvm/debug/$recipe"
        ohcl_symbols="openvmm_hcl"

        args+=" --hv --vtl2 --igvm $windows_temp_win\\$ohcl_name --vtl2-vsock-path $uhdiag_path --com3 term"

        echo "Building OpenHCL..."
        (
            set -x
            cargo xflowey build-igvm $recipe
        )

    else
        echo "Unknown package: $2"
        echo "Usage: $0 $1 <vmm|hcl>"
        exit 2
    fi

    echo

    if [[ $5 == "unstable" ]]; then
        build_args+=" --features unstable_whp"
    fi

    echo "Building openvmm..."
    (
        set -x
        cargo build $build_args
    )
    echo

    # Copy to Windows
    echo "Copying to windows"

    if [[ $3 == "hcl" ]]; then
        (
            set -x
            cp -u "$ohcl_path/$ohcl_name" "$windows_temp/$ohcl_name" -f
            mkdir -p "$windows_enlistment/$ohcl_path"
            cp -u "$ohcl_path/$ohcl_name" "$windows_enlistment/$ohcl_path/$ohcl_name" -f
        ) 
        if $copy_remote; then
            (
                set -x
                powershell.exe Copy-Item "$windows_temp_win\\$ohcl_name" "$remote_temp\\$ohcl_name" -Force
            )
        fi  
        if $copy_symbols; then
            (
                set -x
                cp -u "$ohcl_path/$ohcl_symbols" "$windows_temp/openvmm_hcl" -f
                cp -u "$ohcl_path/$ohcl_symbols.dbg" "$windows_temp/openvmm_hcl.dbg" -f
            )
        fi
    fi

    (
        set -x
        cp -u "$base_win/openvmm.exe" $openvmm_path -f
        mkdir -p "$windows_enlistment/$base_win"
        cp -u "$base_win/openvmm.exe" "$windows_enlistment/$base_win/openvmm.exe" -f
    )
    if $copy_remote; then
        (
            set -x
            powershell.exe Copy-Item "$windows_temp_win\\openvmm.exe" "$remote_temp\\openvmm.exe" -Force
        )
    fi
    if $copy_symbols; then
        (
            set -x
            cp -u "$base_win/openvmm.pdb" "$windows_temp/openvmm.pdb" -f
        )
    fi

    echo

    if [[ $1 == "run" ]]; then
        (
            set -x
            $openvmm_path $args
        )
    else
        echo $openvmm_path $args
    fi

elif [[ $1 == "ohcldiag-dev" ]]; then

    shift 2
    (
        set -x
        cargo build --target $arch-pc-windows-msvc -p ohcldiag-dev
        cp -u $base_win/ohcldiag-dev.exe "$windows_temp/ohcldiag-dev.exe" -f
        "$windows_temp/ohcldiag-dev.exe" "$uhdiag_path" "$@"
    )

else

    echo "Unknown command: $1"
    echo "Usage: $0 <build|run|ohcldiag-dev> <x64|aarch64>..."
    exit 1

fi
```
