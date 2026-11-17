# AK Certificate Renewal

This page describes how OpenHCL-managed virtual TPM attestation-key (AK)
certificates are requested, retained, and replaced.

AK certification is separate from VMGS key release. See
[Attestation and VMGS Protection](attestation.md) for the surrounding flows.
The [IGVM Attestation Protocol](attestation_protocol.md) defines the exact
metadata encoding and response contract.

## Request lifecycle

When OpenHCL is configured to manage AK certificate renewal and the TPM permits
it, the TPM device requests a certificate at initialization and can request
renewal later. Only one certificate request is outstanding at a time.

For each request:

1. Read the current `TPM_NV_INDEX_AIK_CERT` contents, using the index's declared
   size rather than a fixed legacy-size buffer.
2. Decode the leading DER certificate and extract renewal metadata. Trailing
   NV padding or bytes left by an earlier, longer certificate are not part of
   that certificate.
3. Include successfully decoded metadata in the runtime claims before hashing
   them and obtaining the attestation report. Record whether metadata was
   included in this particular request.
4. Send the request through the host IGVM agent and process its response
   asynchronously.

An absent or uninitialized NV index supplies no metadata. Read or decode
failures are logged and omit the metadata, allowing normal issuance to proceed.
The decoder requires issuer and subject common names; the authority key
identifier is optional.

If including metadata would exceed GET's 4 KiB request limit, OpenHCL omits it
before hashing the claims and generating the attestation report. This is
unlikely with ordinary production certificates. A metadata-free request that
still exceeds the transport limit fails rather than being sent.

Metadata is read for each request, not cached separately. Certificate
replacement, TPM key refresh, and state restoration therefore do not require
maintaining a second copy of certificate metadata.

## Renewal outcomes

A nonempty certificate response follows the existing NV replacement path.
OpenHCL writes the returned certificate to the certificate index and advances
the renewal timer only after the write succeeds. NV write failures are logged
and emit a certificate-renewal-failed event without advancing the timer, so the
next eligible guest NV read can retry. NV writes are not atomic: a failure
after some chunks have been written can leave a partially replaced certificate.

A valid no-renewal response is accepted only if this request included
successfully decoded existing-certificate metadata. OpenHCL keeps the NV
contents without a write and advances the renewal timer. Without that
request-specific metadata, the same response is logged as a renewal failure
and does not advance the timer.

An empty transport response or an empty certificate payload indicates service
unavailability, not approval of the existing certificate. OpenHCL logs it,
leaves NV unchanged, and advances the timer to avoid retrying on every read.
Transport or response-parsing errors log a renewal failure without advancing
the timer.

Subsequent renewal is driven by guest NV reads and the existing 24-hour
renewal interval, rather than by an expiry-time alarm. The interval uses
OpenHCL system time, which can diverge from wall-clock elapsed time across
suspension and resumption.

## Trust and guest compatibility

Certificate metadata is a renewal hint. Decoding it does not validate the
certificate signature, chain, validity period, or relationship to the TPM AK.
The no-renewal signal is a host response, not a signed certificate-validation
result.

Guest-requested attestation reports remain separate from renewal requests.
They retain their V1 layout and do not read or include certificate renewal
metadata, preserving the existing guest-visible NV format.

## Implementation references

- [`tpm_device`][tpm-rustdoc] owns NV access, pending requests, and renewal
  scheduling.
- [`tpm_resources`][resources-rustdoc] defines the request helper interface
  and typed certificate/no-renewal outcomes.
- `openhcl/underhill_core/src/emuplat/tpm.rs` connects the TPM helper to
  metadata parsing, report generation, and GET.
- [`underhill_attestation`][attestation-rustdoc] extracts certificate metadata
  and builds and parses attestation messages.

[tpm-rustdoc]: https://openvmm.dev/rustdoc/tpm_device/index.html
[resources-rustdoc]: https://openvmm.dev/rustdoc/tpm_resources/index.html
[attestation-rustdoc]:
  https://openvmm.dev/rustdoc/underhill_attestation/index.html
