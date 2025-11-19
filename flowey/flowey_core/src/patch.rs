// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::node::FlowNodeBase;
use crate::node::NodeHandle;
use crate::node::WriteVar;
use std::collections::BTreeMap;
use std::sync::OnceLock;

pub type PatchFn = fn(&mut PatchManager<'_>);

// A patchfn that does nothing. Can be useful when writing logic that
// conditionally applies patches.
pub fn noop_patchfn(_: &mut PatchManager<'_>) {}

enum PatchEvent {
    Swap {
        from_old_node: NodeHandle,
        with_new_node: NodeHandle,
    },
    InjectSideEffect {
        from_old_node: NodeHandle,
        with_new_node: NodeHandle,
        side_effect_var: String,
        req: Box<[u8]>,
    },
}

trait PatchManagerBackend {
    fn new_side_effect_var(&mut self) -> String;
    fn on_patch_event(&mut self, event: PatchEvent);
}

/// Passed to patch functions
pub struct PatchManager<'a> {
    backend: &'a mut dyn PatchManagerBackend,
}

impl PatchManager<'_> {
    pub fn hook<N: FlowNodeBase>(&mut self) -> PatchHook<'_, N> {
        PatchHook {
            backend: self.backend,
            _kind: std::marker::PhantomData,
        }
    }
}

/// Patch operations in the context of a particular Node.
pub struct PatchHook<'a, N: FlowNodeBase> {
    backend: &'a mut dyn PatchManagerBackend,
    _kind: std::marker::PhantomData<N>,
}

impl<N> PatchHook<'_, N>
where
    N: FlowNodeBase + 'static,
{
    /// Swap out the target Node's implementation with a different
    /// implementation.
    pub fn swap_with<M>(&mut self) -> &mut Self
    where
        M: 'static,
        // use the type system to enforce that patch nodes have an identical
        // request type
        M: FlowNodeBase<Request = N::Request>,
    {
        self.backend.on_patch_event(PatchEvent::Swap {
            from_old_node: NodeHandle::from_type::<N>(),
            with_new_node: NodeHandle::from_type::<M>(),
        });
        self
    }

    /// Inject a side-effect dependency, which runs before any other steps in
    /// the Node.
    pub fn inject_side_effect<T, M>(
        &mut self,
        f: impl FnOnce(WriteVar<T>) -> M::Request,
    ) -> &mut Self
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
        M: 'static,
        M: FlowNodeBase,
    {
        let backing_var = self.backend.new_side_effect_var();
        let req = f(crate::node::thin_air_write_runtime_var(backing_var.clone()));

        self.backend.on_patch_event(PatchEvent::InjectSideEffect {
            from_old_node: NodeHandle::from_type::<N>(),
            with_new_node: NodeHandle::from_type::<M>(),
            side_effect_var: backing_var,
            req: serde_json::to_vec(&req).map(Into::into).unwrap(),
        });
        self
    }
}

pub fn patchfn_by_modpath() -> &'static BTreeMap<String, PatchFn> {
    static MODPATH_LOOKUP: OnceLock<BTreeMap<String, PatchFn>> = OnceLock::new();

    MODPATH_LOOKUP.get_or_init(|| {
        let mut lookup = BTreeMap::new();
        for (f, module_path, fn_name) in private::PATCH_FNS {
            let existing = lookup.insert(format!("{}::{}", module_path, fn_name), *f);
            // Rust would've errored out at module defn time with a duplicate fn name error
            assert!(existing.is_none());
        }
        lookup
    })
}

/// [`PatchResolver`]
#[derive(Debug, Clone)]
pub struct ResolvedPatches {
    pub swap: BTreeMap<NodeHandle, NodeHandle>,
    pub inject_side_effect: BTreeMap<NodeHandle, Vec<(NodeHandle, String, Box<[u8]>)>>,
}

impl ResolvedPatches {
    pub fn build() -> PatchResolver {
        PatchResolver {
            side_effect_var_idx: 0,
            swap: BTreeMap::default(),
            inject_side_effect: BTreeMap::new(),
        }
    }
}

/// Helper method to resolve multiple patches into a single [`ResolvedPatches`]
#[derive(Debug)]
pub struct PatchResolver {
    side_effect_var_idx: usize,
    swap: BTreeMap<NodeHandle, NodeHandle>,
    inject_side_effect: BTreeMap<NodeHandle, Vec<(NodeHandle, String, Box<[u8]>)>>,
}

impl PatchResolver {
    pub fn apply_patchfn(&mut self, patchfn: PatchFn) {
        patchfn(&mut PatchManager { backend: self });
    }

    pub fn finalize(self) -> ResolvedPatches {
        let Self {
            swap,
            mut inject_side_effect,
            side_effect_var_idx: _,
        } = self;

        // take into account the interaction between swaps and injected effects
        for (from, to) in &swap {
            let injected = inject_side_effect.remove(from);
            if let Some(injected) = injected {
                inject_side_effect.insert(*to, injected);
            }
        }

        ResolvedPatches {
            swap,
            inject_side_effect,
        }
    }
}

impl PatchManagerBackend for PatchResolver {
    fn new_side_effect_var(&mut self) -> String {
        self.side_effect_var_idx += 1;
        format!("patch_side_effect:{}", self.side_effect_var_idx)
    }

    fn on_patch_event(&mut self, event: PatchEvent) {
        match event {
            PatchEvent::Swap {
                from_old_node,
                with_new_node,
            } => {
                let existing = self.swap.insert(from_old_node, with_new_node);
                // FUTURE: add some better error reporting / logging to
                // allow doing this, albeit with a warning
                assert!(
                    existing.is_none(),
                    "cannot double-patch the same node combo"
                );
            }
            PatchEvent::InjectSideEffect {
                from_old_node,
                with_new_node,
                side_effect_var,
                req,
            } => {
                self.inject_side_effect
                    .entry(from_old_node)
                    .or_default()
                    .push((with_new_node, side_effect_var, req));
            }
        }
    }
}

#[doc(hidden)]
pub mod private {
    use super::PatchFn;
    pub use linkme;

    #[linkme::distributed_slice]
    pub static PATCH_FNS: [(PatchFn, &'static str, &'static str)] = [..];

    /// Register a patch function which can be used when emitting flows.
    ///
    /// The function must conform to the signature of [`PatchFn`]
    #[macro_export]
    macro_rules! register_patch {
        ($patchfn:ident) => {
            const _: () = {
                use $crate::node::private::linkme;

                #[linkme::distributed_slice($crate::patch::private::PATCH_FNS)]
                #[linkme(crate = linkme)]
                pub static PATCH_FNS: ($crate::patch::PatchFn, &'static str, &'static str) =
                    ($patchfn, module_path!(), stringify!($patchfn));
            };
        };
    }
}
