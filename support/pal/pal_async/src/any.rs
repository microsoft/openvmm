// Copyright (C) Microsoft Corporation. All rights reserved.

/// Internal trait for getting an `Any` trait object for an object.
pub trait AsAny {
    /// Get the `Any` trait object.
    fn as_any(&self) -> &dyn std::any::Any;
}

impl<T: std::any::Any> AsAny for T {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}