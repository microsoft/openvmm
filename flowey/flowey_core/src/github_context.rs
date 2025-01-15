// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core types and traits used to read GitHub context variables.

use crate::node::{user_facing::GhContextVar, ClaimVar, NodeCtx, ReadVar, StepCtx};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct Head {
    #[serde(rename = "ref")]
    pub head_ref: String,
}

#[derive(Serialize, Deserialize)]
pub struct GhContextVarReaderEventPullRequest {
    pub head: Head,
}

pub enum Root {}
pub enum Event {}

#[derive(Clone)]
pub struct GhVarState {
    pub raw_name: String,
    pub backing_var: String,
    pub is_secret: bool,
    pub is_object: bool,
}

pub struct GhContextVarReader<'a, S> {
    pub ctx: NodeCtx<'a>,
    pub _state: std::marker::PhantomData<S>,
}

impl<'a> GhContextVarReader<'a, Root> {
    pub fn global(&self, gh_var: GhContextVar) -> ReadVar<String> {
        let (var, write_var) = self.ctx.new_maybe_secret_var(gh_var.is_secret(), "");
        let write_var = write_var.claim(&mut StepCtx {
            backend: self.ctx.backend.clone(),
        });
        let var_state = GhVarState {
            raw_name: gh_var.as_raw_var_name(),
            backing_var: write_var.backing_var,
            is_secret: write_var.is_secret,
            is_object: false,
        };
        let gh_to_rust = vec![var_state];

        self.ctx.backend.borrow_mut().on_emit_gh_step(
            &format!("🌼 read {}", gh_var.as_raw_var_name()),
            "",
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            BTreeMap::new(),
            gh_to_rust,
            Vec::new(),
        );
        var
    }

    pub fn event(self) -> GhContextVarReader<'a, Event> {
        GhContextVarReader {
            ctx: self.ctx,
            _state: std::marker::PhantomData,
        }
    }
}

impl GhContextVarReader<'_, Event> {
    pub fn pull_request(self) -> ReadVar<Option<GhContextVarReaderEventPullRequest>> {
        let var_name = "github.event.pull_request".to_string();
        let (var, write_var) = self.ctx.new_var();
        let write_var = write_var.claim(&mut StepCtx {
            backend: self.ctx.backend.clone(),
        });
        let var_state = GhVarState {
            raw_name: var_name.clone(),
            backing_var: write_var.backing_var,
            is_secret: write_var.is_secret,
            is_object: true,
        };

        let gh_to_rust = vec![var_state];

        self.ctx.backend.borrow_mut().on_emit_gh_step(
            &format!("🌼 read {}", var_name),
            "",
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            BTreeMap::new(),
            gh_to_rust,
            Vec::new(),
        );
        var
    }
}
