// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core types and traits used to read GitHub context variables.

use crate::node::ClaimVar;
use crate::node::NodeCtx;
use crate::node::ReadVar;
use crate::node::StepCtx;
use crate::pipeline::GhUserSecretVar;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
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

pub mod state {
    pub enum Root {}
    pub enum Global {}
    pub enum Event {}
}

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

impl<S> GhContextVarReader<'_, S> {
    fn read_var<T: Serialize + DeserializeOwned>(
        &self,
        var_name: impl AsRef<str>,
        is_secret: bool,
        is_object: bool,
    ) -> ReadVar<T> {
        let (var, write_var) = self.ctx.new_maybe_secret_var(is_secret, "");
        let write_var = write_var.claim(&mut StepCtx {
            backend: self.ctx.backend.clone(),
        });
        let var_state = GhVarState {
            raw_name: var_name.as_ref().to_string(),
            backing_var: write_var.backing_var,
            is_secret: write_var.is_secret,
            is_object,
        };
        let gh_to_rust = vec![var_state];

        self.ctx.backend.borrow_mut().on_emit_gh_step(
            &format!("🌼 read {}", var_name.as_ref()),
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

impl<'a> GhContextVarReader<'a, state::Root> {
    pub fn global(self) -> GhContextVarReader<'a, state::Global> {
        GhContextVarReader {
            ctx: self.ctx,
            _state: std::marker::PhantomData,
        }
    }

    pub fn event(self) -> GhContextVarReader<'a, state::Event> {
        GhContextVarReader {
            ctx: self.ctx,
            _state: std::marker::PhantomData,
        }
    }

    pub fn secret(self, secret: GhUserSecretVar) -> ReadVar<String> {
        self.read_var(format!("secrets.{}", secret.0), true, false)
    }
}

impl GhContextVarReader<'_, state::Global> {
    pub fn repository(self) -> ReadVar<String> {
        self.read_var("github.repository", false, false)
    }

    pub fn runner_temp(self) -> ReadVar<String> {
        self.read_var("runner.temp", false, false)
    }

    pub fn workspace(self) -> ReadVar<String> {
        self.read_var("github.workspace", false, false)
    }

    pub fn token(self) -> ReadVar<String> {
        self.read_var("github.token", true, false)
    }
}

impl GhContextVarReader<'_, state::Event> {
    pub fn pull_request(self) -> ReadVar<Option<GhContextVarReaderEventPullRequest>> {
        self.read_var("github.event.pull_request", false, true)
    }
}
