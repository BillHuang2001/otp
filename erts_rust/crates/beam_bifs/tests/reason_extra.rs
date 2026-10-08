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

//! Integration tests for `erlang:error/3`, driven through [`BifRegistry::call`]
//! (`erlang:get/0` and `erlang:erase/0` are pinned in `tests/dict.rs`).
//!
//! `error/3` is `error/1` with the second and third arguments folded into the
//! **stacktrace** only. Ground truth `error_3` in
//! `../../erts/emulator/beam/bif.c:1462`:
//!
//! ```c
//! BIF_RETTYPE error_3(BIF_ALIST_3) {
//!     ...scan BIF_ARG_3 for {error_info, Map}...
//!     if (is_value(error_info)) { fvalue = TUPLE3(ARG_1, ARG_2, error_info); BIF_ERROR(.., EXC_ERROR_3); }
//!     else                      { fvalue = TUPLE2(ARG_1, ARG_2);              BIF_ERROR(.., EXC_ERROR_2); }
//! }
//! ```
//!
//! and the raise handler, which digs the real reason out of that tuple
//! (`beam_common.c:478-490`): `Value = tp[1]` = `BIF_ARG_1` in both branches. So
//! the caught reason is always the **first** argument, whatever `Args`/`Options`
//! look like — the C never raises `badarg` for them.
//!
//! Oracle probes (erts-17.1, compiled module, `try erlang:error(R,A,O) catch
//! C:X -> io:format("~p:~p~n",[C,X]) end`):
//!
//! | call | caught |
//! |---|---|
//! | `error(foo, [1,2], [])` | `error:foo` |
//! | `error({bad,x}, [1,2], [])` | `error:{bad,x}` |
//! | `error(foo, [], [{error_info, #{}}])` | `error:foo` |
//! | `error(foo, notalist, [])` | `error:foo` |
//! | `error(foo, [], notalist)` | `error:foo` |
//! | `error(foo, [], [{bogus,1}])` | `error:foo` |
//! | `error(foo, [], [bogus])` | `error:foo` |
//! | `error(foo, [], [{error_info, bogus}])` | `error:foo` |

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{make_small, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "undefined", "foo", "normal", "shutdown"]);
    atoms
}

/// The `(class, reason, live reason term)` of the raise `result` carries.
fn raise_of(result: Result<Eterm>) -> (ExceptionClass, ExitReason, Option<Eterm>) {
    match result {
        Err(BeamError::Raise(raise)) => {
            let live = raise
                .live_reason()
                .and_then(|l| l.downcast_ref::<Eterm>().copied());
            (raise.class, raise.reason, live)
        }
        other => panic!("expected a raise, got {other:?}"),
    }
}

/// One `error/3` call. `args`/`options` are immediates or terms built on a
/// scratch heap; only the *reason* term matters for the assertion.
fn error_of3(reg: &BifRegistry, reason: Eterm, args: Eterm, options: Eterm)
    -> (ExceptionClass, ExitReason, Option<Eterm>)
{
    let mut heap = Heap::new(256).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    raise_of(reg.call("erlang", "error", 3, &mut ctx, &[reason, args, options]))
}

/// `erlang:error(foo, [1,2], [])` ⇒ `error:foo`.
#[test]
fn atom_reason_is_the_first_argument_with_an_argument_list() {
    let reg = default_registry();
    let mut heap = Heap::new(256).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let foo = ctx.atom("foo").expect("seeded");
    let args = ctx.list(&[make_small(1), make_small(2)]).expect("fits");

    assert_eq!(
        error_of3(&reg, foo, args, NIL),
        (ExceptionClass::Error, ExitReason::atom("foo"), None)
    );
}

/// A non-atom reason rides the `Raise::live_reason` slot, exactly as `error/1`:
/// `error([], [1,2], [])` ⇒ `error:[]`, not `error:structured`.
#[test]
fn non_atom_reason_carries_its_live_term() {
    let reg = default_registry();
    assert_eq!(
        error_of3(&reg, NIL, NIL, NIL),
        (ExceptionClass::Error, ExitReason::Structured, Some(NIL))
    );
}

/// A well-known reason atom maps to its dedicated [`ExitReason`] variant and
/// carries no live slot (`error(shutdown, [], [])` ⇒ `error:shutdown`).
#[test]
fn well_known_reason_atom_maps_to_its_variant() {
    let reg = default_registry();
    let mut heap = Heap::new(64).expect("test heap");
    let atoms = seeded_atoms();
    let ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let shutdown = ctx.atom("shutdown").expect("seeded");

    assert_eq!(
        error_of3(&reg, shutdown, NIL, NIL),
        (ExceptionClass::Error, ExitReason::Shutdown, None)
    );
}

/// `Args` and `Options` are never validated: a non-list `Args`, a non-list
/// `Options` and a malformed option all keep the reason and never raise `badarg`
/// (the C silently falls back to its `error/2` shape).
#[test]
fn args_and_options_are_not_validated() {
    let reg = default_registry();
    let mut heap = Heap::new(256).expect("test heap");
    let atoms = seeded_atoms();
    let (foo, args, info, bogus_option, not_a_list) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let foo = ctx.atom("foo").expect("seeded");
        let args = ctx.list(&[make_small(1), make_small(2)]).expect("fits");
        // `{foo, 1}` — a 2-tuple that is *not* an `{error_info, Map}` option, so
        // the C's scan breaks out immediately.
        let info = ctx.tuple(&[foo, make_small(1)]).expect("fits");
        // A list of one illegal element.
        let bogus_option = ctx.list(&[foo]).expect("fits");
        (foo, args, info, bogus_option, make_small(7))
    };

    let expected = (ExceptionClass::Error, ExitReason::atom("foo"), None);

    // A non-list `Args` with a proper option list.
    assert_eq!(error_of3(&reg, foo, not_a_list, NIL), expected);
    // A non-list `Options`.
    assert_eq!(error_of3(&reg, foo, args, not_a_list), expected);
    // An option list whose elements are not `{error_info, Map}` 2-tuples.
    assert_eq!(error_of3(&reg, foo, args, bogus_option), expected);
    // A third argument that is not a list at all.
    assert_eq!(error_of3(&reg, foo, args, info), expected);
}

/// The key is registered under `erlang:error/3` — and is *distinct* from the
/// pre-existing `error/1,2` entries (the registry is keyed on arity).
#[test]
fn error_3_is_registered_alongside_error_1_and_2() {
    let reg = default_registry();
    assert!(reg.contains("erlang", "error", 1));
    assert!(reg.contains("erlang", "error", 2));
    assert!(reg.contains("erlang", "error", 3));
    assert!(reg.lookup("erlang", "error", 3).is_some());
    // Arity 4 does not exist in OTP (`bif.tab:775` declares only `error/3`).
    assert!(!reg.contains("erlang", "error", 4));
}
