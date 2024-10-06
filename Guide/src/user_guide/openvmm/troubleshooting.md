# Troubleshooting

This page includes a miscellaneous collection of troubleshooting tips for common
issues you may encounter when running OpenVMM.

If you are still running into issues, consider filing an issue on the OpenVMM
GitHub Issue tracker.

### failed to invoke protoc

**Error:**

```
error: failed to run custom build command for `inspect_proto v0.0.0 (/home/daprilik/src/openvmm/support/inspect_proto)`

Caused by:
  process didn't exit successfully: `/home/daprilik/src/openvmm/target/debug/build/inspect_proto-e959f9d63c672ccc/build-script-build` (exit status: 101)
  --- stderr
  thread 'main' panicked at support/inspect_proto/build.rs:23:10:
  called `Result::unwrap()` on an `Err` value: Custom { kind: NotFound, error: "failed to invoke protoc (hint: https://docs.rs/prost-build/#sourcing-protoc): (path: \"/home/daprilik/src/openvmm/.packages/Google.Protobuf.Tools/tools/protoc\"): No such file or directory (os error 2)" }
  note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
warning: build failed, waiting for other jobs to finish...
```

Note: the specific package that throws this error may vary, and may not always be `inspect_proto`

**Solution:**

You attempted to build OpenVMM without first restoring necessary packages.

Please run `cargo xflowey restore-packages`, and try again.

### failed to open `/dev/kvm/`

**Error:**

```
fatal error: failed to launch vm worker

Caused by:
    0: failed to launch worker
    1: failed to create the prototype partition
    2: kvm error
    3: failed to open /dev/kvm
    4: Permission denied (os error 13)
```

**Solution:**

When launching from a Linux/WSL host, your user account will need permission to
interact with `/dev/kvm`.

For example, you could add yourself to the group that owns that file:

```bash
sudo usermod -a -G <group> <username>
```

For this change to take effect, you may need to restart. If using WSL2, you can
simply restart WSL2 (run `wsl --shutdown` from Powershell and reopen the WSL
window).

Alternatively, for a quick-and-dirty solution that will only persist for the
duration of the current user session:

```bash
sudo chown <username> /dev/kvm
```
