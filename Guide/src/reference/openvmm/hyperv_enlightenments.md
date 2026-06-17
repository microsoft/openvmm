# Hyper-V enlightenments

On a KVM host, `--hypervisor kvm:hv=<spec>` chooses which Hyper-V enlightenments
OpenVMM advertises to the guest (in the synthetic-hypervisor CPUID leaves) and
enables in KVM (via the matching capabilities).

## Spec syntax

`hv=<spec>` is a `+`-separated list of tokens, applied left to right so a later
token wins. Each token is one of:

* a preset (`default`, `windows`, or `none`), allowed only as the first token,
  which sets the starting enlightenment set;
* a flag name (for example `evmcs`), which turns that enlightenment on;
* a `no_`-prefixed flag name (for example `no_evmcs`), which turns it off;
* `spinlocks=<n>`, which sets the spinlock retry count (decimal or `0x` hex).

There is no `+`/`-` syntax: the `+` only separates tokens, and a flag is turned
off with `no_`, not `-`. Any flag can be turned on or off this way, and a flag
or `no_` token may also appear first. If the first token is not a preset, the
starting set is `default`; when `nested_virt` is set and no `hv=` is given at
all, the `windows` preset is used instead.

```bash
--hypervisor kvm:hv=windows                   # the windows preset as-is
--hypervisor kvm:hv=windows+no_evmcs          # windows, with evmcs turned off
--hypervisor kvm:hv=default+stimer_direct     # default set, with stimer_direct on
--hypervisor kvm:hv=none+synic+stimer+vapic   # start from nothing, turn three on
--hypervisor kvm:hv=windows+spinlocks=0x1fff  # windows, with a finite spinlock count
```

## Presets

* `default`: reference time, frequency MSRs, hypercall, VP index, VP runtime,
  SynIC, synthetic timer, and APIC MSRs. This is the set OpenVMM advertised
  before this option existed.
* `windows`: the Windows enlightenment set. It adapts to `nested_virt`. With
  `nested_virt` set it is the `default` set plus what a guest Hyper-V needs to
  run nested: enlightened VMCS, direct synthetic timers, reenlightenment, relaxed
  timing, remote TLB flush, and cluster IPI. With `nested_virt` clear it drops
  the two nested-only flags, enlightened VMCS and reenlightenment, and keeps the
  rest, so you do not need `+no_evmcs+no_reenlightenment` by hand for a plain
  Windows guest (that override still works). CPUID enforcement is left out either
  way because it stops a nested guest from booting on a KVM host (see
  `enforce_cpuid`). On a KVM host direct synthetic timers are auto-detected:
  `stimer_direct` only takes effect when the host advertises direct mode; on a
  host without it the request is ignored, the flag stays off, and a warning is
  logged (see `stimer_direct`). Enlightened VMCS is not host-gated: the preset
  keeps it on for a nested guest, and `+no_evmcs` turns it off.
* `none`: start with everything off and add flags explicitly.

## Flags

Toggle with the bare name to enable, or `no_<name>` to disable.

* `time`: the reference counter and reference TSC page.
* `frequencies`: the TSC and APIC frequency MSRs.
* `hypercall`: the hypercall MSRs.
* `vpindex`: the VP index MSR.
* `runtime`: the VP runtime MSR.
* `synic`: the synthetic interrupt controller. Also enables the SynIC2 KVM
  capability.
* `stimer`: synthetic timers.
* `stimer_direct`: direct-mode synthetic timers, which a nested Hyper-V needs for
  its own timers. Only works when the host advertises direct mode
  (`HV_STIMER_DIRECT_MODE_AVAILABLE`, bit 19 of CPUID `0x40000003` EDX). On a KVM
  host that does not, OpenVMM forces this off, since it cannot work otherwise; an
  explicit `+stimer_direct` is still left off and a warning is logged.
