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
//! module's own atom table. `-on_load(F/A)` names a **local** function (the `on_load` chunk
//! carries only name/arity, never an export — `crypto:on_load/0` is not in `crypto`'s `ExpT`
//! chunk), so the hook is entered by function index through the module's **function** table
//! (`Module::function(..).entry_label`) via [`RuntimeContext::run_label_blocking`], never
//! [`RuntimeContext::run_export`]. The run sets `current_module` to the module's name for the
//! whole hook (so `erlang:load_nif/2` inside it registers its library under the right owner)
//! and observes the return term synchronously.

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
    /// `badarg` when `name` is not loaded, the loaded image has no `-on_load` entry, or the
    /// entry's `(function, arity)` is not defined by the module's function table. The hook's own
    /// raise propagates unchanged (after [`RuntimeContext::run_label_blocking`] names the reason
    /// atom).
    pub(crate) fn run_on_load_function(&mut self, name: &str) -> Result<Eterm> {
        let handle = self
            .runtime
            .code_server
            .module_handle(name)
            .ok_or(BeamError::BadArg)?;
        let (function, arity) = handle.on_load.ok_or(BeamError::BadArg)?;
        // `-on_load(F/A)` names a *local* function — the `on_load` chunk carries only the
        // name/arity, and ERTS runs it by function index, never through the export table:
        // `crypto:on_load/0` is not in `crypto`'s `ExpT` chunk yet still runs. Resolve the
        // entry label from the module's **function** table (locals included), not `exports`.
        let entry_label = handle
            .function(function, arity)
            .map(|function| function.entry_label)
            .ok_or(BeamError::BadArg)?;
        // Everything the hook executes is this module's code, so `erlang:load_nif/2` reached
        // from it must see this name (the same identity `run_export` establishes for an export).
        let previous = self
            .current_module
            .replace(handle.atom_name(handle.name).unwrap_or_default().to_string());
        let result = self
            .run_label_blocking(&handle, entry_label, arity, &[])
            .map_err(|err| error::name_atom_raise(err, self.runtime.atoms()));
        self.current_module = previous;
        result
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
        // A `-on_load` body that must `receive` cannot run inline inside a scheduler slice: the
        // slice's nested machine cannot pump the scheduler (`pump_once` answers `Ok(None)` under
        // `may_pump_scheduler() == false`), so the hook's `receive` suspends and the load folds
        // into `blocked()`. Instead, defer the hook to the scheduler boundary's drain
        // ([`Runtime::drain_pending_on_loads`], called from `pump_run_to_completion`), which runs
        // it on a pumpable context exactly like the `-eval` route. A previously failed drain
        // consumed here makes the re-executing `code:ensure_loaded/1` answer `on_load_failure`
        // rather than deferring forever.
        if self.runtime.on_load_failed.remove(name) {
            return Err(error::message("on_load_failure"));
        }
        if !self.may_pump_scheduler() {
            if !self.runtime.pending_on_loads.iter().any(|n| n == name) {
                self.runtime.pending_on_loads.push(name.to_string());
            }
            self.request_suspend();
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
