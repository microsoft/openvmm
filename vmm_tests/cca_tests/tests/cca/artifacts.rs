// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CCA emulation artifacts used by the CCA Petri test.

macro_rules! declare_cca_artifacts {
    (
        $(
            $(#[$doc:meta])*
            $name:ident
        ),*
        $(,)?
    ) => {
        ::petri_artifacts_core::declare_artifacts_inner!($(
            $(#[$doc])*
            $name(::petri_artifacts_core::DOES_NOT_SUPPORT_BLOB_DISK, "", ANY),
        )*);
    };
}

declare_cca_artifacts! {
    /// Arm shrinkwrap executable used to launch CCA emulation
    SHRINKWRAP,
    /// Python virtual environment for shrinkwrap
    VENV,
    /// Shrinkwrap-built CCA host rootfs image
    ROOTFS,
    /// Buildroot host e2fsck binary matching the CCA rootfs
    E2FSCK,
    /// Buildroot host resize2fs binary matching the CCA rootfs
    RESIZE2FS,
    /// Guest disk image passed into the Realm
    GUEST_DISK,
    /// Plane0 Linux kernel image
    PLANE0_LINUX_IMAGE,
    /// KVMTOOL EFI firmware image
    KVMTOOL_EFI,
    /// kvmtool binary used by the host rootfs to launch the Realm
    LKVM,
}
