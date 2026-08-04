// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test modules driving OpenTMK tests.

use opentmk_protocol::OpenTmkConfig;

mod hyperv;

/// Generates `run_named` to map test names to `exec`. Modules must be declared
/// separately with explicit `mod` statements; per-entry attributes gate the
/// matching dispatch arm.
#[macro_export]
macro_rules! opentmk_tests {
    (
        ctx: $ctx:ty,
        tests: { $( $(#[$meta:meta])* $module:ident ),* $(,)? } $(,)?
    ) => {
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

/// Generates `dispatch` to select a backend, build context, and run the named test.
/// Returns `false` if unknown; entries map names to `|params: &serde_json::Value| -> Ctx`.
#[macro_export]
macro_rules! opentmk_backends {
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

crate::opentmk_backends! {
    hyperv => |_params: &serde_json::Value| {
        let mut ctx = crate::platform::hyperv::ctx::HvTestCtx::new();
        ctx.init(hvdef::Vtl::Vtl0).expect("failed to init on BSP");
        ctx
    },
}

/// The embedded config region, patched in place by host tooling to select the
/// test to run. Layout and parsing live in [`opentmk_protocol`].
// SAFETY: `OPENTMK_CONFIG` is unique, so `no_mangle` cannot collide.
// `link_section = ".tmkcfg"` gives it a dedicated section with the patcher layout.
#[used]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".tmkcfg")]
pub static OPENTMK_CONFIG: OpenTmkConfig = OpenTmkConfig::new();

/// Reads the embedded config and runs the selected backend/test.
/// Panics if config is invalid or names an unknown backend/test.
pub fn run_test() {
    // `black_box` forces an opaque load so the optimizer cannot fold in the empty
    // initializer; host patching happens after build and is invisible to the compiler.
    let cfg = core::hint::black_box(&OPENTMK_CONFIG);
    let Some(cfg) = cfg.parse() else {
        panic!("TMK config missing or invalid: binary must be patched with a backend and test");
    };
    if !dispatch(&cfg.backend, &cfg.test, &cfg.params) {
        panic!("unknown backend/test: '{}'/'{}'", cfg.backend, cfg.test);
    }
}
