#!/bin/bash

# See the guide page on more information on required dependencies.

# Validate that a tool is present.
function check_cross_tool {
    if ! command -v "$1" >/dev/null 2>/dev/null; then
        >&2 echo "missing $1 - Try 'sudo apt install clang-tools-14 lld-14' or check the guide."
        false
    fi
}

function fatal_error {
    >&2 echo -e "\033[0;31m$1\033[0m"
    exit 1
}

function tool {
    set -e
    local tooldir="$1"
    local tool="$2"
    [ -h "$tooldir/$tool" ] || fatal_error "$tool is not a symbolic link, check git config core.symlinks"
    echo "$tooldir/$tool"
}

function setup_windows_cross {
    local mydir="$(dirname -- "${BASH_SOURCE[0]}")"
    local myfulldir="$(realpath "$mydir")"

    if [[ -x /bin/wslpath ]] && [[ $(wslpath -aw "$myfulldir") != '\\wsl.localhost\'* ]];
    then
        fatal_error "\033[0;33mWARNING: This script is being run from a Windows partition. This will not work. Please move your repo clone to the WSL filesystem.\033[0m"
    fi

    local tooldir="$(realpath "$myfulldir/windows_cross")"
    export CC_aarch64_pc_windows_msvc=$(tool "$tooldir" aarch64-clang-cl)
    export CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER=$(tool "$tooldir" aarch64-lld-link)
    export AR_aarch64_pc_windows_msvc=$(tool "$tooldir" aarch64-llvm-lib)
    export RC_aarch64_pc_windows_msvc=$(tool "$tooldir" aarch64-llvm-rc)
    export DLLTOOL_aarch64_pc_windows_msvc=$(tool "$tooldir" aarch64-llvm-dlltool)
    export CC_x86_64_pc_windows_msvc=$(tool "$tooldir" x86_64-clang-cl)
    export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER=$(tool "$tooldir" x86_64-lld-link)
    export AR_x86_64_pc_windows_msvc=$(tool "$tooldir" x86_64-llvm-lib)
    export RC_x86_64_pc_windows_msvc=$(tool "$tooldir" x86_64-llvm-rc)
    export DLLTOOL_x86_64_pc_windows_msvc=$(tool "$tooldir" x86_64-llvm-dlltool)
}

# Check if this file was run directly instead of sourced, and fail with a
# warning if so.
(return 0 2>/dev/null) || fatal_error "You must run $0 by sourcing it. Try instead:\n  . $0"

setup_windows_cross
