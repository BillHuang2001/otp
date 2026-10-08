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

//! **Clause dispatch on the list predicates** — `is_nil` / `is_list` /
//! `is_nonempty_list`, pinned against the system `erl` oracle.
//!
//! # Why this file exists
//!
//! A prior corpus sweep reported that `edlin_context:get_context/1` reached its
//! `function_clause` through a *different* control-flow path under this engine
//! than under the oracle — with `is_nil` / `is_nonempty_list` letting the call run
//! *past* a clause the oracle fails on. An agreeing *reason* reached for the wrong
//! reason is not proof of correctness, so the case was excluded from that sweep's
//! corpus. This file settles it: the engine is **branch-for-branch identical** to
//! the C on every input the host can run, and that agreement is pinned here.
//!
//! ## The C ground truth (`../../erts/emulator/beam/emu/instrs.tab`)
//!
//! ```c
//! is_nonempty_list(Fail, Src) {          /* :881 */  `ops.tab:764`  is_nonempty_list f xy
//!     if (is_not_list($Src)) { $FAIL($Fail); }
//! }
//! is_list(Fail, Src) {                   /* :981 */  `ops.tab:739`  is_list f x
//!     if (is_not_list($Src) && is_not_nil($Src)) { $FAIL($Fail); }
//! }
//! is_nil(Fail, Src) {                    /* :993 */
//!     if (is_not_nil($Src)) { $FAIL($Fail); }
//! }
//! ```
//! with (`erl_term.h:222-229`, `:293-295`)
//!
//! ```c
//! #define _unchecked_is_not_list(x) ((x) & (_TAG_PRIMARY_MASK - TAG_PRIMARY_LIST))   /* x & 0x2 */
//! #define is_list(x)                (!is_not_list((x)))
//! #define is_nil(x)                 ((x) == NIL)                                       /* NIL == 0x3B, an IMMED1 */
//! ```
//!
//! So, exactly:
//!
//! | instruction          | passes for               | fails for                            |
//! |----------------------|--------------------------|--------------------------------------|
//! | `is_nil`             | `[]` only                | a cons cell and every non-list       |
//! | `is_list`            | `[]` **and** cons cells  | every non-list                       |
//! | `is_nonempty_list`   | cons cells only          | `[]` **and** every non-list          |
//!
//! `NIL` is an *immediate* (`_TAG_IMMED2_NIL`), so `is_list(NIL)` is `false`:
//! the "non-empty list" test is the plain cons-cell test and carries **no** extra
//! nil check, while `is_list` carries an explicit `is_not_nil` escape.
//! `beam_term::list` mirrors this bit-for-bit (`is_not_nil_list` is documented as
//! identical to `is_list`, `is_nil` as `x == NIL`), and `src/interpreter.rs`'s
//! `TestKind::{IsNil,IsList,IsNonemptyList}` arms are written directly against it:
//!
//! ```ignore
//! TestKind::IsNil          => is_nil(v),
//! TestKind::IsList         => is_nil(v) || is_not_nil_list(v),
//! TestKind::IsNonemptyList => is_not_nil_list(v),
//! ```
//!
//! ## The case that was reported
//!
//! `edlin_context:get_context/1` (`lib/stdlib/src/edlin_context.erl:78`) parses
//! its argument through `edlin_expand:over_word/1`, whose first step is
//! `edlin_expand:over_white/3` (`:1290`):
//!
//! ```erlang
//! over_white([$\s|Cs], Stack, N) -> over_white(Cs, [$\s|Stack], N+1);
//! over_white([$\t|Cs], Stack, N) -> over_white(Cs, [$\t|Stack], N+1);
//! over_white(Cs, Stack, N) when is_list(Cs) -> {Cs, Stack, N}.
//! ```
//!
//! and the compiler emits exactly the predicate chain under test
//! (`beam_disasm:file/1`, label 471/474):
//!
//! ```text
//! {label,471},
//! {test,is_nonempty_list,{f,474},[{x,0}]},          % clause 1/2: [$\s|Cs] / [$\t|Cs] need a cons
//! {get_list,{x,0},{x,3},{x,4}},
//! {select_val,{x,3},{f,474},{list,[{integer,9},{f,473},{integer,32},{f,472}]}},
//! ...
//! {label,474},
//! {test,is_list,{f,470},[{x,0}]},                   % clause 3's `when is_list(Cs)` guard
//! ...
//! {label,470},{line,[{location,"edlin_expand.erl",1292}]},
//! {func_info,{atom,edlin_expand},{atom,over_white},3},   % f470 == the clause-failure raise
//! ```
//!
//! Running the engine with a temporary `eprintln!` trace on every
//! `is_nil`/`is_list`/`is_nonempty_list` test and on `func_info` (removed again
//! before this commit — it is a diagnostic, not a deliverable) shows, for the
//! oracle-raising inputs `genesis_poison`, `7`, `{}`, `<<1,2>>`, `#{}` and `3.5`:
//!
//! ```text
//! TRACETEST over_white/76 pc=4  IsNonemptyList args=[X(0)] => false
//! TRACETEST over_white/76 pc=22 IsList         args=[X(0)] => false
//! TRACEFUNCINFO edlin_expand:over_white /3
//! ```
//!
//! i.e. the engine **fails** `is_nonempty_list`, **fails** the `is_list` guard and
//! jumps to `over_white/3`'s `func_info` — which is *precisely* the oracle's path,
//! whose stacktrace item is
//! `{edlin_expand,over_white,[genesis_poison,[],0],[{file,"edlin_expand.erl"},{line,1292}]}`
//! — line 1292 being that same `func_info`'s line record. No clause is run past.
//!
//! For `[]` the trace shows the converse, matching the C:
//!
//! ```text
//! TRACETEST over_white/76 pc=4  IsNonemptyList args=[X(0)] => false
//! TRACETEST over_white/76 pc=22 IsList         args=[X(0)] => true     % clause 3 matches
//! ```
//!
//! # What this file pins
//!
//! Every expectation below is the **real** system Erlang's (OTP 29 / erts-17.1)
//! and the producing `erl -noshell -eval …` command is quoted beside it, so any
//! line can be re-verified on its own. Nothing here was derived from this engine's
//! own output, and a value where the oracle raises is a failure, never a pass. The
//! tests skip **loudly** when the OTP install is unavailable.
//!
//! 1. `edlin_expand:over_word/1` and `edlin_context:get_context/1` reject every
//!    non-list poison with the oracle's `error:function_clause` (the `over_white/3`
//!    `func_info` raise) — the reported case.
//! 2. `is_list` accepts `[]` while `is_nonempty_list` rejects it: `over_word([])`
//!    returns the clause-3 result, and `lists:reverse([])`/`lists:last([])`
//!    disagree on `[]` exactly as the two C macros do.
//! 3. `lists:reverse/1`'s clause chain over the full term-shape matrix agrees
//!    (values and `function_clause` alike) — the same predicate pair on a module
//!    with no host-BIF dependency at all.
mod support;

