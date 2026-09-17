# Attestation and VMGS protection

OpenHCL uses attestation and hardware sealing to protect confidential VM guest
state stored in VMGS and recover access to it across boots and live migration.

## Boot and key protection

Depending on the VM's configuration, platform-security initialization obtains
key material through secure key release (SKR), hardware unsealing, and guest
state protection services. Hardware sealing can provide either the sole means
of recovering the VMGS datastore key (DEK) or a recovery path alongside other
key protectors.

The hardware protector contains the sealed DEK and the security version numbers
(SVNs) needed to reproduce its hardware-derived key. It is stored in
`HW_KEY_PROTECTOR` without VMGS-level encryption so it can be read before VMGS
is unlocked. Its header, including SVN metadata, and encrypted DEK are covered
by a hardware-keyed HMAC.

Boot unsealing uses the protector's recorded SVN, which can be lower than the
current hardware SVN. Recovery requires hardware support for that derivation
and successful protector authentication. This differs from the minimum TCB
policy used for runtime resealing below.

The main implementation is in
[`underhill_attestation`](https://openvmm.dev/rustdoc/linux/underhill_attestation/index.html),
with local hardware access provided by
[`tee_call`](https://openvmm.dev/rustdoc/linux/tee_call/index.html).

## Resiliency

### Recovery after live migration

A hardware-derived sealing key may change when a VM migrates to another host.
OpenHCL retains the active DEK in protected memory, allowing it to create a
destination-compatible protector without recovering the DEK from the source
protector.

The GET `NOTIFY_POST_LIVE_MIGRATION` notification triggers this recovery:

1. Obtain a local report and check it against the runtime TCB floor.
2. Derive destination hardware keys and seal the **unchanged** active DEK.
3. Verify the candidate protector, then write and flush it through the VMGS
   broker. A defensive active-key check prevents publishing a stale DEK if
   concurrent key rotation is introduced.
4. Verify again after persistence to detect hardware or SVN changes during the
   operation.

Recovery is enabled only for encrypted VMGS with a supported hardware-sealing
policy and a trusted runtime floor. It neither changes that policy nor rotates
the DEK. Hardware calls run off the VP and GET executors. Notifications are
coalesced, and failed attempts retry with bounded backoff. There is no periodic
verification; after success, the worker waits for another event.

### Runtime TCB floor

The trusted computing base (TCB) floor prevents runtime resealing at a lower
accepted security version. Boot establishes it from local reports already
obtained during platform-security initialization, including reports obtained
before failed SKR callouts. It is held in protected OpenHCL memory, not loaded
from host-controlled VMGS metadata.

Compatible higher observations advance the floor. Retries, persistence failures,
reset, and normal stop/start cannot lower it. If boot cannot establish a usable
floor, runtime resealing stays disabled with a warning; existing boot recovery
behavior is unchanged.

The comparison is a partial order, not a packed-integer or lexicographic order:

- **SNP:** known TCB components must be non-decreasing within the same supported
  report version and CPU family/model; reserved bytes must remain equal.
  Milan/Genoa and Turin use different layouts. Version 2 and unknown domains
  require exact SVN equality; cross-domain transitions are rejected. The floor
  tracks the reported SVN used for key derivation.
- **TDX:** CPU SVN components must be non-decreasing. TEE SVN byte 1 identifies
  the module and must match, including legacy identity 0; the remaining bytes
  must be individually non-decreasing. This minimum-TCB comparison does not
  determine Intel security status, which requires authenticated Intel TCB Info.

Candidate verification also checks a fresh report. Its SVN must match the
candidate exactly, even if the hardware could still derive an older SVN's key.

### Lifecycle and limits

Memory-preserving migration retains the worker and its floor. Stopping the
worker drains an in-flight recovery attempt. Serialized save/restore of the
resealer is unsupported because it has no protected floor-transfer format;
reconstruction must not replace the source floor with a destination report.

Cold boot establishes a new runtime lifetime. The floor is not a persistent
anti-rollback counter and does not prevent replay of an entire VMGS snapshot.
Rejecting resealing on a lower-TCB destination also cannot undo delivery of the
resident DEK by migration; migration admission is a separate security boundary.

```admonish warning
Recovery is best effort. A missed notification can leave the protector stale,
and a crash before durable destination resealing can leave hardware-only VMGS
unrecoverable. Report, derivation, and persistence operations are not atomic
with migration. Durability also depends on backing storage honoring flushes.
```
