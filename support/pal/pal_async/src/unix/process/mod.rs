// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix process wait implementations.

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

cfg_select! {
    target_os = "linux" => {
        pub(crate) use linux::WaitInner;
    }
    target_os = "macos" => {
        pub(crate) use macos::WaitInner;
    }
}
