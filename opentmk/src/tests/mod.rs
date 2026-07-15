// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test modules driving OpenTMK tests.

use opentmk_protocol::OpenTmkConfig;

mod hyperv;

/// Declares a backend's test modules and generates `run_named`, which maps a
/// test name to its `exec` function. Per-entry attributes gate the module and
/// its dispatch arm together.
#[macro_export]
macro_rules! tmk_tests {
    (
        ctx: $ctx:ty,
        tests: { $( $(#[$meta:meta])* $module:ident ),* $(,)? } $(,)?
    ) => {
        $( $(#[$meta])* pub mod $module; )*

        /// Runs the named test. Returns `false` if no such test exists.
        pub fn run_named(test: &str, ctx: &mut $ctx) -> bool {
            match test {
                $(
                    $(#[$meta])*
                    _ if test == stringify!($module) => {
                        $module::exec(ctx);
                        true
                    }
                )*
                _ => false,
            }
        }
    };
}

/// Generates `dispatch`, which selects a backend, builds its context, and runs
/// the named test. Returns `false` if the backend or test is unknown.
///
/// Each entry maps a backend name to a context builder closure
/// `|params: &serde_json::Value| -> Ctx`.
#[macro_export]
macro_rules! tmk_backends {
    ( $( $(#[$meta:meta])* $backend:ident => $build:expr ),* $(,)? ) => {
        fn dispatch(backend: &str, test: &str, params: &::serde_json::Value) -> bool {
            match backend {
                $(
                    $(#[$meta])*
                    _ if backend == stringify!($backend) => {
                        let build = $build;
                        let mut ctx = build(params);
                        $backend::run_named(test, &mut ctx)
                    }
                )*
                _ => false,
            }
        }
    };
}

crate::tmk_backends! {
    hyperv => |_params: &serde_json::Value| {
        let mut ctx = crate::platform::hyperv::ctx::HvTestCtx::new();
        ctx.init(hvdef::Vtl::Vtl0).expect("failed to init on BSP");
        ctx
    },
}

/// The embedded config region, patched in place by host tooling to select the
/// test to run. Layout and parsing live in [`opentmk_protocol`].
#[used]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".tmkcfg")]
pub static OPENTMK_CONFIG: OpenTmkConfig = OpenTmkConfig::new();

/// Reads the embedded config and runs the selected backend/test.
///
/// Panics if the config is missing/invalid or names an unknown backend or test.
pub fn run_test() {
    // Read through a volatile load so the optimizer cannot assume the static
    // still holds its empty initializer: the host patches these bytes in the
    // on-disk image after the build.
    // SAFETY: `OPENTMK_CONFIG` is a valid, initialized, aligned static of this type.
    let cfg = unsafe { core::ptr::read_volatile(&raw const OPENTMK_CONFIG) };
    let Some(cfg) = cfg.parse() else {
        panic!("TMK config missing or invalid: binary must be patched with a backend and test");
    };
    if !dispatch(&cfg.backend, &cfg.test, &cfg.params) {
        panic!("unknown backend/test: '{}'/'{}'", cfg.backend, cfg.test);
    }
}
