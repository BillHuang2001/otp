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

//! Integration tests for the match-spec **`self/0` seam** this crate installs over
//! `beam_ets` (`src/ets/self_hook.rs`) and for the **empty-spec** rule of
//! `ets:match_spec_compile/1`.
//!
//! `beam_ets` resolves a spec's `self/0` through a thread-local `SelfHook`; with
//! none installed its default answers the typed `badarg`, so a `self/0` **guard**
//! silently skips its clause and a `self/0` **body** silently becomes the `EXIT`
//! sentinel — a wrong result, not a typed gap. Every match-spec-evaluating BIF here
//! now opens `self_hook::SpecEval` (`src/ets/self_hook.rs`), which installs the
//! calling process's pid for the evaluation and restores the outer scope (or the
//! seam's default) when it drops.
//!
//! Everything is driven through the **public** API — `BifRegistry::call` over a real
//! `TableRegistry` — and every expectation carries the probe that pinned it against
//! the system `erl` (OTP 29.1.1 / erts-17.1); the last test replays the whole corpus
//! under that oracle (skipping **loudly** when no `erl` is on `PATH`).
//!
//! The spec terms use `{self}` (a 1-tuple holding the atom `self`) — the match-spec
//! spelling of a `self/0` call, which is what the oracle's own `db_match_compile`
//! decodes. Writing `self()` in Erlang *source* would instead be evaluated by
//! `erl_eval` while the argument list is built, so it would not exercise the seam.

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{make_atom, AtomTable, Eterm, NIL};

/// The atom names the `ets`/spec BIFs and the assertions below need.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "error",
        "set",
        "ordered_set",
        "$end_of_table",
        "table",
        "self",
        "=:=",
        "EXIT",
        "$1",
        "_",
        "a",
        "b",
        "tab",
        "no_such_fun",
    ]);
    atoms
}

/// A heap sized for the tables, specs and results below (it never relocates, so a
/// term built by one context stays valid for the later ones in a test).
fn test_heap() -> Heap {
    Heap::new(65536).expect("test heap")
}

/// `ets:new(Name, [Kind])` through the registry, returning the handle.
fn new_table(reg: &BifRegistry, ctx: &mut BifContext<'_>, name: &str, kind: &str) -> Eterm {
    let name = ctx.atom(name).expect("seeded");
    let kind = ctx.atom(kind).expect("seeded");
    let options = ctx.list(&[kind]).expect("fits");
    reg.call("ets", "new", 2, ctx, &[name, options])
        .expect("ets:new/2")
}

/// The 2-tuple `{Key, Name}` built on `ctx`.
fn object(ctx: &mut BifContext<'_>, key: &str, name: &str) -> Eterm {
    let k = ctx.atom(key).expect("seeded");
    let v = ctx.atom(name).expect("seeded");
    ctx.tuple(&[k, v]).expect("fits")
}

/// The full table `[{a,b},{b,a}]` — two objects, so every `ets:select/2` below
/// selects twice.
fn two_object_table(reg: &BifRegistry, ctx: &mut BifContext<'_>) -> Eterm {
    let handle = new_table(reg, ctx, "tab", "set");
    for (key, name) in [("a", "b"), ("b", "a")] {
        let obj = object(ctx, key, name);
        reg.call("ets", "insert", 2, ctx, &[handle, obj])
            .expect("ets:insert/2");
    }
    handle
}

/// `{self}` — the match-spec spelling of a `self/0` call.
fn self_call(ctx: &mut BifContext<'_>) -> Eterm {
    let s = ctx.atom("self").expect("seeded");
    ctx.tuple(&[s]).expect("fits")
}

