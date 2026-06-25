// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource types and probe infrastructure for hypervisor backends.
//!
//! This crate defines [`HypervisorKind`] (the resource kind for hypervisor
//! backends), per-backend handle types, and the [`HypervisorProbe`] trait +
//! distributed slice used for auto-detection.
//!
//! Backends register probes via the [`register_hypervisor_probes!`] macro.
//! Callers use [`probes()`] to iterate registered backends
//! and [`probe_by_name()`] to look up a specific one.

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::ResourceKind;

/// Resource kind for hypervisor backends.
///
/// A [`Resource<HypervisorKind>`] identifies which hypervisor backend to use
/// and can carry backend-specific initialization data.
pub enum HypervisorKind {}

impl ResourceKind for HypervisorKind {
    const NAME: &'static str = "hypervisor";
}

/// Selects which Hyper-V enlightenments the partition advertises to the guest
/// (in the synthetic-hypervisor CPUID leaves) and enables in the backend (via
/// the matching KVM capabilities). [`Default`] is the base set.
#[derive(Debug, Copy, Clone, MeshPayload)]
pub struct HvEnlightenments {
    /// Reference time: the partition reference counter and reference TSC page.
    pub time: bool,
    /// The TSC and APIC frequency MSRs.
    pub frequencies: bool,
    /// The hypercall MSRs (`HV_X64_MSR_GUEST_OS_ID` / `HV_X64_MSR_HYPERCALL`).
    pub hypercall: bool,
    /// The VP index MSR.
    pub vp_index: bool,
    /// The VP runtime MSR.
    pub vp_runtime: bool,
    /// The synthetic interrupt controller; also enables `KVM_CAP_HYPERV_SYNIC2`.
    pub synic: bool,
    /// Synthetic timers.
    pub stimer: bool,
    /// Direct-mode synthetic timers.
    pub stimer_direct: bool,
    /// APIC access through MSRs and the virtual APIC assist page.
    pub vapic: bool,
    /// Relaxed timing: slacken watchdog and spinlock deadlines because the
    /// guest is virtualized.
    pub relaxed: bool,
    /// Recommend the hypercall-based remote-TLB-flush enlightenment, with
    /// extended processor masks.
    pub tlbflush: bool,
    /// Recommend the synthetic cluster IPI enlightenment.
    pub ipi: bool,
    /// Enlightened VMCS for a nested hypervisor; also enables
    /// `KVM_CAP_HYPERV_ENLIGHTENED_VMCS`.
    pub evmcs: bool,
    /// Reenlightenment control MSRs, used by a nested hypervisor on a TSC
    /// frequency change.
    pub reenlightenment: bool,
    /// Enforce that the guest may only use enlightenments advertised in its
    /// CPUID; also enables `KVM_CAP_HYPERV_ENFORCE_CPUID`.
    pub enforce_cpuid: bool,
    /// The spinlock-retry count reported in the enlightenment-recommendations
    /// leaf. `0xffffffff` disables spinlock-failure notifications.
    pub spinlock_retries: u32,
}

impl Default for HvEnlightenments {
    /// The base set: reference time, frequencies, hypercall, VP index, VP
    /// runtime, SynIC, synthetic timer, and APIC MSRs, with spinlock
    /// notifications off.
    fn default() -> Self {
        Self {
            time: true,
            frequencies: true,
            hypercall: true,
            vp_index: true,
            vp_runtime: true,
            synic: true,
            stimer: true,
            stimer_direct: false,
            vapic: true,
            relaxed: false,
            tlbflush: false,
            ipi: false,
            evmcs: false,
            reenlightenment: false,
            enforce_cpuid: false,
            spinlock_retries: 0xffffffff,
        }
    }
}

impl HvEnlightenments {
    /// Every enlightenment off; the base for the `none` preset.
    fn none() -> Self {
        Self {
            time: false,
            frequencies: false,
            hypercall: false,
            vp_index: false,
            vp_runtime: false,
            synic: false,
            stimer: false,
            stimer_direct: false,
            vapic: false,
            relaxed: false,
            tlbflush: false,
            ipi: false,
            evmcs: false,
            reenlightenment: false,
            enforce_cpuid: false,
            spinlock_retries: 0xffffffff,
        }
    }

