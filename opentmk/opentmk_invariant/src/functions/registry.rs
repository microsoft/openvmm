use inv_decoder::SafeMemoryMap;

use crate::functions::{FuzzFunction, FuzzFunctionVariable};

#[allow(unused_imports)]
use crate::prelude::*;

struct RegisteredFn {
    name: String,
    call: FuzzFunction,
}

#[derive(Default)]
pub struct FunctionRegistry {
    fns: Vec<RegisteredFn>,
}

impl FunctionRegistry {
    pub fn register(&mut self, name: &str, f: FuzzFunction) {
        self.fns.push(RegisteredFn {
            name: String::from(name),
            call: f,
        });
    }

    pub fn exec(
        &self,
        mem: &mut dyn SafeMemoryMap,
        function_name: String,
        input: Vec<FuzzFunctionVariable>,
    ) -> FuzzFunctionVariable {
        let mut func: Option<&RegisteredFn> = None;

        for f in &self.fns {
            if f.name == function_name {
                func = Some(f);
                break;
            }
        }

        let func = match func {
            None => {
                return FuzzFunctionVariable::Error(format!(
                    "Invalid function name: {}",
                    &function_name
                ));
            }
            Some(f) => f,
        };

        (func.call)(mem, input).into()
    }
}
