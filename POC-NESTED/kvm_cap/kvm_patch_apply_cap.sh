#!/bin/bash
#
# kvm_patch_apply_cap.sh
#
# Per-VM-capability form of the nested-vmbus relay (the production form).
#
# Unlike the global kvm_intel.nested_vmbus_relay module parameter
# (kvm_patch_apply.sh), this adds a per-VM KVM capability,
# KVM_CAP_NESTED_VMBUS_RELAY (0x4f564d52, "OVMR", a private out-of-tree number),
# that openvmm enables on its own kvm fd when nested virt is on. Only the VM
# that opts in takes the relay branch in nested_vmx_reflect_vmexit; every other
# guest (nested or not) is untouched, and multiple nested-vmbus guests coexist.
# It pairs with the openvmm change Partition::enable_nested_vmbus_relay()
# (vm/kvm) called from the virt_kvm nested_virt path.
#
# It rebuilds the KVM modules (kvm.ko + kvm-intel.ko). No Windows guest patch.
#
# The edits are applied by anchored text insertion (not a context diff), so they
# survive minor source drift across kernel point releases. Five files change:
#   include/uapi/linux/kvm.h          + #define KVM_CAP_NESTED_VMBUS_RELAY
#   arch/x86/include/asm/kvm_host.h   + bool nested_vmbus_relay in struct kvm_arch
#   arch/x86/kvm/x86.c                + KVM_ENABLE_CAP case sets the per-VM flag
#   arch/x86/kvm/vmx/nested.c         + the relay branch in nested_vmx_reflect_vmexit,
#                                       gated on nested_evmcs_l2_tlb_flush_enabled()
#                                       (L1's per-L2 nested-hypercall authorization)
#   arch/x86/kvm/hyperv.c             + translate the relayed synic post's input GPA
#                                       L2->L1 (HvPostMessage / HvSignalEvent)
#
# Getting the source: proxmox's apt repos publish NO source index, so
# `apt-get source` does not work for the proxmox kernel. Obtain the matching
# source tree via the proxmox-kernel git (heavy: it fetches the Ubuntu kernel
# base) and point this script at it with KVM_RELAY_SRC=/path/to/linux-source,
# or drop the extracted tree under $WORK. See the README.
#
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
KREL="$(uname -r)"                              # e.g. 7.0.2-7-pve
WORK="${KVM_RELAY_WORK:-/usr/src/kvm-nested-relay}"
JOBS="$(nproc)"
CAP_HEX="0x4f564d52"

[ "$(id -u)" = 0 ] || { echo "error: run as root" >&2; exit 1; }
[ -f "/boot/config-${KREL}" ] || { echo "error: /boot/config-${KREL} missing" >&2; exit 1; }
command -v make >/dev/null || { echo "error: install build-essential bc flex bison libelf-dev libssl-dev dwarves" >&2; exit 1; }

echo "== running kernel: ${KREL} =="
MARKER="/var/lib/kvm-nested-relay/cap-applied-${KREL}"
if [ -e "$MARKER" ] && [ "${FORCE:-0}" != 1 ]; then
    echo "== already built for ${KREL} (${MARKER}); FORCE=1 to redo =="; exit 0
fi

# --- locate the kernel source tree ----------------------------------------- #
find_src() { find "${1:-$WORK}" -maxdepth 7 -path '*/arch/x86/kvm/vmx/nested.c' -printf '%h\n' 2>/dev/null \
             | sed 's,/arch/x86/kvm/vmx,,' | head -1 || true; }
SRC=""
if [ -n "${KVM_RELAY_SRC:-}" ]; then SRC="$(find_src "$KVM_RELAY_SRC")"; fi
if [ -z "$SRC" ]; then SRC="$(find_src "$WORK")"; fi
if [ -z "$SRC" ]; then
    mkdir -p "$WORK"; cd "$WORK"
    echo "== no source tree found; trying apt-get source (usually unavailable on proxmox) =="
    apt-get source "proxmox-kernel-${KREL}" 2>/dev/null || true
    SRC="$(find_src "$WORK")"
fi
[ -n "$SRC" ] && [ -d "$SRC" ] || {
    cat >&2 <<EOF
error: kernel source tree not found.
  proxmox publishes no apt source index, so 'apt-get source' does not work here.
  Obtain the matching source and re-run with KVM_RELAY_SRC pointing at its root:

    git clone --depth 1 https://git.proxmox.com/git/proxmox-kernel.git
    cd proxmox-kernel && make submodule    # fetches the Ubuntu kernel base
    # the extracted Linux tree appears under submodules/ubuntu-kernel (or build/)
    KVM_RELAY_SRC=\$PWD/submodules/ubuntu-kernel $0
EOF
    exit 1
}
echo "== source tree: ${SRC} =="