use beam_common::{BeamError, ExceptionClass};
use beam_engine::terms;
use beam_term::{cmp, make_atom, make_small, Eterm, NIL};
use support::{app_ebin, skip_reason, stdlib_ebin, term_list, term_tuple, ModuleHost};

/// The multi-module host over the installed `stdlib` (+ `kernel` as a search
/// directory), or `None` when the OTP install cannot be located.
fn oracle_host() -> Option<ModuleHost> {
    let ebin = stdlib_ebin()?;
    let mut host = ModuleHost::new(ebin);
    if let Some(kernel) = app_ebin("kernel") {
        host.add_search_dir(kernel);
    }
    Some(host)
}

/// True when `result` is the raise the C's `func_info` produces: class `error`
/// with the bare atom reason `function_clause`
/// (`erts/emulator/beam/emu/beam_emu.c:578-583`: `c_p->freason =
/// EXC_FUNCTION_CLAUSE; goto handle_error`).
///
/// The engine delivers that reason in one of the two shapes its raise machinery
/// produces for the *same* C behaviour — the `ExitReason::Atom` spelling when
/// [`beam_common::ExitReason::as_atom`] can name it, and otherwise the equivalent
/// live reason *term* in [`beam_common::Raise::live_reason`] (the one every
/// instruction-family raise goes through, `Machine::throw_with_trace`). Both are
/// the oracle's `error:function_clause`; anything else is not.
fn is_function_clause(host: &ModuleHost, result: &Result<Eterm, BeamError>) -> bool {
    let Err(BeamError::Raise(raise)) = result else {
        return false;
    };
    if raise.class != ExceptionClass::Error {
        return false;
    }
    if raise.reason.as_atom() == Some("function_clause") {
        return true;
    }
    let Some(reason) = raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
    else {
        return false;
    };
    let Some(atom) = host.atom_table().lookup("function_clause") else {
        return false;
    };
    cmp(reason, make_atom(atom), host.atom_table()).is_eq()
}

