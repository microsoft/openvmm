#!/bin/sh

set -e

case "$CARGO_PKG_NAME" in
    "openvmm"|"tmk_vmm")
        # Add entitlements for using hypervisor framework.
        entitlements=$(dirname "$0")/entitlements.xml
        codesign --entitlements "$entitlements" -f -s - "$1" > /dev/null
        ;;
    *)
        ;;
esac

exec "$@"
