# Attestation and VMGS Protection

This page describes boot-time attestation, VM Guest State (VMGS) protection,
key-release context adoption, and hardware recovery in OpenHCL.

```admonish note title="See also"
[IGVM Attestation Protocol](attestation_protocol.md) specifies request and
response versions, binary framing, JSON envelopes, and context encoding.
```

## Boot lifecycle

During platform security initialization, OpenHCL obtains the keys needed to
unlock encrypted VMGS before using its protected state. Secure key release
(SKR) obtains a tenant key-encryption key through attestation; it is not the
VMGS data-encryption key (DEK). Key protectors and guest state protection
(GSP), as required by the encryption scheme, provide the material used to
recover the DEK. Hardware sealing provides a local recovery path when the
required services are unavailable.

The boot flow is:

1. Read the security profile and any hardware protector from VMGS. Load the
   cached context before creating requests or retrying SKR.
2. For stateful operation, create an ephemeral RSA transfer key and runtime
   claims. Obtain a TEE report binding the claims hash, then request wrapped
   key material and perform SKR through the host IGVM agent. Whether wrapped
   key material is required depends on the existing VMGS state.
3. Validate the response and unwrap the released key. Derive ingress keys
   (for existing state) and egress keys (for state to be written), or attempt
   eligible hardware recovery.
4. Unlock VMGS and persist the required key/protector updates. Return the
   successfully adopted context for later attestation claims.

### Stateful and stateless operation

Stateful operation uses SKR; hardware sealing is a backup, not a replacement
for the normal attestation flow. A host request for exclusive hardware
sealing in stateful mode does not bypass SKR.

Stateless operation suppresses SKR and does not adopt a response context.
Normally it bypasses VMGS encryption. With a supported TEE, an enabled
hardware sealing policy, and a host `HardwareSealing` encryption request,
hardware sealing instead becomes the exclusive VMGS protection source.
That path creates a random DEK and rotates it on subsequent encrypted boots.
An unusable exclusive hardware sealing configuration fails rather than
silently disabling encryption.

Opening a context-bound V4 protector with attestation suppressed is rejected
rather than silently dropping its hash. The legacy runtime claim
`tpm-persisted` denotes stateful mode, not whether TPM state is actually
persisted in every configuration.

### Hardware sealing policy

The boot configuration selects `HardwareSealingPolicy`. Hardware sealing
also requires a TEE that supports key derivation.

| Policy | Hardware derivation behavior |
| --- | --- |
| `None` | Does not enable exclusive hardware sealing. |
| `Hash` | Mix the OpenHCL measurement into the hardware key. |
| `Signer` | Use measurement-independent derivation where supported. |

`Hash` binds recovery to the measured OpenHCL image. `Signer` permits
measurement changes, subject to the TEE's identity and SVN constraints and
the VM configuration binding described below. TDX does not support
signer-based sealing: exclusive use fails, while stateful SKR can proceed
without a hardware backup. A protector records the derivation SVN and
`mix_measurement` policy used to seal it; recovery must use that recorded
policy, consistent with the current VM configuration.

## Context trust boundary

The host-provided `key_release_context_hash` is context for key derivation,
not an authorization decision. The host extension has no signed authenticity
of its own. A successful release and key unwrap permit adoption of the
context, but do not prove that the service signed or authenticated this
extension. Existing service-payload checks and key unwrap remain necessary.

