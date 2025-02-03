#!/bin/bash

# See the guide page on more information on required dependencies.

# Validate that a tool is present.
function check_cross_tool {
    if ! command -v "$1" >/dev/null 2>/dev/null; then
        >&2 echo "missing $1 - Try 'sudo apt install clang-tools-14 lld-14' or check the guide."
        false
    fi
}

# Extract the contents of the Windows INCLUDE and LIB environment variables
# after running vcvarsall.bat.
function extract_include_lib {
    (
        set -e

        arch="$1"
        component="$2"
        arch_param="$3"

        vswhere="$(wslpath 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe')"
        [ -x "$vswhere" ] || {
            >&2 echo "$arch: missing Visual Studio installation"
            exit 1
        }

        vcvarsall="$(</dev/null "$vswhere" -requires "$component" -products '*' -latest -find 'VC\Auxiliary\Build\vcvarsall.bat' -format value)"
        [ -n "$vcvarsall" ] || {
            >&2 echo "warning: $arch: failed to find VC tools"
            exit 1
        }

        vcvarsall_wsl="$(wslpath "$vcvarsall")"
        vcvarsall_path="$(dirname "$vcvarsall_wsl")"
        cd "$vcvarsall_path"

        # Run the script in cmd, then re-invoke WSL with WSLENV to convert the
        # Windows path list variables INCLUDE and LIB to Linux path list
        # variables. Then convert the Linux path separator : back to ; since
        # that's what clang/LLVM expects.
        #
        # shellcheck disable=SC2016
        </dev/null cmd.exe /v:on /c ".\\vcvarsall.bat $arch_param > nul" "&&" \
            set "WSLENV=INCLUDE/l:LIB/l" "&&" \
            wsl -d "$WSL_DISTRO_NAME" echo '$INCLUDE' '^&^&' echo '$LIB' \
        | tr ':' ';'
        [[ ${PIPESTATUS[0]} == 0 ]]
    )
}

function fatal_error {
    >&2 echo -e "\033[0;31m$1\033[0m"
    exit 1
}

function setup_windows_cross {
    local llvm_version=${OPENVMM_LLVM_VERSION:-${HVLITE_LLVM_VERSION:-14}}
    # NOTE: clang-cl-<ver> is the msvc style arguments, which is what we want. This
    #       is sometimes in a different package than the default clang-<ver> which
    #       is gcc style.
    local clang=clang-cl-"$llvm_version"
    local lld=lld-link-"$llvm_version"
    local lib=llvm-lib-"$llvm_version"
    local dlltool=llvm-dlltool-"$llvm_version"
    local rc=llvm-rc-"$llvm_version"

    check_cross_tool "$clang" || return 1
    check_cross_tool "$lld" || return 1
    check_cross_tool "$lib" || return 1
    check_cross_tool "$dlltool" || return 1
    check_cross_tool "$rc" || return 1

    export WINDOWS_CROSS_CL="$clang"
    export WINDOWS_CROSS_LINK="$lld"
    export DLLTOOL="$dlltool"
    local mydir="$(dirname -- "${BASH_SOURCE[0]}")"
    local myfulldir="$(realpath "$mydir")"

    if [[ -x /bin/wslpath ]] && [[ $(wslpath -aw "$myfulldir") != '\\wsl.localhost\'* ]];
    then
        fatal_error "\033[0;33mWARNING: This script is being run from a Windows partition. This will not work. Please move your repo clone to the WSL filesystem.\033[0m"
    fi

    local tooldir="$(realpath "$myfulldir/../build_support/windows_cross")"
    echo $tooldir

    extract_include_lib x86_64 Microsoft.VisualStudio.Component.VC.Tools.x86.x64 x64

    if env=$(extract_include_lib x86_64 Microsoft.VisualStudio.Component.VC.Tools.x86.x64 x64); then
        # Extract the variables, one line each.
        IFS=$'\n' read -rd '' INCLUDE LIB <<< "$env" || true
        export WINDOWS_CROSS_X86_64_LIB="$LIB"
        export WINDOWS_CROSS_X86_64_INCLUDE="$INCLUDE"
        export CC_x86_64_pc_windows_msvc="$tooldir/x86_64-clang-cl"
        export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER="$tooldir/x86_64-lld-link"
        export AR_x86_64_pc_windows_msvc="$lib"
        export RC_x86_64_pc_windows_msvc="$rc"

        printenv
	[ -h $CC_x86_64_pc_windows_msvc ] || fatal_error "$CC_x86_64_pc_windows_msvc is not a symbolic link, check git config core.symlinks:\n"
        [ -h $CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER ] || fatal_error "$CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER is not a symbolic link, check git config core.symlinks:\n"
        echo x86_64
    fi

    # FUTURE: use just "arm64" for the arch when the host arch is arm64.
    if env=$(extract_include_lib aarch64 Microsoft.VisualStudio.Component.VC.Tools.ARM64 x64_arm64); then
        # Extract the variables, one line each.
        IFS=$'\n' read -rd '' INCLUDE LIB <<< "$env" || true
        export WINDOWS_CROSS_AARCH64_LIB="$LIB"
        export WINDOWS_CROSS_AARCH64_INCLUDE="$INCLUDE"
        export CC_aarch64_pc_windows_msvc="$tooldir/aarch64-clang-cl"
        export CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER="$tooldir/aarch64-lld-link"
        export AR_aarch64_pc_windows_msvc="$lib"
        export RC_aarch64_pc_windows_msvc="$rc"
	[ -h $CC_aarch64_pc_windows_msvc ] || fatal_error "$CC_aarch64_pc_windows_msvc is not a symbolic link, check git config core.symlinks:\n"
        [ -h $CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER ] || fatal_error "$CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER is not a symbolic link, check git config core.symlinks:\n"
        echo aarch64
    fi
}

# Check if this file was run directly instead of sourced, and fail with a
# warning if so.
(return 0 2>/dev/null) || fatal_error "You must run $0 by sourcing it. Try instead:\n  . $0"

setup_windows_cross