/// `[{Head, Guards, Body}]` from the three element lists.
fn one_clause(ctx: &mut BifContext<'_>, head: Eterm, guards: &[Eterm], body: &[Eterm]) -> Eterm {
    let guards = ctx.list(guards).expect("fits");
    let body = ctx.list(body).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

/// `[{'$1', [], [{self}]}]` — every object selected, each yielding the calling pid.
fn self_body_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let v1 = ctx.atom("$1").expect("seeded");
    let body = self_call(ctx);
    one_clause(ctx, v1, &[], &[body])
}

/// `[{'_', [], [{self}]}]` — the same body over the wildcard head (the
/// `ets:match_spec_compile/1` and `erlang:match_spec_test/3` spelling).
fn wildcard_self_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let wild = ctx.atom("_").expect("seeded");
    let body = self_call(ctx);
    one_clause(ctx, wild, &[], &[body])
}

/// `[{'$1', [{'=:=',{self},{self}}], [ok]}]` — a **guard** that holds only when
/// `self/0` resolves, so a missing hook skips the clause instead of selecting it.
fn self_guard_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let v1 = ctx.atom("$1").expect("seeded");
    let left = self_call(ctx);
    let right = self_call(ctx);
    let eq = ctx.atom("=:=").expect("seeded");
    let guard = ctx.tuple(&[eq, left, right]).expect("fits");
    let ok = ctx.atom("ok").expect("seeded");
    one_clause(ctx, v1, &[guard], &[ok])
}

/// The list `[1, 2]`.
fn one_and_two(ctx: &mut BifContext<'_>) -> Eterm {
    let one = ctx.int(1).expect("fits");
    let two = ctx.int(2).expect("fits");
    ctx.list(&[one, two]).expect("fits")
}

/// The list `[Pid, Pid]` — what a `self/0` body yields per selected object.
fn twice(ctx: &mut BifContext<'_>, term: Eterm) -> Eterm {
    ctx.list(&[term, term]).expect("fits")
}

/// `erlang:length/1` of the list `term`, through the registry.
fn length(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> i64 {
    let len = reg
        .call("erlang", "length", 1, ctx, &[term])
        .expect("erlang:length/1");
    beam_term::signed_value(len)
}

/// `lists:nth/2` — the `N`th (1-based) element of the proper list `term`.
fn nth(reg: &BifRegistry, ctx: &mut BifContext<'_>, pos: i64, term: Eterm) -> Eterm {
    let pos = ctx.int(pos).expect("fits");
    reg.call("lists", "nth", 2, ctx, &[pos, term])
        .expect("lists:nth/2")
}

/// `exact(left, right)` — structural term equality.
fn exact(atoms: &AtomTable, left: Eterm, right: Eterm) -> bool {
    beam_term::compare::eq(left, right, atoms)
}

/// Runs `spec` against `list` straight through `beam_ets`'s executor, with **no**
/// guard of this crate's own — so the only `self/0` hook in force is one a caller
/// left installed.
fn direct_run(heap: &mut Heap, atoms: &AtomTable, list: Eterm, spec: Eterm) -> Vec<Eterm> {
    beam_ets::match_spec_run(list, spec, atoms, heap).expect("beam_ets::match_spec_run")
}

// ---------------------------------------------------------------------------
// The `self/0` hook on every spec-running entry point
// ---------------------------------------------------------------------------

/// `ets:select(T, [{'$1',[],[{self}]}])` ⇒ `[Pid, Pid]`.
///
/// Oracle: `[<0.10.0>,<0.10.0>]` for a 2-object table (probed on OTP 29.1.1).
#[test]
fn ets_select_2_answers_the_calling_pid_for_every_selected_object() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let pid = ProcessId::new(3, 0);
    let mut ctx = BifContext::new(&mut heap, &atoms, pid).with_tables(&mut tables);

    let handle = two_object_table(&reg, &mut ctx);
    let spec = self_body_spec(&mut ctx);
    let got = reg
        .call("ets", "select", 2, &mut ctx, &[handle, spec])
        .expect("ets:select/2");

    assert_eq!(length(&reg, &mut ctx, got), 2, "one result per selected object");
    let pid_term = pid.to_eterm();
    for pos in [1, 2] {
        let element = nth(&reg, &mut ctx, pos, got);
        assert!(
            exact(&atoms, element, pid_term),
            "element {pos} must be the calling pid, not the EXIT sentinel"
        );
    }
}

