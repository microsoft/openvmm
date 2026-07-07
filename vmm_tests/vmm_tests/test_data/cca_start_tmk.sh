#!/root/busybox sh
set -e

echo "[plane0] start-tmk.sh reached"

mkdir -p /root/mount
mount -t 9p -o trans=virtio cca_mount /root/mount

cd /root/mount
export RUST_BACKTRACE=1

echo "[plane0] Launching tmk_vmm..."
# Do not exec: keep the shell alive to report PASS after tmk_vmm exits cleanly.
./tmk_vmm --hv cca --tmk ./simple_tmk
echo "PASS"