/// Asserts `result` is the oracle's `error:function_clause` for `label`.
fn assert_function_clause(host: &ModuleHost, label: &str, result: Result<Eterm, BeamError>) {
    assert!(
        is_function_clause(host, &result),
        "{label}: want the oracle's `error:function_clause`, got {result:?}"
    );
}

/// Asserts `got` is the `{[], []}` 2-tuple `edlin_expand:over_word([])` returns.
fn assert_empty_word_pair(host: &mut ModuleHost, label: &str, got: Eterm) {
    let want = term_tuple(host, &[NIL, NIL]);
    assert!(
        cmp(got, want, host.atom_table()).is_eq(),
        "{label}: want {{[],[]}}, got {got:?}"
    );
}

/// The non-list term shapes the C's `is_list`/`is_nonempty_list` must all reject:
/// `(label, poison)` with the exact oracle command quoted at the call site.
fn non_list_poisons(host: &mut ModuleHost) -> Vec<(&'static str, Eterm)> {
    let atom = host.intern("genesis_poison");
    let int = make_small(7);
    let tuple0 = term_tuple(host, &[]);
    let binary = terms::make_heap_binary(host, &[1, 2]).expect("build <<1,2>>");
    let map = terms::make_flatmap(host, &[]).expect("build #{}");
    let float = terms::make_float(host, 3.5).expect("build 3.5");
    vec![
        ("genesis_poison", atom),
        ("7", int),
        ("{}", tuple0),
        ("<<1,2>>", binary),
        ("#{}", map),
        ("3.5", float),
    ]
}

// ---------------------------------------------------------------------------
// 1. The reported case: every non-list poison must fail `over_white/3`'s clause 3
// ---------------------------------------------------------------------------

/// `edlin_context:get_context/1` on a non-list argument raises the oracle's
/// `function_clause` — from `edlin_expand:over_white/3`'s `func_info`, i.e. the
/// engine must reach it by failing the `is_nonempty_list` **and** `is_list` tests,
/// never by running past them.
#[test]
fn edlin_get_context_rejects_every_non_list_poison_like_the_oracle() {
    let Some(mut host) = oracle_host() else {
        println!("{}", skip_reason().unwrap_or_default());
        return;
    };
    for (label, poison) in non_list_poisons(&mut host) {
        // oracle: erl -noshell -eval 'edlin_context:get_context(<poison>)'  =>
        //   error:function_clause
        //   {edlin_expand,over_white,[<poison>,[],0],[{file,"edlin_expand.erl"},{line,1292}]}
        let result = host.call("edlin_context", "get_context", &[poison]);
        assert_function_clause(&host, &format!("edlin_context:get_context({label})"), result);
    }
}

/// The same predicate chain, entered one level lower: `edlin_expand:over_word/1`
/// (the first call `get_context/1` makes) must reject the same poisons with the
/// same reason — so the agreement above is clause dispatch, not an artefact of the
/// caller.
#[test]
fn edlin_over_word_rejects_every_non_list_poison_like_the_oracle() {
    let Some(mut host) = oracle_host() else {
        println!("{}", skip_reason().unwrap_or_default());
        return;
    };
    for (label, poison) in non_list_poisons(&mut host) {
        // oracle: erl -noshell -eval 'edlin_expand:over_word(<poison>)'  =>
        //   error:function_clause
        //   {edlin_expand,over_white,[<poison>,[],0],[{file,"edlin_expand.erl"},{line,1292}]}
        let result = host.call("edlin_expand", "over_word", &[poison]);
        assert_function_clause(&host, &format!("edlin_expand:over_word({label})"), result);
    }
}

// ---------------------------------------------------------------------------
// 2. `[]` split: `is_list([])` succeeds where `is_nonempty_list([])` fails
// ---------------------------------------------------------------------------

