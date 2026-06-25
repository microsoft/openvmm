// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM hypervisor backend.

#![cfg(all(target_os = "linux", feature = "virt_kvm", guest_is_native))]

use anyhow::Context as _;
use hypervisor_resources::HvEnlightenments;
#[cfg(guest_arch = "x86_64")]
use hypervisor_resources::HvHostAdjustments;
#[cfg(guest_arch = "x86_64")]
use hypervisor_resources::HvHostCaps;
use hypervisor_resources::HvSpecOverrides;
use hypervisor_resources::HypervisorKind;
use hypervisor_resources::KvmHandle;
use vm_resource::IntoResource;
use vm_resource::Resource;

/// KVM probe for auto-detection.
pub struct KvmProbe;

impl hypervisor_resources::HypervisorProbe for KvmProbe {
    fn name(&self) -> &str {
        "kvm"
    }

    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>> {
        let kvm = match open_kvm() {
            Ok(kvm) => kvm,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(Some(
            KvmHandle {
                kvm: kvm.into(),
                nested_virt: false,
                hv_enlightenments: HvEnlightenments::default(),
                cpu_model: None,
            }
            .into_resource(),
        ))
    }

    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>> {
        let mut nested_virt = false;
        let mut hv_enlightenments = HvEnlightenments::default();
        let mut hv_spec = None;
        let mut overrides = HvSpecOverrides::default();
        let mut cpu_model = None;
        for &(key, val) in params {
            match key {
                "nested_virt" => {
                    if cfg!(guest_arch = "x86_64") {
                        nested_virt = parse_bool_param(key, val)?;
                    } else {
                        anyhow::bail!("kvm parameter {key} is only supported for x86_64 guests");
                    }
                }
                "hv" => {
                    if cfg!(guest_arch = "x86_64") {
                        // Defer applying the spec until the whole param list is
                        // parsed: the `windows` preset is nested-aware, so it
                        // needs the final `nested_virt`, which may appear after
                        // `hv=` on the command line.
                        hv_spec = Some(val);
                    } else {
                        anyhow::bail!("kvm parameter {key} is only supported for x86_64 guests");
                    }
                }
                "cpu" => {
                    if !cfg!(guest_arch = "x86_64") {
                        anyhow::bail!("kvm parameter {key} is only supported for x86_64 guests");
                    }
                    if val.is_empty() {
                        anyhow::bail!("kvm cpu parameter requires a model name");
                    }
                    cpu_model = Some(val.to_owned());
                }
                _ => anyhow::bail!("unknown kvm parameter: {key}"),
            }
        }
        if let Some(spec) = hv_spec {
            // The `windows` preset resolves to the nested or non-nested set
            // based on `nested_virt`; later `+name`/`+no_name` tokens still win.
            overrides = hv_enlightenments
                .apply_spec(spec, nested_virt)
                .map_err(|e| anyhow::anyhow!("kvm hv parameter: {e}"))?;
        } else if nested_virt {
            // A nested guest needs the nested enlightenment set; default to it
            // when nesting is on and no explicit `hv` spec was given.
            hv_enlightenments = HvEnlightenments::windows_nested();
        }

        // eVMCS and reenlightenment are nested-only enlightenments: only a guest
        // hypervisor uses them, and their CPUID is coherent only when `nested`
        // is also advertised (the eVMCS version lives in the HV_NESTED_FEATURES
        // leaf, which is emitted only for a nested partition). Reject the
        // inconsistent combination up front instead of booting a guest that sees
        // eVMCS advertised with nesting off. The presets already gate both flags
        // on `nested_virt`, so this only catches an explicit `hv=...+evmcs`.
        if (hv_enlightenments.evmcs || hv_enlightenments.reenlightenment) && !nested_virt {
            anyhow::bail!(
                "kvm hv parameter: evmcs and reenlightenment are nested-only \
                 enlightenments and require nested_virt; add nested_virt or drop them"
            );
        }

        let kvm = open_kvm().context("KVM is not available")?;

        // Narrow stimer_direct to what this host actually supports, leaving it
        // alone if the user pinned it in the `hv=` spec. Probe through a
        // throwaway handle on the same `/dev/kvm`: the check is a read-only
        // system-level ioctl and does not touch the partition. Only relevant on
        // x86_64; the host caps are all false on other guest arches, which
        // leaves the (already-default-off) flag untouched. Gate with `#[cfg]`,
        // not `cfg!(...)`: the probe helpers are themselves
        // `#[cfg(guest_arch = "x86_64")]`, so a runtime `cfg!` still typechecks
        // the call and fails to compile on other guest arches.
        #[cfg(guest_arch = "x86_64")]
        if hv_enlightenments.stimer_direct {
            // stimer_direct is the only host-gated flag, so skip the extra
            // `/dev/kvm` fd and the ioctl when it is not in the requested set.
            match probe_hv_host_caps() {
                Ok(caps) => {
                    let adjustments = hv_enlightenments.adjust_for_host(caps, overrides);
                    warn_host_adjustments(adjustments);
                }
                Err(err) => {
                    // Conservative on a probe failure: keep the requested set
                    // rather than break a host that may well support it.
                    tracing::warn!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "could not probe KVM host timer-enlightenment support; \
                         keeping the requested Hyper-V enlightenments unchanged"
                    );
                }
            }
        }
        // `overrides` only feeds the x86_64 host-adjust above; consume it on
        // other guest arches so the binding does not warn as unused.
        #[cfg(not(guest_arch = "x86_64"))]
        let _ = overrides;

        Ok(KvmHandle {
            kvm: kvm.into(),
            nested_virt,
            hv_enlightenments,
            cpu_model,
        }
        .into_resource())
    }
}