/// `ets:select(T, [{'$1',[{'=:=',{self},{self}}],[ok]}])` ⇒ `[ok, ok]`.
///
/// Oracle: `[ok,ok]` (probed). Without the hook the guard raises, so the clause
/// would be **skipped** and the result would be `[]` — the silently wrong result
/// this pins against.
#[test]
fn a_self_guard_selects_its_clause_instead_of_skipping_it() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(3, 0)).with_tables(&mut tables);

    let handle = two_object_table(&reg, &mut ctx);
    let spec = self_guard_spec(&mut ctx);
    let got = reg
        .call("ets", "select", 2, &mut ctx, &[handle, spec])
        .expect("ets:select/2");

    assert_eq!(length(&reg, &mut ctx, got), 2, "the self/0 guard must hold");
    let ok = ctx.atom("ok").expect("seeded");
    let expected = twice(&mut ctx, ok);
    assert!(
        exact(&atoms, got, expected),
        "the guard body must be selected, not skipped"
    );
}

/// `ets:match_spec_run([1,2], ets:match_spec_compile([{'_',[],[{self}]}]))` ⇒
/// `[Pid, Pid]`.
///
/// Oracle: `[<0.10.0>,<0.10.0>]` (probed). This is the compiled-spec route, distinct
/// from the select family.
#[test]
fn ets_match_spec_run_2_answers_the_pid_per_matching_element() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let pid = ProcessId::new(3, 0);
    let mut ctx = BifContext::new(&mut heap, &atoms, pid).with_tables(&mut tables);

    let spec = wildcard_self_spec(&mut ctx);
    let compiled = reg
        .call("ets", "match_spec_compile", 1, &mut ctx, &[spec])
        .expect("ets:match_spec_compile/1");
    let list = one_and_two(&mut ctx);
    let got = reg
        .call("ets", "match_spec_run", 2, &mut ctx, &[list, compiled])
        .expect("ets:match_spec_run/2");

    let expected = twice(&mut ctx, pid.to_eterm());
    assert!(
        exact(&atoms, got, expected),
        "ets:match_spec_run/2 must answer the pid per element"
    );
}

/// `erlang:match_spec_test(1, [{'_',[],[{self}]}], table)` ⇒ `{ok, Pid, [], []}`.
///
/// Oracle: `{ok,<0.10.0>,[],[]}` (probed).
#[test]
fn erlang_match_spec_test_3_reports_the_pid_for_a_self_body() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(3, 0));

    let spec = wildcard_self_spec(&mut ctx);
    let against = ctx.int(1).expect("fits");
    let dialect = ctx.atom("table").expect("seeded");
    let got = reg
        .call(
            "erlang",
            "match_spec_test",
            3,
            &mut ctx,
            &[against, spec, dialect],
        )
        .expect("erlang:match_spec_test/3");

    let ok = ctx.atom("ok").expect("seeded");
    let expected = ctx
        .tuple(&[ok, ProcessId::new(3, 0).to_eterm(), NIL, NIL])
        .expect("fits");
    assert!(
        exact(&atoms, got, expected),
        "erlang:match_spec_test/3 must report the pid, not the EXIT sentinel"
    );
}

// ---------------------------------------------------------------------------
// Nesting and leak-freedom
// ---------------------------------------------------------------------------

/// A `FunCaller` whose "fun" is a nested `ets:select/2` with a `self/0` body: the
/// shape `ets:foldl/3`'s fun takes when it inspects a table of its own. It runs the
/// nested BIF on its **own** heap and its **own** calling pid, so the result proves
/// the nested install used the inner context's identity.
struct NestedSelect<'a> {
    reg: &'a BifRegistry,
    atoms: &'a AtomTable,
    heap: Heap,
    pid: ProcessId,
    handle: Eterm,
    spec: Eterm,
}

