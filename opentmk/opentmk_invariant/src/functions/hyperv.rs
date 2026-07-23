use crate::functions::hvcall_meta::unpack_hvcall_meta;
use crate::functions::{FuzzFunctionVariable, VerifyFuzzVariables};
#[allow(unused)]
use crate::prelude::*;
use hvdef::Vtl;
use inv_decoder::SafeMemoryMap;
use opentmk_core::context::HypercallPlatformTrait;
use opentmk_core::platform::hyperv::ctx::{HvTestCtx, HyperVHypercallConfig};
use spin::Mutex;

/// Future-proof for future multi-VP usage to ensure writing to input_page and
/// then dispatching the hypercall is done in one shot. Today we are running
/// single-threaded, and this is mainly used to keep rust happy.
static CALLS: Mutex<(HvTestCtx, bool)> = Mutex::new((HvTestCtx::new(), false));

/// Makes a hypervisor call from a fuzz function.
///
/// The grammar invokes us with exactly three arguments:
///
/// * `meta`        — an `i64` carrying a packed
///   [`HvcallMeta`](crate::functions::hvcall_meta::HvcallMeta) with the
///   static (`code`, `header_size`, `element_size`) triple for this
///   hypercall.
/// * `in_page`     — pointer to the syzkaller-generated input buffer.
/// * `in_page_len` — the byte size of `*in_page`
///   (`BYTE_SIZE("../in")` in the grammar).
///
/// `rep` is computed at call-time as
/// `(in_page_len − header_size) / element_size`.
pub fn hvcall(
    mem: &mut dyn SafeMemoryMap,
    vars: Vec<FuzzFunctionVariable>,
) -> Result<FuzzFunctionVariable, String> {
    // Verify and parse parameters.
    let [meta, in_page, in_page_len] = vars.verify_num_params()?;
    let meta = unpack_hvcall_meta(meta.expect_int("meta")?);
    let in_page = in_page.expect_int("in_page")? as usize;
    let in_page_len = in_page_len.expect_int("in_page_len")? as usize;

    // Read in the input page from `in_page` (only if any input is
    // expected — `void`-input hypercalls send `in_page_len == 0`).
    let mut hvc_lock = CALLS.lock();
    let (hvc, init) = &mut *hvc_lock;
    if !*init {
        hvc.init(Vtl::Vtl0)
            .map_err(|e| format!("Failed to initialize HvTestCtx: {e}"))?;
        *init = true;
    }

    let mut in_args = vec![0; in_page_len];
    match mem.try_read_mem(in_page, &mut in_args) {
        Ok(_) => (),
        Err(e) => log::info!("hvcall: Failed to read input: {e}"),
    }

    // Compute rep from the static header/element sizes plus the
    // grammar-supplied buffer size.
    let header_size = meta.header_size as usize;
    let element_size = meta.element_size as usize;
    let rep_count = if element_size == 0 || in_page_len <= header_size {
        None
    } else {
        Some((in_page_len - header_size) / element_size)
    };

    let cfg = HyperVHypercallConfig {
        rep_start: None,
        rep_count,
        size: None, // TODO: certain calls require instead of or in addition to rep_count
        fast_call: false, // TODO: we may like to change this in fuzzing
    };

    // Invoke actual call.
    let result = hvc.hypercall(meta.code.into(), &in_args, &mut [], cfg);
    match result {
        Ok(_) => (),
        Err(e) => log::info!("hvcall: Failed to make hypercall: {e}"),
    }

    Ok(FuzzFunctionVariable::Void)
}
