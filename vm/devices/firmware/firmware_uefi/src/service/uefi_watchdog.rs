// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::UefiDevice;
use inspect::Inspect;
use vmcore::vmtime::VmTimeAccess;
use watchdog_core::platform::WatchdogPlatform;
use watchdog_core::WatchdogServices;

#[derive(Inspect)]
pub struct UefiWatchdogServices {
    pub watchdog: WatchdogServices,
}

impl UefiWatchdogServices {
    pub async fn new(
        vmtime: VmTimeAccess,
        platform: Box<dyn WatchdogPlatform>,
        is_restoring: bool,
    ) -> UefiWatchdogServices {
        UefiWatchdogServices {
            watchdog: WatchdogServices::new("uefi-watchdog", vmtime, platform, is_restoring).await,
        }
    }
}

impl UefiDevice {
    pub(crate) fn handle_watchdog_read(&mut self, reg: watchdog_core::Register) -> u32 {
        match self.service.uefi_watchdog.watchdog.read(reg) {
            Ok(val) => val,
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "Error while reading from watchdog device"
                );
                !0
            }
        }
    }

    pub(crate) fn handle_watchdog_write(&mut self, reg: watchdog_core::Register, val: u32) {
        match self.service.uefi_watchdog.watchdog.write(reg, val) {
            Ok(()) => (),
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "Error while writing to watchdog device"
                );
            }
        }
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "firmware.uefi.watchdog")]
        pub struct SavedState {
            #[mesh(1)]
            pub inner: <watchdog_core::WatchdogServices as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for UefiWatchdogServices {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let saved_state = state::SavedState {
                inner: self.watchdog.save()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { inner } = state;
            self.watchdog.restore(inner)?;
            Ok(())
        }
    }
}
