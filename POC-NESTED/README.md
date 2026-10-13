# Nested Hyper-V vmbus relay

A stock, unmodified Hyper-V/VBS-enabled Windows 11 guest boots to the desktop under
OpenVMM on the KVM backend, including the root partition's own vmbus (storvsc boot disk
and netvsp). Without the relay it bugchecks `0x7B INACCESSIBLE_BOOT_DEVICE` about 80s into
boot.

The Windows kernel runs as hvix64's root partition, i.e. an L2 guest (guest kernel ->
hvix64 L1 -> KVM L0). Its vmbus hypercalls (`HvPostMessage` 0x5c, `HvSignalEvent` 0x5d)
are VMCALLs that exit to L0/KVM before hvix64 sees them, and its synic is L0-visible. So
**KVM keeps those posts in L0 and hands them to OpenVMM's vmbus server; hvix64 is unpatched
and the guest is unmodified.** `nested_synic_writeup_jstarks.md` is the full design write-up
and the request to help upstream the KVM change; this README is the reproduction recipe.

No guest patch, no test-signing, Secure Boot stays on. An earlier PoC needed a 2-byte
`vmbus.sys` patch; the OpenVMM enlightenment set in this branch (notably granting the synic
APIC MSRs and advertising reference-TSC / reenlightenment / enlightened-VMCS when nesting)
makes the stock signed driver register its synic interrupt and post `InitiateContact` on
its own, so the guest patch is gone.

## Two relay forms

The relay (keep the L2 root's `0x5c`/`0x5d` posts in L0 instead of reflecting them to
hvix64) is a small KVM change. Two forms ship here:

1. **Production: a per-VM capability, `KVM_CAP_NESTED_VMBUS_RELAY`.** OpenVMM enables it on
   its own VM when nesting; KVM checks a per-`struct kvm` flag in
   `nested_vmx_reflect_vmexit`, gated on the L1's per-L2 nested-hypercall authorization (the
   same enlightened-VMCS gate, `nested_evmcs_l2_tlb_flush_enabled()`, KVM already trusts for
   the L2 TLB-flush hypercall). Scoped by construction, no ftrace, no pid arming. The kernel
   side lives in `kvm_cap/` (patch + build script) and as full trees at:
   * mainline: https://github.com/bitranox/linux-nested-vmbus-relay (branch `nested-vmbus-relay`)
   * Proxmox VE kernel: https://github.com/bitranox/pve-nested-vmbus-relay
2. **PoC: an ftrace hook**, `kvm_hook/hvpost_hook.c`. It overrides
   `nested_vmx_reflect_vmexit` from a loadable module, default-off, armed by writing the
   target VM's vCPU-worker pid to `relay_pid`. Use it to try the relay without rebuilding
   KVM. Scoped to one VM at a time.

## Contents

```
POC-NESTED/
├── README.md                         this file
├── nested_synic_writeup_jstarks.md   the design write-up + upstreaming request
├── kvm_cap/                          production per-VM capability (recommended)
│   ├── kvm_patch_apply_cap.sh        patch a kernel source tree + build/install the KVM modules
│   ├── kvm-nested-vmbus-relay-linux.patch   unified diff vs mainline (kvm-x86/next)
│   └── kvm-nested-vmbus-relay-pve.patch     unified diff vs a Proxmox VE 7.0.x kernel
└── kvm_hook/                         the ftrace PoC (no kernel rebuild)
    ├── hvpost_hook.c
    └── Makefile
```

## OpenVMM side (already in this branch)

The nested-virt source changes are applied here: the `virt_kvm` partition-privilege block
that grants the synic enlightenments when nesting (`vmm_core/virt_kvm/src/arch/x86_64/mod.rs`),
the enlightened-VMCS capability (`vm/kvm/src/lib.rs`), and the `HvFeatures` bits
(`vm/hv1/hvdef/src/lib.rs`). Build and deploy as usual:

```bash
cargo build --release -p openvmm
strip target/release/openvmm
# deploy the binary to the host that runs the guest
```

Launch the guest with `--hypervisor kvm:nested_virt`. The PoC hook path works with this
branch as-is. The per-VM cap path additionally needs OpenVMM to enable the capability on the
VM (a `KVM_ENABLE_CAP(KVM_CAP_NESTED_VMBUS_RELAY)` call when `nested_virt` is set); that
one-line integration is the production counterpart to the kernel patch in `kvm_cap/`.

## Patching the kernel for the per-VM cap (production)

The relay touches five files: the cap number in `include/uapi/linux/kvm.h`, a `bool` in
`struct kvm_arch`, the `KVM_ENABLE_CAP` case in `arch/x86/kvm/x86.c`, the relay branch in
`arch/x86/kvm/vmx/nested.c`, and the L2->L1 input-GPA translation in `arch/x86/kvm/hyperv.c`.

