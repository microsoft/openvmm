#[cfg(test)]
mod test;

use crate::{
    deserializer::Deserializer,
    executor::ExecutorError,
    functions::{FunctionRegistry, FuzzFunctionVariable},
    prelude::*,
};
use inv_decoder::{InputCase, InputResult, SafeMemoryMap, exec_testcases_safe};
use inv_packet::OpenTMKFuzzTest;
use spin::Mutex;

// For now we are using the syz-decoder library, and unfortunately to get it to work here, is not clean as it has a different design.
// TODO: Refactor syz-decoder in a way to make this interface much cleaner
pub const SYZLANG_DESERIALIZER_ERROR_CODE: u64 = 0x133713381339;

#[derive(Default)]
struct SyzlangState {
    error_list: Vec<String>,
    function_registry: Arc<Mutex<FunctionRegistry>>,
    glob_mapping: Vec<String>,
}

impl SyzlangState {
    /// Executes a testcase with this state object
    fn exec_syzlang_testcase_line(
        &mut self,
        mem: &mut dyn SafeMemoryMap,
        input_struct: InputCase,
    ) -> InputResult {
        // resolve the call number to a pseudo syscall
        if self.glob_mapping.len() as u64 <= input_struct.call_num {
            log::error!(
                "[fatal] invalid call number {} received from syzlang",
                input_struct.call_num,
            );
            return InputResult {
                code: SYZLANG_DESERIALIZER_ERROR_CODE,
                name: String::default(), //never used!
                is_success: false,
            };
        }

        let handler_name = &self.glob_mapping[input_struct.call_num as usize];
        let input = SyzlangDeserializer::to_function_variables(&input_struct);

        match self
            .function_registry
            .lock()
            .exec(mem, handler_name.clone(), input)
        {
            FuzzFunctionVariable::Void => (),
            FuzzFunctionVariable::Int(_) => (), // TODO
            FuzzFunctionVariable::Error(e) => {
                let error_str = format!("Error recorded in function: {handler_name} error: {e}");
                log::error!("{error_str:?}");
                self.error_list.push(error_str);

                return InputResult {
                    code: SYZLANG_DESERIALIZER_ERROR_CODE,
                    name: String::default(),
                    is_success: false,
                };
            }
        }

        InputResult {
            code: 0,
            name: String::default(),
            is_success: true,
        }
    }

    fn dump_errors(&mut self) -> Result<(), ExecutorError> {
        if !self.error_list.is_empty() {
            let resp = Err(ExecutorError::SyzlangDeserializerFailed(
                self.error_list.join(", "),
            ));
            self.error_list.clear();
            return resp;
        }

        Ok(())
    }
}

pub struct SyzlangDeserializer {
    tc_slice: Vec<u8>,
    st: Mutex<SyzlangState>,
    mem: (Vec<u8>, usize),
}

impl SyzlangDeserializer {
    pub fn new() -> Self {
        Self {
            tc_slice: vec![0; inv_decoder::SUPPORTED_INPUT_SIZE],
            st: Default::default(),
            mem: (
                vec![0; inv_decoder::EXEC_INPUT_REQ_SIZE],
                inv_decoder::ADDR_SYZ_BEGIN as usize,
            ),
        }
    }

    fn to_function_variables(input: &InputCase) -> Vec<FuzzFunctionVariable> {
        let mut resp = Vec::new();
        for arg in 0..input.num_args {
            // for now everything is a u64
            resp.push(FuzzFunctionVariable::Int(input.args[arg as usize]));
        }
        resp
    }
}

impl Deserializer for SyzlangDeserializer {
    fn set_function_registry(&mut self, registry: Arc<Mutex<FunctionRegistry>>) {
        self.st.lock().function_registry = registry;
    }

    fn set_mappings(&mut self, mappings: Vec<u8>) -> Result<(), ExecutorError> {
        // for syzlang the mappings are to map the syscall number
        // to the string name of the pseudo syscall
        // e.g.
        // [0] => mmio_read
        // [1] => mmio_write
        // etc.
        self.st.lock().glob_mapping = match postcard::from_bytes(&mappings) {
            Ok(m) => m,
            Err(_) => {
                return Err(ExecutorError::DecoderMappingsDeserializeFailed);
            }
        };

        Ok(())
    }

    fn deserialize_and_execute(
        &mut self,
        testcase: &mut OpenTMKFuzzTest,
    ) -> Result<u64, ExecutorError> {
        self.tc_slice.fill(0);

        let src = &testcase.testcase_vcpu0.as_slice();
        self.tc_slice[..src.len()].copy_from_slice(src);
        self.mem.0.as_mut_slice().fill(0);

        // Borrow choreography:
        //   - `&mut self.mem` and `&mut self.tc_slice` are disjoint fields (split borrow).
        //   - The closure captures `&self.st` only (Rust 2021 disjoint capture), not
        //     `&self`, so it doesn't conflict with the &mut borrows above.
        //   - `decoder` is `&DecodedProgram` (shared); `&mut decoder` borrows the local
        //     binding, not the program. SafeMemoryMap is impl'd for `&DecodedProgram`
        //     with interior mutability via the inner Mutex<MyMemoryMap>.
        //   - `self.st` is locked per-call inside the closure and again at
        //     `dump_errors()` below; safe because exec_testcases_safe is synchronous
        //     and the closure is dropped before we return here. We also need to
        //     ensure that `self.st` is Sync + Send (which means the underlying
        //     state should be `Send`)
        let results = exec_testcases_safe(&mut self.mem, &mut self.tc_slice, |mut decoder, inp| {
            self.st.lock().exec_syzlang_testcase_line(&mut decoder, inp)
        });

        // See if we hit any errors.
        self.st.lock().dump_errors()?;
        match results {
            Ok(_) => {
                // For now lets ignore the response data
                //
                // TODO: we should recover this and use it for minimization steps etc when we start
                // doing more complicated fuzzing.  it also needs reactoring since the string is
                // never used!
                Ok(0)
            }
            Err(e) => Err(ExecutorError::SyzlangDeserializerFailed(format!("{e:?}"))),
        }
    }
}
