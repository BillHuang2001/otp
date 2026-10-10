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

//! The `erts_internal:trace/3` + `erts_internal:trace_pattern/3` half of the
//! `erts_internal` floor (contract **A.7**/**A.8**).
//!
//! `erlang:trace/2,3` and `erlang:trace_pattern/2,3` are `erlang.erl` wrappers over
//! these two BIFs (`erl_bif_trace.c`'s `trace_3`/`trace_pattern_3`), so the tracing
//! *surface* is a distinct responsibility from the rest of the floor and lives in this
//! submodule (the parent [`super`] owns the dispatch arm: `("trace", 3)` /
//! `("trace_pattern", 3)`).
//!
//! The installed state is the runtime's own [`crate::trace`] tables — the per-process
//! tracee flags ([`crate::trace::TraceState::trace_mut`]) and the per-function patterns
//! ([`crate::trace::PatternTable`]) — which the engine's execution-trace seam consults.
//! Only **process** targets are modelled (see [`RuntimeContext::erts_trace`]); the
//! port/`new`-style `PidSpec`s are the honest typed capability gap, never a fabricated
//! count.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_process::ProcessId;
use beam_term::{is_small, make_small, signed_value, Eterm};

use super::RuntimeContext;
use crate::error;

impl RuntimeContext<'_> {
    // -- erts_internal:trace/3 -----------------------------------------------

    /// `erts_internal:trace/3` — the body `erlang:trace/2,3` calls (`erl_bif_trace.c`'s
    /// `trace_3`). `PidSpec` selects the tracees, `How` sets (`true`) or clears (`false`) their
    /// trace flags, and `FlagList` is the flag set to install when setting. The **calling**
    /// process becomes the tracer.
    ///
    /// Only **process** targets are modelled: a pid the process table holds, or one of the group
    /// atoms `all`/`existing`/`existing_processes`/`processes`, which select every process in the
    /// table. The port/`new`-style PidSpecs (`new`, `new_processes`, `ports`, `existing_ports`,
    /// `new_ports`) are the honest typed capability gap — never a fabricated count.
    ///
    /// The answer is the number of tracees affected (the C's return value), so a pid the table
    /// does not hold selects nothing and contributes `0`.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a `PidSpec` that is neither a pid nor the atom `all`, a `How`
    /// that is not the atom `true`/`false`, a `FlagList` that is not a proper list, an element of
    /// `FlagList` that is not an atom, or an atom [`crate::trace::flags_from_atoms`] rejects.
    pub(crate) fn erts_trace(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let pid_spec = *args.first().ok_or(BeamError::BadArg)?;
        let how = *args.get(1).ok_or(BeamError::BadArg)?;
        let flag_list = *args.get(2).ok_or(BeamError::BadArg)?;
        // `How` is the boolean `true` (set) / `false` (clear).
        let setting = match self.atom_name(how).as_deref() {
            Some("true") => true,
            Some("false") => false,
            _ => return Err(BeamError::BadArg),
        };
        let targets = self.trace_targets(pid_spec)?;
        if setting {
            let names = self.trace_flag_names(flag_list)?;
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let flags = crate::trace::flags_from_atoms(&refs).ok_or(BeamError::BadArg)?;
            let tracer = Some(self.pid());
            for pid in &targets {
                self.runtime.trace_mut().set(*pid, flags, tracer);
            }
        } else {
            for pid in &targets {
                self.runtime.trace_mut().clear(*pid);
            }
        }
        Ok(make_small(targets.len() as i64))
    }

    /// `erlang:trace(PidSpec, How)` — the `/2` form of [`Self::erts_trace`], with an empty
    /// `FlagList` (the `erlang.erl` wrapper's own expansion).
    pub(crate) fn erts_trace_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let pid_spec = *args.first().ok_or(BeamError::BadArg)?;
        let how = *args.get(1).ok_or(BeamError::BadArg)?;
        self.erts_trace(&[pid_spec, how, beam_term::NIL])
    }

    // -- erts_internal:trace_pattern/3 ---------------------------------------

    /// `erts_internal:trace_pattern/3` — the body `erlang:trace_pattern/2,3` calls
    /// (`erl_bif_trace.c`'s `trace_pattern_3`). `MFA` selects the **functions** to trace
    /// (each of module/function/arity is a name or the `'_'` wildcard), `MatchSpec` installs
    /// (`true`, `[match_spec()]`) or removes (`false`, `[]`) their match spec, and `FlagList`
    /// are the per-function tracing flags to install alongside.
    ///
    /// The answer is the number of **exported** functions of the modules this runtime has
    /// loaded that the `MFA` **argument pattern** matches — the C's return value. It is
    /// computed from the argument alone, so it is the same whether the call installs or
    /// removes; a valid pattern that matches nothing (an unloaded module, an unknown
    /// function, an arity no export has) is `0`, never a raise. `module_info/0,1` — which
    /// every module exports — is counted like any other export, exactly as the C's walk of
    /// the export table counts it.
    ///
    /// The installed patterns live in the runtime's [`crate::trace::PatternTable`]; no BIF
    /// reports them back (see [`crate::trace::PatternTable::matches`], the predicate a future
    /// `beam_engine` per-call hook will consult before emitting a `{trace, …}` event).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] (the `erlang.erl` wrapper turns the raise into `{error, badarg}`)
    /// for:
    /// * an `MFA` that is not a 3-tuple, or whose module/function element is neither an atom
    ///   nor `'_'`, or whose arity element is neither a non-negative small integer nor `'_'`;
    /// * a `MatchSpec` that is neither `true`/`false`, nor a proper list whose every element
    ///   is a two- or three-tuple match function;
    /// * a `FlagList` that is not a proper list of atoms, or that holds any atom outside the
    ///   seven `trace_pattern` flags (`local`, `global`, `meta`, `call_count`, `call_time`,
    ///   `call_memory`, `silent`) — `[call]` is the oracle-observed rejection.
    pub(crate) fn erts_trace_pattern(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let mfa = *args.first().ok_or(BeamError::BadArg)?;
        let match_spec_arg = *args.get(1).ok_or(BeamError::BadArg)?;
        let flag_list = *args.get(2).ok_or(BeamError::BadArg)?;

        // Every argument is validated **before** any state changes, so a bad `MatchSpec` or
        // `FlagList` never half-installs a pattern (the C parses the whole call first).
        let pattern = self.trace_pattern_mfa(mfa)?;
        let match_spec = self.trace_pattern_match_spec(match_spec_arg)?;
        let names = self.trace_flag_names(flag_list)?;
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let flags = crate::trace::pattern_flags_from_atoms(&refs).ok_or(BeamError::BadArg)?;

        // "The number of functions matched": the **argument** pattern against every loaded
        // module's `ExpT` exports, read from the loader IR (the code server's own cache).
        let count = {
            let code_server = &self.runtime.code_server;
            let modules = code_server.modules().map(|module| {
                let name = module.atom_name(module.name).unwrap_or_default();
                let exports = module.exports.iter().filter_map(|export| {
                    module
                        .atom_name(export.function)
                        .map(|function| (function, export.arity))
                });
                (name, exports)
            });
            pattern.count_matches(modules)
        };

        // The match spec is read **later**, by the tracer that emits the events — after the
        // process that installed the pattern may have been collected. Copy its functions onto
        // the pointer-stable arena first (see `Runtime::try_stabilize_escaped`, which reports a
        // function the copier genuinely cannot copy as the honest `system_limit`).
        let match_spec = match match_spec {
            Some(crate::trace::MatchSpec::Functions(functions)) => {
                let functions = functions
                    .into_iter()
                    .map(|function| self.runtime.try_stabilize_escaped(function))
                    .collect::<Result<Vec<Eterm>>>()?;
                Some(crate::trace::MatchSpec::Functions(functions))
            }
            other => other,
        };

        let table = self.runtime.trace_mut().patterns_mut();
        match match_spec {
            Some(match_spec) => table.install(pattern, match_spec, flags),
            None => {
                table.remove(&pattern);
            }
        }
        Ok(make_small(count as i64))
    }

    /// `erlang:trace_pattern(MFA, MatchSpec)` — the `/2` form of
    /// [`Self::erts_trace_pattern`], with an empty `FlagList` (the `erlang.erl` wrapper's own
    /// expansion).
    pub(crate) fn erts_trace_pattern_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let mfa = *args.first().ok_or(BeamError::BadArg)?;
        let match_spec = *args.get(1).ok_or(BeamError::BadArg)?;
        self.erts_trace_pattern(&[mfa, match_spec, beam_term::NIL])
    }

    /// The `{Module, Function, Arity}` pattern of a `trace_pattern` call.
    ///
    /// Each selector is `'_'` (the wildcard, `None`) or a concrete name/arity; anything else —
    /// a non-tuple, a tuple of the wrong arity, a non-atom module/function, a negative or
    /// non-integer arity — is `badarg`.
    fn trace_pattern_mfa(&mut self, term: Eterm) -> Result<crate::trace::MfaPattern> {
        if terms::tuple_arity(term).map_err(|_| BeamError::BadArg)? != 3 {
            return Err(BeamError::BadArg);
        }
        let module = terms::tuple_element(term, 0).map_err(|_| BeamError::BadArg)?;
        let function = terms::tuple_element(term, 1).map_err(|_| BeamError::BadArg)?;
        let arity = terms::tuple_element(term, 2).map_err(|_| BeamError::BadArg)?;
        Ok(crate::trace::MfaPattern {
            module: self.trace_pattern_name(module)?,
            function: self.trace_pattern_name(function)?,
            arity: self.trace_pattern_arity(arity)?,
        })
    }

    /// A `trace_pattern` module/function selector: `'_'` is the wildcard (`None`), any other
    /// atom its own name, and a non-atom `badarg`.
    fn trace_pattern_name(&self, term: Eterm) -> Result<Option<String>> {
        let name = self.atom_name(term).ok_or(BeamError::BadArg)?;
        Ok(if name == "_" { None } else { Some(name) })
    }

    /// A `trace_pattern` arity selector: `'_'` is the wildcard (`None`), a non-negative small
    /// integer its own value (`Some`), and anything else — including a negative or a bignum,
    /// which is not an arity — `badarg`.
    fn trace_pattern_arity(&self, term: Eterm) -> Result<Option<u32>> {
        if let Some(name) = self.atom_name(term) {
            return if name == "_" {
                Ok(None)
            } else {
                Err(BeamError::BadArg)
            };
        }
        if !is_small(term) {
            return Err(BeamError::BadArg);
        }
        u32::try_from(signed_value(term))
            .map(Some)
            .map_err(|_| BeamError::BadArg)
    }

    /// The `MatchSpec` of a `trace_pattern` call: `Some(..)` to install, `None` for the
    /// removal forms.
    ///
    /// * `true` installs a catch-all ([`crate::trace::MatchSpec::MatchAll`]);
    /// * `false` and the empty list `[]` are the two **removal** forms;
    /// * a non-empty proper list is a `[match_spec()]`: every element must be a two- or
    ///   three-tuple match function and the list is then retained verbatim
    ///   ([`crate::trace::MatchSpec::Functions`]). Only the shape is checked — compiling a
    ///   match spec is a later stage, and nothing here ever evaluates one, so a stored spec
    ///   can never produce a wrong result.
    ///
    /// Anything else — another atom, a non-list, an improper list, a list holding a non-tuple
    /// or a tuple of another arity — is `badarg`.
    fn trace_pattern_match_spec(&self, term: Eterm) -> Result<Option<crate::trace::MatchSpec>> {
        if let Some(name) = self.atom_name(term) {
            return match name.as_str() {
                "true" => Ok(Some(crate::trace::MatchSpec::MatchAll)),
                "false" => Ok(None),
                _ => Err(BeamError::BadArg),
            };
        }
        let functions = terms::list_elements(term).map_err(|_| BeamError::BadArg)?;
        if functions.is_empty() {
            return Ok(None);
        }
        for function in &functions {
            let tuple_arity = terms::tuple_arity(*function).map_err(|_| BeamError::BadArg)?;
            if tuple_arity != 2 && tuple_arity != 3 {
                return Err(BeamError::BadArg);
            }
        }
        Ok(Some(crate::trace::MatchSpec::Functions(functions)))
    }

    /// The pids a `PidSpec` selects, filtered to those the process table holds. A pid the table
    /// does not hold selects nothing (the oracle's `0`), so the count an `erts_trace` caller sees
    /// is always the number of tracees actually affected.
    fn trace_targets(&mut self, pid_spec: Eterm) -> Result<Vec<ProcessId>> {
        if let Ok(pid) = ProcessId::from_eterm(pid_spec) {
            return Ok(if self.process_table_mut().contains(pid) {
                vec![pid]
            } else {
                Vec::new()
            });
        }
        match self.atom_name(pid_spec).as_deref() {
            Some("all" | "existing" | "existing_processes" | "processes") => {
                Ok(self.process_table_mut().pids().collect())
            }
            Some(name @ ("new" | "new_processes" | "ports" | "existing_ports" | "new_ports")) => {
                Err(error::unsupported(&format!(
                    "erts_internal:trace/3 with a `{name}` PidSpec (only process targets are \
                     implemented)"
                )))
            }
            _ => Err(BeamError::BadArg),
        }
    }

    /// The atom names in a `FlagList`, rejecting a non-list or a non-atom element.
    fn trace_flag_names(&mut self, flag_list: Eterm) -> Result<Vec<String>> {
        let elements = terms::list_elements(flag_list).map_err(|_| BeamError::BadArg)?;
        let mut names = Vec::with_capacity(elements.len());
        for element in elements {
            let name = self.atom_name(element).ok_or(BeamError::BadArg)?;
            names.push(name);
        }
        Ok(names)
    }
}