### Option A: the build script (Proxmox VE, or any distro kernel)

`kvm_cap/kvm_patch_apply_cap.sh` applies the change by anchored text edits (so it survives
point-release drift) and rebuilds only the KVM modules (`kvm.ko`, `kvm-intel.ko`). It does
not rebuild the whole kernel.

```bash
# build deps (Debian/Ubuntu/Proxmox):
sudo apt install build-essential bc flex bison libelf-dev libssl-dev dwarves libdw-dev

# point it at a kernel source tree that matches the running kernel, then run as root:
sudo KVM_RELAY_SRC=/path/to/linux-source kvm_cap/kvm_patch_apply_cap.sh
```

Getting the matching source: for the Proxmox kernel, clone `proxmox-kernel.git` and run
`make submodule` (it fetches the Ubuntu kernel base); the script header has the details.
For a mainline build, use the matching kernel tree.

### Option B: apply the patch to a kernel you build yourself

```bash
cd linux                                                   # your kernel source tree
patch -p1 < kvm_cap/kvm-nested-vmbus-relay-linux.patch     # mainline (kvm-x86/next)
# or, for a Proxmox VE 7.0.x kernel source tree:
patch -p1 < kvm_cap/kvm-nested-vmbus-relay-pve.patch
# then build and install the KVM modules, or the whole kernel, as you normally would
```

The cap number `0x4f564d52` is an out-of-tree private sentinel; an upstream merge would take
an assigned `KVM_CAP_*` value.

### Loading the rebuilt modules

The modules are in use while VMs run, so either reboot, or hot-swap with all VMs stopped:

```bash
sudo rmmod kvm_intel kvm && sudo modprobe kvm_intel        # pulls in the rebuilt kvm.ko
```

Verify the rebuilt module is the loaded one with `modinfo -F filename kvm-intel`. No module
parameter and no kernel command line change is needed: OpenVMM enables the cap per-VM.

## The ftrace PoC (no kernel rebuild)

If you want to try the relay without touching the kernel build, use `kvm_hook/`. The struct
offsets in `hvpost_hook.c` are kernel-version-specific.

```bash
# build deps: matching kernel headers + gcc/flex/bison/libelf-dev/libssl-dev/bc/dwarves
cd kvm_hook && make                                        # builds hvpost_hook.ko
sudo insmod hvpost_hook.ko                                 # loads DEFAULT-OFF (relay_pid=0)
```

Arm it at launch by pointing `relay_pid` at the guest's vCPU-running worker. OpenVMM is
multi-process: the main `openvmm-kvm` process (which writes `--pidfile`) spawns an
`openvmm-vm` worker that runs the vCPUs. Set `relay_pid` before the kernel posts (~2-3s
after launch, right after ExitBootServices):

```bash
for i in $(seq 1 120); do
  MP=$(cat <pidfile> 2>/dev/null)
  [ -n "$MP" ] && RP=$(pgrep -P "$MP" -x openvmm-vm | head -1)
  [ -n "$RP" ] && { echo "$RP" | sudo tee /sys/module/hvpost_hook/parameters/relay_pid; break; }
  sleep 0.1
done
```

`rmmod hvpost_hook` removes it cleanly. If your kernel differs, re-derive the two offsets in
`hvpost_hook.c` with `pahole kvm_vcpu` / `pahole -C vcpu_vmx` against the running kernel's BTF.

## Verify

- The OpenVMM log shows a **second** `Guest negotiated version` after `Vmbus disconnected`
  (the L2 kernel's `InitiateContact` via the relay), then storvsc sub-channels and netvsp.
- The screen goes `0x7B` (without the relay) to the boot logo to the Windows 11 desktop.
- With the PoC hook, `dmesg | grep "caught HvPostMessage"` shows it firing during boot. With
  the per-VM cap there is no host-side log; the second negotiation and the desktop are the
  signal.

Beyond boot, the guest's hvix64 has been validated running real workloads under OpenVMM/KVM:
a bare child VM (`New-VM`/`Start-VM`), and Hyper-V-isolated Windows containers (each its own
utility VM under hvix64), with host-filesystem isolation intact. See the write-up's
"Validation" section.

## Scoping

`nested_vmx_reflect_vmexit` runs only for nested (L2) guests. The per-VM cap relays only for
the VM whose `struct kvm` opted in, and only for an L2 the L1 authorized for direct nested
hypercalls, so a grandchild guest of the root is never relayed and other VMs are untouched.
The PoC hook relays only when `current->tgid == relay_pid`, and `relay_pid=0` (the default)
intercepts nothing.
