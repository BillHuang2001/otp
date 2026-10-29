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

//! The `erlang` raise helpers (`error/1`, `error/2`, `exit/1`, `throw/1`,
//! `nif_error/1,2`) and the [`ExitReason`] ↔ reason-term mapping.
//!
//! Ground truth (read-only): `bif erlang:error/1`, `error/2`, `exit/1`,
//! `throw/1` and `nif_error/1,2` in `../../erts/emulator/beam/bif.tab`
//! (lines 65, 149, 284–285, 568) and the well-known reason atoms in
//! `beam_common::ExitReason`. The `nif_error/2` implementation is `nif_error_2`
//! in `../../erts/emulator/beam/bif.c:1525` (the second argument is metadata for
//! the crash report, **not** the reason).
//!
//! # Return-based raising
//!
//! A raise is a *value*, not a `longjmp` (workspace `CONTEXT.md` §3): the BIFs
//! return `Err(BeamError::Raise(..))` and the engine installs the exception tag in
//! `x(0)`. `error/1`, `exit/1` and `throw/1` set the [`ExceptionClass`] to
//! `Error`, `Exit` and `Throw` respectively.
//!
//! # The documented `Structured` gap
//!
//! [`ExitReason`] models the well-known atoms as dedicated variants and any other
//! atom as `Atom(name)`, but a **non-atom** reason is
//! [`ExitReason::Structured`], which itself carries no term — `beam_common` has no
//! term dependency. [`raise`] therefore attaches the argument term to the returned
//! [`Raise`] through the opaque [`Raise::live_reason`](beam_common::Raise::live_reason)
//! slot (its workspace convention: the payload **is** the live reason term). An
//! **atom** reason keeps its exact `ExitReason` shape and carries **no** live slot.
//!
//! [`reason_term_from_exit_reason`] only ever sees an [`ExitReason`]; it still rejects
//! `Structured` with `badarg`, because a caller that took the reason apart to an
//! `ExitReason` has already dropped the term — the term now travels on the `Raise`
//! returned by the raise BIFs. This is the handoff `../beam_process/CONTEXT.md` documents.
//!
//! Both [`exit_reason_from_term`] and [`reason_term_from_exit_reason`] are part of
//! the crate's public API (re-exported from the crate root) — the documented
//! `beam_process`/`beam_rt` handoff, not crate-internal helpers.
//!
//! The **other** half of a BIF frame's `error_info` payload — the `cause` the
//! oracle packs beside `module` — is modelled next door, by
//! `crate::error_cause::error_cause`; this module owns only the
//! `erlang:error/2,3` argument/`error_info` payload.

