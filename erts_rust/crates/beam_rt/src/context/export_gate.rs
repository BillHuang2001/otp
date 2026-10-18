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

//! The `[safe]` `binary_to_term/2` **active-export gate** — the host half of
//! [`beam_codec::ExportGate`] (contract A.2/A.5).
//!
//! `binary_to_term(Bin, [safe])` does not merely reject unknown atoms: an `EXPORT_EXT` (`113`)
//! fun must name an `M:F/A` that is **active on this node right now**. The C
//! (`external.c:5032-5036`):
//!
//! ```c
//! if (flags & ERTS_DIST_EXT_BTT_SAFE) {
//!     if (!erts_active_export_entry(mod, name, arity)) goto error;
//! }
//! ```
//!
//! `erts_active_export_entry` (`export.h:176-183`) is a lookup in the **running node's active
//! export table**, i.e. "is `M:F/A` exported by code that is loaded *now*?" — runtime state the
//! codec cannot see. The codec therefore owns the *check site* (its `decode/fun.rs::decode_export`,
//! under `[safe]` and only for an `EXPORT_EXT`) and this crate owns the *registry*:
//! [`ExportGateBridge`] is the one implementation of that seam, installed by
//! [`RuntimeContext::call_registered`](super::RuntimeContext) on exactly the key that reads it
//! (`erlang:binary_to_term/2`, `beam_bifs::EXPORT_GATE_BIFS`).
//!
//! # The predicate: registry-inclusive, and why
//!
//! The answer is
//!
//! ```text
//! is_builtin(M, F, A)                      (the emulator's own `bif.tab` surface)
//! || code_server.module(M) exports (F, A)  (a module loaded right now)
//! || registry.contains(M, F, A)            (this runtime's `beam_bifs` implementations)
//! ```
//!
//! The first two disjuncts are [`is_active_export`] — literally the predicate
//! `erlang:function_exported/3` answers with, shared rather than re-derived (see
//! [`RuntimeHooksBridge::function_exported`](super::RuntimeHooksBridge)). The third is
//! this gate's own, and it is what makes the answer usable at all on **this** runtime:
//! the code server holds a much smaller module set than a booted OTP node (it loads the 22
//! preloaded `.beam`s plus whatever a run resolves on demand — `lists` is not among them),
//! so the oracle's `binary_to_term(term_to_binary(fun lists:map/2), [safe])` — accepted,
//! because `lists` is loaded on a real node — would otherwise be refused with `badarg` for
//! a module whose code this runtime implements as a `beam_bifs` fast path and can apply.
//! The widening is safe in the only direction that matters: a key that no host source serves
//! (`lists:no_such_fun_xyz/1`, `erlang:no_such_bif_xyz/1`, a module this runtime neither
//! loads nor implements) is still refused, so the gate never turns a genuinely dead export
//! into an accepted fun.
//!
//! `is_builtin` deliberately keeps its own narrow meaning (see its docs): the registry's stdlib
//! *fast paths* are not emulator builtins, so `erlang:function_exported(lists, map, 2)` must
//! stay `false` — that is why this file adds the registry disjunct here instead of widening
//! the shared helper.
//!
//! # Cost and shape
//!
//! The bridge is two shared references and three hash lookups per asked export: no
//! allocation, no mutation, no interior mutability. It is installed only for
//! `erlang:binary_to_term/2` (an un-gated runtime decodes exactly as before — the codec asks
//! the gate **only** under `[safe]` **and** only for an `EXPORT_EXT`), and it borrows the code
//! server and the registry **immutably**, which is why
//! [`call_registered`](super::RuntimeContext::call_registered) gives it its own mutually
//! exclusive dispatch arm next to the record-source one.

use beam_bifs::BifRegistry;
use beam_codec::ExportGate;

use crate::codeserver::CodeServer;

use super::runtime_hooks::is_active_export;

/// The [`beam_codec::ExportGate`] implementation over **this runtime's** loaded modules and
/// registered BIFs.
///
/// Holds only the two immutable views the predicate needs — the code server's module cache and
/// the shared [`BifRegistry`] — so it can live across one `BifContext` call without conflicting
/// with anything else the arm borrows (see the module docs).
pub(super) struct ExportGateBridge<'a> {
    /// The code server: its module cache is "code loaded right now", the module half of the
    /// active-export lookup.
    pub(super) code_server: &'a CodeServer,
    /// The shared BIF registry: every `M:F/A` this runtime can apply without loaded code
    /// (`lists:map/2` and the rest of the stdlib fast paths, plus the emulator's own names).
    pub(super) registry: &'a BifRegistry,
}

impl<'a> ExportGateBridge<'a> {
    /// A gate over `code_server`'s loaded modules and `registry`'s registered BIFs.
    pub(super) fn new(code_server: &'a CodeServer, registry: &'a BifRegistry) -> Self {
        ExportGateBridge {
            code_server,
            registry,
        }
    }
}

