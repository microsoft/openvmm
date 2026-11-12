# Attestation and VMGS Protection

This page introduces OpenHCL's attestation flows and directs you to their
lifecycle and protocol references.

## Attestation flows

OpenHCL uses attestation for two distinct purposes:

- **VM Guest State (VMGS) protection:** secure key release obtains key material
  used to recover access to encrypted VMGS. Hardware sealing provides local
  key protection where supported and enabled by the VM configuration.
- **TPM attestation-key (AK) certification:** the virtual TPM requests a
  certificate for its AK and can later request renewal. Issuing or retaining
  this certificate is separate from releasing keys to unlock VMGS.

The host IGVM agent is reached through the Guest Emulation Transport (GET).
For hardware-attested requests, OpenHCL obtains a TEE report that binds a hash
of the serialized runtime claims. Binding the claims does not by itself
authenticate host-supplied metadata or establish certificate trust.

## Reference pages

- [AK Certificate Renewal](ak_cert.md) owns certificate storage, renewal
  decisions, scheduling, and the guest-report boundary.
- [IGVM Attestation Protocol](attestation_protocol.md) owns the AK request
  claims, response framing, signal definitions, and compatibility rules.

## Implementation references

- [`underhill_attestation`][attestation-rustdoc] prepares attestation requests,
  parses responses, and implements VMGS key protection.
- [`tee_call`][tee-rustdoc] provides access to hardware attestation and
  supported key derivation.

[attestation-rustdoc]:
  https://openvmm.dev/rustdoc/underhill_attestation/index.html
[tee-rustdoc]:
  https://openvmm.dev/rustdoc/tee_call/index.html

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

1. Obtain a local report and, for SNP, check it against the runtime TCB floor.
2. Derive destination hardware keys and seal the **unchanged** active DEK.
3. Verify the candidate protector, then write and flush it through the VMGS
   broker. A defensive active-key check prevents publishing a stale DEK if
   concurrent key rotation is introduced.
4. Verify again after persistence to detect hardware or SVN changes during the
   operation.

Recovery is enabled only when the successful boot-unlock attempt has sealed,
written, and flushed a hardware protector for the active DEK and established a
trusted runtime observation under a supported policy. This eligibility comes from
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

### Runtime TCB policy

Live migration is currently supported for TDX only. The resealing worker is also
enabled for eligible SNP VMs, so lack of SNP migration support does not prevent
the host from sending a completion notification without a migration.

For SNP, the trusted computing base (TCB) floor prevents runtime resealing at a
lower accepted security version. Boot establishes it from local reports already
obtained during platform-security initialization, including reports obtained
before failed SKR callouts. It is held in protected OpenHCL memory, not loaded
from host-controlled VMGS metadata.

The SNP-specific threat is a host lowering `reported_tcb` through `SNP_CONFIG`
and then triggering resealing on the same platform. This changes neither the
executing firmware nor the resident DEK, but OpenHCL selects that reported TCB
for key derivation. Without the floor, it could publish a protector recoverable
at a lower TCB. Report authenticity and successful unsealing alone do not reject
that policy change. See [AMD 56860][amd-snp], sections 3.4 and 8.7.

For TDX, migration admission is responsible for destination TCB policy before
transferring the VM. OpenHCL uses fresh local observations without comparing
their SVNs to earlier observations. A destination may therefore have lower or
incomparable SVNs, or a different module identity. The completion notification
triggers recovery; it does not establish that admission succeeded.

TDX sealing uses local module/CPU SVN metadata, not SNP's host-configurable
reported-TCB selection. Intel's [module loader][intel-seamldr], section 4.2,
rejects lower `SEAMSVN` during `UPDATE`; its [microcode update
architecture][intel-mcu], section 4.1, rejects loads below the committed MCU SVN.
Deferred commit can allow rollback of an uncommitted update. These checks bound
security SVNs, not build revisions, and do not establish that every rebootless
update preserves a running TD.

OpenHCL supplies `TEE_TCB_SVN` and `CPUSVN` to `TDG.MR.KEY.GET`. Across module
updates, `TEE_TCB_SVN` is creation-bound, unlike current-module `TEE_TCB_SVN2`
([TDX Module Base specification][intel-tdx-base], section 12.7.1). A fresh report
therefore need not select the newest module SVN. The MCU anti-rollback SVN is
also distinct from the raw CPU SVN array; platform validation must connect the
update rules to these sealing inputs. Local anti-rollback does not replace
destination admission.

Compatible higher SNP observations advance the floor. Retries, persistence
failures, reset, and normal stop/start cannot lower it. If boot cannot establish
a usable observation, runtime resealing stays disabled with a warning; existing
boot recovery behavior is unchanged.

If optional boot sealing does not succeed, no state is exported to the runtime
worker. When hardware sealing is only a backup, finalization failure disables
runtime recovery without failing boot. When sealing is required, or boot used
hardware unsealing to recover the DEK, failure to confirm the active sealed key
or flush completed writes fails boot. The full unlock/key-rotation sequence is
not automatically retried after this failure because it may already have changed
the active DEK. Existing protector write/unlock errors retain their handling.

