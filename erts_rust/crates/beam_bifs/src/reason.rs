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
//! in `../../erts/emulator/beam/bif.c:1525-1531` (exactly `error/2`: the second
//! argument is that raise's stacktrace `Args`, **not** the reason).
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
//! # The stacktrace-side payload of `error/2,3` — modelled, not yet attachable
//!
//! `erlang:error/2,3` carry a payload that is neither the reason nor an error: the
//! caller's **argument list** and, for `error/3`, the `{error_info, Map}` the C digs
//! out of the option list. ERTS keeps them in the exception object — `{Reason, Args}`
//! for `error/2` and `{Reason, Args, ErrorInfo}` for `error/3`
//! (`../../erts/emulator/beam/bif.c:1448-1503`) — and `handle_error` extracts them
//! again when the stacktrace is saved
//! (`../../erts/emulator/beam/beam_common.c:478-495`), so the **innermost**
//! `[{M,F,A,Location}]` item of a caught `S` renders the argument list in its `A` slot
//! instead of the declared arity and ends its `Location` list with
//! `{error_info, Map}` (`erts_build_mfa_item`,
//! `../../erts/emulator/beam/utils.c:3430-3457`). Measured on the oracle: `S`'s head
//! for `erlang:error(boom, [1,2], [{error_info,#{a=>1}}])` is
//! `{m, f, [1,2], [{file, …},{line, N},{error_info,#{a => 1}}]}`.
//!
//! [`error_meta`] reproduces the C's **option rule** exactly, and it is the handoff a
//! host uses to attach that payload. It is not attached yet, and it cannot be from
//! this crate: `beam_common::Raise` carries `class`, `reason`, `stacktrace` and the
//! opaque `live_reason` slot only, `StackFrame` is metadata-only
//! (module/function/arity/location), and the one opaque slot is already spoken for by
//! the reason term (above). The payload therefore has nowhere to live — a recorded
//! capability gap, reported to the parent node, not an unobservable simplification.
//! [`error_2`]/[`error_3`] still raise the oracle's class and reason exactly.

use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_term::{atom_val, make_atom, AtomTable, Eterm};

use crate::access::{car, cdr, is_list, is_map, is_nil, is_tuple, tuple_element, tuple_size};
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

/// The stacktrace-side payload of `erlang:error/2,3` — the caller's argument list and,
/// for `error/3`, the `error_info` map the raise carries.
///
/// Built by [`error_meta`]. ERTS keeps the pair in the exception object
/// (`{Reason, Args}` / `{Reason, Args, ErrorInfo}`, `error_2`/`error_3` in
/// `../../erts/emulator/beam/bif.c:1448-1503`) and renders it into the **innermost**
/// stacktrace item: the argument list in the item's `A` slot, the map as the last
/// element of its `Location` list (`erts_build_mfa_item`,
/// `../../erts/emulator/beam/utils.c:3430-3457`).
///
/// # Why this is a value and not part of the raise (the blocked `Raise` seam)
///
/// It has nowhere to live yet: `beam_common::Raise` carries `class`, `reason`,
/// `stacktrace` and the opaque `live_reason` slot, and `StackFrame` is metadata-only —
/// while `live_reason` is the *reason* term by workspace convention. So the payload is
/// **modelled here** (with the C's exact option rule) for the engine's stacktrace
/// renderer to consume once that slot exists; attaching it today would mean
/// overloading the reason slot. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorMeta {
    /// The caller's argument list, **verbatim** — never validated, never coerced.
    ///
    /// ERTS stores whatever `error/2,3`'s second argument was, whatever its shape
    /// (`error(foo, notalist)` keeps the atom `notalist`); the renderer is what
    /// degrades a non-list to the plain arity (`is_list(args) || is_nil(args)` in
    /// `erts_build_mfa_item`), not this rule.
    pub args: Eterm,
    /// The `error_info` map the raise carries, or `None` when the option list held no
    /// conforming one (see [`error_meta`]).
    pub error_info: Option<Eterm>,
}

