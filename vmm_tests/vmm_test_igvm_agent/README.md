# Use test IGVM Agent for local single-test runs

Windows-only helper to start `test_igvm_agent_rpc_server.exe` when it is not already running
(flowey still owns CI). For local nextest runs we assume one test process at a time; the helper
starts the server at test begin, streams stderr into the test output, and tears it down via a
guard when the test ends. Concurrency is not supported or guaranteed.

Local runs must explicitly opt-in to the helper by exporting
`VMM_TEST_IGVM_AGENT_LOCAL_AUTOSTART=1` (or `true`/`yes`). CI never sets this variable so tests
fail loudly if Flowey forgets to launch the server. The crate exposes this variable as the
`LOCAL_AUTOSTART_ENV` constant for reuse in callers.
