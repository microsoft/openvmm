# NetVSP Fuzzing

## NetVSP fuzzing design

- Attempt to catch bugs that appear after a sequence of state transitions.
  - A test consumes one byte stream and interprets it as a sequence of actions, not just one packet.
  - The harness panics on timeout (`500ms`) to turn hangs into detectable failures.
- Add support for `Arbitrary` to NVSP/RNDIS protocol types.
  - Enables control, OID, and data fields to be mutated directly.
  - Example: `#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]`
- Provide a robust fuzzing dictionary `netvsp_rndis.dict`.
  - Includes NVSP versions, message IDs, transaction IDs, RNDIS message types, OIDs, status codes, and handshake payload fragments.
- Provide infrastructure mocks to remove external environment setup.
  - Fuzzing tests are focused on parser and state-machine behavior.
- Split fuzzing into separate fuzz targets.
  - Faster triage as crashes are scoped to a domain.
  - Each corpus can specialize.

### What each `fuzz_netvsp_*` target fuzzes

Legend: **✓ primary focus**, **~ partial/indirect coverage**

| Target | NVSP control | RNDIS OID | TX packet path | RX packet path | Link events | VF/SR-IOV | Subchannels | Save/Restore |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `fuzz_netvsp_control` | ✓ | ~ | ~ | ~ |  |  | ~ |  |
| `fuzz_netvsp_oid` | ~ | ✓ |  |  |  |  |  |  |
| `fuzz_netvsp_tx_path` | ~ | ~ | ✓ | ~ |  |  |  |  |
| `fuzz_netvsp_rx_path` | ~ | ~ | ~ | ✓ |  |  |  |  |
| `fuzz_netvsp_interop` | ✓ | ✓ | ✓ | ✓ | ~ | ~ | ~ |  |
| `fuzz_netvsp_link_status` | ~ |  | ~ | ~ | ✓ |  |  |  |
| `fuzz_netvsp_vf_state` | ~ |  | ~ |   |  | ✓ |  |  |
| `fuzz_netvsp_subchannel` | ✓ |  | ✓ | ~ |  |  | ✓ |  |
| `fuzz_netvsp_save_restore` | ~ | ~ | ~ | ~ | ~ | ✓ | ~ | ✓ |

### Execution model of `fuzz_netvsp_interop`

1. Negotiates NetVSP to ready state.
2. Mostly initializes RNDIS (90% of runs), but sometimes deliberately skips it (10%) to test pre-init interactions.
3. Runs arbitrary interleavings until input is exhausted.
4. Periodically drains queue to preserve pressure while preventing backlog stalls.

## Mocks in NetVSP fuzz tests

- **`FuzzEndpoint` (loopback-based endpoint wrapper)**
  - Controllable RX packet injection and endpoint
    action injection (example: link notifications).
  - Source pointers: `fuzz_helpers/endpoint.rs`

- **`FuzzMockVmbus` + `MultiChannelMockVmbus`**
  - In-process VMBus, with offer/open lifecycle, GPADL
    registration, ring plumbing, and server request handling hooks.
  - Source pointers: `fuzz_helpers/vmbus.rs`

- **`FuzzVirtualFunction` (mock VF provider)**
  - VF ID updates and state-change signaling hooks.
  - Source pointers: `fuzz_helpers/vf.rs`

- **`FuzzNicConfig` + NIC setup/teardown**
  - NIC on a mock VMBus channel, with reproducible memory layout,
    send and receive buffers, and customizability for different tests.
  - Source pointers: `fuzz_helpers/nic_setup.rs`

- **`SubchannelOpener` and multi-channel mock state**
  - Synthetic opening of pending subchannel offers, including GPADL + ring handshake
    for each opened subchannel.
  - Source pointers: `fuzz_helpers/nic_setup.rs`

- **Reusable fuzz loop/runtime controls in `fuzz_helpers`**
  - Common `run_fuzz_loop*`, timeout guardrails, queue drain helpers,
    and canonical negotiation helpers.
  - Source pointers: `fuzz_helpers/mod.rs`

## Helper xtask commands

Run all netvsp fuzz tests for 1 minute (default is 5).
Note: To run with sanitizers, supply a toolchain that supports unstable compiler features.

`cargo xtask fuzz netvsp 1 --toolchain nightly`

```
==============================================
 Campaign Complete
==============================================
  fuzz_netvsp_control: clean
  fuzz_netvsp_tx_path: clean
  fuzz_netvsp_oid: clean
  fuzz_netvsp_interop: clean
  fuzz_netvsp_rx_path: clean
  fuzz_netvsp_link_status: clean
  fuzz_netvsp_vf_state: clean
  fuzz_netvsp_subchannel: clean
  fuzz_netvsp_save_restore: clean

 Total (new): 0 crashes, 0 timeouts, 0 slow-units, 0 OOM
 Targets failed: 0 / 9
```

Collect coverage data from all netvsp fuzz tests.
Note: To run with sanitizers, supply a toolchain that supports unstable compiler features.

`cargo xtask fuzz netvsp-coverage --toolchain nightly`

```
----------------------------------------------
 Per-File Coverage
----------------------------------------------

  buffers.rs                                116 /  153 lines ( 75.8%)  8/11 fn (72.7%)
  lib.rs                                   3088 / 3529 lines ( 87.5%)  205/218 fn (94.0%)
  protocol.rs                                 0 /    9 lines (  0.0%)  0/3 fn (0.0%)
  resolver.rs                                 0 /    2 lines (  0.0%)  0/1 fn (0.0%)
  rndisprot.rs                               85 /  147 lines ( 57.8%)  20/36 fn (55.6%)
  rx_bufs.rs                                 74 /   78 lines ( 94.9%)  11/11 fn (100.0%)
  saved_state.rs                             11 /   11 lines (100.0%)  3/3 fn (100.0%)
```
