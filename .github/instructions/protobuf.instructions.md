---
applyTo: "**/*.proto"
---

# Protocol Buffers Enums

These conventions apply to OpenVMM-owned `.proto` schemas, including gRPC
and ttrpc APIs.

- Every new enum must declare `<ENUM_NAME>_UNSPECIFIED = 0` as its first
  value. Zero means unspecified, not a meaningful option such as `NONE`,
  `DISABLED`, or the first operating mode. Protobuf uses zero when a
  non-optional enum field is omitted.
- Every new or renamed enum value must be `UPPER_SNAKE_CASE`, prefixed
  with the enum type's name converted to `UPPER_SNAKE_CASE` plus `_`.
  This includes the unspecified value and enums nested in messages.
  Enum values share the enclosing scope, not a scope named by the enum.

```proto
enum BootMode {
  BOOT_MODE_UNSPECIFIED = 0;
  BOOT_MODE_UEFI = 1;
  BOOT_MODE_DIRECT = 2;
}
```

In code review, flag violations introduced by the change and require them
to be fixed before merge. Existing violations are grandfathered, even in
a file being edited: do not require renaming, renumbering, or changing the
meaning of existing values, or adding an unspecified value to an existing
enum, merely to conform.
New values added to an existing enum must still use the enum-name prefix,
even if its existing values do not.

See the [Protocol Buffers enum style guide](https://protobuf.dev/programming-guides/style/#enums)
for the underlying conventions.