use beam_common::{BeamError, ExceptionClass, ExitReason, LiveReason, Result};
use beam_term::{atom_val, is_atom, make_atom, AtomTable, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error::{self, raise_error, raise_exit, raise_throw};
use crate::registry::BifRegistry;

/// Maps a live reason **term** to an [`ExitReason`].
///
/// An atom maps to its dedicated [`ExitReason`] variant when well-known
/// (`normal`, `shutdown`, `noproc`, `noconnection`, `killed`, `aborted`) and to
/// `Atom(name)` otherwise; a non-atom maps to [`ExitReason::Structured`].
///
/// An atom whose name is absent from `atoms` (a mismatched table) also maps to
/// `Structured`: `ExitReason::Atom` requires a name and there is none to attach
/// (see the module docs).
pub fn exit_reason_from_term(term: Eterm, atoms: &AtomTable) -> ExitReason {
    if !beam_term::is_atom(term) {
        return ExitReason::Structured;
    }
    match atoms.name(atom_val(term)) {
        Some("normal") => ExitReason::Normal,
        Some("shutdown") => ExitReason::Shutdown,
        Some("noproc") => ExitReason::NoProc,
        Some("noconnection") => ExitReason::NoConnection,
        Some("killed") => ExitReason::Killed,
        Some("aborted") => ExitReason::Aborted,
        Some(name) => ExitReason::Atom(name.to_string().into()),
        None => ExitReason::Structured,
    }
}

/// Maps an [`ExitReason`] back to its interned reason **term**.
///
/// # Errors
///
/// [`BeamError::BadArg`] for [`ExitReason::Structured`] (and any future
/// non-exhaustive variant): an `ExitReason` alone carries no term to reproduce, so a
/// structured reason taken apart to an `ExitReason` is unrecoverable here — the term
/// rides the [`Raise::live_reason`](beam_common::Raise::live_reason) slot of the raise
/// the BIF returned (see [`raise`]).
pub fn reason_term_from_exit_reason(
    reason: &ExitReason,
    atoms: &mut AtomTable,
) -> Result<Eterm> {
    match reason {
        ExitReason::Normal => Ok(make_atom(atoms.intern("normal"))),
        ExitReason::Shutdown => Ok(make_atom(atoms.intern("shutdown"))),
        ExitReason::NoProc => Ok(make_atom(atoms.intern("noproc"))),
        ExitReason::NoConnection => Ok(make_atom(atoms.intern("noconnection"))),
        ExitReason::Killed => Ok(make_atom(atoms.intern("killed"))),
        ExitReason::Aborted => Ok(make_atom(atoms.intern("aborted"))),
        ExitReason::Atom(name) => Ok(make_atom(atoms.intern(name.as_str()))),
        _ => Err(BeamError::BadArg),
    }
}

/// A raise of `class` with `term` as the reason term.
///
/// An **atom** reason (well-known or plain) maps to its exact [`ExitReason`] and
/// carries no live slot. A **non-atom** reason is [`ExitReason::Structured`] and the
/// argument term itself rides the [`Raise::live_reason`](beam_common::Raise::live_reason)
/// slot, so the caller sees the real reason term (`{bad_generator,X}`, `[1,2]`, …).
///
/// Pinned by the oracle:
/// `erl -noshell -eval 'try erlang:error({bad_generator,5}) catch C:R -> io:format("~p:~p~n",[C,R]) end, halt().'`
/// ⇒ `error:{bad_generator,5}`; `erlang:throw(42)` ⇒ `throw:42`;
/// `erlang:exit({shutdown,1})` ⇒ `exit:{shutdown,1}`.
fn raise(class: ExceptionClass, term: Eterm, atoms: &AtomTable) -> BeamError {
    let reason = exit_reason_from_term(term, atoms);
    if reason == ExitReason::Structured {
        return error::raise_term(class, term);
    }
    match class {
        ExceptionClass::Exit => raise_exit(reason),
        ExceptionClass::Throw => raise_throw(reason),
        // `ExceptionClass` is `#[non_exhaustive]`; a known-atom reason under any
        // other class is the `error` class.
        _ => raise_error(reason),
    }
}

/// Attaches the `error/2,3` stacktrace payloads to a raise built by [`raise`].
///
/// `saved_args` is the raise's second argument, stored **verbatim** (the C's
/// `TUPLE2(BIF_ARG_1, BIF_ARG_2)`, `bif.c:1450`); `error_info` is the map the
/// `error/3` scan accepted, when it accepted one. Both ride the opaque
/// [`Raise::saved_args`](beam_common::Raise::saved_args) /
/// [`Raise::error_info`](beam_common::Raise::error_info) slots with the workspace
/// payload convention: the **live `Eterm` word** of the argument.
///
/// # Live-term contract
///
/// The two payloads are raw words on the **caller's** heap, not copies, so nothing
/// between reading them out of `args` and returning this raise may allocate on a
/// BEAM heap, intern an atom or call back into the host — the engine publishes them
/// into its root set the moment it receives the raise. (`LiveReason::new` boxes the
/// word in an `Arc`, a Rust-side allocation that moves no BEAM term; the
/// [`BifContext::atoms`] reads in the scan are non-interning lookups.)
///
/// [`raise`] only ever builds a `Raise`; any other [`BeamError`] passes through
/// unchanged rather than being swallowed.
fn with_error_payloads(
    raised: BeamError,
    saved_args: Eterm,
    error_info: Option<Eterm>,
) -> BeamError {
    match raised {
        BeamError::Raise(raise) => {
            let raise = raise.with_saved_args(LiveReason::new(saved_args));
            let raise = match error_info {
                Some(map) => raise.with_error_info(LiveReason::new(map)),
                None => raise,
            };
            BeamError::Raise(raise)
        }
        other => other,
    }
}

/// The C's `error_3` option scan (`../../erts/emulator/beam/bif.c:1462-1501`):
/// the **last** `{error_info, Map}` of a wholly-valid option list, else `None`.
///
/// The scan mirrors the C literally:
///
/// - it walks a cons-list, so `[]` (the C's `is_list(NIL)` is **false**, `NIL` being
///   an immediate) and any non-list stop it immediately with nothing accepted;
/// - an element that is not a 2-tuple `{error_info, Map}` **breaks** the walk, and a
///   trailing non-`NIL` tail — an improper list, or a legal pair followed by any
///   illegal element — **invalidates** whatever was accepted;
/// - every accepted pair overwrites the previous one, so the **last wins**.
///
/// Every deviation therefore yields `None` — the silent `error/2` fallback, never
/// `badarg`. A table with no `error_info` atom can match no term, which is also
/// `None` (correct, not a fallback).
fn scan_error_info(ctx: &BifContext<'_>, options: Eterm) -> Option<Eterm> {
    scan_error_info_at(options, ctx.atoms())
}

/// The stacktrace-side payload of `erlang:error/2,3` — the caller's argument list and,
/// for `error/3`, the `error_info` map the raise carries.
///
/// Built by [`error_meta`] from the same option rule [`scan_error_info`] applies. ERTS
/// keeps the pair in the exception object (`{Reason, Args}` / `{Reason, Args, ErrorInfo}`,
/// `error_2`/`error_3` in `../../erts/emulator/beam/bif.c:1448-1503`) and renders it into
/// the **innermost** stacktrace item: the argument list in the item's `A` slot, the map as
/// the last element of its `Location` list (`erts_build_mfa_item`,
/// `../../erts/emulator/beam/utils.c:3430-3457`).
///
/// This is the host-facing **model** of that payload; the raise itself carries the two
/// words on [`Raise::saved_args`](beam_common::Raise::saved_args) /
/// [`Raise::error_info`](beam_common::Raise::error_info) (see [`with_error_payloads`]), and
/// the engine's `families/exceptions/bif_frame.rs` renders them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorMeta {
    /// The caller's argument list, **verbatim** — never validated, never coerced.
    ///
    /// ERTS stores whatever `error/2,3`'s second argument was, whatever its shape
    /// (`error(foo, notalist)` keeps the atom `notalist`); the renderer is what degrades a
    /// non-list to the plain arity (`is_list(args) || is_nil(args)` in `erts_build_mfa_item`),
    /// not this rule.
    pub args: Eterm,
    /// The `error_info` map the raise carries, or `None` when the option list held no
    /// conforming one (see [`error_meta`]).
    pub error_info: Option<Eterm>,
}

