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
