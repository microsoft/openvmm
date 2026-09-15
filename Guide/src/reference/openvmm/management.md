# Configuration and Management

OpenVMM exposes three configuration and management interfaces. The CLI and
interactive console support direct VM configuration and lifecycle operations,
while gRPC and ttrpc provide programmatic control.

```admonish note title="Interface compatibility"
These interfaces continue to evolve and may change between releases. Consult
the documentation for your OpenVMM version, especially when upgrading.
```

- **CLI**: Used to configure and launch a single VM
  - This allows configuring static VM resource assignments, such as the
    number of processors, RAM size, UEFI, graphic console, etc.. as well as what
    devices are exposed to the Guest, such as a virtual NIC, Storage, vTPM,
    etc..
- **Interactive console**: Used to interact with a VM at runtime
  - This interface allows users to perform core VM operations such as stop,
    restart, save, restore, pause, resume, etc.. as well as things like storage
    hot-add, VTL2 servicing, running Inspect queries, etc..
- **gRPC / ttrpc**: Programmatic APIs for configuring and interacting with VMs.
  API compatibility and implementation coverage continue to evolve.

## Management Capabilities

The following table summarizes the status of selected management features.

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

For additional feature requests and status information, search the
[OpenVMM GitHub issues](https://github.com/microsoft/openvmm/issues).