/// [`ErrorMeta::args`] plus the `error_info` the C's `error/3` extracts from `options`
/// (`../../erts/emulator/beam/bif.c:1462-1503`).
///
/// `options` is `None` for the `error/2` shape, which always carries `args` and never an
/// `error_info`. The rule is [`scan_error_info`]'s: a proper, non-empty list whose every
/// element is `{error_info, Map}` with `is_map(Map)` yields the **last** such map;
/// everything else (empty list, improper list, any illegal element, a non-list) yields
/// `None` — never `badarg`, because `args` and `options` are never validated.
///
/// `atoms` resolves the `error_info` name; a table that does not know it can never match,
/// which is the same namespace every comparison in this crate reads.
///
/// Oracle-pinned (`tests/error_meta.rs`): `[]` ⇒ none, `[{error_info,#{}}]` ⇒ `#{}`,
/// `[{error_info,#{a=>1}},{error_info,#{b=>2}}]` ⇒ `#{b=>2}` (last wins),
/// `[{error_info,bogus}]` / `[{bogus,1}]` / `[bogus]` / `notalist` /
/// `[{error_info,#{a=>1}}|tail]` / `[{error_info,#{a=>1}},bogus]` ⇒ none.
#[must_use]
pub fn error_meta(args: Eterm, options: Option<Eterm>, atoms: &AtomTable) -> ErrorMeta {
    let error_info = match options {
        None => None,
        Some(options) => scan_error_info_at(options, atoms),
    };
    ErrorMeta { args, error_info }
}

/// [`scan_error_info`]'s rule, resolved against an [`AtomTable`] instead of a
/// [`BifContext`] (the shape [`error_meta`] exposes to hosts).
fn scan_error_info_at(options: Eterm, atoms: &AtomTable) -> Option<Eterm> {
    let err_info_atom = atoms.lookup("error_info")?;
    let mut error_info: Option<Eterm> = None;
    let mut list = options;
    while access::is_list(list) {
        let elem = access::car(list);
        if !access::is_tuple(elem) || access::tuple_size(elem) != 2 {
            break;
        }
        let key = access::tuple_element(elem, 0);
        if !is_atom(key) || atom_val(key) != err_info_atom {
            break;
        }
        let second = access::tuple_element(elem, 1);
        if !access::is_map(second) {
            break;
        }
        error_info = Some(second);
        list = access::cdr(list);
    }
    // The C's `if (is_not_nil(list)) error_info = THE_NON_VALUE;` invalidation.
    if !access::is_nil(list) {
        return None;
    }
    error_info
}

/// `erlang:error/1`.
fn error_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise(ExceptionClass::Error, args[0], ctx.atoms()))
}

/// `erlang:error/2` — the reason plus the raise's **argument list**.
///
/// `error_2` (`../../erts/emulator/beam/bif.c:1448-1452`) builds
/// `TUPLE2(Arg1, Arg2)` as the raise's `fvalue` and raises `EXC_ERROR_2`, so the
/// second argument is kept **verbatim and unvalidated** (`error(foo, notalist)`
/// succeeds) and the reason is still the first argument. It is not the reason's
/// payload but the innermost stacktrace item's `Args` slot, which the renderer uses
/// only when it is a list (`utils.c:3430-3451` — hence `notalist` prints the arity).
///
/// The list rides the [`Raise::saved_args`](beam_common::Raise::saved_args) slot as
/// the live `Eterm` word (see [`with_error_payloads`]); the engine renders it into
/// the innermost frame. Oracle probes (erts-17.1, compiled module, `hd(S)`):
///
/// ```text
/// erlang:error(foo)            ⇒ error:foo, head {P,F,0,[{file,_},{line,_}]}
/// erlang:error(foo, [1,2])     ⇒ error:foo, head {P,F,[1,2],[{file,_},{line,_}]}
/// erlang:error(foo, notalist)  ⇒ error:foo, head {P,F,0,[{file,_},{line,_}]}
/// erlang:error({bad,x}, [1,2]) ⇒ error:{bad,x}, head {P,F,[1,2],[{file,_},{line,_}]}
/// ```
fn error_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let raised = raise(ExceptionClass::Error, args[0], ctx.atoms());
    Err(with_error_payloads(raised, args[1], None))
}

