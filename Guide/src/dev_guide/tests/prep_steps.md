# Windows Test Image Preparation

`prep_steps` prepares reusable Windows disks for Petri VMM tests by modifying
them from a temporary OpenVMM guest.

## Why preparation is separate

Windows guest images require changes that are awkward or unsafe to perform by
editing the VHD directly. `prep_steps` boots a known Windows utility VM,
attaches a copy of the target image as a data disk, and performs the changes
through Windows itself.

Keeping this work outside individual tests has two benefits:

- Every test starts from the same prepared image.
- Expensive service and driver installation can be cached and reused.

The tool remains intentionally small. Changes that can be made after the test
guest boots should be performed through Pipette in the test instead.

## Normal use

You normally do not invoke `prep_steps` yourself. Artifact discovery in
`cargo xflowey vmm-tests-run` determines whether a selected test needs a
prepared Windows image, builds the helper, and runs the required preparation
mode before nextest starts.

To force regeneration while developing preparation logic, disable reuse:

```bash
cargo xflowey vmm-tests-run \
  --no-reuse-prepped-vhds \
  --filter "test(my_windows_test)"
```

The resulting images and preparation logs live below the VMM test content
directory selected by `--dir`.

## Standard image flow

The standard path performs the following work:

1. Resolve the utility boot image, source test VHD, firmware, OpenVMM, Pipette,
   and output directory.
2. Copy the source disk to its prepared output path.
3. Randomize disk and partition identifiers so the utility VM can attach the
   source and result disks without Windows treating them as duplicates.
4. Boot the utility Windows VM with the result disk attached.
5. Connect to the utility VM's Pipette agent.
6. Apply the IMC registry hive to the offline Windows installation on the
   attached disk.
7. Shut down the utility VM and retain the prepared disk.

The hive configures Pipette as an automatically started Windows service and
enables the VMBus behavior expected by isolated test guests.

## Outputs and caching

Prepared images are derived artifacts. Reusing one is correct only while its
source image and preparation inputs remain compatible. Use
`--no-reuse-prepped-vhds` after changing:

- `prep_steps` behavior.
- The Pipette service hive.
- Drivers installed by the `no-vmbus` mode.
- The source Windows image assumptions.

Do not edit a shared source VHD in place. Preparation works on a result copy so
tests remain repeatable.

## Troubleshooting

Preparation failures usually occur before the actual nextest run. Check the
preparation log for the stage that failed:

- Artifact errors indicate a missing source VHD, firmware, Pipette, or driver
  build.
- Disk errors can indicate a corrupt image or failure while rewriting disk
  identifiers.
- A Pipette timeout indicates that the utility boot VM did not start its agent.
- Windows servicing failures in `no-vmbus` usually identify a missing or
  incompatible `virtio-win` file.

When cross-running Windows tests from WSL, Flowey reports if the output
directory must be placed on a Windows filesystem.