    /// The base set plus the nested additions: enlightened VMCS, direct
    /// synthetic timers, reenlightenment, relaxed timing, remote TLB flush, and
    /// cluster IPI.
    ///
    /// `stimer_direct` is set here, but on a KVM host the backend adjusts it to
    /// what the host actually supports unless the user pinned it in the `hv=`
    /// spec. See [`adjust_for_host`] for the rules and the spec grounding.
    ///
    /// `evmcs` is in the preset and stays on unconditionally, because a nested
    /// Windows guest needs it to boot from a synthetic (VMBus) storage
    /// controller. Without enlightened VMCS the guest hypervisor takes a
    /// VMREAD/VMWRITE exit storm on its nested VMCS and the storage path times
    /// out before the boot disk responds. To turn it off, pass
    /// `hv=windows+no_evmcs`.
    ///
    /// [`adjust_for_host`]: HvEnlightenments::adjust_for_host
    ///
    /// `enforce_cpuid` is deliberately left out, kept below as a commented line
    /// so the reasoning survives and it is not re-added by reflex. It enables
    /// `KVM_CAP_HYPERV_ENFORCE_CPUID`, after which the host rejects any Hyper-V
    /// MSR or hypercall whose CPUID feature bit is not advertised. A nested
    /// hypervisor accesses synthetic MSRs and hypercalls while bringing up its
    /// own partition, before its first guest entry, so the rejection stalls it
    /// there and the OS never runs as its child partition. The TLFS defines no
    /// "deny if absent from CPUID" behavior; it is a host hardening knob, off by
    /// default, and enabling it for a nested guest is counterproductive. It
    /// stays reachable as `hv=windows+enforce_cpuid`.
    pub fn windows_nested() -> Self {
        Self {
            relaxed: true,
            tlbflush: true,
            ipi: true,
            evmcs: true,
            reenlightenment: true,
            stimer_direct: true,
            // enforce_cpuid: true, // omitted, see the doc comment above
            ..Self::default()
        }
    }

    /// The Windows set for a partition that does **not** run its own
    /// hypervisor: [`windows_nested`] minus `evmcs` and `reenlightenment`.
    ///
    /// Enlightened VMCS and reenlightenment are nested-virtualization
    /// optimizations. `evmcs` speeds up a guest hypervisor's access to its
    /// nested VMCS (TLFS Appendix A); `reenlightenment` lets a guest hypervisor
    /// react to a TSC frequency change while it runs its own partitions. A
    /// plain Windows guest has no nested VMCS and runs no child partition, so
    /// neither does anything for it. The rest of the
    /// nested set still helps a non-nested guest: `stimer_direct` lowers
    /// synthetic-timer latency, and `relaxed`/`tlbflush`/`ipi` plus the default
    /// base apply to any virtualized Windows guest, so they are kept.
    ///
    /// This is what `hv=windows` resolves to when the partition's `nested_virt`
    /// is clear. With `nested_virt` set it resolves to [`windows_nested`]
    /// instead. Explicit `+name`/`+no_name` tokens still win over either base,
    /// and [`adjust_for_host`] still narrows `stimer_direct` to the host after.
    ///
    /// [`windows_nested`]: HvEnlightenments::windows_nested
    /// [`adjust_for_host`]: HvEnlightenments::adjust_for_host
    pub fn windows_non_nested() -> Self {
        Self {
            evmcs: false,
            reenlightenment: false,
            ..Self::windows_nested()
        }
    }

