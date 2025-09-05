pub mod hypercall;

cfg_if::cfg_if!(
    if #[cfg(target_arch = "x86_64")] {
        mod x86_64;
        pub use x86_64::*;
    } else if #[cfg(target_arch = "aarch64")] {
        mod aarch64;
        pub use aarch64::*;
    } else {
        compile_error!("target_arch is not supported");
    }
);
