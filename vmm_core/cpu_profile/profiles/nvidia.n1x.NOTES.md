# NVIDIA N1X WHP profile

`nvidia.n1x.v1` adds a pinned cold-boot CPU profile for NVIDIA N1X on Windows
ARM64 with Windows Hypervisor Platform (WHP).

## Tested hardware

- Product: **MECHREVO N1X Spark** (owner-reported product model).
- CPU: **NVIDIA N1X**, reported by Windows as `NVIDIA RTX Spark N1X`.
- Host: 20 logical processors, approximately 32 GiB RAM,
  Windows build `10.0.28000.3231`.
- Validation date: 2026-11-14.

This records one physical machine's observations, not vendor certification or
qualification of other products using these Arm CPU part numbers. The profile
matches CPU identity and verifies the WHP feature surface; it does not match
the product's SMBIOS name.

## CPU contract

The native WHP MIDRs were `0x410fd851` and `0x410fd871`. A thread-affinity scan
of all 20 logical CPUs observed ten of each. All 15 configured ID registers
matched on every CPU. The model mask ignores variant/revision and part-number
bit 1, admitting exactly parts `0xd85` and `0xd87` with implementer `0x41` and
architecture `0xf`. An exhaustive part-number test rejects the other 4094 parts.
The profile uses the existing 36-bit address contract and existing state policy.
It does not enable snapshot/restore, nested virtualization, PMU, SVE, SME or MTE.

The 15 register values were captured through WHP on 2026-11-14 after applying
feature banks `0x7de1ef07fff03eff`, `0x000000180040dc34` and a 36-bit physical
address width. The existing PFR0 override removes EL2/EL3 descriptions; it was
applied and read back successfully. Native banks were `0x7de1ef07fff33eff`,
`0x00087e780640dc3d`; the native counter capability reported 1,000,000,000 Hz.

Use `auto` or the explicit `nvidia.n1x.v1` selection. Native-host derivation has
not been extended to filter N1X's extra state families and may reject `host`.

The implementation retains the shared WHP feature translation and effective
register checks. It does not rewrite the MIDR, bypass feature checks, or
manufacture unsupported guest feature values.

## Validation

The profile was tested on OpenVMM base
`b09c58843ae310dfff9ae68e9e822f9c5846b914` with the N1X changes and NVX
`24bdced073a24418c1ebbf1d0775bb5ba42266ba`.
The explicit profile booted Alpine 3.24.1 / Linux 6.18.38 with 512 MiB and one
vCPU, executed shell commands, checked synchronized time and powered off with
status zero. Default
`auto` then passed 17 additional executions: 128/256 MiB with one vCPU,
512 MiB with 2/4/8 vCPUs, and simultaneous batches of 4 and 8 guests, each
256 MiB / one vCPU. Each of these 17 runs checked private temporary files,
16 MiB file I/O
and SHA-256, synchronized time status, and guest power-off with status zero.
The concurrent batches held every guest at a barrier before releasing work.

The unit tests cover selection of both observed MIDRs, exclusion of other
part numbers, the existing Qualcomm selection, digest tampering, unsupported
extended state, translation into the measured WHP feature banks, and rejection
when a required feature is unavailable.

These short smoke tests do not qualify maximum capacity, adversarial isolation,
managed sandbox lifecycle, E2B compatibility, or production reliability.