    /// Applies a `+`-separated spec to `self`. The first token may be a preset
    /// (`default`, `windows`, or `none`); each remaining token enables a flag
    /// by name, or disables it with a `no_` prefix (e.g. `windows+no_evmcs`).
    /// `spinlocks=<n>` sets the retry count.
    ///
    /// The `windows` preset is nested-aware: with `nested_virt` set it selects
    /// [`windows_nested`](Self::windows_nested), and with it clear it selects
    /// [`windows_non_nested`](Self::windows_non_nested) (the same set without
    /// `evmcs` and `reenlightenment`, which only a guest hypervisor uses). The
    /// `default` and `none` presets ignore the flag.
    ///
    /// Returns which flags the spec set explicitly, so a later host-aware step
    /// ([`adjust_for_host`](Self::adjust_for_host)) can tell a user-pinned flag
    /// from a preset default and only narrow the latter. A preset token (the
    /// first token) is not counted as explicit, since it carries the defaults
    /// that the host narrowing is allowed to override; a later `+name` or
    /// `+no_name` token is.
    pub fn apply_spec(&mut self, spec: &str, nested_virt: bool) -> Result<HvSpecOverrides, String> {
        let mut overrides = HvSpecOverrides::default();
        let mut first = true;
        let mut applied = false;
        for token in spec.split('+') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            applied = true;
            if first {
                first = false;
                match token {
                    "default" => {
                        *self = Self::default();
                        continue;
                    }
                    "windows" => {
                        *self = if nested_virt {
                            Self::windows_nested()
                        } else {
                            Self::windows_non_nested()
                        };
                        continue;
                    }
                    "none" => {
                        *self = Self::none();
                        continue;
                    }
                    _ => {}
                }
            }
            if let Some(n) = token.strip_prefix("spinlocks=") {
                self.spinlock_retries =
                    parse_int(n).ok_or_else(|| format!("invalid spinlocks value: {n}"))?;
                continue;
            }
            let (name, value) = match token.strip_prefix("no_") {
                Some(rest) => (rest, false),
                None => (token, true),
            };
            let field = self
                .field_mut(name)
                .ok_or_else(|| format!("unknown hv enlightenment: {name}"))?;
            *field = value;
            if name == "stimer_direct" {
                overrides.stimer_direct = true;
            }
        }
        if !applied {
            return Err("empty hv enlightenment spec".to_string());
        }
        Ok(overrides)
    }

    /// Narrows `stimer_direct`, the one host-sensitive enlightenment, to what
    /// the KVM host actually supports, leaving it alone if the user pinned it in
    /// the `hv=` spec (tracked in `overrides`). eVMCS is never narrowed here.
    ///
    /// One flag is adjusted, grounded in the TLFS contract and the KVM
    /// capability that backs it:
    ///
    /// * `stimer_direct`: direct-mode synthetic timers (TLFS 11.8.4, "Synthetic
    ///   Timer Configuration Register", `DirectMode`) only work when the host
    ///   advertises `HV_STIMER_DIRECT_MODE_AVAILABLE`, bit 19 of the synthetic
    ///   hypervisor feature CPUID leaf `0x40000003` EDX
    ///   (`HV_CPUID_FUNCTION_MS_HV_FEATURES`). KVM returns that bit through
    ///   `KVM_GET_SUPPORTED_HV_CPUID` only when the in-kernel LAPIC is in use;
    ///   there is no separate enable capability. When `caps.stimer_direct` is
    ///   false the flag is forced off, because the guest cannot use a feature
    ///   the host does not implement. `hv=windows+stimer_direct` is honored as a
    ///   request, but it still cannot be enabled, so it is also forced off with
    ///   a warning rather than advertised and left to fail.
    ///
    /// Returns a record of the adjustments made so the caller can emit a
    /// one-time warning describing what changed and how to override it.
    pub fn adjust_for_host(
        &mut self,
        caps: HvHostCaps,
        overrides: HvSpecOverrides,
    ) -> HvHostAdjustments {
        let mut adjustments = HvHostAdjustments::default();

        // Direct stimers: force off when the host does not advertise direct
        // mode. This cannot be overridden, since the feature is unimplemented
        // on the host; an explicit `+stimer_direct` is downgraded to off and
        // flagged so the caller can warn the user their request was ignored.
        if self.stimer_direct && !caps.stimer_direct {
            self.stimer_direct = false;
            if overrides.stimer_direct {
                adjustments.stimer_direct_request_unsupported = true;
            } else {
                adjustments.stimer_direct_dropped_unsupported = true;
            }
        }

        adjustments
    }

    fn field_mut(&mut self, name: &str) -> Option<&mut bool> {
        Some(match name {
            "time" => &mut self.time,
            "frequencies" => &mut self.frequencies,
            "hypercall" => &mut self.hypercall,
            "vpindex" | "vp_index" => &mut self.vp_index,
            "runtime" | "vp_runtime" => &mut self.vp_runtime,
            "synic" => &mut self.synic,
            "stimer" => &mut self.stimer,
            "stimer_direct" => &mut self.stimer_direct,
            "vapic" => &mut self.vapic,
            "relaxed" => &mut self.relaxed,
            "tlbflush" => &mut self.tlbflush,
            "ipi" => &mut self.ipi,
            "evmcs" => &mut self.evmcs,
            "reenlightenment" => &mut self.reenlightenment,
            "enforce_cpuid" => &mut self.enforce_cpuid,
            _ => return None,
        })
    }
}

