// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(with_encryption)]

// Both backends now delegate to the `crypto` crate, so the platform-specific
// modules are identical. Keep one active module to avoid dead code.
#[cfg(unix)]
mod ossl;
#[cfg(unix)]
pub use ossl::vmgs_decrypt;
#[cfg(unix)]
pub use ossl::vmgs_encrypt;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::vmgs_decrypt;
#[cfg(windows)]
pub use win::vmgs_encrypt;