# --- anchored, idempotent edits --------------------------------------------- #
python3 - "$SRC" "$CAP_HEX" <<'PY'
import sys, io, re, os
src, cap = sys.argv[1], sys.argv[2]
def edit(path, fn):
    p = os.path.join(src, path)
    with io.open(p, encoding="utf-8") as f: t = f.read()
    nt = fn(t)
    if nt is None:
        print("  unchanged (already applied): %s" % path); return
    with io.open(p, "w", encoding="utf-8") as f: f.write(nt)
    print("  edited: %s" % path)

def kvm_h(t):
    if "KVM_CAP_NESTED_VMBUS_RELAY" in t: return None
    m = re.search(r'^#define KVM_CAP_SPLIT_IRQCHIP .*$', t, re.M)
    if not m: raise SystemExit("kvm.h: KVM_CAP_SPLIT_IRQCHIP anchor not found")
    ins = ("\n/* provmm out-of-tree: nested-Hyper-V vmbus relay (per-VM cap). "
           "Private sentinel, must match openvmm vm/kvm. */\n"
           "#define KVM_CAP_NESTED_VMBUS_RELAY %s" % cap)
    return t[:m.end()] + ins + t[m.end():]

def kvm_host_h(t):
    if "nested_vmbus_relay" in t: return None
    m = re.search(r'^struct kvm_arch \{\n', t, re.M)
    if not m: raise SystemExit("kvm_host.h: struct kvm_arch anchor not found")
    ins = ("\t/* provmm: nested-Hyper-V vmbus relay opted in via "
           "KVM_CAP_NESTED_VMBUS_RELAY */\n\tbool nested_vmbus_relay;\n")
    return t[:m.end()] + ins + t[m.end():]

def x86_c(t):
    if "KVM_CAP_NESTED_VMBUS_RELAY" in t: return None
    f = t.find("kvm_vm_ioctl_enable_cap(struct kvm *kvm")
    if f < 0: raise SystemExit("x86.c: kvm_vm_ioctl_enable_cap not found")
    s = t.find("switch (cap->cap) {", f)
    if s < 0: raise SystemExit("x86.c: switch in enable_cap not found")
    nl = t.find("\n", s) + 1
    case = ("\tcase KVM_CAP_NESTED_VMBUS_RELAY:\n"
            "\t\t/* provmm: keep this VM's L2 Hyper-V root vmbus posts in L0 */\n"
            "\t\tkvm->arch.nested_vmbus_relay = true;\n"
            "\t\tr = 0;\n"
            "\t\tbreak;\n")
    return t[:nl] + case + t[nl:]

def nested_c(t):
    if "nested_vmbus_relay" in t: return None
    anchor = "trace_kvm_nested_vmexit(vcpu, KVM_ISA_VMX);"
    i = t.find(anchor)
    if i < 0: raise SystemExit("nested.c: trace_kvm_nested_vmexit anchor not found")
    nl = t.find("\n", i) + 1
    blk = (
"\n"
"\t/*\n"
"\t * provmm nested-Hyper-V vmbus relay (per-VM, KVM_CAP_NESTED_VMBUS_RELAY).\n"
"\t * A Hyper-V root running as an L2 guest posts HvPostMessage (RCX low16\n"
"\t * 0x5c) / HvSignalEvent (0x5d) as VMCALLs with the Hyper-V nested bit\n"
"\t * (RCX bit 31). Only relay when L1 authorized this L2 for direct nested\n"
"\t * hypercalls, the same eVMCS gate (nested_flush_hypercall plus the\n"
"\t * VP-assist directhypercall feature) KVM already trusts for the L2\n"
"\t * TLB-flush hypercall, so an L2 that L1 did not authorize (a grandchild\n"
"\t * guest of the root) is never relayed and keeps its own L1 synic. Keep\n"
"\t * the post in L0 for the opted-in VM (clear the nested bit so KVM's\n"
"\t * hypercall path accepts the call) instead of reflecting to L1.\n"
"\t */\n"
"\tif (vcpu->kvm->arch.nested_vmbus_relay &&\n"
"\t    exit_reason.basic == EXIT_REASON_VMCALL &&\n"
"\t    nested_evmcs_l2_tlb_flush_enabled(vcpu)) {\n"
"\t\tunsigned long code = kvm_rcx_read(vcpu);\n"
"\n"
"\t\tif (((code & 0xffff) == 0x5c || (code & 0xffff) == 0x5d) &&\n"
"\t\t    (code & (1ULL << 31))) {\n"
"\t\t\tkvm_rcx_write(vcpu, code & ~(1ULL << 31));\n"
"\t\t\treturn false;\n"
"\t\t}\n"
"\t}\n")
    return t[:nl] + blk + t[nl:]

