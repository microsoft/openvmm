# IGVM Agent Test Server

`test_igvm_agent_rpc_server` is a Windows-only test double for the host IGVM
agent RPC interface used by guest attestation flows.

## Purpose

Attestation tests need deterministic host responses for conditions that are
difficult to reproduce with a real service. This executable hosts the expected
Windows RPC facade and can install predefined response plans before accepting
requests.

It is test infrastructure, not an IGVM agent suitable for deployment. The
implementation and scenario names follow the in-tree tests rather than a
stable external API.

## Normal orchestration

For tests that declare this artifact, Flowey:

1. Builds the server for Windows MSVC.
2. Copies it into the VMM-test content directory.
3. Starts it before nextest and redirects output to
   `test_igvm_agent_rpc_server.log`.
4. Verifies that it did not exit immediately.
5. Runs the selected attestation tests.
6. Terminates the background server during cleanup.

This lifecycle is part of `cargo xflowey vmm-tests-run`; most developers do not
need to start the process themselves.

## Key-release context coverage

The server selects per-VM behavior from the test name. Linux and Windows
guests use the same plans on SNP and TDX:

- `use_hw_unseal` supplies a context hash on the first V3 key release and
  retains it in memory. Later requests must contain that hash; the agent
  then returns a service error with hardware unsealing allowed. The test
  checks that recovery preserves the context, AK, and TPM NV data.
- `cvm_guest` (in both the TPM 1.85 and TPM 1.38 modules) returns a new
  hash on each successful V3 release. Each request must contain the hash
  issued in the preceding response. The tests verify changed guest-visible
  context across reboot while AK and NV data remain intact. VBS variants
  retain their ordinary attestation behavior.
- `ctx_v2` responds to V3 key requests with V2 responses. It verifies absent
  context, V3 hardware protectors, and persistent state across a cold boot.

These request checks include Hyper-V's automatic initial reboot. A failed
context assertion returns a distinct service error with hardware unsealing
disabled, so recovery cannot hide a failed test expectation. Parser and
boot-flow unit tests cover malformed and missing required context separately.

## Direct use

For focused Windows debugging, build and launch it directly:

```powershell
cargo build -p test_igvm_agent_rpc_server `
  --target x86_64-pc-windows-msvc
```

```powershell
.\target\x86_64-pc-windows-msvc\debug\test_igvm_agent_rpc_server.exe `
  --test-config AkCertRequestFailureAndRetry
```

The server continues running while it services RPC requests. Stop it after the
test so it does not affect a later scenario.

Some local test paths support an explicit autostart environment variable. Use
the instructions emitted by the test or its local-autostart helper rather than
running multiple server instances.

## Troubleshooting

- An immediate unsupported-platform failure means the executable was launched
  outside Windows.
- A startup failure can indicate that another instance owns the RPC endpoint.
- Check `test_igvm_agent_rpc_server.log` before guest logs when no host request
  reaches the planned scenario.
- Confirm that the selected test configuration matches the scenario expected
  by the Petri test.
- Stop stale instances before rerunning a different plan.
