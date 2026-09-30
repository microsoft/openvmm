// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This is an extended prelude crate that imports a number of common rust API entities that
//! would otherwise be imported from the `alloc` crate.
extern crate alloc;
pub use alloc::boxed::Box;
pub use alloc::string::String;
pub use alloc::sync::Arc;
pub use alloc::vec;
pub use alloc::vec::Vec;
