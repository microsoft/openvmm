#!/bin/bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

set -euo pipefail

package=$(realpath "$1")
workspace=$(realpath -m "$2")
script_dir=$(cd "$(dirname "$0")" && pwd)
revision=78489ebc95ec31f426a44062051cf27bd9d9c7d8
source_dir="$workspace/source"
build_dir="$workspace/build"
output_dir="$workspace/package"

mkdir -p "$source_dir" "$build_dir" "$output_dir"
if [ ! -f "$source_dir/Makefile" ]; then
    curl --fail --location --proto '=https' --tlsv1.2 \
        "https://github.com/microsoft/OHCL-Linux-Kernel/archive/$revision.tar.gz" \
        --output "$workspace/source.tar.gz"
    tar -xzf "$workspace/source.tar.gz" --strip-components=1 -C "$source_dir"
    git -C "$source_dir" apply "$script_dir/startup_trace_kernel.patch"
fi

cp "$package/kernel_config" "$build_dir/.config"
"$source_dir/scripts/config" --file "$build_dir/.config" \
    --enable FTRACE --enable TRACING --enable EVENT_TRACING \
    --enable TRACEPOINTS --enable CONTEXT_SWITCH_TRACER \
    --enable DEBUG_FS --enable IKCONFIG --enable IKCONFIG_PROC

make_args=(
    -C "$source_dir" O="$build_dir" ARCH=arm64
    CROSS_COMPILE=aarch64-linux-gnu-
    CC=aarch64-linux-gnu-gcc-13 HOSTCC=gcc-13
    LOCALVERSION=
    KBUILD_BUILD_USER=builder KBUILD_BUILD_HOST=nixos KBUILD_BUILD_VERSION=1
)
make "${make_args[@]}" olddefconfig
make "${make_args[@]}" -j4 Image modules
make "${make_args[@]}" INSTALL_MOD_PATH="$workspace/staging" \
    INSTALL_MOD_STRIP=1 modules_install
kernel_release=$(make "${make_args[@]}" --no-print-directory -s kernelrelease)

cp "$build_dir/arch/arm64/boot/Image" "$output_dir/Image"
cp "$build_dir/vmlinux.unstripped" "$output_dir/vmlinux.dbg"
aarch64-linux-gnu-strip --strip-all \
    -o "$output_dir/vmlinux" "$build_dir/vmlinux.unstripped"
mkdir -p "$output_dir/modules/kernel"
cp -a "$workspace/staging/lib/modules/$kernel_release/kernel/." "$output_dir/modules/kernel/"
cp "$workspace/staging/lib/modules/$kernel_release"/modules.* "$output_dir/modules/"
cp "$build_dir/.config" "$output_dir/kernel_config"
cp "$package/kernel_build_metadata.json" "$output_dir/kernel_build_metadata.json"
python3 - "$output_dir/kernel_build_metadata.json" <<'PY'
import json
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as source:
    metadata = json.load(source)
assert metadata["git_revision"] == "78489ebc95ec31f426a44062051cf27bd9d9c7d8"
metadata["startup_trace_diagnostic"] = True
with open(path, "w", encoding="utf-8") as output:
    json.dump(metadata, output, indent=2)
    output.write("\n")
PY
