# Running OpenVMM from WSL2

WSL2 runs inside a Hyper-V VM, so when you run OpenVMM from WSL you have two
options with very different behavior: cross-compile to Windows, or build natively
for Linux.

## Option A: Cross-compile to Windows (recommended)

Cross-compiling produces a Windows `.exe` that runs natively on the host, outside
the WSL2 VM. This is the recommended approach for most development.

```bash
cargo run --target x86_64-pc-windows-msvc
```

- Uses **WHP** (Windows Hypervisor Platform) — runs directly on the host
  hypervisor with no nesting overhead.
- **Supports OpenHCL / VTL2** — the full Hyper-V paravisor stack works because
  WHP provides VTL support.
- Best performance — your guest VM runs at native speed on the host.

**Setup:**

```bash
rustup target add x86_64-pc-windows-msvc
```

You also need Visual Studio Build Tools installed on the Windows side. See the
[Cross Compiling](./cross_compile.md) page for full setup instructions.

Because the resulting binary is a Windows process, you must translate WSL paths
to Windows paths with `wslpath -w`. See
[Running Windows binaries from WSL2](./cross_compile.md#running-windows-binaries-from-wsl2)
for details.

## Option B: Build natively for Linux

Building with the default Linux target produces a Linux binary that runs inside
the WSL2 VM.

```bash
cargo run
```

- Uses **KVM** as the hypervisor backend. (MSHV — the Microsoft Hypervisor for
  Linux — is tried first but requires a specialized kernel module that is not
  publicly available. In WSL2, KVM is what you'll get.)
- Does **not** support OpenHCL. OpenHCL requires Hyper-V VTL support, which KVM
  does not provide.
- Performance suffers from nested virtualization — your guest VM runs inside
  WSL2's Hyper-V VM, adding overhead.
- Fine for testing OpenVMM without VTL2, for example booting a simple guest VM
  with `--uefi`.

```admonish note
KVM requires access to `/dev/kvm`. Add your user to the `kvm` group:

~~~bash
sudo usermod -aG kvm $USER
~~~

Then log out and back in. Verify with `groups | grep kvm`.
```

## Backend auto-detection

OpenVMM auto-selects the hypervisor backend based on the platform. On Linux it
tries MSHV first (requires a specialized kernel module not publicly available),
then falls back to KVM. On Windows it uses WHP. You can override with
`--hypervisor` but rarely need to.

## Summary

| Approach | Binary | Backend | OpenHCL? | Performance |
|----------|--------|---------|----------|-------------|
| Cross-compile (`--target x86_64-pc-windows-msvc`) | Windows | WHP | ✅ Yes | Best — native, no nesting |
| Native Linux build | Linux | KVM/MSHV | ❌ No | Nested — may be slow |

```admonish tip
Use cross-compilation for most development. It's what the project's CI uses,
it supports OpenHCL, and it avoids nested virtualization overhead.
```
