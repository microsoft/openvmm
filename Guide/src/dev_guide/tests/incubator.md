# Incubator

Incubator runs commands and interactive shells in disposable Linux VMs using
Petri, keeping builds, caches, and logs on the host.

## Launcher and boot selection

`[incubator]` selects the launcher and VM resources; `[boot]` selects the guest
environment and defaults to `linux-direct`. These profiles run on x64 Linux
hosts; OpenVMM requires KVM.

| Launcher | Boot environment | Current support |
|----------|------------------|-----------------|
| `qemu-tcg` | `linux-direct` | AArch64 guest on x64 Linux |
| `openvmm` | `linux-direct` | x64 Linux guest on x64 Linux |
| `openvmm` | `mshv` | x64 mshv root partition on x64 Linux |
| `qemu-tcg` | `mshv` | Not implemented |

Registered VMM tests use [Petri nested tests](vmm.md#nested-tests), not an
Incubator Flowey target runner. Each Incubator invocation cold-boots a VM.

## QEMU commands

```bash
cargo run -p incubator -- \
  --profile petri/incubator/profiles/aarch64-tcg-pcie.toml \
  --kernel path/to/Image --initrd path/to/initrd \
  --share path/to/shared-root \
  --guest-pipette /share/pipette \
  --map-command-path path/to/shared-root/test-binary
```

The kernel and static Pipette binary must match the guest architecture. Place
Pipette and the command binary beneath the shared directory.
Profile devices are provisioned for VFIO before the command runs.

## OpenVMM commands

Build the native launcher and Incubator from the same checkout for protocol
compatibility, and build a static guest agent:

```bash
cargo build -p openvmm -p incubator
cargo build -p pipette --target x86_64-unknown-linux-musl
```

Copy `petri/incubator/profiles/x86_64-mshv.toml` outside the checkout and set
absolute paths for `binary`, `firmware`, and `boot-artifacts`. This environment
requires `nested-virt = true`, a compatible mshv root kernel, and the
[boot artifacts below](#mshv-boot-artifacts-and-kernel).

```bash
target/debug/incubator \
  --profile path/to/mshv-profile.toml \
  --kernel path/to/mshv/bzImage \
  --initrd path/to/test/initrd \
  --share "$PWD" \
  --guest-pipette /share/target/x86_64-unknown-linux-musl/debug/pipette \
  --output-dir target/mshv-logs \
  --no-pty /bin/sh -c 'test -c /dev/mshv && uname -a'
```

Omit `--no-pty` and run `/bin/sh` for an interactive session. Pipette must be
inside the shared directory. Keep the mshv root kernel separate from any
child VM's kernel, and the native outer OpenVMM separate from guest binaries.

Incubator supplies Petri with a `UefiGuest::Mshv` boot recipe: resolved kernel
and payload paths plus an owned, prepared initrd. Petri composes and owns the
temporary boot disk during VM preparation; the separate mshv platform preset
configures nested virtualization and the required devices.

For OpenVMM **without mshv**, use
`petri/incubator/profiles/x86_64-openvmm.toml` with a native executable path,
an ordinary x64 Linux kernel, and the test initrd. No firmware or hypervisor
payloads are needed. Set `nested-virt = true` to create child KVM VMs, and replace
the example's `/dev/mshv` check with your command.

## mshv boot artifacts and kernel

Incubator does not download payloads. Keep authorized binaries, their packaged
licenses, and generated images outside Git; public availability does not change
licensing terms.

The artifact directory must contain:

```text
boot/
  EFI/BOOT/bootx64.efi    shim
  EFI/BOOT/grubx64.efi    GRUB
  HvLoader.efi
  lxhvloader.dll
  Windows/System32/...   complete matching runtime
```

Both `hvix64.exe` and `hvax64.exe` must be present. Use x64 shim configured to
load the adjacent `grubx64.efi`, and a prebuilt GRUB with embedded FAT/GPT,
Linux, chain, normal, and boot support and prefix `/boot/grub2`. Retain shim:
HvLoader requires its verification protocol even with Secure Boot disabled.

The validated Azure Linux 3 recipe uses shim 15.8-6, GRUB 2.06-27,
edk2-hvloader 20240524git3e722403cd16-20, and matching `mshv` /
`mshv-bootloader-lx` 26100.9444.2609032029.1-1 payloads. The composer does not
verify provenance, signatures, architecture, or compatibility.

The root kernel needs the mshv EFI-launch patches, `CONFIG_EFI_STUB=y`,
`CONFIG_MSHV_ROOT=y`, and built-in PCIe, virtio-blk, virtio-net, virtio-9p,
devtmpfs, and initramfs support. Disable modules and reuse the OpenVMM test
initrd. A stock upstream test kernel cannot launch this environment.

The public `rolling-lts/kata/6.18.34.mshv2` kernel needs corrections for root
IRQ mapping lifecycle and the build-26100 two-bank `CreatePartition` ABI.
See the development
[IRQ fix](https://github.com/jstarks/OHCL-Linux-Kernel/commit/4178badb4e34) and
[ABI fix](https://github.com/jstarks/OHCL-Linux-Kernel/commit/88ec03f5e6fc);
these do not guarantee compatibility with other kernel or hypervisor versions.

To compose an image without launching it:

```bash
cargo run -p incubator --bin incubator-mshv-image -- \
  --shim path/to/artifacts/boot/EFI/BOOT/bootx64.efi \
  --grub path/to/artifacts/boot/EFI/BOOT/grubx64.efi \
  --payload path/to/artifacts/boot \
  --kernel path/to/bzImage --initrd path/to/initrd \
  --kernel-cmdline 'console=ttyS0 rdinit=/init' \
  --output path/to/new-mshv.img
```

The output parent must exist; existing output is never overwritten. Payload
files must be regular files with ASCII FAT-compatible names. Kernel arguments
cannot contain quotes, control characters, or space-containing option values.

## Sharing and guest trust

```admonish warning
The host share is writable. Incubator is a trusted development environment,
not a sandbox for untrusted guest programs.
```

All environments copy the host CA bundle into the temporary initrd and export
`SSL_CERT_FILE=/incubator-ca-certificates.crt` for guest TLS clients. A nonempty
host `SSL_CERT_FILE` selects the bundle; otherwise a conventional Linux system
bundle is required. An invalid or unreadable bundle fails the run. The guest
inherits that trust, including enterprise CAs, without copying private keys or
disabling verification. Its clock is initialized from the host.

## Output and failures

Each invocation creates an `incubator-*` directory beneath `--output-dir`, with
serial logs (`uefi.log` or `guest.log`), VMM logs (`openvmm.log` or `qemu.log`),
and `petri.jsonl`. Guest stdout/stderr remain separate from runner diagnostics.
Use `--guest-env` or `INCUBATOR_ENV` for guest environment overrides.

`--timeout` / `INCUBATOR_TIMEOUT` bounds boot for Linux-direct and boot plus
command execution for mshv. Graceful shutdown and teardown each have a
ten-second deadline. OpenVMM cleanup is best-effort: startup cancellation or
an unresponsive VMM can leave a host process requiring manual termination.
Guest exit codes propagate; unexpected VM exit/reset and timeouts fail.
SIGINT/SIGTERM attempt cleanup; SIGKILL cannot. Temporary boot files are removed
on normal teardown and handled errors; logs remain on the host.
