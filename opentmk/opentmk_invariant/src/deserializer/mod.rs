// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod syzlang;

use spin::Mutex;

use crate::{executor::ExecutorError, functions::FunctionRegistry, prelude::*};

use inv_packet::OpenTMKFuzzTest;

pub trait Deserializer {
    fn deserialize_and_execute(
        &mut self,
        testcase: &mut OpenTMKFuzzTest,
    ) -> Result<u64, ExecutorError>;
    fn set_function_registry(&mut self, registry: Arc<Mutex<FunctionRegistry>>);
    fn set_mappings(&mut self, mappings: Vec<u8>) -> Result<(), ExecutorError>;
}