/// Probes the KVM host for the capabilities that gate the timer enlightenments.
///
/// Opens a fresh `/dev/kvm` handle for the probe rather than reusing the
/// partition's, since both checks are read-only system-level ioctls
/// (`KVM_CHECK_EXTENSION` and `KVM_GET_SUPPORTED_HV_CPUID`) and the result feeds
/// the preset before the partition exists.
#[cfg(guest_arch = "x86_64")]
fn probe_hv_host_caps() -> anyhow::Result<HvHostCaps> {
    // CPUID `0x40000003` (HV_CPUID_FUNCTION_MS_HV_FEATURES) EDX bit 19,
    // HV_STIMER_DIRECT_MODE_AVAILABLE: the host advertises direct-mode
    // synthetic timers. KVM returns it through KVM_GET_SUPPORTED_HV_CPUID only
    // when the in-kernel LAPIC is in use; there is no separate enable cap.
    const HV_CPUID_FUNCTION_MS_HV_FEATURES: u32 = 0x4000_0003;
    const HV_STIMER_DIRECT_MODE_AVAILABLE: u32 = 1 << 19;

    let kvm = kvm::Kvm::new().context("failed to open /dev/kvm for capability probe")?;

    let hv_cpuid = kvm
        .supported_hv_cpuid()
        .context("KVM_GET_SUPPORTED_HV_CPUID failed")?;
    let stimer_direct = hv_cpuid
        .iter()
        .find(|e| e.function == HV_CPUID_FUNCTION_MS_HV_FEATURES)
        .is_some_and(|e| e.edx & HV_STIMER_DIRECT_MODE_AVAILABLE != 0);

    Ok(HvHostCaps { stimer_direct })
}

/// Emits a one-time setup warning for each host-driven enlightenment change.
#[cfg(guest_arch = "x86_64")]
fn warn_host_adjustments(adjustments: HvHostAdjustments) {
    if adjustments.stimer_direct_dropped_unsupported {
        tracing::warn!(
            "host does not advertise direct-mode synthetic timers \
             (HV_STIMER_DIRECT_MODE_AVAILABLE absent in CPUID 0x40000003 EDX); \
             disabling stimer_direct, which cannot work without host support."
        );
    }
    if adjustments.stimer_direct_request_unsupported {
        tracing::warn!(
            "stimer_direct was requested but the host does not advertise \
             direct-mode synthetic timers (HV_STIMER_DIRECT_MODE_AVAILABLE absent \
             in CPUID 0x40000003 EDX); leaving it off, since it cannot work \
             without host support."
        );
    }
}

fn open_kvm() -> std::io::Result<fs_err::File> {
    fs_err::File::options()
        .read(true)
        .write(true)
        .open("/dev/kvm")
}

use crate::parse_bool_param;
