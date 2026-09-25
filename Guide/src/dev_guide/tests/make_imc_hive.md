# IMC Hive Generator

`make_imc_hive` regenerates the Windows registry hive used to configure Petri
test guests during image preparation.

## Purpose

The checked-in IMC hive is applied to an offline Windows installation by
`prep_steps`. It describes settings that must exist before the prepared test
guest first boots, most importantly the Pipette service.

This is a generator for a repository artifact, not part of every VMM test run.
Normal tests consume the already-generated hive.

## Platform requirement

The implementation uses Windows offline-registry APIs and only runs on
Windows. The workspace builds a stub on other platforms, but that stub exits
with an unsupported-platform error.

## Regenerating the hive

Run the generator from Windows and pass the output path as its single
positional argument:

```powershell
cargo run -p make_imc_hive -- petri\guest-bootstrap\imc.hiv
```

The tool removes an existing output file before saving the new hive.

Regenerate the hive when changing Pipette's service path, startup account,
dependencies, isolation policy, or initial crash-dump configuration. Changes
that can be made after Pipette connects usually belong in a Petri test instead.

## How the hive is consumed

`prep_steps` embeds or resolves the generated hive and makes it available to
the temporary utility guest. Windows applies the hive to the offline SYSTEM
configuration on the target test disk. The prepared disk is then cached for
later VMM tests.

## Troubleshooting

An offline-registry error generally means the destination cannot be created or
the platform APIs rejected a key operation. A later guest failure can indicate
that the hive was valid structurally but contains an incorrect service path or
startup dependency. Inspect the prepared guest's SYSTEM hive and Windows
service logs in that case.