impl NestedSelect<'_> {
    /// `ets:select(Handle, Spec)` on a fresh context, with `tables` installed when
    /// the outer BIF lent its registry.
    fn nested_select(
        &mut self,
        tables: Option<&mut TableRegistry>,
    ) -> beam_common::Result<Eterm> {
        let ctx = BifContext::new(&mut self.heap, self.atoms, self.pid);
        let mut ctx = match tables {
            Some(tables) => ctx.with_tables(tables),
            None => ctx,
        };
        self.reg
            .call("ets", "select", 2, &mut ctx, &[self.handle, self.spec])
    }
}

impl FunCaller for NestedSelect<'_> {
    fn call_fun(&mut self, _fun: Eterm, _args: &[Eterm]) -> beam_common::Result<Eterm> {
        // `ets:foldl/3` reaches the caller through `call_fun_with_tables` (it always
        // lends its registry), so this arm is only a stand-in for the trait's
        // required method.
        self.nested_select(None)
    }

    fn call_fun_with_tables(
        &mut self,
        _fun: Eterm,
        _args: &[Eterm],
        tables: Option<&mut TableRegistry>,
    ) -> beam_common::Result<Eterm> {
        self.nested_select(tables)
    }
}

/// `ets:foldl(fun(_,_) -> ets:select(T,[{'$1',[],[{self}]}]) end, [], T)`.
///
/// Oracle: `[[<0.10.0>,<0.10.0>]]` — the fun's own `ets:select/2` sees *its* calling
/// process's pid. Here the nested call runs under a **different** pid than the outer
/// BIF, so a stale or inherited hook would answer the outer pid and fail this test.
#[test]
fn a_nested_ets_select_through_the_fun_caller_sees_its_own_calling_pid() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let outer = ProcessId::new(3, 0);
    let inner = ProcessId::new(4, 0);

    // Phase 1: create the table and build the nested call's arguments.
    let (handle, spec, dummy_fun, acc0) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, outer).with_tables(&mut tables);
        let handle = two_object_table(&reg, &mut ctx);
        let spec = self_body_spec(&mut ctx);
        let dummy_fun = ctx.atom("no_such_fun").expect("seeded");
        let acc0 = ctx.list(&[]).expect("fits");
        (handle, spec, dummy_fun, acc0)
    };

    // Phase 2: `ets:foldl/3` with the nested-select caller. Two objects ⇒ two calls,
    // so the accumulator is the last call's `[Pid, Pid]`.
    let mut caller = NestedSelect {
        reg: &reg,
        atoms: &atoms,
        heap: Heap::new(65536).expect("nested test heap"),
        pid: inner,
        handle,
        spec,
    };
    let got = {
        let mut ctx = BifContext::new(&mut heap, &atoms, outer)
            .with_tables(&mut tables)
            .with_fun_caller(&mut caller);
        reg.call("ets", "foldl", 3, &mut ctx, &[dummy_fun, acc0, handle])
            .expect("ets:foldl/3")
    };

    let expected = {
        let mut ctx = BifContext::new(&mut heap, &atoms, outer);
        twice(&mut ctx, inner.to_eterm())
    };
    assert!(
        exact(&atoms, got, expected),
        "the nested ets:select/2 must answer the *inner* calling pid"
    );
}