/// The `erlang:error/2,3` stacktrace payload: `args` verbatim plus the `error_info` the
/// C's `error/3` extracts from `options` (`error_3`,
/// `../../erts/emulator/beam/bif.c:1462-1503`).
///
/// `options` is `None` for the `error/2` shape, which always carries `args` and never an
/// `error_info`.
///
/// # The C's exact rule
///
/// `error/3` walks `options` while it is a **cons**:
///
/// 1. an element that is not a tuple, or whose arity is not `2`, or whose first element
///    is not the atom `error_info`, ends the walk;
/// 2. otherwise the tuple's second element becomes the candidate `error_info` and, when
///    it is **not** a map (`is_not_map`), that also ends the walk — with the non-map
///    value in hand;
/// 3. the walk continues on the tail only for a conforming `{error_info, Map}`.
///
/// Afterwards, if what remains is **not** `[]` the candidate is **discarded**, so an
/// improper list or any illegal element invalidates the whole `error_info` — including a
/// conforming map found earlier. The net rule is therefore: a proper, non-empty list
/// whose every element is `{error_info, Map}` with `is_map(Map)` yields the **last** such
/// map; everything else (empty list, improper list, any illegal element, a non-list)
/// yields `None`. `error/3` then falls back to the `error/2` shape, never a `badarg` —
/// `args` and `options` are never validated.
///
/// `atoms` resolves the `error_info` name; a table that does not know it can never
/// match, which is the same namespace every comparison in this crate reads.
///
/// Oracle-pinned (`tests/error_meta.rs`, a compiled module under the live system `erl`):
/// `[]` ⇒ none, `[{error_info,#{}}]` ⇒ `#{}`,
/// `[{error_info,#{a=>1}},{error_info,#{b=>2}}]` ⇒ `#{b=>2}` (last wins),
/// `[{error_info,bogus}]` / `[{bogus,1}]` / `[bogus]` / `notalist` /
/// `[{error_info,#{a=>1}}|tail]` / `[{error_info,#{a=>1}},bogus]` ⇒ none.
#[must_use]
pub fn error_meta(args: Eterm, options: Option<Eterm>, atoms: &AtomTable) -> ErrorMeta {
    let mut error_info = None;
    let mut list = match options {
        // `error/2`: no option list at all, so no `error_info` (`bif.c:1448-1454`).
        None => {
            return ErrorMeta {
                args,
                error_info: None,
            }
        }
        Some(options) => options,
    };
    while is_list(list) {
        let element = car(list);
        if !is_tuple(element) || tuple_size(element) != 2 {
            break;
        }
        if !is_error_info_atom(tuple_element(element, 0), atoms) {
            break;
        }
        let candidate = tuple_element(element, 1);
        // The C assigns `error_info` *before* its map test, which is why a non-map value
        // here ends the walk with the candidate armed and then invalidated below.
        error_info = Some(candidate);
        if !is_map(candidate) {
            break;
        }
        list = cdr(list);
    }
    // Anything but a cleaned-off `[]` is an improper list (or an unlistable tail): the
    // C invalidates whatever it collected (`bif.c:1485-1488`).
    if !is_nil(list) {
        error_info = None;
    }
    ErrorMeta { args, error_info }
}

/// `true` when `term` is the atom `error_info` in `atoms`.
fn is_error_info_atom(term: Eterm, atoms: &AtomTable) -> bool {
    beam_term::is_atom(term) && atoms.name(atom_val(term)) == Some("error_info")
}

/// `erlang:error/1`.
fn error_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise(ExceptionClass::Error, args[0], ctx.atoms()))
}

/// `erlang:error/2` — the reason is the first argument; the second is the caller's
/// **argument list**, which ERTS publishes into the stacktrace.
///
/// # What already agrees with the oracle, and what is missing
///
/// The raise **class and reason are exact**: a well-known atom keeps its [`ExitReason`]
/// variant and a compound one rides the
/// [`Raise::live_reason`](beam_common::Raise::live_reason) slot (see [`raise`]), so
/// `error(foo, [1,2])` and `error({bad,x}, [1,2])` are `error:foo` / `error:{bad,x}`
/// here exactly as on the oracle.
///
/// The second argument is **observable** in a `try … catch C:R:S` handler and is
/// therefore not a simplification this crate may make: ERTS stores `{Reason, Args}` as
/// the exception value (`../../erts/emulator/beam/bif.c:1448-1454`) and the innermost
/// stacktrace item then renders `{Module, Function, [1,2], Location}` instead of the
/// declared arity (`erts_build_mfa_item`,
/// `../../erts/emulator/beam/utils.c:3448-3455`). That payload is modelled by
/// [`error_meta`] but **cannot be attached yet**: `beam_common::Raise` has no slot for
/// it (see [`ErrorMeta`]), so it is dropped at that seam — a recorded capability gap,
/// with the reason and class preserved.
fn error_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise(ExceptionClass::Error, args[0], ctx.atoms()))
}

