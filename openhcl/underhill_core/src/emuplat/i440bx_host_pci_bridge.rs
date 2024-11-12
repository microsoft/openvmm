// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements Underhill's PAM register support for the i440bx host PCI bridge.
//!
//! The PAM registers specify the memory visibility for regions within the first
//! 1MB of guest memory. Each region can be mapped as read-write, read-only, or
//! unmapped (or write-only, but that's generally unused and is too hard to
//! support in virtualization platforms); accesses that do not match the mapping
//! state will be treated as MMIO.
//!
//! These registers are typically used to enable ROM shadowing, where a ROM
//! mapped at a low address (such as the VGA BIOS at 0xc0000) will be copied to
//! RAM at the same address. They are also used to ensure that the BIOS images
//! are not modified after POST, as a sort of memory protection.
//!
//! Underhill's implementation of these registers uses the GET protocol to
//! create/destroy GPA ranges to remap guest memory from different locations or
//! to map memory read-only. Instead of implementing the PAM registers as a
//! physical machine would (and as described above), it implements a subset of
//! operations that are known to work with our PCAT and SVGA BIOSes. This
//! matches what Hyper-V does. This has the advantage that it works with the
//! Hyper-V memory model, and it does not require additional guest RAM
//! allocations for ROMs (which need to be preserved since they may change on
//! disk after a VM boots, either due to a servicing operation or due to a
//! migration to a different host).
//!
//! In the future, we should implement PAM registers faithfully.

use chipset_legacy::i440bx_host_pci_bridge::AdjustGpaRange;
use chipset_legacy::i440bx_host_pci_bridge::GpaState;
use guest_emulation_transport::api::CreateRamGpaRangeFlags;
use guest_emulation_transport::api::RemoteRamGpaRangeHandle;
use memory_range::MemoryRange;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveRestore;

#[derive(Debug, PartialEq, Eq)]
struct HandleMetadata {
    range: MemoryRange,
    gpa_offset: u64,
    is_rom: bool,
}

#[derive(Debug)]
struct Handle {
    remote_handle: RemoteRamGpaRangeHandle,
    meta: HandleMetadata,
}

struct GetBackedAdjustGpaRangeState {
    base_slot: u32,
    handles: [Option<Handle>; BIOS_MEMORY_RANGE_BOUNDS.len()],
}

/// GET-backed implementation of AdjustGpaRange.
pub struct GetBackedAdjustGpaRange {
    // Static config
    rom_bios_offset: u64,

    // Runtime glue
    get: guest_emulation_transport::GuestEmulationTransportClient,

    // Volatile state
    state: GetBackedAdjustGpaRangeState,
}

impl GetBackedAdjustGpaRange {
    pub fn new(
        get: guest_emulation_transport::GuestEmulationTransportClient,
        base_slot: u32,
        rom_bios_offset: u64,
        saved_state: Option<<Self as SaveRestore>::SavedState>,
    ) -> Result<Self, RestoreError> {
        let mut this = Self {
            rom_bios_offset,

            get,

            state: GetBackedAdjustGpaRangeState {
                base_slot,
                handles: [(); BIOS_MEMORY_RANGE_BOUNDS.len()].map(|_| None),
            },
        };

        if let Some(saved_state) = saved_state {
            this.restore(saved_state)?
        }

        Ok(this)
    }

