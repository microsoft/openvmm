# TPM Guest Test Utility

`tpm_guest_tests` is an in-guest CLI for probing TPM 2.0 and OpenHCL
attestation behavior from Petri tests.

## Execution environment

The binary runs inside a Linux or Windows guest with a visible TPM, normally an
OpenVMM or OpenHCL vTPM. It does not operate on a VMGS file and is not a
host-side TPM provisioning utility.

On Unix, it opens `/dev/tpmrm0` first and falls back to `/dev/tpm0`. On Windows,
it sends commands through TPM Base Services. The tool adapts those transports
to the shared `tpm_lib` command implementation.

Petri usually copies the architecture-appropriate executable into the guest
and runs it through Pipette. You can also invoke it from an interactive guest
shell.

## Building and deployment

Let VMM-test artifact discovery build the correct Linux or Windows guest
binary:

```bash
cargo xflowey vmm-tests-run \
  --build-only \
  --filter "test(tpm)"
```

A focused Linux build can be produced with:

```bash
cargo build -p tpm_guest_tests \
  --target x86_64-unknown-linux-musl
```

The binary must still be copied into a guest with the corresponding
architecture and OS before it is useful.

## Exit status and diagnostics

Successful commands exit with status zero. CLI errors, TPM transport failures,
malformed responses, oversized data, and comparison failures print an error
chain and exit nonzero. NV reads include a bounded hexadecimal and ASCII dump
to aid diagnosis without flooding the test log.

If opening the TPM fails, confirm that the VM configuration exposed a vTPM and
that the guest user can access its device or TBS context. An AK certificate may
appear asynchronously during attestation initialization; use comparison retry
only when the test expects that transition.