/// Whether the `hv=` spec set `stimer_direct` explicitly.
///
/// [`HvEnlightenments::adjust_for_host`] only narrows `stimer_direct` when the
/// user did not pin it, so it needs to know whether the flag came from a
/// `+stimer_direct`/`+no_stimer_direct` token rather than from a preset default.
#[derive(Debug, Copy, Clone, Default)]
pub struct HvSpecOverrides {
    /// The spec set `stimer_direct` or `no_stimer_direct` explicitly.
    pub stimer_direct: bool,
}

/// Host capabilities that gate `stimer_direct`, probed from the KVM device
/// before the preset is finalized.
#[derive(Debug, Copy, Clone, Default)]
pub struct HvHostCaps {
    /// The host advertises direct-mode synthetic timers,
    /// `HV_STIMER_DIRECT_MODE_AVAILABLE` (bit 19) in CPUID `0x40000003` EDX
    /// from `KVM_GET_SUPPORTED_HV_CPUID`.
    pub stimer_direct: bool,
}

/// What [`HvEnlightenments::adjust_for_host`] changed, so the caller can emit a
/// single setup-time warning. All-false means the host supports the preset as
/// written (or the user pinned everything).
#[derive(Debug, Copy, Clone, Default)]
pub struct HvHostAdjustments {
    /// `stimer_direct` was dropped (preset default) because the host does not
    /// advertise direct mode.
    pub stimer_direct_dropped_unsupported: bool,
    /// `stimer_direct` was requested explicitly but the host does not support
    /// it, so the request could not be honored.
    pub stimer_direct_request_unsupported: bool,
}

/// Parses a `u32` written in decimal or, with a `0x` prefix, hexadecimal.
fn parse_int(s: &str) -> Option<u32> {
    match s.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// Handle for the KVM hypervisor backend.
///
/// Contains the open `/dev/kvm` file descriptor so that it can be probed
/// early and reused when creating the partition.
#[derive(MeshPayload)]
pub struct KvmHandle {
    /// An open `/dev/kvm` file descriptor, open with read and write
    /// permissions.
    pub kvm: std::fs::File,
    /// Configure the partition for nested virtualization, so that the
    /// guest can run its own hypervisor (Hyper-V, KVM, etc.).
    ///
    /// When false (the default), VMX/SVM CPUID bits and the MS hypervisor
    /// nested-features leaf are stripped from the guest's view.
    pub nested_virt: bool,
    /// Which Hyper-V enlightenments the partition advertises and enables.
    pub hv_enlightenments: HvEnlightenments,
    /// Guest CPU model to present. `None` or `"host"`/`"max"` passes the host
    /// CPU features through; any other name masks the guest CPUID down to that
    /// model's feature set.
    pub cpu_model: Option<String>,
}

impl ResourceId<HypervisorKind> for KvmHandle {
    const ID: &'static str = "kvm";
}

/// Handle for the MSHV hypervisor backend.
#[derive(MeshPayload)]
pub struct MshvHandle {
    /// An open `/dev/mshv` file descriptor.
    pub mshv: std::fs::File,
}

impl ResourceId<HypervisorKind> for MshvHandle {
    const ID: &'static str = "mshv";
}

/// Handle for the WHP hypervisor backend.
#[derive(MeshPayload)]
pub struct WhpHandle {
    /// Use the user-mode APIC emulator instead of the in-hypervisor one.
    ///
    /// Only supported on x86_64. Setting this on aarch64 will cause partition
    /// creation to fail.
    pub user_mode_apic: bool,
    /// Use the hypervisor's in-built enlightenment support if available.
    ///
    /// Only supported on x86_64. Setting this to `false` on aarch64 will cause
    /// partition creation to fail.
    pub offload_enlightenments: bool,
    /// Configure the partition for nested virtualization, so that the
    /// guest can run its own hypervisor (Hyper-V, KVM, etc.).
    ///
    /// Only supported on x86_64. Requires `user_mode_apic = false` and a
    /// host WHP implementation that exposes nested-virt support; partition
    /// creation will fail otherwise.
    pub nested_virt: bool,
}

impl Default for WhpHandle {
    fn default() -> Self {
        Self {
            user_mode_apic: false,
            offload_enlightenments: true,
            nested_virt: false,
        }
    }
}

impl ResourceId<HypervisorKind> for WhpHandle {
    const ID: &'static str = "whp";
}

/// Handle for the HVF hypervisor backend.
#[derive(MeshPayload)]
pub struct HvfHandle;

impl ResourceId<HypervisorKind> for HvfHandle {
    const ID: &'static str = "hvf";
}

/// Trait for probing hypervisor backend availability.
///
/// Each registered backend provides a probe that can check whether the
/// backend is available and construct a resource for it.
pub trait HypervisorProbe: Send + Sync + 'static {
    /// Short name (e.g. "kvm", "whp"). Matches the handle's `ResourceId::ID`.
    fn name(&self) -> &str;

    /// Checks whether this backend is available and, if so, returns a new
    /// [`Resource<HypervisorKind>`] for it with default settings.
    ///
    /// Used for auto-detection: backends are tried in priority order, and
    /// `Ok(None)` means "skip me, try the next one".
    fn try_new_resource(&self) -> anyhow::Result<Option<Resource<HypervisorKind>>>;

    /// Constructs a [`Resource<HypervisorKind>`] for an explicitly selected
    /// backend, with optional parameters.
    ///
    /// Unlike [`try_new_resource`](Self::try_new_resource), this returns
    /// `Err` (not `Ok(None)`) if the backend is unavailable, so the caller
    /// gets a specific error message.
    ///
    /// `params` contains backend-specific key-value pairs parsed from the
    /// `--hypervisor name:key=val,...` CLI syntax. A bare key (no `=`) is
    /// passed as `(key, "true")`. Backends should return an error for
    /// unrecognized keys.
    fn new_resource(&self, params: &[(&str, &str)]) -> anyhow::Result<Resource<HypervisorKind>>;
}