/// `erlang:error/3` — as `error/2`, plus the scanned `error_info` map.
///
/// The reason is the **first** argument, unchanged, for both shapes the C
/// produces (`error_3`, `../../erts/emulator/beam/bif.c:1462`). `Args` is stored
/// verbatim; `Options` is **filtered, never validated**:
///
/// - [`scan_error_info`] keeps the **last** `{error_info, Map}` of a proper,
///   non-empty option list whose every element is such a pair with a map value, and
///   accepts nothing at all on any deviation (a non-list, the empty list, an
///   improper list, a non-tuple element, a wrong arity, a wrong key atom, a non-map
///   value, or a valid pair followed by anything invalid) — silently falling back to
///   the `error/2` shape, never `badarg`.
///
/// Either way the caught value is `CAR(fvalue)` = the first argument
/// (`beam_common.c:478-490`, `EXC_ERROR_2`/`EXC_ERROR_3`): the two payloads only
/// reach the **stacktrace**, riding
/// [`Raise::saved_args`](beam_common::Raise::saved_args) and
/// [`Raise::error_info`](beam_common::Raise::error_info) (see
/// [`with_error_payloads`]). Oracle probes (erts-17.1, compiled module, `hd(S)`):
///
/// ```text
/// error(foo, [1,2], [])                            ⇒ error:foo, {P,F,[1,2],[{file,_},{line,_}]}
/// error(foo, [], [{error_info,#{module=>erl_stdlib_errors}}])
///                                                  ⇒ error:foo, {P,F,[],[{file,_},{line,_},{error_info,#{module => erl_stdlib_errors}}]}
/// error(foo, [3,4], [{error_info,#{k=>v}}])        ⇒ error:foo, {P,F,[3,4],[{file,_},{line,_},{error_info,#{k => v}}]}
/// error(foo, [], [{error_info,#{a=>1}},{error_info,#{b=>2}}])
///                                                  ⇒ error:foo, {P,F,[],[{file,_},{line,_},{error_info,#{b => 2}}]}   (LAST wins)
/// error(foo, notalist, [])                         ⇒ error:foo, {P,F,0,[{file,_},{line,_}]}     (Args not a list ⇒ arity)
/// error(foo, [], [])                               ⇒ error:foo, {P,F,[],[{file,_},{line,_}]}    (empty list ⇒ nothing accepted)
/// error(foo, [], notalist)                         ⇒ error:foo, {P,F,[],[{file,_},{line,_}]}
/// error(foo, [], [{bogus,1}])                      ⇒ error:foo, {P,F,[],[{file,_},{line,_}]}
/// error(foo, [], [bogus])                          ⇒ error:foo, {P,F,[],[{file,_},{line,_}]}
/// error(foo, [], [{error_info,bogus}])             ⇒ error:foo, {P,F,[],[{file,_},{line,_}]}    (non-map value)
/// error(foo, [], [{error_info,#{a=>1}},{bogus,1}]) ⇒ error:foo, {P,F,[],[{file,_},{line,_}]}    (invalidate)
/// error(foo, [], [{error_info,#{}}|a])             ⇒ error:foo, {P,F,[],[{file,_},{line,_}]}    (improper ⇒ invalidate)
/// error(foo, [], [{error_info,#{}},[]])            ⇒ error:foo, {P,F,[],[{file,_},{line,_}]}    (invalid element ⇒ invalidate)
/// ```
///
/// They also pin `error/3`'s **non-atom** behaviour: like `error/1`, a non-atom
/// reason rides the [`Raise::live_reason`](beam_common::Raise::live_reason) slot
/// (see [`raise`]) — `error({bad,x}, [1,2], [])` reports `error:{bad,x}`, not
/// `error:structured`, and `{bad,x}` is *also* the scanned reason while `[1,2]` is
/// the saved args.
fn error_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let error_info = scan_error_info(ctx, args[2]);
    let raised = raise(ExceptionClass::Error, args[0], ctx.atoms());
    Err(with_error_payloads(raised, args[1], error_info))
}

/// `erlang:exit/1`.
fn exit_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise(ExceptionClass::Exit, args[0], ctx.atoms()))
}

/// `erlang:throw/1`.
fn throw_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise(ExceptionClass::Throw, args[0], ctx.atoms()))
}

/// `erlang:nif_error/1` — a NIF raises `error` with its argument as the reason.
///
/// The oracle (erts-17.1) raises the class `error` with the argument *unchanged*:
/// `nif_error(foo)` is `error:foo`, `nif_error(undefined)` is `error:undefined`
/// and `nif_error({nif_not_loaded,m,f,0})` is `error:{nif_not_loaded,m,f,0}`. A
/// non-atom argument rides the [`Raise::live_reason`](beam_common::Raise::live_reason)
/// slot (see [`raise`]).
fn nif_error_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise(ExceptionClass::Error, args[0], ctx.atoms()))
}

/// `erlang:nif_error/2` — as `nif_error/1`; the second argument is metadata.
///
/// `nif_error_2` in `../../erts/emulator/beam/bif.c:1525` raises with the
/// **first** argument as the reason and stores the second (the argument list the
/// crash report prints) exactly as `error/2` does — `TUPLE2(Reason, Args)` with
/// `EXC_ERROR_2` — so `nif_error(foo, [1,2])` is `error:foo` and
/// `nif_error(true, [1,2])` is `error:true`. The raise carries that list in
/// [`Raise::saved_args`](beam_common::Raise::saved_args) like `error/2` does, but
/// **it is stored and not observable** in the oracle's trace: `nif_error/1,2` take
/// the genuine-BIF-frame path, which rebuilds `Args` from the BIF's own arity
/// (`make_arglist`, so the head item is always
/// `{erlang,nif_error,[foo,[1,2]],[{error_info,#{module => erl_erts_errors}}]}`),
/// unlike `error/2,3`, which take `non_bif_stacktrace` and read `fvalue`.
fn nif_error_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let raised = raise(ExceptionClass::Error, args[0], ctx.atoms());
    Err(with_error_payloads(raised, args[1], None))
}