/// `[]` is the one value the two macros disagree on, so it is the sharpest
/// discriminator between them. All three expectations below are the oracle's.
#[test]
fn nil_is_a_list_for_is_list_but_not_a_nonempty_list() {
    let Some(mut host) = oracle_host() else {
        println!("{}", skip_reason().unwrap_or_default());
        return;
    };

    // `over_white([], [], 0)` must take clause 3 (not clause 1/2): the engine
    // reaches it only by FAILING `is_nonempty_list` on `[]` and then PASSING
    // `is_list` on it. A wrong `is_nonempty_list` would `get_list` a `NIL` and the
    // engine's lists family would raise `{badmatch,V}` instead; a wrong `is_list`
    // would raise `function_clause`.
    // oracle: erl -noshell -eval 'io:format("~p~n",[edlin_expand:over_word([])]),halt().'
    //   => {[],[]}
    let got = host
        .call("edlin_expand", "over_word", &[NIL])
        .expect("over_word([]) is a value, not a raise");
    assert_empty_word_pair(&mut host, "edlin_expand:over_word([])", got);

    // A cons argument DOES pass `is_nonempty_list`, so clause 1/2 apply first.
    // oracle: erl -noshell -eval 'io:format("~p~n",[edlin_expand:over_word([1,2])]),halt().'
    //   => {[1,2],[]}
    let list = term_list(&mut host, &[make_small(1), make_small(2)]);
    let got = host
        .call("edlin_expand", "over_word", &[list])
        .expect("over_word([1,2]) is a value, not a raise");
    let want = term_tuple(&mut host, &[list, NIL]);
    assert!(
        cmp(got, want, host.atom_table()).is_eq(),
        "edlin_expand:over_word([1,2]): want {{[1,2],[]}}, got {got:?}"
    );
}

/// The same `[]` split on `lists` — a module with no host-BIF dependency: its
/// clause chain is `is_nonempty_list` (`reverse/1`) plus `is_nil` (the empty-list
/// clause), and both must land exactly as the oracle does.
#[test]
fn lists_distinguishes_nil_from_a_cons_the_way_the_oracle_does() {
    let Some(mut host) = oracle_host() else {
        println!("{}", skip_reason().unwrap_or_default());
        return;
    };

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:reverse([])]),halt().'   => []
    let got = host
        .call("lists", "reverse", &[NIL])
        .expect("lists:reverse([]) is a value");
    assert!(
        cmp(got, NIL, host.atom_table()).is_eq(),
        "lists:reverse([]): want [], got {got:?}"
    );

    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:last([])]),halt().'
    //   => error:function_clause at lists.erl:467 (neither `last([E])` nor
    //      `last([_|T])` matches `[]`: `is_nonempty_list` must fail on `[]`)
    let result = host.call("lists", "last", &[NIL]);
    assert_function_clause(&host, "lists:last([])", result);
}

// ---------------------------------------------------------------------------
// 3. The predicate pair over the full term-shape matrix, on `lists`
// ---------------------------------------------------------------------------

