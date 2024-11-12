# Configuration and Management

```admonish warning title="DISCLAIMER"
OpenVMM's configuration and management interfaces are currently unstable,
incomplete, lightly documented, and broadly speaking - not particularly
"polished".

These interfaces are **strictly for dev use only**.

Refer to the [OpenVMM disclaimer] for more context.
```

At the moment, OpenVMM exposes 3 distinct configuration and management interfaces.

- **CLI**: Used to configure and launch a single VM
  - This allows configuring static VM resource assignments, such as the
    number of processors, RAM size, UEFI, graphic console, etc.. as well as what
    devices are exposed to the Guest, such as a virtual NIC, Storage, vTPM,
    etc..
- **Interactive console**: Used to interact with a VM at runtime
  - This interfaces allows users to perform core VM operations such as stop,
    restart, save, restore, pause, resume, etc.. as well as things like storage
    hot-add, VTL2 servicing, running Inspect queries, etc..
- **gRPC / ttrpc**: A _very_ WIP set of APIs for configuring and interacting
  with VMs

## Missing Functionality (non-exhaustive)

The following is a non-exhaustive list of notable management features that
OpenVMM is currently missing.

<!-- NOTE: this is an HTML table, rather than a markdown table, as certain cells
contain long blocks of text, which aren't easy to write using standard markdown
tables. -->
<div class="table-wrapper">
<table>
    <thead>
        <tr>
            <th>Feature</th>
            <th>Status</th>
        </tr>
    </thead>
    <tbody>
        <tr>
            <td>Suspend / Resume a VM</td>
            <td>
                OpenVMM's existing save/restore infrastructure theoretically
                supports this, but the end-to-end flow has not been wired up to
                any management interface at the moment.
            </td>
        </tr>
        <tr>
            <td>Snapshots</td>
            <td>
                OpenVMM has core infrastructure for performing save/restore operations,
                but there are gaps in device support (notably: no support for storage snapshots).
            </td>
        </tr>
        <tr>
            <td>
                Managing multiple running VMs
            </td>
            <td>
                <p>OpenVMM currently runs a single VM per-process.</p>
                <p>It is not yet clear whether OpenVMM will support managing
                multiple VMs via a single OpenVMM process, or if OpenVMM will
                rely on external management tools (e.g: `libvirt`) interfacing
                with its existing APIs in order to launch and manage multiple VMs.</p>
            </td>
        </tr>
    </tbody>
</table>
</div>

If a feature you is missing from this list, please check if the feature is being
tracked via a Issue on the OpenVMM GitHub, and/or submit a PR adding it to this
list.

[OpenVMM disclaimer]: ../../user_guide/openvmm.md#admonition-disclaimer
