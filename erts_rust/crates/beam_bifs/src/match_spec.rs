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
//!   The dialect's **argument check is reproduced exactly**, because the C makes
//!   it before it lints or compiles: `if (trace && !(is_list(against) ||
//!   against == NIL)) return THE_NON_VALUE;` (`erl_db_util.c:6078-6080`), which
//!   the BIF turns into `error:badarg` (`:6062`). So a **non-list, non-`[]`**
//!   `Against` — a scalar, an atom, a map of either shape — is `badarg` here too,
//!   whether or not the spec would compile; only a list (or `[]`) `Against`
//!   reaches the typed gap. `is_list` in the C is a cons-cell test
//!   (`[]` ⇒ `NIL`), and [`is_list`](crate::access::is_list) is the same test.
//!   Verified: `erlang:match_spec_test(1, [{'_',[],[ok]}], trace)` and the same
//!   with `#{a=>1}`, `hello`, `[]`-spec variants all raise `error:badarg`, while
//!   `erlang:match_spec_test([1,2], [{'_',[],[ok]}], trace)` is `{ok,true,[],[]}`.
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
//! # Head patterns containing a map are delegated to `beam_ets` (verified)
//!
//! A head map is compiled by the shared `beam_ets` match-spec compiler
//! (`beam_ets::select::pattern::decode`), which the `table` dialect already
//! delegates to wholesale: a head map's **keys stay literal** while its
//! **values recurse structurally**, exactly like tuple elements (at every nesting
//! depth, through tuples and conses). That is the C's semantics, so these agree
//! with the oracle rather than being guessed at:
//!
//! * a map pattern matches an object with *more* keys —
//!   `match_spec_test(#{a => 1, b => 2}, [{#{a => 1}, [], [ok]}], table)` is
//!   `{ok, ok, [], []}` (verified);
//! * a map **value** is a sub-pattern that binds —
//!   `match_spec_test(#{a => 1}, [{#{a => '$2'}, [], ['$2']}], table)` is
//!   `{ok, 1, [], []}` (verified);
//! * a map nested inside a tuple head recurses —
//!   `match_spec_test({x, #{a => 1, b => 2}}, [{{x, #{a => 1}}, [], [ok]}], table)`
//!   is `{ok, ok, [], []}` (verified).
//!
//! The one remaining map-related gap is a **variable or `'_'` map key**
//! (`#{'$1' => '$2'}`, `#{'_' => 1}`): the C's linter reports
//! `Variable found in map key.` / `Underscore found in map key.`
//! (`erl_db_util.c:1788-1800`) as `{error, [{error, "…"}]}`, whereas
//! `beam_ets::select::pattern::decode` refuses the spec with a plain `badarg`.
//! That `badarg` reaches [`test_table`], which answers the typed
//! [`unsupported_bif`] — reproducing the C's semantic lint diagnostic is a port
//! of its own (see the lint section above), so it is not guessed here.
//!
//! A map in a **guard or body** needs no special case: `beam_ets` decodes a map
//! expression (`#{K => V}`, the C's `dmc_map`) and **constructs** it at
//! evaluation time — its keys/values may be variables — exactly as the C does
//! (`../beam_ets/src/select/expr.rs:358-363`), so the wholesale delegation below
//! already answers it. Verified: `erlang:match_spec_test(7, [{'$1',[],[#{a=>'$1'}]}], table)`
//! and the crate both yield `{ok, #{a => 7}, [], []}`, and
//! `erlang:match_spec_test(#{a=>1}, [{'$1',[{'is_map','$1'}],[ok]}], table)` is
//! `{ok, ok, [], []}` in both.
//!
//! An `Against` term that *is* a map (flat or HAMT) is likewise not a head
//! pattern and needs no special case — it is the object the head is matched
//! against.

use beam_common::{BeamError, Result};
use beam_term::{AtomTable, Eterm, NIL};

use crate::access::{car, cdr, is_atom, is_list, is_nil, is_tuple, tuple_size};
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
        // The C rejects a non-list, non-`[]` `Against` in the trace dialect
        // **before** it lints or compiles anything
        // (`if (trace && !(is_list(against) || against == NIL))`,
        // `erl_db_util.c:6078-6080` — `is_list` is a cons-cell test there too), so
        // a scalar/map/atom `Against` is `error:badarg` even for a spec that would
        // not compile. Reproduced here.
        Some("trace") if !is_list(against) && !is_nil(against) => Err(badarg()),
        // For a list `Against` the dialect needs a trace programme and a trace
        // runner this runtime does not have; refused rather than faked (module
        // docs).
        Some("trace") => Err(unsupported_bif()),
        // Every other `Type`, atom or not (`erl_db_util.c:6062`).
        _ => Err(badarg()),
    }
}

/// The `table` dialect: lint, then compile and run through `beam_ets`.
fn test_table(ctx: &mut BifContext<'_>, against: Eterm, spec: Eterm) -> Result<Eterm> {
    // The spec's guards/bodies may compute arithmetic and may read `self/0`; install
    // this crate's two match-spec seams over `beam_ets` for the run (the same scoped
    // aggregate every `ets:*` select-family BIF opens — `crate::ets::self_hook`).
    let _eval = crate::ets::self_hook::SpecEval::install(ctx.self_pid());
    if let Some(message) = structural_lint(spec) {
        return lint_error(ctx, message);
    }
    // `beam_ets::match_spec_run` compiles the raw spec internally and matches the
    // head against the **element itself** (any term — `../beam_ets/src/select/
    // run.rs`), which is exactly the C's `erts_match_set_run_ets(p, mps, against,
    // 0, &ret_flags)` (`erl_db_util.c:6133`), so the single term is run as a
    // one-element list.
    let singleton = ctx.list(&[against])?;
    let mut results = {
        // `beam_ets::match_spec_run` needs the atom table and the caller heap at
        // once; the two disjoint fields are split in a single borrow
        // (`ctx.atom_and_heap()`) rather than cloned (the `select_2` pattern).
        let (atoms, heap) = ctx.atom_and_heap();
        match beam_ets::match_spec_run(singleton, spec, atoms, heap) {
            Ok(results) => results,
            // `beam_ets::compile` rejected the spec. That is either a construct
            // outside its subset that the oracle accepts, or one of the C's semantic
            // lint diagnostics this module does not reproduce — a typed capability
            // gap either way, never a guessed `{error, …}` or a fabricated `{ok, …}`
            // (module docs).
            Err(BeamError::BadArg) => return Err(unsupported_bif()),
            Err(other) => return Err(other),
        }
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
