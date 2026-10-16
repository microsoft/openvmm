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
