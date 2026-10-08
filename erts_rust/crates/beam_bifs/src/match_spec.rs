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

//! `erlang:match_spec_test/3` — compile a match specification and run it against
//! one term (or trace against a list), reporting the compiled form's observable
//! results.
//!
//! Ground truth (read-only): `../../../erts/emulator/beam/bif.tab:346`
//! (`bif erlang:match_spec_test/3`), the BIF itself
//! `../../../erts/emulator/beam/erl_db_util.c:6042-6063`, the worker
//! `match_spec_test` (`erl_db_util.c:6065-6158`), the linter `db_match_set_lint`
//! (`erl_db_util.c:1509-1619`), the error renderer `db_format_dmc_err_info`
//! (`erl_db_util.c:3154-3186`) and the ETS-mode runner `erts_match_set_run_ets`
//! (`erl_db_util.c:1654-1679`). Every shape below was probed on the system
//! `erl` (OTP 29.1.1 / erts-17.1); the `tests/match_spec.rs` goldens carry their
//! probes.
//!
//! # Argument order and the result shape (verified)
//!
//! The BIF is `match_spec_test(MatchAgainst, MatchSpec, Type)` — the **term** is
//! argument 1 and the **specification** argument 2 (the C reads
//! `BIF_ARG_1`/`BIF_ARG_2` straight into `against`/`spec`;
//! `erl_db_util.c:6052,6057`). It returns a **4-tuple**
//!
//! * `{ok, Result, Flags, Errors}` on a successful compile —
//!   `TUPLE4(hp, am_ok, res, flg, lint_res)` (`erl_db_util.c:6155`), where
//!   `Result` is the body value of the first clause whose head matches and whose
//!   guards hold, or the atom `false` when nothing matched
//!   (`if (is_non_value(res)) res = am_false;`, `erl_db_util.c:6138-6140`),
//!   `Flags` is a list built from `MATCH_SET_EXCEPTION_TRACE`/
//!   `MATCH_SET_RETURN_TRACE` (empty for the `table` dialect, which rejects the
//!   trace special forms outright), and `Errors` is the lint result;
//! * `{error, Errors}` on a failed compile — `TUPLE2(hp, am_error, lint_res)`
//!   (`erl_db_util.c:6093-6094`), **not** a 3-tuple.
//!
//! `Errors` is a list of `{Severity, "String"}` 2-tuples where the severity is
//! the atom `error` (only; nothing in the C ever emits the `warning` severity —
//! the `WARNING`/`VAR_WARNING`/`TERM_WARNING` macros at `erl_db_util.c:4257-4264`
//! have no call sites) and the "string" is an Erlang **int list**
//! (`buf_to_intlist`, `erl_db_util.c:3179`), not a binary — verified:
//! `is_list/1` is `true` and `is_binary/1` is `false` for it.
//!
//! # Dialects
//!
//! `Type` selects the compile flags: the atom `table` (argument 3) is
//! `DCOMP_TABLE | DCOMP_FAKE_DESTRUCTIVE` and the atom `trace` is additionally
//! `DCOMP_TRACE | DCOMP_CALL_TRACE | DCOMP_ALLOW_TRACE_OPS`
//! (`erl_db_util.c:6081-6090`). Any other `Type` — including a non-atom — is
//! `badarg` (`erl_db_util.c:6062`).
//!
//! * **`table`** is implemented: the compile and the run are delegated wholesale
//!   to `beam_ets` (the same compiler/runner the `ets:select` bridge in
//!   `src/ets.rs` drives — this module never re-implements the matcher), the
//!   term is run as a **one-element list** through
//!   [`beam_ets::match_spec_run`], and the body value of an empty run is the
//!   atom `false` (the C's non-match sentinel).
//! * **`trace`** is a typed [`unsupported_bif`] capability gap. The C's trace
//!   dialect needs the `trace`/`DCOMP_ALLOW_TRACE_OPS` programme (`{message, _}`,
//!   `{return_trace}`, `{exception_trace}`, array/prefix matching and a
//!   `MATCH_SET_*_TRACE` flag set); `beam_ets` has no trace runner, and the
//!   `beam_bifs` `TraceHook`/`Port`-style seams do not exist for it. Matching
//!   its `{ok, TraceReturn, Flags, Errors}` shape by guessing would be a
//!   fabricated result, so the dialect is refused instead.
//!
//! # Lint reproduction (the `Errors` element)
//!
//! The C lints the spec (`db_match_set_lint`) *before* compiling it, and its
//! `err_info` accumulates every diagnostic. Only the three **structural**
//! diagnostics are reproduced here, because they are the only ones that are
//! decided purely by the spec's shape, that short-circuit (`goto done`, so they
//! are always the sole error) and that `beam_ets::compile` cannot tell apart from
//! its own broader `badarg`:
//!
//! | spec shape | message (`erl_db_util.c`) |
//! |---|---|
//! | not a cons (`[]`, an atom, an int, …) | `Match programs are not in a list.` (`:1529-1532`) |
//! | improper list | `Match programs are not in a proper list.` (`:1538-1542`) |
//! | a clause that is not a 3-tuple | `Match program part is not a tuple of arity 3.` (`:1560-1566`) |
//!
//! `is_list` in the C is "is a cons cell", so `[]` is *not* a list there —
//! verified: `match_spec_test(7, [], table)` reports the first message.
//!
//! Every **other** compile failure — an unbound `'$N'` (`Variable $N is
//! unbound.`), a guard that is not a list (`Match expression is not a list.`), an
//! empty body (`Body clause does not return anything.`), an unknown function in a
//! guard (`Function F/A does_not_exist.`), a wrong-dialect special form
//! (`Special form 'X' used in wrong dialect.`), a non-call tuple in a guard
//! (`{…} is neither a function call, nor a tuple …`, which embeds the offending
//! term) and the many `dmc_*` diagnostics besides — is a typed
//! [`unsupported_bif`]. The C's diagnostic set is large, its messages may embed
//! the offending term (`%T`) or a renumbered variable (`ei->var_trans`), and
//! `RETURN_ERROR_X` keeps compiling in lint mode
//! (`erl_db_util.c:4244-4248`), so a single spec can accumulate several
//! diagnostics. Rebuilding that faithfully is a port in its own right; guessing
//! one message where the oracle prints another would be a fabricated result.
//! `beam_ets::compile` rejecting the spec is therefore the honest outside.
//!
//! # Head patterns containing a map are refused (verified divergence)
//!
//! `beam_ets::pattern::decode` treats **every** head map as an exact
//! (`=:=`) literal and documents that a head map "containing wildcards/variables
//! therefore under-matches" (`../beam_ets/src/select/pattern.rs:66-69`). That is
//! not the C's semantics, and it is wrong in **both** directions of interest:
//!
//! * a map pattern matches an object with *more* keys —
//!   `match_spec_test(#{a => 1, b => 2}, [{#{a => 1}, [], [ok]}], table)` is
//!   `{ok, ok, [], []}` on the oracle, while a literal `=:=` comparison of
//!   `#{a => 1}` and `#{a => 1, b => 2}` is false;
//! * a map **value** is itself a sub-pattern that binds —
//!   `match_spec_test(#{a => 1}, [{#{a => '$2'}, [], ['$2']}], table)` is
//!   `{ok, 1, [], []}`, while the literal comparison against the atom `'$2'`
//!   fails;
//! * a map **key** must not be a variable or `'_'` — the C reports
//!   `Variable found in map key.` / `Underscore found in map key.`
//!   (`erl_db_util.c:1788-1800`) where this runtime would silently compare.
//!
//! So any head pattern that mentions a map (at any depth, through tuples and
//! conses) is a typed [`unsupported_bif`]. A map in a **guard or body** needs no
//! special case: `beam_ets`'s expression decoder rejects map expressions outright
//! (`../beam_ets/src/select/expr.rs:316-320`), so it already lands in the typed
//! gap above.