The SNP comparison is a partial order, not a packed-integer or lexicographic
order:

- **SNP:** known TCB components must be non-decreasing within the same supported
  report version and CPU family/model; reserved bytes must remain equal.
  For report versions 3–5, Milan/Genoa (family `19h`, models `00h–1Fh`) and
  Turin (family `1Ah`, models `90h–AFh` and `C0h–CFh`) use distinct typed
  layouts, as defined by AMD 56860 section 2.3. Report version alone does not
  select the layout. Version 2 and unknown domains require exact SVN equality;
  cross-domain transitions are rejected. The floor tracks the reported SVN used
  for key derivation; it does not enforce the extended TCB added in report v6.
- **TDX:** each operation uses its fresh report's raw SVN arrays for derivation
  or candidate verification. No historical SVN is retained, and no historical
  minimum or module-identity comparison is applied.

Candidate verification also checks a fresh report. Its SVN must match the
candidate exactly, even if the hardware could still derive an older SVN's key.

For SNP, `SnpDomain` retains report version and CPU family/model because the raw
TCB bytes do not identify their own layout. CPU family/model selects the layout;
version gates supported report semantics. Exact domain equality is required even
for identical raw SVNs or CPUs sharing a layout. This conservative restriction
avoids interpreting one CPU's component bytes using another CPU's meanings.
See [AMD 56860][amd-snp], revision 1.59, section 2.3, tables 4 and 5.

[amd-snp]: https://docs.amd.com/v/u/en-US/56860_PUB_SEV_SNP
[intel-seamldr]: https://cdrdv2-public.intel.com/733584/343755-004-intel-tdx-seamldr-interface-specification.pdf
[intel-mcu]: https://cdrdv2-public.intel.com/782715/356111-ucode-update.pdf
[intel-tdx-base]: https://cdrdv2-public.intel.com/867568/intel-tdx-module-base-spec-348549008.pdf

### Lifecycle and saved state

Memory-preserving migration retains the worker and its sealing state. Stopping
the worker drains an in-flight recovery attempt. Reset does not request
resealing; it preserves any SNP floor, existing retries and backoff, and latched
notifications.

The stopped worker saves a versioned protobuf state unit named
`hardware_reseal`, containing:

- The accepted SNP TCB floor and report version/CPU comparison domain, or an
  initialized-TDX marker with no historical SVN.
- The serialized VM configuration used by the sealing KDF, checked against the
  destination configuration rather than used to replace it.
- Pending recovery, the failure count, and the latched migration notification.
- The remaining delay before another attempt is allowed, covering retry backoff
  or the minimum interval after success. This is bounded to 60 seconds and saved
  as a relative duration, not a process-specific clock value.

No DEK, hardware secret, derived key, or attestation report is saved by this
worker. Saving does not consume a notification or clear pending work: if another
unit fails to save, the original worker can resume with its obligations intact.

Restore constructs the worker without initialized sealing state, then installs
the validated saved state before starting. It does not obtain a fresh report to
initialize a new SNP floor or trust a VMGS protector as its source. Malformed or
unsupported state, an incompatible TEE or sealing configuration, or a decrease
relative to an existing resident SNP floor fails restore without replacing the
state. TDX requires an explicit matching initialized marker, not a saved SVN. A saved
worker also requires compatible hardware sealing support and encrypted VMGS.

Successful restore preserves retry backoff and merges saved notifications with
notifications received by the new worker. It always schedules a durable rewrite
when the worker starts, even when the saved worker was idle: hardware can change
while the worker is absent, including at the same SVN. Fresh SNP reports must
satisfy the restored floor before any new protector is created. A cached
matching protector does not discharge a failed-flush obligation.

If the saved VM has no `hardware_reseal` unit, runtime resealing stays disabled
after reconstruction. This accommodates state from runtimes that were not
enrolled or did not support resealer state, without inventing replacement
state. Binaries that cannot interpret this state unit cannot restore an enrolled
worker's snapshot.

```admonish warning title="Trusted state transfer"
Protobuf encoding does not authenticate the saved state. The enclosing transfer
must preserve integrity, bind the state to the VM, and prevent rollback to an
older snapshot. The resealer must not load this state from VMGS or
unauthenticated host metadata. Host-returned servicing state cannot initialize
this worker with the current transport and is rejected.

Resealer serialization does not enable isolated-VM servicing by itself. The
host-servicing isolation guard and unsupported SNP/TDX VP save/restore paths
remain in place. A trusted isolated-servicing handoff must satisfy the above
requirements before enabling that path.
```

### Recovery limits

Cold boot establishes a new runtime lifetime. The SNP floor is not a persistent
anti-rollback counter and does not prevent replay of an entire VMGS snapshot.
Rejecting resealing on a lower-TCB destination also cannot undo delivery of the
resident DEK by migration; migration admission is a separate security boundary.

```admonish warning
Recovery is best effort. A missed notification can leave the protector stale,
and a crash before durable destination resealing can leave hardware-only VMGS
unrecoverable. Report, derivation, and persistence operations are not atomic
with migration. Durability also depends on backing storage honoring flushes.
```