* `vapic`: APIC access through MSRs and the virtual APIC assist page.
* `relaxed`: relaxed timing, so the guest does not trip bare-metal watchdog and
  spinlock deadlines while virtualized.
* `tlbflush`: the hypercall-based remote TLB flush, with extended processor
  masks.
* `ipi`: the synthetic cluster IPI.
* `evmcs`: enlightened VMCS for a nested hypervisor (also enables the
  enlightened-VMCS KVM capability). A nested Windows guest needs this to boot from
  a synthetic (VMBus) storage controller; without it the guest hypervisor takes a
  VMREAD/VMWRITE exit storm and the storage path times out. In the `windows`
  preset by default only when `nested_virt` is set; a non-nested `windows` guest
  leaves it off. When the nested preset carries it, it stays on unconditionally;
  turn it off with `hv=windows+no_evmcs`.
* `reenlightenment`: the reenlightenment control MSRs, used by a nested
  hypervisor across a TSC frequency change. In the `windows` preset only when
  `nested_virt` is set; a non-nested `windows` guest leaves it off.
* `enforce_cpuid`: make KVM expose only the advertised enlightenments instead of
  every one it implements (also enables the enforce-CPUID KVM capability). Off in
  every preset: a nested hypervisor uses synthetic MSRs and hypercalls while
  bringing up its partition, before its first guest entry, so enabling this stalls
  the nested guest before it runs. Use it only for strict CPUID masking on a guest
  that does not run nested Hyper-V.
* `spinlocks=<n>`: the spinlock retry count reported to the guest in the
  enlightenment-recommendations leaf. After `<n>` failed acquisitions of a guest
  spinlock, the guest issues a long-spin-wait hypercall so the hypervisor can
  deschedule the spinning VP and run the lock holder instead, avoiding wasted
  spinning when the holder was preempted. `0xffffffff` (the default) disables the
  notification; any finite count enables it. Accepts decimal or `0x`-prefixed hex.
  This is a performance hint, not a capability; it is always settable.

## Tuning the spinlock retry count

The `spinlocks=<n>` flag helps only under VP over-commit with real lock
contention (more guest VPs than free host CPUs, plus a contended multithreaded
workload). A guest whose VPs each have a dedicated host CPU rarely spins long
enough for the notification to matter, which is why the default leaves it off.
Set a finite count when the guest is over-committed and lock-bound.

A smaller count notifies sooner (the holder is rescheduled faster, at the cost of
more hypercalls); a larger count spins longer before giving up. A retry count in
the low thousands, such as `0x1fff`, is a reasonable starting point. To pick one,
benchmark it: run a contended multithreaded workload at your target VP-to-CPU
over-commit ratio and compare the default (`0xffffffff`, off) against one or two
finite counts (`hv=windows+spinlocks=0x1fff`); keep whichever improves throughput
and tail latency, and leave it off if none does.

## Picking enlightenments for your host

Start from `windows` for a nested Hyper-V guest. On a KVM host the preset adapts
to your CPU on its own. It keeps enlightened VMCS on for a nested guest, which a
synthetic (VMBus) boot disk needs to come up. Direct synthetic timers come on
only where the host supports them. Leave `enforce_cpuid` off whenever the guest
runs its own hypervisor. Every flag is independent, so any preset can be tuned
with `+name` or `+no_name`, for example `hv=windows+no_evmcs` or
`hv=default+stimer_direct`.

## Nested versus non-nested

`hv=windows` adapts to `nested_virt` on its own. With `nested_virt` set it
carries enlightened VMCS and reenlightenment, which only a guest hypervisor uses.
With `nested_virt` clear it drops those two and keeps the rest, including direct
synthetic timers, which benefits a non-nested guest as well. So a plain Windows
guest needs no extra spec: `hv=windows` already gives it the right set, and
`hv=windows+no_evmcs+no_reenlightenment` by hand is no longer necessary (it still
works if you prefer to be explicit).
