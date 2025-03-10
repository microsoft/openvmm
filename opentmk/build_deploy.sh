RUST_BACKTRACE=1 cargo build -p opentmk --target x86_64-unknown-uefi
cargo xtask guest-test uefi --bootx64 ./target/x86_64-unknown-uefi/debug/opentmk.efi
qemu-img convert -f raw -O vhdx ./target/x86_64-unknown-uefi/debug/opentmk.img  ~/projects/opentmk.vhdx