impl ExportGate for ExportGateBridge<'_> {
    /// Whether `module:function/arity` is exported by code this runtime can run **now** — a
    /// loaded module's export, an emulator builtin, or a `beam_bifs`-registered implementation.
    ///
    /// `arity` is the low 32 bits of the wire arity (the codec truncates it exactly as the C's
    /// `unsigned int` parameter does, `external.c:5028`); an unanswerable lookup is `false`,
    /// which the codec turns into `badarg` — never a wrong `true`.
    fn is_export_active(&self, module: &str, function: &str, arity: u32) -> bool {
        is_active_export(self.code_server, self.registry, module, function, arity)
            || self.registry.contains(module, function, arity)
    }
}

#[cfg(test)]
mod tests {
    use beam_codec::ExportGate;

    use crate::runtime::Runtime;

    use super::ExportGateBridge;

    /// This crate's failed-head-clause fixture (`tests/fixtures/function_clause_fixture.beam`),
    /// whose module `function_clause_fixture` exports `f/1` and `module_info/0,1` — a module
    /// loaded **by the test** rather than one of the 22 preloaded ones, so the loaded-module
    /// disjunct is exercised for real.
    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/function_clause_fixture.beam"
    );

    /// The gate over a default runtime.
    fn gate(rt: &Runtime) -> ExportGateBridge<'_> {
        ExportGateBridge::new(&rt.code_server, &rt.registry)
    }

    #[test]
    fn a_registered_stdlib_fast_path_is_active_even_though_its_module_is_not_loaded() {
        // `lists` is NOT one of the 22 preloaded modules; `lists:map/2` is a `beam_bifs`
        // implementation. The oracle accepts this export (its own `lists` is loaded), so the
        // registry disjunct is what keeps `binary_to_term(term_to_binary(fun lists:map/2),
        // [safe])` accepting here too.
        let rt = Runtime::with_defaults().expect("the default runtime");
        assert!(
            !rt.code_server.is_loaded("lists"),
            "precondition: `lists` is not loaded in a bare runtime"
        );
        assert!(gate(&rt).is_export_active("lists", "map", 2));
    }

    #[test]
    fn an_emulator_builtin_is_active() {
        // `erlang:system_info/1` is declared in `bif.tab` (and registered in the `erlang`
        // namespace), so it is active through the builtin half of the shared predicate.
        let rt = Runtime::with_defaults().expect("the default runtime");
        assert!(gate(&rt).is_export_active("erlang", "system_info", 1));
        assert!(gate(&rt).is_export_active("erlang", "binary_to_term", 2));
    }

    #[test]
    fn a_loaded_modules_real_export_is_active() {
        let mut rt = Runtime::with_defaults().expect("the default runtime");
        rt.load_module(FIXTURE).expect("the committed fixture loads");
        let gate = gate(&rt);
        assert!(gate.is_export_active("function_clause_fixture", "f", 1));
        assert!(gate.is_export_active("function_clause_fixture", "module_info", 0));
        assert!(gate.is_export_active("function_clause_fixture", "module_info", 1));
        // The loaded module does not export `f/2`, and the registry holds no such key.
        assert!(!gate.is_export_active("function_clause_fixture", "f", 2));
    }

    #[test]
    fn a_wrong_arity_of_a_real_export_is_refused() {
        let rt = Runtime::with_defaults().expect("the default runtime");
        assert!(gate(&rt).is_export_active("lists", "map", 2));
        assert!(!gate(&rt).is_export_active("lists", "map", 3));
        assert!(!gate(&rt).is_export_active("lists", "map", 0));
        assert!(!gate(&rt).is_export_active("erlang", "system_info", 2));
    }

    #[test]
    fn a_nonexistent_function_or_module_is_refused() {
        let rt = Runtime::with_defaults().expect("the default runtime");
        let gate = gate(&rt);
        assert!(!gate.is_export_active("lists", "no_such_fun_xyz", 1));
        assert!(!gate.is_export_active("erlang", "no_such_bif_xyz", 1));
        // A module this runtime neither loads nor implements (the oracle refuses this too:
        // `mnesia` is not loaded in a bare `-noshell` node).
        assert!(!gate.is_export_active("mnesia", "start", 0));
        // An empty name is never an export.
        assert!(!gate.is_export_active("", "", 0));
    }

    #[test]
    fn the_wire_arity_is_the_32_bit_truncation_the_codec_hands_over() {
        // The codec passes the low 32 bits of the wire arity (`external.c:5028`), so a
        // `u32::MAX` arity can never be an active `M:F/A` and a `2^32 + 7` wire arity is asked
        // as `7`. Only the second half is observable here (the truncation is the codec's, and
        // is covered by its own suite).
        let rt = Runtime::with_defaults().expect("the default runtime");
        assert!(!gate(&rt).is_export_active("lists", "map", u32::MAX));
        assert!(gate(&rt).is_export_active("lists", "reverse", 1));
    }
}
