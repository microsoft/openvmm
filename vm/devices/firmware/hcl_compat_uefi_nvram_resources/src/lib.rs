//! Resources for instantiating HCL-compatible UEFI nvram variable storage.

use inspect::Inspect;
use mesh_protobuf::Protobuf;

/// "Quirks" to take into account when loading/storing nvram blob data.
#[derive(Clone, Inspect, Protobuf)]
pub struct HclCompatNvramQuirks {
    /// When loading nvram variables from storage, don't fail the entire load
    /// process when encountering variables that are missing null terminators in
    /// their name. Instead, skip loading any such variables, and continue on
    /// with the load.
    ///
    /// # Context
    ///
    /// Due to a (now fixed) bug in a previous version of Microsoft HCL, it was
    /// possible for non-null-terminated nvram variables to slip-through
    /// validation and get persisted to disk.
    ///
    /// Enabling this quirk will allow "salvaging" the rest of the non-corrupt
    /// nvram variables, which may be preferable over having the VM fail to boot
    /// at all.
    pub skip_corrupt_vars_with_missing_null_term: bool,
}
