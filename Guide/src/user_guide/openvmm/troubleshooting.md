# OpenVMM Troubleshooting

This page includes a miscellaneous collection of troubleshooting tips for common
issues you may encounter when running OpenVMM.

If you are still running into issues, consider filing an issue on the OpenVMM
GitHub Issue tracker.

### failed to open `/dev/kvm/`

**Error:**

```text
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
