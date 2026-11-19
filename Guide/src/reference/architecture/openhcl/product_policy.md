# Measured Product Policy

The measured product policy is configuration that an OpenHCL IGVM image
carries in its initrd. OpenHCL enforces it at boot, and the launch measurement
covers it.

Three parts of the build and boot share this contract:

1. The `openhcl` image in the IGVM manifest names a product policy in
   `product_policy.policy` and, optionally, a PCI device policy file in
   `product_policy.device_policy`, relative to the manifest:

   ```json
   "openhcl": {
       "product_policy": {
           "policy": { "azure": {} },
           "device_policy": "policies/azure-cvm.toml"
       },
       ...
   }
   ```

2. [`igvmfilegen`](../../../dev_guide/dev_tools/igvmfilegen.md) adds the
   device policy's settings to the product policy and appends the result to
   the OpenHCL initrd as `/openhcl-product-policy`. Because the initrd is
   measured, one OpenHCL binary can serve images with different policies, and
   attestation covers each image's policy.

3. OpenHCL reads the file at startup and enforces it.

An image without the file has no policy. OpenHCL fails to boot if the file
can't be read or decoded, or if it holds fields that this OpenHCL version
doesn't know.

```admonish note title="See also"
The [`product_policy`](https://openvmm.dev/rustdoc/linux/product_policy/index.html)
crate for every field.
```

## Products

Each policy names one product:

| Product | What OpenHCL enforces |
|---|---|
| `sivm` | Secure boot, if `require_secure_boot` is set, and the device policy |
| `cwcow` | Secure boot, if `require_secure_boot` is set, and the device policy |
| `azure` | Only the device policy |

If a product requires secure boot and the VM's settings disable it, OpenHCL
fails to boot. The products' other fields are reserved and aren't enforced yet.

## PCI device policy

Every product carries the settings from the image's device policy file: the
PCI devices that OpenHCL's VPCI relay may offer to the guest, and when the
relay runs. Only the device policy file can set them; the product policy in the
manifest can't.

### Device policy file

A device policy names a device catalog, relative to the policy file, and the
devices it allows. `"*"` allows every device.

```toml
devices = "devices.toml"
allowed_pci_devices = ["nvme", "mana"]
vpci_relay = "default_on"
```

`vpci_relay` sets whether the relay runs by default, and whether the
`OPENHCL_ENABLE_VPCI_RELAY` environment variable can change that:

| `vpci_relay` | Relay by default | `OPENHCL_ENABLE_VPCI_RELAY` |
|---|---|---|
| `"default_off"`, used if the file doesn't set `vpci_relay` | Off | `1` turns it on |
| `"default_on"` | On | `0` turns it off |
| `"must_be_on"` | On | Ignored |

The relay is required when the policy is `"must_be_on"` or the variable is `1`.

The environment variable comes from the kernel command line. The host can add to
it only if the manifest sets `static_command_line` to false and OpenHCL trusts
the host: the guest isn't isolated, or the image's measured command line enables
confidential debugging. Otherwise only the measured command line sets it.

The catalog defines each device once, with any of `vendor_id`, `device_id`,
`revision_id`, `base_class`, `sub_class`, and `prog_if`. A field that is left
out matches any value, but an allowed device must set at least one:

```toml
[nvme]
base_class = 0x01
sub_class  = 0x08
prog_if    = 0x02
```

`igvmfilegen` rejects unknown device names or fields.

### OpenHCL behavior

When the relay runs, it offers the guest only the allowed devices:

- OpenHCL reserves high VTL0 MMIO space and VMBus ring memory for 64 devices.
  Without enough high VTL0 MMIO space, OpenHCL fails to boot. More than 64
  devices at boot also make it fail to boot; a device offered later beyond
  the 64 isn't relayed, and OpenHCL logs an error.
- The relay needs a device policy that allows at least one device.
- If the relay can't run, OpenHCL logs a warning that names the missing
  requirement, and VPCI devices reach the guest through the VMBus relay
  without filtering, as when the relay is off. If the relay was required,
  OpenHCL fails to boot with that reason instead.

## In-tree images

The policies are in `vm/loader/manifests/policies`:

| Manifests | Product | Device policy |
|---|---|---|
| `openhcl-x64-cvm-{dev,release}.json` | `azure` | `azure-cvm.toml`: NVMe, MANA, and the Azure Integrated HSM; `default_on` |
| `openhcl-x64-{dev,release}.json`, `openhcl-x64-direct-{dev,release}.json`, `openhcl-aarch64-{dev,release}.json` | `azure` | `azure.toml`: the same devices; `default_off` |
| `openhcl-x64-cvm-sivm-{dev,release}.json` | `sivm` | `sivm.toml`: a GPU; `must_be_on` |
| `openhcl-x64-test-sivm.json` | `sivm` | None |
