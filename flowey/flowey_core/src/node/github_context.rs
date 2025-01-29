// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core types and traits used to read GitHub context variables.

use crate::node::spec::GhContextVarReaderEventPullRequest;
use crate::node::ClaimVar;
use crate::node::GhUserSecretVar;
use crate::node::NodeCtx;
use crate::node::ReadVar;
use crate::node::StepCtx;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::BTreeMap;

pub mod state {
    pub enum Root {}
    pub enum Global {}
    pub enum Event {}
}

#[derive(Clone)]
pub struct GhVarState {
    pub raw_name: Option<String>,
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
            raw_name: Some(var_name.as_ref().to_string()),
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
    /// Access variables that are globally available `github.repository`, `github.workspace`, etc.
    pub fn global(self) -> GhContextVarReader<'a, state::Global> {
        GhContextVarReader {
            ctx: self.ctx,
            _state: std::marker::PhantomData,
        }
    }

    /// Access variables that are only available in the context of a GitHub event. `github.event.pull_request`, etc.
    pub fn event(self) -> GhContextVarReader<'a, state::Event> {
        GhContextVarReader {
            ctx: self.ctx,
            _state: std::marker::PhantomData,
        }
    }

    /// Access a secret
    pub fn secret(self, secret: GhUserSecretVar) -> ReadVar<String> {
        self.read_var(format!("secrets.{}", secret.0), true, false)
    }
}

impl GhContextVarReader<'_, state::Global> {
    /// `github.repository`
    pub fn repository(self) -> ReadVar<String> {
        self.read_var("github.repository", false, false)
    }

    /// `runner.temp`
    pub fn runner_temp(self) -> ReadVar<String> {
        self.read_var("runner.temp", false, false)
    }

    /// `github.workspace`
    pub fn workspace(self) -> ReadVar<String> {
        self.read_var("github.workspace", false, false)
    }

    /// `github.token`
    pub fn token(self) -> ReadVar<String> {
        self.read_var("github.token", true, false)
    }
}

impl GhContextVarReader<'_, state::Event> {
    /// `github.event.pull_request`
    pub fn pull_request(self) -> ReadVar<Option<GhContextVarReaderEventPullRequest>> {
        self.read_var("github.event.pull_request", false, true)
    }
}