/// `erlang:error/3` — as `error/1`, with the argument list and `error_info` published
/// into the **stacktrace** only.
///
/// The reason is the **first** argument, unchanged, for both shapes the C produces
/// (`error_3`, `../../erts/emulator/beam/bif.c:1462`). `Args` and `Options` never become
/// the reason and are **not validated**:
///
/// - the C scans `Options` for a run of `{error_info, Map}` 2-tuples and, on anything
///   else (a non-list, an improper list, an empty list, an illegal element, a non-map
///   `error_info`), silently falls back to the `error/2` shape — no `badarg`;
/// - `Args` is stored verbatim, whatever its shape.
///
/// Either way the caught value is `CAR(fvalue)` = the first argument
/// (`beam_common.c:478-490`, `EXC_ERROR_2`/`EXC_ERROR_3`), so the **reason** and the
/// raise class agree with the oracle in every one of those shapes. Oracle probes
/// (erts-17.1, compiled module):
///
/// ```text
/// erlang:error(foo, [1,2], [])                 ⇒ error:foo
/// erlang:error({bad,x}, [1,2], [])             ⇒ error:{bad,x}
/// erlang:error(foo, [], [{error_info, #{}}])   ⇒ error:foo
/// erlang:error(foo, notalist, [])              ⇒ error:foo
/// erlang:error(foo, [], notalist)              ⇒ error:foo
/// erlang:error(foo, [], [{bogus,1}])           ⇒ error:foo
/// erlang:error(foo, [], [bogus])               ⇒ error:foo
/// erlang:error(foo, [], [{error_info, bogus}]) ⇒ error:foo
/// ```
///
/// They also pin `error/3`'s **non-atom** behaviour: like `error/1`, a non-atom reason
/// rides the [`Raise::live_reason`](beam_common::Raise::live_reason) slot (see
/// [`raise`]) — `error({bad,x}, [1,2], [])` reports `error:{bad,x}`, not
/// `error:structured`.
///
/// # The missing stacktrace-side payload
///
/// Everything above is the *reason*; what a caught `S` additionally shows — the argument
/// list in the innermost item's `A` slot and the `{error_info, Map}` at the end of that
/// item's `Location` list (`utils.c:3430-3457`) — is modelled by [`error_meta`] with the
/// C's exact option rule but **cannot be attached yet**, because
/// `beam_common::Raise` has no slot for it (see [`ErrorMeta`]).
fn error_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise(ExceptionClass::Error, args[0], ctx.atoms()))
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

