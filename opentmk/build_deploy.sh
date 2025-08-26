RUST_BACKTRACE=1  CARGO_PROFILE_RELEASE_force_frame_pointers=yes cargo +nightly-2025-05-09 build -p opentmk --target x86_64-unknown-uefi --release #--target-dir ./target/x86_64-unknown-uefi/debug
cargo xtask guest-test uefi --bootx64 ~/projects-local/openvmm/target/x86_64-unknown-uefi/release/opentmk.efi
qemu-img convert -f raw -O vhdx ~/projects-local/openvmm/target/x86_64-unknown-uefi/release/opentmk.img  ~/projects/opentmk.vhdx
#CARGO_PROFILE_RELEASE_OPT_LEVEL=0