/// Registers `erlang:error/1,2,3`, `exit/1`, `throw/1` and `nif_error/1,2`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "error", 1, error_1);
    reg.register("erlang", "error", 2, error_2);
    reg.register("erlang", "error", 3, error_3);
    reg.register("erlang", "exit", 1, exit_1);
    reg.register("erlang", "throw", 1, throw_1);
    reg.register("erlang", "nif_error", 1, nif_error_1);
    reg.register("erlang", "nif_error", 2, nif_error_2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::ExceptionClass;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    fn seeded() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true",
            "false",
            "ok",
            "hello",
            "normal",
            "shutdown",
            "foo",
            "undefined",
            "nif_not_loaded",
            "m",
            "f",
            // `erlang:error/3`'s well-known option pair (`bif.c:1471`).
            "error_info",
            "module",
            "erl_stdlib_errors",
            "bogus",
            "a",
            "b",
        ]);
        atoms
    }

    /// Runs a raise BIF and returns its `(class, reason)`.
    fn raise_of(
        f: fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>,
        arg: Eterm,
    ) -> (ExceptionClass, ExitReason) {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match f(&mut ctx, &[arg]) {
            Err(BeamError::Raise(raise)) => (raise.class, raise.reason),
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    /// Runs a two-argument raise BIF and returns its `(class, reason)`.
    fn raise_of2(
        f: fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>,
        first: Eterm,
        second: Eterm,
    ) -> (ExceptionClass, ExitReason) {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match f(&mut ctx, &[first, second]) {
            Err(BeamError::Raise(raise)) => (raise.class, raise.reason),
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    /// Runs a raise BIF and returns the whole raise's observable parts plus the
    /// three opaque payloads: `(class, reason, live reason, saved args, error info)`.
    fn raise_payloads(
        f: fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>,
        args: &[Eterm],
    ) -> (
        ExceptionClass,
        ExitReason,
        Option<Eterm>,
        Option<Eterm>,
        Option<Eterm>,
    ) {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match f(&mut ctx, args) {
            Err(BeamError::Raise(raise)) => {
                let slot = |payload: Option<&LiveReason>| {
                    payload.and_then(|l| l.downcast_ref::<Eterm>().copied())
                };
                let live = slot(raise.live_reason());
                let saved = slot(raise.saved_args());
                let info = slot(raise.error_info());
                (raise.class, raise.reason, live, saved, info)
            }
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    /// Runs a raise BIF and returns its `(class, reason, live reason term)`.
    fn raise_of3(
        f: fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>,
        arg: Eterm,
    ) -> (ExceptionClass, ExitReason, Option<Eterm>) {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match f(&mut ctx, &[arg]) {
            Err(BeamError::Raise(raise)) => {
                let live = raise
                    .live_reason()
                    .and_then(|l| l.downcast_ref::<Eterm>().copied());
                (raise.class, raise.reason, live)
            }
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    #[test]
    fn error_exit_and_throw_set_the_class() {
        let mut heap = Heap::new(16).unwrap();
        let atoms = seeded();
        let ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let hello = ctx.atom("hello").unwrap();

        assert_eq!(
            raise_of(error_1, hello),
            (ExceptionClass::Error, ExitReason::atom("hello"))
        );
        assert_eq!(
            raise_of(exit_1, hello),
            (ExceptionClass::Exit, ExitReason::atom("hello"))
        );
        assert_eq!(
            raise_of(throw_1, hello),
            (ExceptionClass::Throw, ExitReason::atom("hello"))
        );
        // `error/2` behaves like `error/1` (the argument list only reaches the
        // stacktrace, never the reason).
        assert_eq!(
            raise_of2(error_2, hello, NIL),
            (ExceptionClass::Error, ExitReason::atom("hello"))
        );
    }

    #[test]
    fn well_known_atoms_map_to_their_variants() {
        let mut heap = Heap::new(16).unwrap();
        let atoms = seeded();
        let ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let normal = ctx.atom("normal").unwrap();
        let shutdown = ctx.atom("shutdown").unwrap();
        assert_eq!(raise_of(exit_1, normal), (ExceptionClass::Exit, ExitReason::Normal));
        assert_eq!(
            raise_of(exit_1, shutdown),
            (ExceptionClass::Exit, ExitReason::Shutdown)
        );
    }

    #[test]
    fn error_3_raises_the_first_argument_as_the_reason() {
        // Oracle (erts-17.1, compiled module): `error_3`
        // (`../../erts/emulator/beam/bif.c:1462`) stores `{Reason, Args}` or
        // `{Reason, Args, ErrorInfo}`; `Args`/`Options` are never validated and
        // the caught reason is always the first argument.
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let foo = ctx.atom("foo").expect("seeded");
        let shutdown = ctx.atom("shutdown").expect("seeded");
        let args = ctx.list(&[make_small(1), make_small(2)]).expect("fits");
        // A 2-tuple: neither a proper option list nor a list item — the C
        // breaks out of its scan and falls back to the `error/2` shape.
        let tuple = ctx.tuple(&[foo, make_small(1)]).expect("fits");

        let classify = |result: Result<Eterm>| match result {
            Err(BeamError::Raise(raise)) => {
                let live = raise
                    .live_reason()
                    .and_then(|l| l.downcast_ref::<Eterm>().copied());
                (raise.class, raise.reason, live)
            }
            other => panic!("expected a raise, got {other:?}"),
        };

        // `erlang:error(foo, [1,2], [])` ⇒ `error:foo`.
        assert_eq!(
            classify(error_3(&mut ctx, &[foo, args, NIL])),
            (ExceptionClass::Error, ExitReason::atom("foo"), None)
        );
        // A non-list `Args` ⇒ still `error:foo` (never `badarg`).
        assert_eq!(
            classify(error_3(&mut ctx, &[foo, make_small(7), NIL])),
            (ExceptionClass::Error, ExitReason::atom("foo"), None)
        );
        // A non-list `Options` ⇒ still `error:foo`.
        assert_eq!(
            classify(error_3(&mut ctx, &[foo, NIL, make_small(7)])),
            (ExceptionClass::Error, ExitReason::atom("foo"), None)
        );
        // A malformed option list (improper / bogus element) ⇒ still `error:foo`.
        assert_eq!(
            classify(error_3(&mut ctx, &[foo, NIL, tuple])),
            (ExceptionClass::Error, ExitReason::atom("foo"), None)
        );
        // A non-atom reason rides the live slot, exactly as `error/1`.
        assert_eq!(
            classify(error_3(&mut ctx, &[tuple, args, NIL])),
            (ExceptionClass::Error, ExitReason::Structured, Some(tuple))
        );
        // A well-known atom keeps its `ExitReason` variant and no live slot.
        assert_eq!(
            classify(error_3(&mut ctx, &[shutdown, NIL, NIL])),
            (ExceptionClass::Error, ExitReason::Shutdown, None)
        );
    }

    #[test]
    fn error_2_saves_the_argument_list_verbatim() {
        // `error_2` (`../../erts/emulator/beam/bif.c:1448-1452`) stores
        // `TUPLE2(Arg1, Arg2)` with `EXC_ERROR_2`: the second argument is kept
        // **verbatim and unvalidated** and the reason is unchanged.
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let foo = ctx.atom("foo").expect("seeded");
        let args = ctx.list(&[make_small(1), make_small(2)]).expect("fits");
        let notalist = make_small(7);

        assert_eq!(
            raise_payloads(error_2, &[foo, args]),
            (
                ExceptionClass::Error,
                ExitReason::atom("foo"),
                None,
                Some(args),
                None
            )
        );
        // `erlang:error(foo, notalist)` ⇒ `error:foo`, the non-list saved anyway
        // (the renderer then prints the arity instead: `utils.c:3430-3451`).
        assert_eq!(
            raise_payloads(error_2, &[foo, notalist]),
            (
                ExceptionClass::Error,
                ExitReason::atom("foo"),
                None,
                Some(notalist),
                None
            )
        );
        // A non-atom reason still rides the live slot, *and* the args are saved.
        let tuple = ctx.tuple(&[foo, make_small(1)]).expect("fits");
        assert_eq!(
            raise_payloads(error_2, &[tuple, args]),
            (
                ExceptionClass::Error,
                ExitReason::Structured,
                Some(tuple),
                Some(args),
                None
            )
        );
    }

    #[test]
    fn error_3_scans_the_error_info_option_list() {
        // `error_3`'s scan (`../../erts/emulator/beam/bif.c:1462-1501`): the **last**
        // `{error_info, Map}` of a wholly-valid proper option list, else nothing.
        let mut heap = Heap::new(512).expect("test heap");
        let atoms = seeded();
        let error_info = make_atom(atoms.lookup("error_info").expect("seeded"));
        let module = make_atom(atoms.lookup("module").expect("seeded"));
        let stdlib = make_atom(atoms.lookup("erl_stdlib_errors").expect("seeded"));
        let bogus = make_atom(atoms.lookup("bogus").expect("seeded"));
        let a = make_atom(atoms.lookup("a").expect("seeded"));
        let b = make_atom(atoms.lookup("b").expect("seeded"));

        // Maps first: `BifContext` borrows the heap for its whole lifetime.
        let empty = access::make_flatmap(&mut heap, &[]).expect("fits");
        let ma = access::make_flatmap(&mut heap, &[(a, make_small(1))]).expect("fits");
        let mb = access::make_flatmap(&mut heap, &[(b, make_small(2))]).expect("fits");
        let meta = access::make_flatmap(&mut heap, &[(module, stdlib)]).expect("fits");

        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let foo = ctx.atom("foo").expect("seeded");
        let args = ctx.list(&[make_small(1), make_small(2)]).expect("fits");

        let pair = |ctx: &mut BifContext<'_>, k: Eterm, v: Eterm| -> Eterm {
            ctx.tuple(&[k, v]).expect("fits")
        };
        // Option lists, one `pair(...)` at a time: `ctx.list` needs `&mut ctx` and so
        // cannot take a `pair(&mut ctx, ..)` inline in its argument.
        let info_meta = pair(&mut ctx, error_info, meta);
        let info_ma = pair(&mut ctx, error_info, ma);
        let info_mb = pair(&mut ctx, error_info, mb);
        let info_bogus = pair(&mut ctx, error_info, bogus);
        // The empty map, for the two invalidation shapes below.
        let info_empty = pair(&mut ctx, error_info, empty);
        let bogus_one = pair(&mut ctx, bogus, make_small(1));

        let valid = ctx.list(&[info_meta]).expect("fits");
        let valid_args = ctx.list(&[info_ma, info_mb]).expect("fits");
        let valid_then_bogus_key = ctx.list(&[info_ma, bogus_one]).expect("fits");
        let valid_then_non_pair = ctx.list(&[info_empty, NIL]).expect("fits");
        let improper = ctx.cons(info_empty, a).expect("fits");
        let non_map = ctx.list(&[info_bogus]).expect("fits");
        let bogus_pair = ctx.list(&[bogus_one]).expect("fits");
        let bare_element = ctx.list(&[bogus]).expect("fits");
        let notalist = make_small(7);

        let err_3 = |args_list: Eterm, options: Eterm| {
            raise_payloads(error_3, &[foo, args_list, options])
        };

        // The two valid shapes: both slots set, `error_info` = the accepted map.
        assert_eq!(
            err_3(NIL, valid),
            (
                ExceptionClass::Error,
                ExitReason::atom("foo"),
                None,
                Some(NIL),
                Some(meta)
            )
        );
        assert_eq!(
            err_3(args, valid),
            (
                ExceptionClass::Error,
                ExitReason::atom("foo"),
                None,
                Some(args),
                Some(meta)
            )
        );
        // Two valid pairs ⇒ the **last** map wins.
        assert_eq!(
            err_3(NIL, valid_args),
            (
                ExceptionClass::Error,
                ExitReason::atom("foo"),
                None,
                Some(NIL),
                Some(mb)
            )
        );
        // Every deviation keeps `saved_args` and drops `error_info`.
        for (name, options) in [
            ("empty list", NIL),
            ("non-list", notalist),
            ("non-map value", non_map),
            ("bogus key", bogus_pair),
            ("bare element", bare_element),
            ("valid then bogus key", valid_then_bogus_key),
            ("valid then non-pair", valid_then_non_pair),
            ("improper tail", improper),
        ] {
            assert_eq!(
                err_3(args, options),
                (
                    ExceptionClass::Error,
                    ExitReason::atom("foo"),
                    None,
                    Some(args),
                    None
                ),
                "error/3 with a {name} options list"
            );
        }
    }

    #[test]
    fn only_the_shapes_that_carry_payloads_set_the_slots() {
        // `error/1`, `exit/1`, `throw/1` and `nif_error/1` set **no** slot; the
        // `error/2,3` and `nif_error/2` shapes set `saved_args` (never an
        // `error_info`).
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let foo = ctx.atom("foo").expect("seeded");
        let hello = ctx.atom("hello").expect("seeded");
        let args = ctx.list(&[make_small(1), make_small(2)]).expect("fits");

        for (name, f, class) in [
            (
                "error/1",
                error_1 as fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>,
                ExceptionClass::Error,
            ),
            ("exit/1", exit_1, ExceptionClass::Exit),
            ("throw/1", throw_1, ExceptionClass::Throw),
            ("nif_error/1", nif_error_1, ExceptionClass::Error),
        ] {
            assert_eq!(
                raise_payloads(f, &[foo]),
                (class, ExitReason::atom("foo"), None, None, None),
                "{name} carries no stacktrace payload"
            );
        }
        assert_eq!(
            raise_payloads(error_1, &[hello]),
            (
                ExceptionClass::Error,
                ExitReason::atom("hello"),
                None,
                None,
                None
            )
        );
        // `nif_error(Reason, Args)` stores the list like `error/2` — the oracle's
        // *trace* overrides it with the BIF's own arglist, but the C stores it.
        assert_eq!(
            raise_payloads(nif_error_2, &[foo, args]),
            (
                ExceptionClass::Error,
                ExitReason::atom("foo"),
                None,
                Some(args),
                None
            )
        );
    }

    #[test]
    fn non_atom_reasons_are_structured_and_carry_their_term() {
        // `erl -noshell -eval 'try erlang:throw(42) catch C:R ->
        // io:format("~p:~p~n",[C,R]) end, halt().'` ⇒ `throw:42`.
        assert_eq!(
            raise_of3(throw_1, make_small(42)),
            (ExceptionClass::Throw, ExitReason::Structured, Some(make_small(42)))
        );
        // `erlang:error([])` ⇒ `error:[]`.
        assert_eq!(
            raise_of3(error_1, NIL),
            (ExceptionClass::Error, ExitReason::Structured, Some(NIL))
        );
        // `erlang:exit({shutdown,1})` ⇒ `exit:{shutdown,1}`.
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let shutdown = ctx.atom("shutdown").unwrap();
        let tuple = ctx.tuple(&[shutdown, make_small(1)]).unwrap();
        assert_eq!(
            raise_of3(exit_1, tuple),
            (ExceptionClass::Exit, ExitReason::Structured, Some(tuple))
        );
    }

    #[test]
    fn atom_reasons_carry_no_live_term() {
        let mut heap = Heap::new(16).unwrap();
        let atoms = seeded();
        let ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let hello = ctx.atom("hello").unwrap();
        assert_eq!(
            raise_of3(error_1, hello),
            (ExceptionClass::Error, ExitReason::atom("hello"), None)
        );
        assert_eq!(
            raise_of3(exit_1, hello),
            (ExceptionClass::Exit, ExitReason::atom("hello"), None)
        );
    }

    /// Oracle-pinned (`erl -eval 'erlang:nif_error(T)'`): the class is always
    /// `error` and the reason is the first argument unchanged.
    #[test]
    fn nif_error_1_raises_error_with_the_argument_as_reason() {
        let mut heap = Heap::new(32).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let foo = ctx.atom("foo").expect("seeded");
        let undefined = ctx.atom("undefined").expect("seeded");
        let tru = ctx.atom("true").expect("seeded");

        // `erlang:nif_error(foo)` ⇒ `error:foo`.
        assert_eq!(
            raise_of(nif_error_1, foo),
            (ExceptionClass::Error, ExitReason::atom("foo"))
        );
        // `erlang:nif_error(undefined)` ⇒ `error:undefined`.
        assert_eq!(
            raise_of(nif_error_1, undefined),
            (ExceptionClass::Error, ExitReason::atom("undefined"))
        );
        assert_eq!(
            raise_of(nif_error_1, tru),
            (ExceptionClass::Error, ExitReason::atom("true"))
        );

        // `erlang:nif_error({nif_not_loaded,m,f,0})` ⇒ `error:{nif_not_loaded,m,f,0}`.
        // A compound reason is `ExitReason::Structured`; the reason term itself rides
        // the opaque `Raise::live_reason` slot (see [`raise`]).
        let nif_not_loaded = ctx.atom("nif_not_loaded").expect("seeded");
        let m = ctx.atom("m").expect("seeded");
        let f = ctx.atom("f").expect("seeded");
        let tuple = ctx
            .tuple(&[nif_not_loaded, m, f, make_small(0)])
            .expect("fits");
        assert_eq!(
            raise_of3(nif_error_1, tuple),
            (ExceptionClass::Error, ExitReason::Structured, Some(tuple))
        );
    }

    /// `erlang:nif_error/2` uses the **first** argument as the reason; the second
    /// (the crash-report argument list) is metadata (`bif.c:1525`, `nif_error_2`).
    #[test]
    fn nif_error_2_uses_the_first_argument_as_the_reason() {
        let mut heap = Heap::new(32).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let foo = ctx.atom("foo").expect("seeded");
        let tru = ctx.atom("true").expect("seeded");
        let meta = ctx.list(&[make_small(1), make_small(2)]).expect("fits");

        // `erlang:nif_error(foo, [1,2])` ⇒ `error:foo`.
        assert_eq!(
            raise_of2(nif_error_2, foo, meta),
            (ExceptionClass::Error, ExitReason::atom("foo"))
        );
        // `erlang:nif_error(true, [1,2])` ⇒ `error:true`.
        assert_eq!(
            raise_of2(nif_error_2, tru, meta),
            (ExceptionClass::Error, ExitReason::atom("true"))
        );
    }

    #[test]
    fn exit_reason_from_term_maps_atoms_and_structures() {
        let mut atoms = seeded();
        let normal = make_atom(atoms.intern("normal"));
        let shutdown = make_atom(atoms.intern("shutdown"));
        let noproc = make_atom(atoms.intern("noproc"));
        let noconnection = make_atom(atoms.intern("noconnection"));
        let killed = make_atom(atoms.intern("killed"));
        let aborted = make_atom(atoms.intern("aborted"));
        let hello = make_atom(atoms.intern("hello"));

        assert_eq!(exit_reason_from_term(normal, &atoms), ExitReason::Normal);
        assert_eq!(exit_reason_from_term(shutdown, &atoms), ExitReason::Shutdown);
        assert_eq!(exit_reason_from_term(noproc, &atoms), ExitReason::NoProc);
        assert_eq!(exit_reason_from_term(noconnection, &atoms), ExitReason::NoConnection);
        assert_eq!(exit_reason_from_term(killed, &atoms), ExitReason::Killed);
        assert_eq!(exit_reason_from_term(aborted, &atoms), ExitReason::Aborted);
        assert_eq!(exit_reason_from_term(hello, &atoms), ExitReason::atom("hello"));

        // Non-atom reasons, including the empty list and integers.
        assert_eq!(exit_reason_from_term(NIL, &atoms), ExitReason::Structured);
        assert_eq!(exit_reason_from_term(make_small(1), &atoms), ExitReason::Structured);

        // A pid term is not an atom either.
        assert_eq!(
            exit_reason_from_term(ProcessId::new(1, 0).to_eterm(), &atoms),
            ExitReason::Structured
        );
    }

    #[test]
    fn reason_term_from_exit_reason_round_trips_atoms() {
        let mut atoms = seeded();
        for reason in [
            ExitReason::Normal,
            ExitReason::Shutdown,
            ExitReason::NoProc,
            ExitReason::NoConnection,
            ExitReason::Killed,
            ExitReason::Aborted,
            ExitReason::atom("hello"),
        ] {
            let term = reason_term_from_exit_reason(&reason, &mut atoms).unwrap();
            assert!(beam_term::is_atom(term));
            assert_eq!(exit_reason_from_term(term, &atoms), reason);
        }
    }

    #[test]
    fn reason_term_from_exit_reason_rejects_structured() {
        let mut atoms = seeded();
        assert!(matches!(
            reason_term_from_exit_reason(&ExitReason::Structured, &mut atoms),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn register_installs_the_raise_helpers() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert!(reg.contains("erlang", "error", 1));
        assert!(reg.contains("erlang", "error", 2));
        assert!(reg.contains("erlang", "error", 3));
        assert!(reg.contains("erlang", "exit", 1));
        assert!(reg.contains("erlang", "throw", 1));
        assert!(reg.contains("erlang", "nif_error", 1));
        assert!(reg.contains("erlang", "nif_error", 2));
        assert_eq!(reg.len(), 7);
    }
}
