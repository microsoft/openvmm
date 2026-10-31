# Runtime Hardware Resealing

OpenHCL refreshes hardware protectors after live migration without rotating
the active VMGS data-encryption key (DEK).

See [Attestation and VMGS Protection](attestation.md) for boot-time sealing
and key-release context adoption.

## Recovery after live migration

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
after success, the worker waits for another event. Restoring serialized resealer
state also schedules a durable rewrite, as described below.

Resealing retains the successfully adopted key-release context in the VM
configuration used by the KDF. A context-bound protector remains V4; without
a context, the replacement uses V3.

## Runtime TCB floor

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
  For report versions 3-5, Milan/Genoa (family `19h`, models `00h-1Fh`) and
  Turin (family `1Ah`, models `90h-AFh` and `C0h-CFh`) use distinct typed
  layouts, as defined by AMD 56860 section 2.3. Report version alone does not
  select the layout. Version 2 and unknown domains require exact SVN equality;
  cross-domain transitions are rejected. The floor tracks the reported SVN used
  for key derivation; it does not enforce the extended TCB added in report v6.
- **TDX:** CPU SVN components must be non-decreasing. TEE SVN is parsed using
  the typed `TeeTcbSvn` ABI layout: major SVN (byte 1) identifies the module
  and must match, including legacy identity 0. Minor SVN (byte 0) and SE_SVN
  (byte 2) must be non-decreasing; reserved bytes 3-15 must remain equal.
  Reserved metadata changes are incompatible, not upgrades. This minimum-TCB
  comparison does not determine Intel security status, which requires
  authenticated Intel TCB Info.

Candidate verification also checks a fresh report. Its SVN must match the
candidate exactly, even if the hardware could still derive an older SVN's key.

CPU SVN comparison uses all 16 unsigned bytes in their original positions:
every destination byte must be at least its resident floor byte. There is no
integer conversion or lexicographic ordering; an increase in one byte cannot
compensate for a decrease in another. Intel PCS
[Get TDX TCB Info V4][intel-tcb], step 3.a, uses component-wise minima for the
16 PCK certificate TCB components;
[Appendix A][intel-tcb-model] describes the components and comparison metadata.
OpenHCL's raw-report comparison is a local no-decrease policy, not that complete
appraisal algorithm: it neither maps raw CPU SVN bytes to PCK component
identities nor establishes cross-platform equivalence or Intel security status.

For SNP, `SnpDomain` retains report version and CPU family/model because the raw
TCB bytes do not identify their own layout. CPU family/model selects the layout;
version gates supported report semantics. Exact domain equality is required even
for identical raw SVNs or CPUs sharing a layout. This conservative restriction
avoids interpreting one CPU's component bytes using another CPU's meanings.
See [AMD 56860][amd-snp], revision 1.59, section 2.3, tables 4 and 5.

[intel-tcb]: https://api.portal.trustedservices.intel.com/content/documentation.html#pcs-tcb-info-tdx-v4
[intel-tcb-model]: https://api.portal.trustedservices.intel.com/content/documentation.html#pcs-tcb-info-model-v3
[amd-snp]: https://docs.amd.com/v/u/en-US/56860_PUB_SEV_SNP

## Lifecycle and saved state

Memory-preserving migration retains the worker and its floor. Stopping the
worker drains an in-flight recovery attempt. Reset does not request resealing;
it preserves the floor, existing retries and backoff, and latched notifications.

The stopped worker saves a versioned protobuf state unit named
`hardware_reseal`, containing:

- The accepted TCB floor: raw SNP TCB and report version/CPU comparison domain,
  or both raw TDX SVN arrays, including identity and reserved bytes.
- The serialized VM configuration used by the sealing KDF, checked against the
  destination configuration rather than used to replace it.
- Pending recovery, the failure count, and the latched migration notification.
- Remaining retry and minimum-interval delays, bounded to 60 seconds. These are
  relative durations, not process-specific clock values.

No DEK, hardware secret, derived key, or attestation report is saved by this
worker. Saving does not consume a notification or clear pending work: if another
unit fails to save, the original worker can resume with its obligations intact.

Restore constructs the worker without a floor, then installs the validated saved
floor before starting. It does not obtain a fresh report to initialize a new
floor or trust a VMGS protector as its source. Malformed or unsupported state,
an incompatible TEE or sealing configuration, or a decrease relative to an
existing resident floor fails restore without replacing that floor. A saved
worker also requires compatible hardware sealing support and encrypted VMGS.

Successful restore preserves retry backoff and merges saved notifications with
notifications received by the new worker. It always schedules a durable rewrite
when the worker starts, even when the saved worker was idle: hardware can change
while the worker is absent, including at the same SVN. Fresh reports must
satisfy the restored floor before any new protector is created. A cached
matching protector does not discharge a failed-flush obligation.

If the saved VM has no `hardware_reseal` unit, runtime resealing stays disabled
after reconstruction. This accommodates state from runtimes that were not
enrolled or did not support resealer state, without inventing a replacement
floor. Binaries that cannot interpret this state unit cannot restore an enrolled
worker's snapshot.

```admonish warning title="Trusted state transfer"
Protobuf encoding does not authenticate the saved floor. The enclosing transfer
must preserve integrity, bind the state to the VM, and prevent rollback to an
older snapshot. The resealer must not load this state from VMGS or
unauthenticated host metadata. Host-returned servicing state cannot initialize
this worker with the current transport and is rejected.

Resealer serialization does not enable isolated-VM servicing by itself. The
host-servicing isolation guard and unsupported SNP/TDX VP save/restore paths
remain in place. A trusted isolated-servicing handoff must satisfy the above
requirements before enabling that path.
```

## Recovery limits

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
