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

Ordinary stateless mode bypasses VMGS encryption when hardware sealing is not
requested. Stateless mode with the `HardwareSealing` encryption policy instead
requires hardware sealing: it seals a new DEK on provisioning and unseals then
rotates it on later boots. Unsupported required sealing or failure to derive,
unseal, write, or finalize the required protector prevents boot from completing.

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

Recovery is enabled only when the successful boot-unlock attempt has sealed,
written, and flushed a hardware protector for the active DEK and established a
trusted runtime floor under a supported policy. This eligibility comes from
trusted operation results, not the presence of an entry on disk. An LM event
maintains an established recovery path; it does not perform first-time sealing.
Once enabled, recovery can repair a subsequently missing or corrupt protector.
It neither changes the policy nor rotates the DEK. Hardware calls run off the
VP and GET executors. GET coalesces notifications received before callback
registration into one pending event and delivers it when registration completes.
The worker also latches events received before startup or during recovery.
Failed attempts retry with bounded backoff. There is no periodic verification;
after success, the worker waits for another event.

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

If optional boot sealing does not succeed, no floor is exported to the runtime
worker. When hardware sealing is only a backup, finalization failure disables
runtime recovery without failing boot. When sealing is required, or boot used
hardware unsealing to recover the DEK, failure to confirm the active sealed key
or flush completed writes fails boot. The full unlock/key-rotation sequence is
not automatically retried after this failure because it may already have changed
the active DEK. Existing protector write/unlock errors retain their handling.

The comparison is a partial order, not a packed-integer or lexicographic order:

- **SNP:** known TCB components must be non-decreasing within the same supported
  report version and CPU family/model; reserved bytes must remain equal.
  For report versions 3–5, Milan/Genoa (family `19h`, models `00h–1Fh`) and
  Turin (family `1Ah`, models `90h–AFh` and `C0h–CFh`) use distinct typed
  layouts, as defined by AMD 56860 section 2.3. Report version alone does not
  select the layout. Version 2 and unknown domains require exact SVN equality;
  cross-domain transitions are rejected. The floor tracks the reported SVN used
  for key derivation; it does not enforce the extended TCB added in report v6.
- **TDX:** CPU SVN components must be non-decreasing. TEE SVN is parsed using
  the typed `TeeTcbSvn` ABI layout: major SVN (byte 1) identifies the module
  and must match, including legacy identity 0. Minor SVN (byte 0) and SE_SVN
  (byte 2) must be non-decreasing; reserved bytes 3–15 must remain equal.
  Reserved metadata changes are incompatible, not upgrades. This minimum-TCB
  comparison does not determine Intel security status, which requires
  authenticated Intel TCB Info.

Candidate verification also checks a fresh report. Its SVN must match the
candidate exactly, even if the hardware could still derive an older SVN's key.

CPU SVN comparison uses all 16 unsigned bytes in their original positions:
every destination byte must be at least its resident floor byte. There is no
integer conversion or lexicographic ordering; an increase in one byte cannot
compensate for a decrease in another. Intel PCS [Get TDX TCB Info V4][intel-tcb],
step 3.a, uses component-wise minima for the 16 PCK certificate TCB components;
[Appendix A][intel-tcb-model] describes the components and comparison metadata.
OpenHCL's raw-report comparison is a local no-decrease policy, not that complete
appraisal algorithm: it neither maps raw CPU SVN bytes to PCK component identities
nor establishes cross-platform equivalence or Intel security status.

For SNP, `SnpDomain` retains report version and CPU family/model because the raw
TCB bytes do not identify their own layout. CPU family/model selects the layout;
version gates supported report semantics. Exact domain equality is required even
for identical raw SVNs or CPUs sharing a layout. This conservative restriction
avoids interpreting one CPU's component bytes using another CPU's meanings.
See [AMD 56860][amd-snp], revision 1.59, section 2.3, tables 4 and 5.

[intel-tcb]: https://api.portal.trustedservices.intel.com/content/documentation.html#pcs-tcb-info-tdx-v4
[intel-tcb-model]: https://api.portal.trustedservices.intel.com/content/documentation.html#pcs-tcb-info-model-v3
[amd-snp]: https://docs.amd.com/v/u/en-US/56860_PUB_SEV_SNP

### Lifecycle and limits

Memory-preserving migration retains the worker and its floor. Stopping the
worker drains an in-flight recovery attempt. Reset does not request resealing;
it preserves the floor, existing retries and backoff, and latched notifications.

Serialized servicing is unsupported for VMs with this worker: its save operation
returns `SaveError::NotSupported`,
which fails the VM save, and its restore operation rejects saved state. There is
no saved-state reconstruction or reconstruction-triggered durable rewrite. The
worker has no protected floor-transfer format, so reconstruction must not
replace the source floor with a destination report. Pending recovery, including a
failed flush obligation, survives only while the resident worker is retained.

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
