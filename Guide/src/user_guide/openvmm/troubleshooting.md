# Troubleshooting

This page includes a miscellaneous collection of troubleshooting tips for common
issues you may encounter when running OpenVMM.

### [Linux Host] Ensure access to `/dev/kvm/`

**Error:**


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
