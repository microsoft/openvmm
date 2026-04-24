# Preface: CVM restrictions

When OpenHCL detects that it is running as a Confidential VM it will restrict the diagnostics it
sends to the VM host. This is done in order to prevent any guest secrets from being leaked to the
host.

```admonish info
Unless otherwise noted, all of the following restrictions only apply to
_release_ builds of OpenHCL for CVMs. The majority of these restrictions will
not apply to debug builds of OpenHCL.

This is controlled by the `enable_debug` flag in the IGVM JSON definition.
```

## Simulating CVM restrictions

Most of these restrictions can be simulated on a non-CVM OpenHCL VM by setting the
`OPENHCL_CONFIDENTIAL` environment variable to `1`, either in your IGVM JSON definition or by
using the `Set-VmFirmwareParameters` cmdlet. This environment variable will cause OpenHCL to
behave as if it is running in a CVM for the purpose of diagnostics.

## Tracing

Tracing statements and spans will still be sent to the host, and therefore will still show up in
ETW traces and Kusto. However, individual statements may opt out of being logged inside a CVM, as a
way of protecting guest secrets.

### For Developers

This is done by using the `CVM_CONFIDENTIAL` constant provided by the
`cvm_tracing` crate. `cvm_tracing` also provides a `CVM_ALLOWED` constant, to
mark statements that do not contain secrets and can be logged in a CVM.

Examples:

```rust,ignore
use cvm_tracing::{CVM_ALLOWED, CVM_CONFIDENTIAL};

tracing::info!(CVM_ALLOWED, foo, ?bar, "This statement will be logged in a CVM");
tracing::info!(baz, "This statement will also be logged in a CVM");
tracing::info!(CVM_CONFIDENTIAL, super_secret, "This statement will not be logged in a CVM");

// This also works with spans.
let span = tracing::info_span!("a span", CVM_CONFIDENTIAL);
my_func.instrument(span).await;

// And the #[instrument] macro.
#[instrument(name = "foo", fields(CVM_CONFIDENTIAL))]
fn my_func() {
    // ...
}
```

```admonish tip
Some of the tracing macros will not accept `cvm_tracing::CVM_CONFIDENTIAL` as an
argument.

Instead, you will need to `use cvm_tracing::CVM_CONFIDENTIAL`, and then use just
`CVM_CONFIDENTIAL`.
```

## ohcldiag-dev

Most ohcldiag-dev commands will not work when connecting to a CVM.

One notable exception is the `inspect` command (albeit with restrictions).

### inspect

The available inspect nodes for a CVM are restricted to prevent exposing guest data.

The `vm/` top-level node is inaccessible, however most nodes containing
information about the VTL2 processes are still available.

## Crash information

Crash dumps can leak quite a bit of information, and as such, are heavily
restricted in CVMs.

### Dumps

Crash dumps will not be generated when a crash occurs in a CVM's VTL2.

### Hyper-V MSRs

The Hyper-V crash MSRs will still be set when a crash occurs in a CVM's VTL2, but the data values
will be sanitized to prevent leaking guest secrets. This will result in Hyper-V logging that a
crash occurred, but there will be no debugging information available.

> NOTE: This restriction also applies to _debug_ builds of OpenHCL when running a CVM.

> NOTE: This restriction cannot be simulated using `OPENHCL_CONFIDENTIAL`.

## Saved state

Extracting the save state of a CVM is not supported. This applies both to the `ohcldiag-dev save` command,
and to the save-on-crash registry key.