def hyperv_c(t):
    # Translate a relayed L2 synic post's input GPA (L2->L1) before the in-kernel
    # read / userspace post, mirroring the L2 TLB-flush slow path. The walk also
    # filters pages removed from the L2 root's GPA space (returns INVALID_GPA).
    if "nested-vmbus relay: a relayed L2 synic post" in t: return None
    anchor = ("\t\tkvm_hv_hypercall_read_xmm(&hc);\n"
              "\t}\n"
              "\n"
              "\tswitch (hc.code) {\n")
    if anchor not in t:
        raise SystemExit("hyperv.c: kvm_hv_hypercall switch anchor not found")
    blk = (
"\t\tkvm_hv_hypercall_read_xmm(&hc);\n"
"\t}\n"
"\n"
"\t/*\n"
"\t * provmm nested-vmbus relay: a relayed L2 synic post (HvPostMessage /\n"
"\t * HvSignalEvent from a nested Hyper-V root) running on nested EPT carries\n"
"\t * an L2 GPA in ingpa. Translate it to an L1 GPA like the L2 TLB-flush slow\n"
"\t * path, gating on mmu_is_nested(): translate_nested_gpa() BUG_ON()s without\n"
"\t * it, and with shadow paging the L2 GPA is already an L1 GPA so no\n"
"\t * translation is needed. The walk also rejects pages removed from the L2\n"
"\t * root's GPA space (INVALID_GPA). Done synchronously in the faulting\n"
"\t * vCPU's exit context and never cached.\n"
"\t */\n"
"\tif (!hc.fast && mmu_is_nested(vcpu) &&\n"
"\t    (hc.code == HVCALL_POST_MESSAGE || hc.code == HVCALL_SIGNAL_EVENT)) {\n"
"\t\thc.ingpa = kvm_x86_ops.nested_ops->translate_nested_gpa(\n"
"\t\t\t\tvcpu, hc.ingpa, PFERR_GUEST_FINAL_MASK, NULL, 0);\n"
"\t\tif (unlikely(hc.ingpa == INVALID_GPA)) {\n"
"\t\t\tret = HV_STATUS_INVALID_HYPERCALL_INPUT;\n"
"\t\t\tgoto hypercall_complete;\n"
"\t\t}\n"
"\t}\n"
"\n"
"\tswitch (hc.code) {\n")
    return t.replace(anchor, blk, 1)

edit("include/uapi/linux/kvm.h", kvm_h)
edit("arch/x86/include/asm/kvm_host.h", kvm_host_h)
edit("arch/x86/kvm/x86.c", x86_c)
edit("arch/x86/kvm/vmx/nested.c", nested_c)
edit("arch/x86/kvm/hyperv.c", hyperv_c)
print("edits done")
PY

# --- configure to the running kernel and build only the KVM modules --------- #
cd "$SRC"
cp -f "/boot/config-${KREL}" .config
[ -f "/usr/src/linux-headers-${KREL}/Module.symvers" ] && cp -f "/usr/src/linux-headers-${KREL}/Module.symvers" Module.symvers || true
make olddefconfig
make modules_prepare
echo "== building kvm + kvm-intel =="
make -j"$JOBS" M=arch/x86/kvm

DEST="/lib/modules/${KREL}/kernel/arch/x86/kvm"
mkdir -p "$DEST"
for ko in kvm.ko kvm-intel.ko; do
    [ -f "arch/x86/kvm/${ko}" ] && cp -v "arch/x86/kvm/${ko}" "${DEST}/${ko}"
done
depmod -a
mkdir -p "$(dirname "$MARKER")" && : > "$MARKER"

cat <<EOF

== done ==
Patched kvm + kvm-intel (per-VM cap) installed for ${KREL}.
Activate (modules are in use while VMs run):
  - stop all VMs, then:  rmmod kvm_intel kvm && modprobe kvm_intel
  - or reboot.
No module parameter and no kernel cmdline change are needed: openvmm enables the
cap per-VM when nested virt is on. Deploy the matching openvmm build (with
Partition::enable_nested_vmbus_relay) and boot a nested guest. NO Windows patch,
NO ftrace hvpost_hook, NO relay_pid.
EOF
