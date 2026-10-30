// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The `-on_load` module hook: run a module's `-on_load` entry point synchronously when it
//! loads, and record the result in the code server's [`on_load_done`](CodeServer::on_load_done)
//! set so a cached image runs its hook exactly once.
//!
//! The entry point is the loader's `Module::on_load: Option<(Atom, u32)>` — the
//! `(function_atom, arity)` derived from the `on_load/0` instruction marker, interned in that
//! module's own atom table. It is run through [`RuntimeContext::run_export`], which lowers the
//! real engine IR (never the tree walker), sets `current_module` to the module's name for the
//! whole run (so `erlang:load_nif/2` inside the hook registers its library under the right
//! owner), and observes the return term synchronously.

use beam_common::{BeamError, Result};
use beam_term::prelude::{atom_val, is_atom};
use beam_term::Eterm;

use crate::context::RuntimeContext;
use crate::error;

impl RuntimeContext<'_> {
    /// Runs `name`'s `-on_load` entry point synchronously and answers its return term.
    ///
    /// # Errors
    ///
    /// `badarg` when `name` is not loaded, or the loaded image has no `-on_load` entry (or its
    /// function atom has no name in the module's atom table). The hook's own raise propagates
    /// unchanged (after [`RuntimeContext::run_export`] names the reason atom).
    pub(crate) fn run_on_load_function(&mut self, name: &str) -> Result<Eterm> {
        let handle = self
            .runtime
            .code_server
            .module_handle(name)
            .ok_or(BeamError::BadArg)?;
        let (function, arity) = handle.on_load.ok_or(BeamError::BadArg)?;
        let function = handle
            .atom_name(function)
            .ok_or(BeamError::BadArg)?
            .to_string();
        self.run_export(&handle, &function, arity, &[])
    }

    /// Runs `name`'s `-on_load` hook **once per cached image**: `Ok(())` on `ok` (recorded in
    /// [`CodeServer::mark_on_load_done`]), and on anything else the module is forgotten and an
    /// error raised — the raise [`RuntimeContext::run_on_load_function`] produced, or
    /// `error on_load_failure` for a non-`ok` return term.
    ///
    /// The no-op cases — `name` not loaded, its image carrying no `-on_load` entry, or the hook
    /// already done for this image — answer `Ok(())`, so the common (hook-less) load path and
    /// an already-hooked reload are unchanged.
    pub(crate) fn run_module_on_load(&mut self, name: &str) -> Result<()> {
        if self.runtime.code_server.on_load_done(name) {
            return Ok(());
        }
        let Some(handle) = self.runtime.code_server.module_handle(name) else {
            return Ok(());
        };
        if handle.on_load.is_none() {
            return Ok(());
        }
        match self.run_on_load_function(name) {
            Ok(term) if is_atom(term) && self.runtime.atoms().name(atom_val(term)) == Some("ok") => {
                self.runtime.code_server.mark_on_load_done(name);
                Ok(())
            }
            Ok(_) => {
                self.runtime.code_server.forget_module(name);
                Err(error::message("on_load_failure"))
            }
            Err(err) => {
                self.runtime.code_server.forget_module(name);
                Err(err)
            }
        }
    }
}
