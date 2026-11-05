# sandbox

Worker-process confinement with a Linux backend and optional PAL/Mesh launch
integration. Existing OpenVMM workers still use ordinary launches; adding this
crate does not enable sandboxing automatically.

- [Architecture overview](../../Guide/src/reference/architecture/sandbox.md):
  trust boundary, component responsibilities, launch lifecycle, and limits.
- [Crate API documentation](https://openvmm.dev/rustdoc/linux/sandbox/index.html):
  profile authoring and the `prepare`, `apply`, and `tighten` contracts.
- [Profile definitions](src/profiles): OpenVMM VM/TPM policies and shared bases.

## Platform support

| Platform | `prepare` | `apply` / `tighten` |
| -------- | --------- | ------------------- |
| Linux | yes | yes |
| Windows | LPAC launch data only | `Error::UnsupportedPlatform` |
| other | no platform-specific launch data | `Error::UnsupportedPlatform` |

PAL supports prepared sandbox launches on Linux only. Ordinary, unsandboxed
launches are unchanged.

Linux availability also depends on host policy permitting user namespaces with
UID/GID mapping and the mount, IPC, and network namespace operations selected by
the profile. These are requirements of sandboxed launches, not just of tests.
Ordinary, unsandboxed launches do not have these requirements.

On Ubuntu 24.04, AppArmor's unprivileged user namespace restrictions can deny
mapping or namespace-local capabilities even when creating a user namespace
succeeds. An administrator may need to approve the application through an
appropriate AppArmor policy; see the
[Ubuntu release notes](https://discourse.ubuntu.com/t/ubuntu-24-04-lts-noble-numbat-release-notes/39890#p-99950-unprivileged-user-namespace-restrictions-15).

## Testing

Run the ordinary PAL and sandbox tests with:

```sh
cargo nextest run --profile agent -p pal -p sandbox --all-features
```

Namespace-dependent tests are ignored by default because they require user
namespaces with UID/GID mapping and namespace operations permitted by host
policy. Run them as an unprivileged user on a suitably configured Linux host,
with `unshare` from util-linux available:

```sh
cargo nextest run --profile agent -p pal -p sandbox --all-features \
    --run-ignored only -E 'not test(::helper_)'
```

The filter excludes internal `helper_*` tests, which must only be invoked by
their parent tests after fixture setup. Explicitly selected namespace tests
still fail if a required operation is unavailable; they do not silently skip
permission errors or fall back to weaker isolation.