/// `lists:reverse/1` over every shape the predicates have to classify. The C
/// branch order is `is_nonempty_list` → (cons: `get_list`/`is_nonempty_list`/
/// `is_nil`) → `is_nil` → `func_info`, so a single wrong predicate arm shows up as
/// the wrong outcome here (a value where the oracle raises, or a `{badmatch,V}`
/// instead of `function_clause`).
#[test]
fn lists_reverse_clause_dispatch_matches_the_oracle_over_every_term_shape() {
    let Some(mut host) = oracle_host() else {
        println!("{}", skip_reason().unwrap_or_default());
        return;
    };

    // `[]` → `[]` (is_nonempty_list fails on NIL, is_nil passes).
    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:reverse([])]),halt().' => []
    let got = host.call("lists", "reverse", &[NIL]).expect("reverse([])");
    assert!(cmp(got, NIL, host.atom_table()).is_eq(), "reverse([])");

    // `[1,2,3]` → `[3,2,1]` (is_nonempty_list passes twice, then is_nil).
    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:reverse([1,2,3])]),halt().'
    //   => [3,2,1]
    let list = term_list(&mut host, &[make_small(1), make_small(2), make_small(3)]);
    let got = host.call("lists", "reverse", &[list]).expect("reverse([1,2,3])");
    let want = term_list(&mut host, &[make_small(3), make_small(2), make_small(1)]);
    assert!(
        cmp(got, want, host.atom_table()).is_eq(),
        "lists:reverse([1,2,3]): want [3,2,1], got {got:?}"
    );

    // `[1|9]` — a cons whose tail is not a list: the first cons test passes, the
    // recursive `reverse/2` then fails its own `is_nonempty_list` on `9`.
    // oracle: erl -noshell -eval 'lists:reverse([1|9])' =>
    //   error:function_clause at lists.erl:292 (reverse/1's `func_info`)
    let improper = terms::make_cons(&mut host, make_small(1), make_small(9)).expect("build [1|9]");
    let result = host.call("lists", "reverse", &[improper]);
    assert_function_clause(&host, "lists:reverse([1|9])", result);

    // Every non-list: `is_nonempty_list` and `is_nil` must both fail, so the
    // `func_info` raise is reached — never a `{badmatch,V}` from `get_list`.
    // oracle: erl -noshell -eval 'lists:reverse(<poison>)'  =>
    //   error:function_clause at lists.erl:292
    let poisons = non_list_poisons(&mut host);
    for (label, poison) in poisons {
        let result = host.call("lists", "reverse", &[poison]);
        assert_function_clause(&host, &format!("lists:reverse({label})"), result);
    }

    // The same matrix through `lists:sum/1`, whose clauses live in `sum/2`
    // (`sum([], Sum)` / `sum([H|T], Sum)`): `[]` is a value, every non-list is the
    // `func_info` raise.
    // oracle: erl -noshell -eval 'io:format("~p~n",[lists:sum([])]),halt().'  => 0
    let got = host.call("lists", "sum", &[NIL]).expect("sum([])");
    assert!(
        cmp(got, make_small(0), host.atom_table()).is_eq(),
        "lists:sum([]): want 0, got {got:?}"
    );
    // oracle: erl -noshell -eval 'lists:sum(genesis_poison)' =>
    //   error:function_clause at lists.erl:572
    let poisons = non_list_poisons(&mut host);
    for (label, poison) in poisons {
        let result = host.call("lists", "sum", &[poison]);
        assert_function_clause(&host, &format!("lists:sum({label})"), result);
    }
    // oracle: erl -noshell -eval 'lists:sum([1|9])' =>
    //   error:function_clause at lists.erl:572
    let improper = terms::make_cons(&mut host, make_small(1), make_small(9)).expect("build [1|9]");
    let result = host.call("lists", "sum", &[improper]);
    assert_function_clause(&host, "lists:sum([1|9])", result);
}

// ---------------------------------------------------------------------------
// 4. The two list-shaped inputs the TEST HOST cannot run (a loud, explicit gap)
// ---------------------------------------------------------------------------

/// `edlin_context:get_context([])` and `get_context([1,2])` return *values* under
/// the oracle but a typed error here, because the code path reaches
/// `unicode:characters_to_nfc_list/1` → `unicode_util:nfc/1` → `re:run/3`, whose
/// implementation is `re`'s NIF: the test host loads no NIFs, so the stub's
/// `erlang:nif_error(undef)` is what runs. That is a **host capability gap, not a
/// clause-dispatch difference** — the clause dispatch for these inputs is the path
/// asserted above — and it is reported loudly here (exactly the
/// `blocked_mechanisms_report_their_typed_error` convention in `tests/stdlib_sweep.rs`)
/// rather than silently skipped.
#[test]
fn the_list_shaped_edlin_inputs_are_a_documented_host_gap() {
    let Some(mut host) = oracle_host() else {
        println!("{}", skip_reason().unwrap_or_default());
        return;
    };
    let list = term_list(&mut host, &[make_small(1), make_small(2)]);
    for (label, arg, oracle) in [
        ("edlin_context:get_context([])", NIL, "{term,[],[]}"),
        ("edlin_context:get_context([1,2])", list, "{term}"),
    ] {
        // oracle: erl -noshell -eval 'io:format("~p~n",[<call>]),halt().'  => the
        // value below (the `re` NIF is loaded there).
        let result = host.call("edlin_context", "get_context", &[arg]);
        match result {
            Ok(value) => panic!(
                "{label}: the host must not fabricate the oracle's {oracle}; got value {value:?}"
            ),
            Err(error) => println!(
                "SKIP: {label} — the oracle returns {oracle}, this host returns the typed \
                 {error:?}: the path needs `re:run/3`'s NIF, which the test host does not load"
            ),
        }
    }
}