/// After a spec-running BIF returns, the `self/0` seam is back to its default: a
/// direct `beam_ets::match_spec_run` with a `self/0` body answers the typed gap (the
/// `EXIT` sentinel), never a stale pid.
#[test]
fn the_self_seam_does_not_leak_after_a_spec_bif_returns() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let pid = ProcessId::new(3, 0);

    // Build the table, the `self/0` spec and a runnable list in one context.
    let (handle, spec, list) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, pid).with_tables(&mut tables);
        let handle = two_object_table(&reg, &mut ctx);
        let spec = self_body_spec(&mut ctx);
        let list = one_and_two(&mut ctx);
        (handle, spec, list)
    };

    // A real spec-running BIF: it installs the hook for the call only.
    let selected = {
        let mut ctx = BifContext::new(&mut heap, &atoms, pid).with_tables(&mut tables);
        let got = reg
            .call("ets", "select", 2, &mut ctx, &[handle, spec])
            .expect("ets:select/2");
        length(&reg, &mut ctx, got)
    };
    assert_eq!(selected, 2, "the select must have selected both objects");

    // The BIF has returned, so no hook is installed: the `self/0` body degrades to
    // the `EXIT` sentinel (the documented typed gap), never a stale pid.
    let exit_atom = make_atom(atoms.lookup("EXIT").expect("seeded"));
    assert_eq!(
        direct_run(&mut heap, &atoms, list, spec),
        vec![exit_atom, exit_atom],
        "the seam must be back to its default once the BIF returned"
    );
}

// ---------------------------------------------------------------------------
// The empty-spec rule
// ---------------------------------------------------------------------------

/// `ets:match_spec_compile([])` ⇒ `error:badarg`, while the select family keeps
/// accepting `[]` (`ets:select(T,[])` ⇒ `[]`, the counts ⇒ `0`, the reverse ⇒ `[]`),
/// and `erlang:match_spec_test(1,[],table)` stays the linter's `{error,…}`, not
/// `badarg`.
///
/// All six oracle answers were probed on OTP 29.1.1 / erts-17.1.
#[test]
fn match_spec_compile_rejects_the_empty_list_but_the_select_family_accepts_it() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(3, 0)).with_tables(&mut tables);

    let handle = two_object_table(&reg, &mut ctx);
    let empty = ctx.list(&[]).expect("fits");

    // `ets:match_spec_compile([])` is `badarg` — unlike every select-family verb.
    let err = reg
        .call("ets", "match_spec_compile", 1, &mut ctx, &[empty])
        .expect_err("ets:match_spec_compile([]) must raise");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");

    // The select family genuinely accepts the empty clause list.
    let selected = reg
        .call("ets", "select", 2, &mut ctx, &[handle, empty])
        .expect("ets:select(T,[])");
    assert_eq!(length(&reg, &mut ctx, selected), 0);
    for verb in ["select_count", "select_delete", "select_replace"] {
        let got = reg
            .call("ets", verb, 2, &mut ctx, &[handle, empty])
            .expect("count/delete/replace");
        assert_eq!(beam_term::signed_value(got), 0, "ets:{verb}/2 on []");
    }
    let reversed = reg
        .call("ets", "select_reverse", 2, &mut ctx, &[handle, empty])
        .expect("ets:select_reverse(T,[])");
    assert_eq!(length(&reg, &mut ctx, reversed), 0);

    // `ets:match_spec_run/2` takes only a *compiled* handle, so `[]` is `badarg`.
    let list = one_and_two(&mut ctx);
    let err = reg
        .call("ets", "match_spec_run", 2, &mut ctx, &[list, empty])
        .expect_err("ets:match_spec_run([1,2],[]) must raise");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");

    // `erlang:match_spec_test/3` lints the empty list — a result, not a raise.
    let against = ctx.int(1).expect("fits");
    let dialect = ctx.atom("table").expect("seeded");
    let got = reg
        .call(
            "erlang",
            "match_spec_test",
            3,
            &mut ctx,
            &[against, empty, dialect],
        )
        .expect("erlang:match_spec_test(1,[],table)");
    let expected = lint_not_a_list(&mut ctx);
    assert!(
        exact(&atoms, got, expected),
        "the empty-spec lint result must be `{{error,[{{error,\"Match programs are not in a list.\"}}]}}`"
    );
}