use beam_common::{BeamError, Result};
use beam_term::{AtomTable, Eterm, NIL};

use crate::access::{
    car, cdr, is_atom, is_flatmap, is_hashmap, is_list, is_nil, is_tuple, tuple_element, tuple_size,
};
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::registry::BifRegistry;

/// Registers `erlang:match_spec_test/3` (`bif.tab:346`).
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "match_spec_test", 3, match_spec_test_3);
}

/// The three structural lint messages `db_match_set_lint` reports before it ever
/// compiles a clause (`erl_db_util.c:1529-1566`), verbatim.
const NOT_A_LIST: &str = "Match programs are not in a list.";
const NOT_A_PROPER_LIST: &str = "Match programs are not in a proper list.";
const NOT_A_3_TUPLE: &str = "Match program part is not a tuple of arity 3.";

/// `erlang:match_spec_test(MatchAgainst, MatchSpec, Type)`.
///
/// See the module docs for the result shapes, the dialect split and the
/// documented capability gaps.
fn match_spec_test_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (against, spec, dialect) = (args[0], args[1], args[2]);
    match atom_name(ctx.atoms(), dialect).as_deref() {
        Some("table") => test_table(ctx, against, spec),
        // The trace dialect needs a trace programme and a trace runner this
        // runtime does not have; refused rather than faked (module docs).
        Some("trace") => Err(unsupported_bif()),
        // Every other `Type`, atom or not (`erl_db_util.c:6062`).
        _ => Err(badarg()),
    }
}

