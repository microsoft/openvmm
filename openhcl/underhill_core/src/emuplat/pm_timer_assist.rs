// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PM timer assist resolver for Underhill.
//!
//! This module provides the [`UnderhillPmTimerAssistResolver`] which resolves
//! platform resources for the PM timer assist in the Underhill environment.

use chipset_resources::pm::PmTimerAssist;
use chipset_resources::pm::PmTimerAssistHandleKind;
use chipset_resources::pm::ResolvedPmTimerAssist;
use cvm_tracing::CVM_ALLOWED;
use virt_mshv_vtl::UhPartition;
use vm_resource::PlatformResource;
use vm_resource::ResolveResource;

/// PM timer assist implementation backed by the Underhill partition.
pub struct UnderhillPmTimerAssist {
    pub partition: std::sync::Weak<UhPartition>,
}

impl PmTimerAssist for UnderhillPmTimerAssist {
    fn set(&self, port: Option<u16>) {
        if let Some(partition) = self.partition.upgrade() {
            if let Err(err) = partition.set_pm_timer_assist(port) {
                tracing::warn!(
                    CVM_ALLOWED,
                    error = &err as &dyn std::error::Error,
                    ?port,
                    "failed to set PM timer assist"
                );
            }
        }
    }
}

/// Resolver for the PM timer assist platform resource in Underhill.
pub struct UnderhillPmTimerAssistResolver {
    pub partition: std::sync::Weak<UhPartition>,
}

impl ResolveResource<PmTimerAssistHandleKind, PlatformResource> for UnderhillPmTimerAssistResolver {
    type Output = ResolvedPmTimerAssist;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        _resource: PlatformResource,
        _input: (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedPmTimerAssist(Box::new(UnderhillPmTimerAssist {
            partition: self.partition.clone(),
        })))
    }
}