/// `{error, [{error, "Match programs are not in a list."}]}` — the C
/// `db_match_set_lint` diagnostic for a non-list match program (the message is an
/// int list of latin-1 bytes).
fn lint_not_a_list(ctx: &mut BifContext<'_>) -> Eterm {
    let severity = ctx.intern_atom("error").expect("seeded");
    let message = "Match programs are not in a list.";
    let mut chars = Vec::with_capacity(message.len());
    for byte in message.bytes() {
        chars.push(ctx.int(i64::from(byte)).expect("fits"));
    }
    let text = ctx.list(&chars).expect("fits");
    let entry = ctx.tuple(&[severity, text]).expect("fits");
    let errors = ctx.list(&[entry]).expect("fits");
    let tag = ctx.intern_atom("error").expect("seeded");
    ctx.tuple(&[tag, errors]).expect("fits")
}

// ---------------------------------------------------------------------------
// The live differential cross-check
// ---------------------------------------------------------------------------

/// Runs one `-eval` statement under the system `erl`; `None` when the oracle is
/// unavailable (the test then skips loudly).
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-eval")
        .arg(statement)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .output();
    match output {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

/// Replays the whole `self/0` / empty-spec corpus under the system oracle and
/// asserts the pinned answers, then asserts this crate answers the very same
/// boolean properties.
///
/// The oracle statement reports one boolean per property (the pid itself is
/// VM-specific, so it is compared by identity with the oracle's own `self()`).
#[test]
fn the_live_oracle_agrees_on_the_self_and_empty_spec_corpus() {
    let eval = r#"
T = ets:new(t,[set]),
ets:insert(T,[{1,a},{2,b}]),
SelBody = ets:select(T,[{'$1',[],[{self}]}]),
SelGuard = ets:select(T,[{'$1',[{'=:=',{self},{self}}],[ok]}]),
MS = ets:match_spec_compile([{'_',[],[{self}]}]),
RunRes = ets:match_spec_run([1,2],MS),
TestRes = erlang:match_spec_test(1,[{'_',[],[{self}]}],table),
A = case SelBody of [P1,P1] -> is_pid(P1) andalso P1 =:= self(); _ -> false end,
B = SelGuard =:= [ok,ok],
C = case RunRes of [P2,P2] -> is_pid(P2) andalso P2 =:= self(); _ -> false end,
D = case TestRes of {ok,P3,[],[]} -> is_pid(P3) andalso P3 =:= self(); _ -> false end,
E = case catch ets:match_spec_compile([]) of {'EXIT',{badarg,_}} -> true; _ -> false end,
F = ets:select(T,[]) =:= [],
G = ets:select_count(T,[]) =:= 0,
H = ets:select_delete(T,[]) =:= 0,
I2 = ets:select_reverse(T,[]) =:= [],
J = case catch ets:match_spec_run([1,2],[]) of {'EXIT',{badarg,_}} -> true; _ -> false end,
K = erlang:match_spec_test(1,[],table) =:= {error,[{error,"Match programs are not in a list."}]},
io:format("~w~n",[{A,B,C,D,E,F,G,H,I2,J,K}]),
halt()"#;
    let Some(stdout) = oracle_eval(eval) else {
        eprintln!("SKIP: the self/0 / empty-spec corpus needs the system oracle");
        return;
    };
    assert_eq!(
        stdout.trim(),
        "{true,true,true,true,true,true,true,true,true,true,true}",
        "the live OTP 29.1.1 oracle disagrees with the pinned self/0 expectations"
    );

    // This crate's side of the same properties, through the public registry.
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let pid = ProcessId::new(3, 0);
    let mut ctx = BifContext::new(&mut heap, &atoms, pid).with_tables(&mut tables);

    let handle = two_object_table(&reg, &mut ctx);
    let body_spec = self_body_spec(&mut ctx);
    let guard_spec = self_guard_spec(&mut ctx);
    let wildcard_spec = wildcard_self_spec(&mut ctx);
    let pid_term = pid.to_eterm();

    // A: `ets:select/2` with a `self/0` body.
    let sel_body = reg
        .call("ets", "select", 2, &mut ctx, &[handle, body_spec])
        .expect("ets:select/2");
    assert!(
        exact(&atoms, sel_body, twice(&mut ctx, pid_term)),
        "A: ets:select/2 must answer the pid per object"
    );

    // B: the `self/0` guard selects.
    let ok = ctx.atom("ok").expect("seeded");
    let sel_guard = reg
        .call("ets", "select", 2, &mut ctx, &[handle, guard_spec])
        .expect("ets:select/2 guard");
    assert!(
        exact(&atoms, sel_guard, twice(&mut ctx, ok)),
        "B: the self/0 guard must hold"
    );

    // C: the compiled-spec route.
    let compiled = reg
        .call("ets", "match_spec_compile", 1, &mut ctx, &[wildcard_spec])
        .expect("ets:match_spec_compile/1");
    let list = one_and_two(&mut ctx);
    let run_res = reg
        .call("ets", "match_spec_run", 2, &mut ctx, &[list, compiled])
        .expect("ets:match_spec_run/2");
    assert!(
        exact(&atoms, run_res, twice(&mut ctx, pid_term)),
        "C: ets:match_spec_run/2 must answer the pid per element"
    );

    // D: `erlang:match_spec_test/3`.
    let against = ctx.int(1).expect("fits");
    let dialect = ctx.atom("table").expect("seeded");
    let test_res = reg
        .call(
            "erlang",
            "match_spec_test",
            3,
            &mut ctx,
            &[against, wildcard_spec, dialect],
        )
        .expect("erlang:match_spec_test/3");
    let ok_atom = ctx.atom("ok").expect("seeded");
    let expected_test = ctx.tuple(&[ok_atom, pid_term, NIL, NIL]).expect("fits");
    assert!(
        exact(&atoms, test_res, expected_test),
        "D: erlang:match_spec_test/3 must report the pid"
    );

    // E: the empty-spec rejection on the compiled-spec verb only.
    let empty = ctx.list(&[]).expect("fits");
    assert!(
        matches!(
            reg.call("ets", "match_spec_compile", 1, &mut ctx, &[empty]),
            Err(BeamError::BadArg)
        ),
        "E: ets:match_spec_compile([]) must be badarg"
    );

    // F–I2: the select family still accepts the empty clause list.
    let selected = reg
        .call("ets", "select", 2, &mut ctx, &[handle, empty])
        .expect("F: ets:select(T,[])");
    assert_eq!(length(&reg, &mut ctx, selected), 0, "F");
    for verb in ["select_count", "select_delete", "select_replace"] {
        let got = reg
            .call("ets", verb, 2, &mut ctx, &[handle, empty])
            .expect("G/H");
        assert_eq!(beam_term::signed_value(got), 0, "{verb} on []");
    }
    let reversed = reg
        .call("ets", "select_reverse", 2, &mut ctx, &[handle, empty])
        .expect("I2: ets:select_reverse(T,[])");
    assert_eq!(length(&reg, &mut ctx, reversed), 0, "I2");

    // J: `ets:match_spec_run/2` requires a compiled handle.
    assert!(
        matches!(
            reg.call("ets", "match_spec_run", 2, &mut ctx, &[list, empty]),
            Err(BeamError::BadArg)
        ),
        "J: ets:match_spec_run([1,2],[]) must be badarg"
    );

    // K: the empty-spec lint, not a raise.
    let lint = reg
        .call("erlang", "match_spec_test", 3, &mut ctx, &[against, empty, dialect])
        .expect("K: erlang:match_spec_test(1,[],table)");
    let expected_lint = lint_not_a_list(&mut ctx);
    assert!(
        exact(&atoms, lint, expected_lint),
        "K: the empty-spec lint result"
    );
}