The value represents a service-defined SHA-256 digest. OpenHCL validates its
representation, not its construction. The digest input, key identity, policy
representation, framing, and any domain separation must be agreed externally
with the service. The protocol does not specify a concatenation or
canonicalization algorithm for those inputs. See the protocol's
[context hash encoding](attestation_protocol.md#context-hash-encoding) for
the wire representation and normalization rules.

## Stateful context lifecycle

1. Before SKR or its retries, OpenHCL reads `HW_KEY_PROTECTOR`. A structurally
   valid V4 header supplies a cached hash. A missing entry or legacy/V3 entry
   supplies `None`. Malformed existing protectors are errors, not absence.
2. The cached hash is an **untrusted hint** until hardware unsealing verifies
   the HMAC. OpenHCL puts that hint in the new request's runtime claims as
   `key-release-context-hash`. It does not use a caller-supplied configuration
   snapshot as the source of the binding.
3. A successful key-release response must match a present cached hash. A
   changed or omitted required hash, including a legacy response without
   one, is rejected **before parsing the inner JSON/JWT or unwrapping its
   key**, so malformed inner key material cannot conceal a mismatch and
   enable fallback.
4. With no existing hash, a valid response may supply one. Only successful
   key release and key unwrap allow that hash into the egress sealing
   configuration. After VMGS unlock succeeds, the adopted value is exposed
   to later runtime claims. A response arriving after report creation cannot
   retroactively bind that report.
5. When hardware sealing succeeds, the new protector uses V4 if a context
   hash is present, or V3 if it is absent. A successful V2 response or V3
   response that omits the hash can therefore produce a V3 protector when
   no cached hash is required. An all-zero 32-byte hash is a present value,
   not an absence sentinel. Successful SKR does not require a hardware
   backup to be available.

```admonish warning title="Absence is not first-boot authorization"
A missing or legacy protector is not proof of first boot: it can also occur
on later boots without a stored hash. Neither a cached host-controlled hint
nor its absence authorizes initial key release. A report binds the claims
bytes it contains, not the truth or service authenticity of those claims.
```

## Hardware recovery and KDF binding

For a service outage, existing recovery eligibility and the
`skip_hw_unsealing` signal still apply. Protocol/context failures instead
fail closed, as specified in the protocol's
[failure handling](attestation_protocol.md#limits-and-failure-handling).

Recovery restores only the cached hash and recorded hardware derivation
policy from the protector. The KDF uses the **current remaining VM
configuration**, with `current_time = None`, and the canonical text encoding
of the stored hash. It does not restore a historical configuration snapshot.
Other KDF-bound configuration must still match; changing it can prevent
recovery even when the hash is unchanged.

Key derivation has two stages:

1. The TEE derives a hardware secret using the recorded SVN and measurement
   policy. This stage does not take the serialized `AttestationVmConfig`.
2. OpenHCL uses that secret as the key for HMAC-SHA-256 KBKDF, with label
   `ISOHWKEY` and the UTF-8 JSON serialization of `AttestationVmConfig` as
   the KDF context. The 64-byte output is split into a 32-byte AES key and
   a 32-byte HMAC key used to seal and authenticate the VMGS DEK.

The context hash therefore contributes through the second stage, alongside
every other serialized VM configuration field. It is not merely metadata in
the protector, nor is it a separate hardware key-derivation parameter. Its
canonical lowercase hex string is included when present and omitted when
absent. The SKR request can include the current host time; the boot sealing
and recovery paths explicitly set `current_time = None` for the KDF.

```admonish warning title="Keep the KDF configuration consistent"
Sealing and unsealing must reproduce the same serialized KDF context, not
just the same hardware secret and context hash. Changes to other bound VM
settings, JSON field names/order, or optional-field serialization can change
the AES/HMAC keys. Protector authentication then fails before decryption,
so hardware recovery cannot recover the DEK.

V4 persists only the context hash, not the complete configuration. The other
fields are reconstructed from current VM settings. Treat changes to this
serialization as a key-compatibility change requiring a recovery or migration
plan; do not bypass HMAC verification to accommodate them.
```

Only successful HMAC verification makes the cached hash eligible for
adoption through hardware recovery. Recovery without successful SKR reseals
the recovered DEK using the authenticated stored context. If SKR succeeds
while another required service is unavailable, recovery uses the stored
ingress context to unseal and the released context for egress sealing. This
can upgrade an unbound legacy protector to V4; it does not permit changing
an already-required hash.

## Hardware protector formats

V4 `HW_KEY_PROTECTOR` entries store the decoded 32 raw hash bytes, not their
hex string. Earlier versions contain no context hash. The exact version and
entry size identify the layout. Protector versions are independent of the
IGVM response versions.

| Version | Entry size | Header size | Recovery support | New writes |
| --- | --- | --- | --- | --- |
| V2 | 104 | 24 | Legacy SNP | No |
| V3 | 128 | 48 | SNP and TDX, no context hash | Without a hash |
| V4 | 160 | 80 | SNP and TDX, context-bound | With a hash |

Sizes and offsets in this section are in bytes. V1 shares the 104-byte
legacy layout but has no usable derivation policy in this implementation.

### V3 and V4 headers

The common V3/V4 header fields have these offsets:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 4 | `version` (3 or 4) |
| 4 | 4 | `length` (128 or 160) |
| 8 | 4 | `tee_type` (0 = SNP, 1 = TDX) |
| 12 | 32 | TEE-specific `svn` |
| 44 | 1 | `mix_measurement` (0 or 1) |
| 45 | 3 | Reserved, zero |

For SNP, `svn[0..8]` is the little-endian reported TCB and `svn[8..32]` is
zero. For TDX, `svn[0..16]` is `TEE_TCB_SVN` and `svn[16..32]` is `CPU_SVN`.
V4 appends its 32-byte raw context hash at offset 48, making its header
80 bytes rather than V3's 48 bytes.

### Payload offsets and authentication

| Field | Size | V2 offset | V3 offset | V4 offset |
| --- | --- | --- | --- | --- |
| Raw context hash | 32 | Absent | Absent | 48 |
| AES-CBC IV | 16 | 24 | 48 | 80 |
| Encrypted key | 32 | 40 | 64 | 96 |
| HMAC-SHA-256 | 32 | 72 | 96 | 128 |

The HMAC covers the complete header, IV, and ciphertext: bytes `[0, 72)` in
V2, `[0, 96)` in V3, and `[0, 128)` in V4. Thus V4 authenticates the hash
and every other header byte. Unsealing verifies the HMAC before decrypting.
Unknown versions, incorrect entry sizes or lengths, invalid TEE tags,
invalid measurement flags, or invalid reserved/SNP padding bytes are rejected
for V3/V4. Legacy metadata retains its historical validation rules.

## Compatibility and rollback

With `Hash` policy, an OpenHCL update that changes the measurement prevents
unsealing a protector created by the previous image. Stateful operation must
obtain the required keys through SKR to unlock VMGS and create a replacement
hardware protector. The stored V4 context hash is still read before SKR:
measurement changes do not clear the requirement for a matching response
hash.

```admonish warning title="Rollover, replay, and binary rollback"
There is no context-hash rollover protocol: changing or removing an existing
required hash is rejected. This binding adds no anti-replay mechanism or
monotonic freshness counter; an HMAC alone does not prevent replay of old
valid state. Deleting a protector is not an authenticated reset procedure.

Older binaries without V4 support cannot read a V4 protector. Once V4 is
persisted, binary rollback can lose hardware recovery and can prevent boot
when that protector is needed. Do not strip the hash or reinterpret V4 as
V3 to enable rollback; deployment and recovery plans must account for the
on-disk version change.
```

## Implementation references

- [`openhcl_attestation_protocol::vmgs`][vmgs-rustdoc] defines the on-disk
  protector layouts.
- [`underhill_attestation`][attestation-rustdoc] implements secure key
  release, VMGS validation, and hardware sealing/recovery.
- [`underhill_core`][core-rustdoc] propagates the adopted context to later
  attestation configuration.

[vmgs-rustdoc]:
   https://openvmm.dev/rustdoc/openhcl_attestation_protocol/vmgs/index.html
[attestation-rustdoc]:
   https://openvmm.dev/rustdoc/underhill_attestation/index.html
[core-rustdoc]:
   https://openvmm.dev/rustdoc/underhill_core/index.html
