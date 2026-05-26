---
applyTo: "vmm_tests/**,petri/**"
---

# Running VMM Tests

VMM tests are **not** regular unit tests. Do NOT run them with
`cargo nextest run -p vmm_tests` or `cargo test -p vmm_tests` — they require
external artifacts (disk images, firmware, OpenHCL binaries) that won't be
present, causing confusing failures.

Use `cargo xflowey vmm-tests-run` instead. It automatically discovers
artifacts, builds dependencies, and runs tests in a single command:

```bash
cargo xflowey vmm-tests-run --filter "test(my_test_name)" --dir /tmp/vmm-tests-run
```

To learn the full workflow (filter syntax, cross-compilation, logging, common
pitfalls), load the `vmm-tests` skill.