/// Private module for linkme infrastructure.
#[doc(hidden)]
pub mod private {
    // UNSAFETY: Needed for linkme.
    #![expect(unsafe_code)]

    pub use linkme;

    use super::HypervisorProbe;

    // Use Option<&X> in case the linker inserts some stray nulls, as we
    // think it might on Windows.
    //
    // See <https://devblogs.microsoft.com/oldnewthing/20181108-00/?p=100165>.
    #[linkme::distributed_slice]
    pub static HYPERVISOR_PROBES: [Option<&'static dyn HypervisorProbe>] = [..];

    // Always have at least one entry to work around linker bugs.
    //
    // See <https://github.com/llvm/llvm-project/issues/65855>.
    #[linkme::distributed_slice(HYPERVISOR_PROBES)]
    static WORKAROUND: Option<&'static dyn HypervisorProbe> = None;
}

/// Returns an iterator over all registered hypervisor probes.
///
/// Probes are returned in registration order (highest priority first).
pub fn probes() -> impl Iterator<Item = &'static dyn HypervisorProbe> {
    private::HYPERVISOR_PROBES.iter().flatten().copied()
}

/// Looks up a probe by backend name.
pub fn probe_by_name(name: &str) -> Option<&'static dyn HypervisorProbe> {
    probes().find(|p| p.name() == name)
}

/// Registers hypervisor backend probes for auto-detection.
///
/// Each entry is a unit struct implementing
/// [`HypervisorProbe`].
///
/// Probes are checked in registration order when auto-detecting the
/// hypervisor, so register them from highest to lowest priority.
///
/// Resource resolvers should be registered separately via
/// [`vm_resource::register_static_resolvers!`].
///
/// # Example
///
/// ```ignore
/// hypervisor_resources::register_hypervisor_probes! {
///     #[cfg(all(target_os = "linux", feature = "virt_kvm", guest_is_native))]
///     openvmm_hypervisors::kvm::KvmProbe,
/// }
/// ```
#[macro_export]
macro_rules! register_hypervisor_probes {
    {} => {};
    { $( $(#[$a:meta])* $probe:path ),+ $(,)? } => {
        $(
        $(#[$a])*
        const _: () = {
            static PROBE_INSTANCE: $probe = $probe;

            #[hypervisor_resources::private::linkme::distributed_slice(
                hypervisor_resources::private::HYPERVISOR_PROBES
            )]
            #[linkme(crate = hypervisor_resources::private::linkme)]
            static PROBE: Option<&'static dyn hypervisor_resources::HypervisorProbe> =
                Some(&PROBE_INSTANCE);
        };
        )*
    };
}
