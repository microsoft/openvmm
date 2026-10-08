# vtpm_util

`vtpm_util` creates and inspects file-backed vTPM provisioning artifacts.

The tool currently uses the Microsoft TPM 2.0 reference implementation version
1.38. A vTPM blob contains the serialized nonvolatile state of that TPM,
including persistent objects created during provisioning. The commands in this
tool do not seal or unseal guest data.

```admonish warning
Treat vTPM blobs and TPM import blobs as security-sensitive files. An import
blob produced by this tool contains RSA private-key material and is not
protected by TPM authorization until another provisioning component imports
it into a TPM.
```

The tool refuses to overwrite existing vTPM blobs and TPM import blobs. On
Unix, these files are created with mode `0600`, independent of the process
umask. On Windows, newly created files inherit the destination directory's
access control list, so use a directory restricted to the intended user.

## Building

Build the tool from the repository root:

```bash
cargo build -p vtpm_util
```

Use `cargo run -p vtpm_util --` in place of the binary name in the examples
below. Pass `--help` to see the current command interface.

## Creating a vTPM blob

Create a new vTPM and write its serialized state to a file:

```bash
cargo run -p vtpm_util -- create-vtpm-blob path/to/vtpm.blob
```

The command creates an RSA storage root key (SRK) and persists it at the
standard SRK handle before saving the vTPM state.

## Exporting the SRK public area

Export the persisted SRK public area as a `TPM2B_PUBLIC` structure:

```bash
cargo run -p vtpm_util -- write-srk \
    path/to/vtpm.blob path/to/srk.tpm2b
```

Print the TPM key name for an exported SRK:

```bash
cargo run -p vtpm_util -- print-key-name path/to/srk.tpm2b
```

The printed name consists of the public area's name-algorithm identifier
followed by the digest of the serialized public area. The command displays the
result in Base64.

## Creating an RSA import blob

Generate an RSA-2048 key pair, write its public key in DER PKCS#1 format, and
write its private material in TPM 2.0 import format:

```bash
cargo run -p vtpm_util -- \
    create-random-key-in-tpm2-import-blob-format rsa \
    path/to/public.der path/to/private.tpm2b
```

The import file contains a `TPM2B_PUBLIC`, a `TPM2B_PRIVATE`, and an empty
`TPM2B_ENCRYPTED_SECRET`, in that order. Only RSA keys are supported.