    async fn adjust(&mut self, range: MemoryRange, state: GpaState) {
        // The host already sets VGA to be MMIO, so ignore changes to VGA
        // state.
        if range == MemoryRange::new(0xa0000..0xc0000) {
            assert_eq!(state, GpaState::Mmio);
            return;
        }

        let (index, range_bound) = BIOS_MEMORY_RANGE_BOUNDS
            .iter()
            .enumerate()
            .find(|(_, x)| x.range == range)
            .expect("unknown pam range");

        // workaround for missing WriteOnly support
        let state = if matches!(state, GpaState::WriteOnly) {
            tracing::warn!("Write-only RAM mapping not supported");
            GpaState::Writable
        } else {
            state
        };

        // Allocate the new range. Note that it's not necessary to allocate
        // a range if it's read/write RAM because the RAM range already covers
        // this range. In other words, no override range is necessary.
        let meta = match state {
            GpaState::Mmio => {
                match range_bound.kind {
                    BiosMemoryRangeKind::SystemBios => Some(HandleMetadata {
                        range: range_bound.range,
                        gpa_offset: range_bound.range.start() + self.rom_bios_offset,
                        is_rom: true,
                    }),
                    // Map everything else straight through, except the VGA BIOS TSR portion,
                    // which needs to be read/write at all times
                    BiosMemoryRangeKind::VgaBiosTsr => None,
                    _ => Some(HandleMetadata {
                        range: range_bound.range,
                        gpa_offset: range_bound.range.start(),
                        is_rom: true,
                    }),
                }
            }
            GpaState::WriteProtected => {
                // Map straight through read only, except for the top of system BIOS,
                // which is always read/write.
                Some(HandleMetadata {
                    range: MemoryRange::new(
                        range_bound.range.start()
                            ..range_bound.range.end() - range_bound.space_to_leave_writable,
                    ),
                    gpa_offset: range_bound.range.start(),
                    is_rom: true,
                })
            }
            GpaState::Writable => {
                // For most ranges in this state, we have nothing to do because
                // there is already a "catch all" range in place for mapping the
                // entire RAM range. However, for VGA, we need to use a gross kludge.
                //
                // The problem with VGA is that the BIOS needs to act more like a
                // writable area of RAM as opposed to a write-protected Rom. This is
                // because the VGA BIOS (at least the portion from C8000-CC000) used
                // to be a DOS TSR (terminate & stay resident program), and it wasn't
                // written to be Rom-able.
                //
                // Unfortunately, the BIOS writes over the top of this range during the POST.
                // But it puts the memory range into this state first. So, we add a kludge
                // to remap the area to high RAM instead. That way, the BIOS can write to it
                // without overwriting the video BIOS contents.

                if matches!(
                    range_bound.kind,
                    BiosMemoryRangeKind::VgaBios | BiosMemoryRangeKind::VgaBiosTsr
                ) {
                    Some(HandleMetadata {
                        range: range_bound.range,
                        gpa_offset: range_bound.range.start() + self.rom_bios_offset,
                        is_rom: false,
                    })
                } else {
                    None
                }
            }
            GpaState::WriteOnly => unreachable!("stubbed out above"),
        };

        // skip re-creating range if nothing has changed
        //
        // this is particularly relevant during servicing, as devices will issue
        // `adjust` calls as part of their restore path
        if self.state.handles[index].as_ref().map(|h| &h.meta) == meta.as_ref() {
            tracing::debug!("skipping GET calls - nothing has changed");
            return;
        }

        // make sure we reset existing mappings prior to creating new ones
        if let Some(handle) = self.state.handles[index].take() {
            self.get.reset_ram_gpa_range(handle.remote_handle).await
        }

        // if there's no meta - we're done. no need to create anything.
        let meta = match meta {
            Some(meta) => meta,
            None => return,
        };

        match self
            .get
            .create_ram_gpa_range(
                self.state.base_slot + index as u32,
                meta.range.start(),
                meta.range.len(),
                meta.gpa_offset,
                CreateRamGpaRangeFlags::new().with_rom_mb(meta.is_rom),
            )
            .await
        {
            Ok(remote_handle) => {
                self.state.handles[index] = Some(Handle {
                    remote_handle,
                    meta,
                });
            }
            Err(err) => {
                tracing::warn!(
                    index,
                    ?meta,
                    error = &err as &dyn std::error::Error,
                    "error invoking get.create_ram_gpa_range"
                );
            }
        }
    }
}

impl AdjustGpaRange for GetBackedAdjustGpaRange {
    fn adjust_gpa_range(&mut self, range: MemoryRange, state: GpaState) {
        // Note that this synchronously blocks on the GET. This is OK because
        // the GET runs on a separate thread from the VP threads and has no
        // dependencies on tasks on the VP threads, and this will never be
        // called from the GET thread.
        pal_async::local::block_with_io(|_| self.adjust(range, state))
    }
}

pub struct ArcMutexGetBackedAdjustGpaRange(
    pub std::sync::Arc<parking_lot::Mutex<GetBackedAdjustGpaRange>>,
);

// required for emuplat servicing optimization
impl AdjustGpaRange for ArcMutexGetBackedAdjustGpaRange {
    fn adjust_gpa_range(&mut self, range: MemoryRange, state: GpaState) {
        self.0.lock().adjust_gpa_range(range, state)
    }
}

enum BiosMemoryRangeKind {
    None,
    SystemBios,
    VgaBios,
    VgaBiosTsr,
}

struct BiosMemoryRangeBounds {
    range: MemoryRange,
    // Space at end of range to not write protect
    space_to_leave_writable: u64,
    kind: BiosMemoryRangeKind,
}

