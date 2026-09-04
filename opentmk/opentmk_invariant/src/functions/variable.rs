// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use inv_decoder::SafeMemoryMap;

#[cfg_attr(not(target_os = "uefi"), expect(unused_imports))]
use crate::prelude::*;

#[derive(Debug, Clone)]
pub enum FuzzFunctionVariable {
    Void,
    Int(u64),
    Error(String),
}

impl FuzzFunctionVariable {
    pub fn name(&self) -> &str {
        match self {
            FuzzFunctionVariable::Void => "void",
            FuzzFunctionVariable::Int(_) => "int",
            FuzzFunctionVariable::Error(_) => "error",
        }
    }

    pub fn expect_int(&self, arg: &str) -> Result<u64, String> {
        match self {
            Self::Int(val) => Ok(*val),
            _ => Err(format!("{arg} is not int but {}", self.name())),
        }
    }
}

impl From<Result<FuzzFunctionVariable, String>> for FuzzFunctionVariable {
    fn from(value: Result<FuzzFunctionVariable, String>) -> Self {
        match value {
            Ok(val) => val,
            Err(err) => FuzzFunctionVariable::Error(err),
        }
    }
}

pub type FuzzFunction =
    fn(&mut dyn SafeMemoryMap, Vec<FuzzFunctionVariable>) -> Result<FuzzFunctionVariable, String>;

pub trait VerifyFuzzVariables {
    fn verify_num_params<const NUM: usize>(&self) -> Result<&[FuzzFunctionVariable; NUM], String>;
}

impl VerifyFuzzVariables for Vec<FuzzFunctionVariable> {
    fn verify_num_params<const NUM: usize>(&self) -> Result<&[FuzzFunctionVariable; NUM], String> {
        if self.len() != NUM {
            return Err(format!(
                "Expected {:?} params, but received {:?}",
                NUM,
                self.len()
            ));
        }
        Ok(self.first_chunk().unwrap())
    }
}