/// `erlang:nif_error/2` — exactly `error/2`, with a reason class Dialyzer treats as
/// arbitrary.
///
/// `nif_error_2` in `../../erts/emulator/beam/bif.c:1525-1531` stores
/// `TUPLE2(BIF_ARG_1, BIF_ARG_2)` in the exception value and raises `EXC_ERROR_2` —
/// the same shape and the same exception code as `error_2`, so the **first** argument
/// is the reason (`nif_error(foo, [1,2])` is `error:foo`, `nif_error(true, [1,2])` is
/// `error:true`) and the second is the `Args` half of the stacktrace payload, which
/// [`ErrorMeta`] models and which this raise cannot carry yet. It is **not** a
/// crash-report-only annotation, and it is not dropped by the C.
fn nif_error_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    Err(raise(ExceptionClass::Error, args[0], ctx.atoms()))
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
            "error_info",
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
        // `error/2` raises `error/1`'s class and reason; the argument list it also
        // carries belongs to the stacktrace (see `ErrorMeta`) and is not part of the
        // raise this crate can return today.
        assert_eq!(
            raise_of(error_2, hello),
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

    // -----------------------------------------------------------------------
    // `error_meta` — the `error/3` option rule (`bif.c:1462-1503`)
    // -----------------------------------------------------------------------

    /// Builds `{Tag, Value}` on `ctx`'s heap.
    fn pair(ctx: &mut BifContext<'_>, tag: Eterm, value: Eterm) -> Eterm {
        ctx.tuple(&[tag, value]).expect("the 2-tuple fits")
    }

    /// Builds a flat map whose single key is `key` — enough to tell one candidate map
    /// from another by term identity.
    fn map_of(ctx: &mut BifContext<'_>, key: Option<Eterm>) -> Eterm {
        match key {
            None => crate::access::make_flatmap(ctx.heap(), &[]).expect("the empty map fits"),
            Some(key) => crate::access::make_flatmap(ctx.heap(), &[(key, make_small(1))])
                .expect("the one-pair map fits"),
        }
    }

    /// The conforming shapes: a proper, non-empty list of `{error_info, Map}` keeps the
    /// **last** map (`bif.c:1478` overwrites `error_info` on every conforming element).
    #[test]
    fn error_meta_keeps_the_last_conforming_error_info_map() {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let error_info = ctx.atom("error_info").expect("seeded");
        let foo = ctx.atom("foo").expect("seeded");
        let args = ctx.list(&[make_small(1), make_small(2)]).expect("fits");

        let empty = map_of(&mut ctx, None);
        let one = map_of(&mut ctx, Some(foo));
        let empty_info = pair(&mut ctx, error_info, empty);
        let one_info = pair(&mut ctx, error_info, one);

        // `[{error_info, #{}}]` ⇒ `Some(#{})` — an *empty* map is still a map.
        let options = ctx.list(&[empty_info]).expect("fits");
        assert_eq!(
            error_meta(args, Some(options), &atoms),
            ErrorMeta {
                args,
                error_info: Some(empty)
            }
        );

        // `[{error_info, #{a=>1}}, {error_info, #{}}]` ⇒ last wins.
        let options = ctx.list(&[one_info, empty_info]).expect("fits");
        assert_eq!(
            error_meta(args, Some(options), &atoms),
            ErrorMeta {
                args,
                error_info: Some(empty)
            }
        );

        // …and in the other order the same list keeps the other map: the rule is
        // "last element", never "the biggest" or "the first".
        let options = ctx.list(&[empty_info, one_info]).expect("fits");
        assert_eq!(
            error_meta(args, Some(options), &atoms).error_info,
            Some(one)
        );
    }

    /// Every illegal option list invalidates the whole `error_info` — including a
    /// conforming map that came earlier (`bif.c:1485-1488`), which is why the walk
    /// cannot stop at the first element.
    #[test]
    fn error_meta_rejects_every_illegal_option_list() {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let error_info = ctx.atom("error_info").expect("seeded");
        let foo = ctx.atom("foo").expect("seeded");
        let hello = ctx.atom("hello").expect("seeded");
        let args = ctx.list(&[make_small(1), make_small(2)]).expect("fits");

        let map = map_of(&mut ctx, None);
        let good = pair(&mut ctx, error_info, map);
        let not_a_map = pair(&mut ctx, error_info, hello);
        let wrong_tag = pair(&mut ctx, foo, map);
        let three_tuple = ctx.tuple(&[error_info, map, hello]).expect("fits");
        let nested = ctx.list(&[good]).expect("fits");

        let cases: Vec<(&str, Eterm)> = vec![
            ("the empty list", NIL),
            ("a non-list", hello),
            ("a bare atom element", ctx.list(&[hello]).expect("fits")),
            ("a wrong-tag 2-tuple", ctx.list(&[wrong_tag]).expect("fits")),
            ("an `error_info` that is not a map", ctx.list(&[not_a_map]).expect("fits")),
            ("a 3-tuple", ctx.list(&[three_tuple]).expect("fits")),
            ("a nested list element", ctx.list(&[nested]).expect("fits")),
            // An improper tail: the cons chain itself is fine, `CDR` is not `[]`.
            ("an improper list", ctx.cons(good, hello).expect("fits")),
            // A conforming map followed by anything illegal loses the map.
            (
                "a conforming map then a bare atom",
                ctx.list(&[good, hello]).expect("fits"),
            ),
            (
                "a conforming map then a non-map `error_info`",
                ctx.list(&[good, not_a_map]).expect("fits"),
            ),
        ];
        for (what, options) in cases {
            assert_eq!(
                error_meta(args, Some(options), &atoms),
                ErrorMeta {
                    args,
                    error_info: None
                },
                "{what} must yield no error_info"
            );
        }
    }

    /// `error/2`'s shape (`options: None`) never carries an `error_info`, and `args` is
    /// stored verbatim in **every** shape — a non-list `Args` is not validated either
    /// (`error(foo, notalist)` on the oracle is `error:foo`, and the renderer — not this
    /// rule — is what falls back to the plain arity).
    #[test]
    fn error_meta_stores_args_verbatim_and_error_2_has_no_error_info() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let hello = ctx.atom("hello").expect("seeded");
        let error_info = ctx.atom("error_info").expect("seeded");
        let map = map_of(&mut ctx, None);
        let good = pair(&mut ctx, error_info, map);
        let options = ctx.list(&[good]).expect("fits");

        for args in [
            NIL,
            make_small(7),
            hello,
            options,
        ] {
            assert_eq!(
                error_meta(args, None, &atoms),
                ErrorMeta {
                    args,
                    error_info: None
                },
                "the error/2 shape keeps args verbatim and never an error_info"
            );
        }

        // The same non-list `args` with a *conforming* option list: the map is kept, the
        // argument term stays exactly as given.
        assert_eq!(
            error_meta(hello, Some(options), &atoms),
            ErrorMeta {
                args: hello,
                error_info: Some(map)
            }
        );
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