/// The `table` dialect: lint, refuse the unsupported head shapes, then compile
/// and run through `beam_ets`.
fn test_table(ctx: &mut BifContext<'_>, against: Eterm, spec: Eterm) -> Result<Eterm> {
    if let Some(message) = structural_lint(spec) {
        return lint_error(ctx, message);
    }
    // `structural_lint` established a proper list of 3-tuples, so the head of
    // every clause is `tuple_element(clause, 0)`.
    if head_mentions_a_map(spec) {
        return Err(unsupported_bif());
    }
    // `beam_ets::match_spec_run` compiles the raw spec internally and matches the
    // head against the **element itself** (any term — `../beam_ets/src/select/
    // run.rs`), which is exactly the C's `erts_match_set_run_ets(p, mps, against,
    // 0, &ret_flags)` (`erl_db_util.c:6133`), so the single term is run as a
    // one-element list.
    let atoms = ctx.atoms().clone();
    let singleton = ctx.list(&[against])?;
    let mut results = match beam_ets::match_spec_run(singleton, spec, &atoms, ctx.heap()) {
        Ok(results) => results,
        // `beam_ets::compile` rejected the spec. That is either a construct
        // outside its subset that the oracle accepts, or one of the C's semantic
        // lint diagnostics this module does not reproduce — a typed capability
        // gap either way, never a guessed `{error, …}` or a fabricated `{ok, …}`
        // (module docs).
        Err(BeamError::BadArg) => return Err(unsupported_bif()),
        Err(other) => return Err(other),
    };
    let value = match results.pop() {
        Some(value) => value,
        // No clause matched: the C's `is_non_value(res)` sentinel.
        None => ctx.boolean(false)?,
    };
    let ok = ctx.ok()?;
    // `Flags` is empty for `table` (nothing can raise `MATCH_SET_*_TRACE` in a
    // dialect that rejects the trace special forms), `Errors` is empty because a
    // spec that compiles lints clean (the C emits no warnings at all).
    ctx.tuple(&[ok, value, NIL, NIL])
}

/// The C lint's three structural diagnostics, in the C's order
/// (`erl_db_util.c:1529-1566`). `None` when the spec passes all three — the
/// semantic diagnostics are deliberately *not* reproduced here (module docs).
fn structural_lint(spec: Eterm) -> Option<&'static str> {
    // `!is_list(matchexpr)` — a cons-cell test, so `[]` fails it too.
    if !is_list(spec) {
        return Some(NOT_A_LIST);
    }
    let mut clauses = Vec::new();
    let mut cursor = spec;
    while is_list(cursor) {
        clauses.push(car(cursor));
        cursor = cdr(cursor);
    }
    if !is_nil(cursor) {
        return Some(NOT_A_PROPER_LIST);
    }
    if clauses
        .iter()
        .any(|clause| !is_tuple(*clause) || tuple_size(*clause) != 3)
    {
        return Some(NOT_A_3_TUPLE);
    }
    None
}

/// Does any clause head mention a map (at any depth)? See the module docs: a head
/// map is a documented `beam_ets` under-match and must not be guessed at.
fn head_mentions_a_map(spec: Eterm) -> bool {
    let mut cursor = spec;
    while is_list(cursor) {
        let clause = car(cursor);
        // `structural_lint` ran first, so every clause here is a 3-tuple.
        if term_mentions_a_map(tuple_element(clause, 0)) {
            return true;
        }
        cursor = cdr(cursor);
    }
    false
}

/// A structural walk over a head pattern: `true` when a map appears anywhere
/// (a tuple element, a cons head, or an improper cons tail).
fn term_mentions_a_map(term: Eterm) -> bool {
    if is_flatmap(term) || is_hashmap(term) {
        return true;
    }
    if is_tuple(term) {
        return (0..tuple_size(term)).any(|index| term_mentions_a_map(tuple_element(term, index)));
    }
    if is_list(term) {
        let mut cursor = term;
        while is_list(cursor) {
            if term_mentions_a_map(car(cursor)) {
                return true;
            }
            cursor = cdr(cursor);
        }
        // The tail of an improper list is itself a pattern.
        return term_mentions_a_map(cursor);
    }
    false
}

/// `{error, [{error, "Message"}]}` — the C's failed-compile result
/// (`TUPLE2(hp, am_error, lint_res)`, `erl_db_util.c:6093-6094`) over
/// `db_format_dmc_err_info`'s `{error, IntList}` entry (`erl_db_util.c:3178-3182`).
///
/// The message is an **int list** of latin1 bytes, not a binary (verified).
/// The `error` atom (used both as the outer tuple tag and as the per-entry
/// severity) is interned through the context's `AtomInterner` seam because it
/// need not be pre-interned in a minimal embedding; an unknown atom without the
/// hook is a typed `TermNotFound`, never a wrong atom.
fn lint_error(ctx: &mut BifContext<'_>, message: &str) -> Result<Eterm> {
    let severity = ctx.intern_atom("error")?;
    let mut chars = Vec::with_capacity(message.len());
    for byte in message.bytes() {
        chars.push(ctx.int(i64::from(byte))?);
    }
    let text = ctx.list(&chars)?;
    let entry = ctx.tuple(&[severity, text])?;
    let errors = ctx.list(&[entry])?;
    let tag = ctx.intern_atom("error")?;
    ctx.tuple(&[tag, errors])
}

/// The name of `term` when it is an atom the table knows; `None` otherwise (a
/// non-atom, or an atom outside the table — both land on the C's `badarg`).
fn atom_name(atoms: &AtomTable, term: Eterm) -> Option<String> {
    if !is_atom(term) {
        return None;
    }
    atoms.name(beam_term::atom_val(term)).map(str::to_owned)
}