impl BiosMemoryRangeBounds {
    const fn new(
        start: u64,
        len: u64,
        space_to_leave_writable: u64,
        kind: BiosMemoryRangeKind,
    ) -> Self {
        Self {
            range: MemoryRange::new(start..start + len),
            space_to_leave_writable,
            kind,
        }
    }
}

const BIOS_MEMORY_RANGE_BOUNDS: &[BiosMemoryRangeBounds] = &[
    // The top 8K of this range are backed by RAM, not Rom
    BiosMemoryRangeBounds::new(0x0F0000, 0x010000, 0x2000, BiosMemoryRangeKind::SystemBios),
    //
    // C0000-C4000
    BiosMemoryRangeBounds::new(0x0C0000, 0x004000, 0x0000, BiosMemoryRangeKind::VgaBios),
    // C4000-C8000
    BiosMemoryRangeBounds::new(0x0C4000, 0x004000, 0x0000, BiosMemoryRangeKind::VgaBios),
    // C8000-CC000: Our VGA BIOS assumes it's writable
    BiosMemoryRangeBounds::new(0x0C8000, 0x004000, 0x0000, BiosMemoryRangeKind::VgaBiosTsr),
    BiosMemoryRangeBounds::new(0x0CC000, 0x004000, 0x0000, BiosMemoryRangeKind::None),
    //
    BiosMemoryRangeBounds::new(0x0D0000, 0x004000, 0x0000, BiosMemoryRangeKind::None),
    BiosMemoryRangeBounds::new(0x0D4000, 0x004000, 0x0000, BiosMemoryRangeKind::None),
    BiosMemoryRangeBounds::new(0x0D8000, 0x004000, 0x0000, BiosMemoryRangeKind::None),
    BiosMemoryRangeBounds::new(0x0DC000, 0x004000, 0x0000, BiosMemoryRangeKind::None),
    //
    BiosMemoryRangeBounds::new(0x0E0000, 0x004000, 0x0000, BiosMemoryRangeKind::SystemBios),
    BiosMemoryRangeBounds::new(0x0E4000, 0x004000, 0x0000, BiosMemoryRangeKind::SystemBios),
    BiosMemoryRangeBounds::new(0x0E8000, 0x004000, 0x0000, BiosMemoryRangeKind::SystemBios),
    BiosMemoryRangeBounds::new(0x0EC000, 0x004000, 0x0000, BiosMemoryRangeKind::SystemBios),
];

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use memory_range::MemoryRange;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf)]
        #[mesh(package = "underhill.emuplat.i440bx.host_pci_bridge")]
        pub struct SavedHandle {
            #[mesh(1)]
            pub remote_handle: u32,
            #[mesh(2)]
            pub range: MemoryRange,
            #[mesh(3)]
            pub gpa_offset: u64,
            #[mesh(4)]
            pub is_rom: bool,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "underhill.emuplat.i440bx.host_pci_bridge")]
        pub struct SavedState {
            #[mesh(1)]
            pub base_slot: u32,
            #[mesh(2)]
            pub handles: [Option<SavedHandle>; 13],
        }
    }

    impl SaveRestore for GetBackedAdjustGpaRange {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let GetBackedAdjustGpaRangeState { base_slot, handles } = &self.state;

            Ok(state::SavedState {
                base_slot: *base_slot,
                handles: {
                    let mut saved_handles: [Option<state::SavedHandle>; 13] = Default::default();
                    for (dst, src) in saved_handles.iter_mut().zip(handles.iter()) {
                        *dst = src.as_ref().map(|h| {
                            let Handle {
                                remote_handle,
                                meta:
                                    HandleMetadata {
                                        range,
                                        gpa_offset,
                                        is_rom,
                                    },
                            } = h;

                            state::SavedHandle {
                                remote_handle: remote_handle.as_raw(),
                                range: *range,
                                gpa_offset: *gpa_offset,
                                is_rom: *is_rom,
                            }
                        })
                    }
                    saved_handles
                },
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { base_slot, handles } = state;

            self.state = GetBackedAdjustGpaRangeState {
                base_slot,
                handles: handles.map(|h| {
                    let state::SavedHandle {
                        remote_handle,
                        range,
                        gpa_offset,
                        is_rom,
                    } = h?;

                    Some(Handle {
                        remote_handle: RemoteRamGpaRangeHandle::from_raw(remote_handle),
                        meta: HandleMetadata {
                            range,
                            gpa_offset,
                            is_rom,
                        },
                    })
                }),
            };

            Ok(())
        }
    }
}
