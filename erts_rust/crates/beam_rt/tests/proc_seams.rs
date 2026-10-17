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

//! The `ProcessHook` host seam, end to end through the runtime glue.
//!
//! `beam_bifs`'s `proc_reg.rs` registers the eleven process/name/global-state BIFs
//! (`erlang:whereis/1`, `register/2`, `unregister/1`, `registered/0`, `is_process_alive/1`,
//! `processes/0`, `ports/0`, `process_info/1,2`, `process_flag/2`, `group_leader/0`) but the
//! answers are all **runtime state** — the name registry and the process table live in
//! [`beam_process`], the port table in `crate::port`, the per-process flags in the processes
//! themselves — which no BIF can reach. Every operation therefore routes through the optional
//! `beam_bifs::ProcessHook` seam, and `beam_rt::context::call_registered` installs the concrete
//! [`ProcessBridge`] over its own `ProcessTable` / `Rc<RefCell<PortRegistry>>`.
//!
//! This suite pins that path at the library boundary: **nine** of the eleven BIFs are served
//! through the registry + bridge, while `process_info/1,2` deliberately stay on the
//! runtime-native floor (`src/native.rs` — see the module docs there), so they are exercised by
//! `native::memory`'s own tests, not here.
//!
//! # Oracle ground truth (OTP 29.1.1 / erts-17.1)
//!
//! ```text
//! $ erl -noshell -eval '
//!     R1 = erlang:register(zz_seams_name, self()),
//!     W1 = is_pid(erlang:whereis(zz_seams_name)),
//!     R2 = (catch erlang:register(zz_seams_name, self())),
//!     U1 = erlang:unregister(zz_seams_name),
//!     U2 = (catch erlang:unregister(zz_seams_name)),
//!     R3 = (catch erlang:register(init, self())),
//!     R4 = (catch erlang:register(undefined, self())),
//!     Parent = self(),
//!     spawn(fun() -> T1 = erlang:process_flag(trap_exit, true), ...,
//!                     Parent ! {flags, T1, T2, P1, S1, C1} end),
//!     {T1,T2,P1,S1,C1} = receive {flags,A,B,C,D,E} -> {A,B,C,D,E} end,
//!     ... io:format("~s~n",[lists:join("|",[atom_to_list(F) || F <- Flags])]), halt(0).'
//! true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true
//! ```
//!
//! (an abridgement — `…` elides the five `process_flag` calls and most of the flag list; the
//! **full** expression this comparison test actually runs is the `ORACLE_PROBE` literal below,
//! handed to `support::oracle_eval` and compared verbatim, so the executed probe can never drift
//! from the quoted result). Every one of the 24 checks is a *comparison*
//! (`… =:= 0`, `element(1, catch …) =:= 'EXIT'`, `is_pid(…)`, …) so the two sides print the
//! same alphabet; the 11th is `is_process_alive(list_to_pid("<0.9999.0>")) =:= false` — a pid
//! that names no live process. The five `process_flag/2` previous-value checks are made in a
//! **freshly spawned** worker, because the `erl -eval` process itself has `trap_exit` already
//! set (`process_flag(trap_exit, true)` returns `true` there — probed); a `spawn`ed process has
//! the documented defaults (`trap_exit`/`sensitive` `false`, `priority` `normal`, `save_calls`
//! `0`), which is what this runtime's own root process has too.
//!
//! # The name registry holds a pid **or** a port
//!
//! The oracle's register table is keyed by name and holds either a pid or a port
//! (`erts/emulator/beam/register.c:175-300`), and so does this bridge: `register/2` of a **live**
//! port is `true`, `whereis/1` then answers the port (so `is_port/1` is `true`), `registered/0`
//! lists the name and `unregister/1` removes it. A port the port registry does not hold — a
//! closed port, or a number nothing was ever opened under — is the oracle's `badarg`, as is a
//! second name for a port that already has one. Pinned by
//! `registering_a_live_port_round_trips_and_refuses_the_rest` below.
//!
//! # Documented divergences (honoured, never hidden)
//!
//! * `save_calls` with a **non-integer** value: the oracle raises `badarg`
//!   (`../../erts/emulator/beam/bif.c:1911`), while the bridge treats `true`/`false` as `1`/`0`
//!   and leaves the flag untouched for anything else. Pinned directly in the bridge's unit
//!   tests (`src/context/process_bridge.rs`) and deliberately **not** part of the live
//!   comparison below, which only exercises values the two agree on.
mod support;

use beam_bifs::NullSink;
use beam_engine::{terms, ExecutionContext};
use beam_process::{PortId, ProcessId};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{is_list, is_nil, is_small, make_small, signed_value, Eterm};

/// A runtime with the root `init` process registered (the default runtime).
fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// Run `f` against a fresh `RuntimeContext` over `rt`; the context (and its sink) are dropped
/// before `rt` is borrowed again.
fn with_context<T>(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    f(&mut ctx)
}

/// Call an `erlang:*` BIF through the runtime's `ExecutionContext` seam (the path that installs
/// the [`ProcessBridge`](beam_rt::context) for these names).
fn call(rt: &mut Runtime, name: &str, arity: u32, args: &[Eterm]) -> beam_common::Result<Eterm> {
    with_context(rt, |ctx| ctx.call_bif("erlang", name, arity, args))
}

/// Whether `term` is an Erlang list — a cons cell **or** the empty list `NIL`, matching the
/// oracle's `is_list/1` (`beam_term::is_list` is the cons-cell tag test alone).
fn is_erlang_list(term: Eterm) -> bool {
    is_nil(term) || is_list(term)
}

/// Whether `term` is the atom `name` as this runtime's shared table spells it.
fn is_atom_named(rt: &Runtime, term: Eterm, name: &str) -> bool {
    rt.atom_name_of(term) == Some(name)
}

/// Whether `result` is the oracle's plain `badarg` raise.
fn is_badarg(result: beam_common::Result<Eterm>) -> bool {
    matches!(result, Err(error) if error.as_atom() == Some("badarg"))
}

// ---------------------------------------------------------------------------------------
// registered/0
// ---------------------------------------------------------------------------------------

/// `registered/0` is a proper list of atoms that contains `init` — the runtime registers its
/// root process under that name, exactly as the oracle registers its `<0.0.0>`.
#[test]
fn registered_is_a_proper_list_of_atoms_containing_init() {
    let mut rt = runtime();
    let list = call(&mut rt, "registered", 0, &[]).expect("registered/0");
    assert!(is_erlang_list(list), "registered/0 is a list, got {list:?}");
    let names = terms::list_elements(list).expect("a proper list");
    assert!(!names.is_empty(), "the runtime registers at least `init`");
    assert!(
        names.iter().all(|name| rt.atom_name_of(*name).is_some()),
        "every entry is a nameable atom: {names:?}"
    );
    let init = rt.intern("init");
    assert!(names.contains(&init), "`init` is registered: {names:?}");
}

// ---------------------------------------------------------------------------------------
// is_process_alive/1
// ---------------------------------------------------------------------------------------

/// `is_process_alive/1` is `true` for a live pid, `false` for a pid that names no live process,
/// and the oracle's `badarg` for anything that is not an internal pid.
#[test]
fn is_process_alive_is_true_false_and_badarg() {
    let mut rt = runtime();
    let self_pid = rt.self_pid().to_eterm();
    let alive = call(&mut rt, "is_process_alive", 1, &[self_pid]).expect("is_process_alive/1");
    assert!(is_atom_named(&rt, alive, "true"), "self is alive: {alive:?}");

    // A pid the table never allocated names no live process.
    let dead = ProcessId::new(999_999, 0).to_eterm();
    let gone = call(&mut rt, "is_process_alive", 1, &[dead]).expect("is_process_alive/1");
    assert!(is_atom_named(&rt, gone, "false"), "an unknown pid: {gone:?}");

    // The argument check happens before the table is consulted (the `proc_reg.rs` rule): a
    // non-pid — here a small integer — is `badarg`.
    support::assert_badarg(
        call(&mut rt, "is_process_alive", 1, &[make_small(42)])
            .expect_err("a non-pid is badarg"),
    );
}

// ---------------------------------------------------------------------------------------
// process_flag/2
// ---------------------------------------------------------------------------------------

/// `process_flag/2` returns the **previous** value of each flag this runtime implements, and
/// the oracle's `badarg` for an unknown flag, a non-boolean `trap_exit` value and a non-atom
/// flag.
#[test]
fn process_flag_returns_the_previous_value_and_badargs_the_rest() {
    let mut rt = runtime();
    let trap = rt.intern("trap_exit");
    let v_true = rt.intern("true");
    let v_false = rt.intern("false");

    // `trap_exit`: a fresh process does not trap exits, so the first set returns `false` and
    // the reset returns the `true` it replaced.
    let first = call(&mut rt, "process_flag", 2, &[trap, v_true]).expect("process_flag/2");
    assert!(is_atom_named(&rt, first, "false"), "{first:?}");
    let second = call(&mut rt, "process_flag", 2, &[trap, v_false]).expect("process_flag/2");
    assert!(is_atom_named(&rt, second, "true"), "{second:?}");

    // `priority`: the default is the atom `normal`.
    let priority = rt.intern("priority");
    let high = rt.intern("high");
    let previous = call(&mut rt, "process_flag", 2, &[priority, high]).expect("process_flag/2");
    assert!(is_atom_named(&rt, previous, "normal"), "{previous:?}");

    // `sensitive`: the default is `false`.
    let sensitive = rt.intern("sensitive");
    let previous = call(&mut rt, "process_flag", 2, &[sensitive, v_true]).expect("process_flag/2");
    assert!(is_atom_named(&rt, previous, "false"), "{previous:?}");

    // `save_calls`: the default is `0` and the previous value is a (small) integer.
    let save_calls = rt.intern("save_calls");
    let previous =
        call(&mut rt, "process_flag", 2, &[save_calls, make_small(100)]).expect("process_flag/2");
    assert!(is_small(previous) && signed_value(previous) == 0, "{previous:?}");
    let previous =
        call(&mut rt, "process_flag", 2, &[save_calls, make_small(7)]).expect("process_flag/2");
    assert!(is_small(previous) && signed_value(previous) == 100, "{previous:?}");

    // An unknown flag is the oracle's `badarg` (the hook's `Ok(None)` contract) ...
    let unknown = rt.intern("zz_proc_seams_frobnicate");
    assert!(is_badarg(call(&mut rt, "process_flag", 2, &[unknown, make_small(1)])));
    // ... as is `trap_exit` with a non-boolean value ...
    let not_a_bool = rt.intern("zz_proc_seams_not_a_bool");
    assert!(is_badarg(call(&mut rt, "process_flag", 2, &[trap, not_a_bool])));
    // ... and a flag that is not an atom at all (decided from the argument alone).
    assert!(is_badarg(call(&mut rt, "process_flag", 2, &[make_small(42), v_true])));
}

// ---------------------------------------------------------------------------------------
// whereis/1, register/2, unregister/1
// ---------------------------------------------------------------------------------------

/// `register/2` → `whereis/1` → `unregister/1` round-trips, and every failure the oracle
/// renders as `badarg` is one here: a taken name, the atom `undefined`, a non-pid id, a
/// non-atom name and unregistering an unknown name.
#[test]
fn register_whereis_unregister_round_trip() {
    let mut rt = runtime();
    let name = rt.intern("zz_proc_seams_name");
    let undefined = rt.intern("undefined");
    let pid = rt.self_pid().to_eterm();

    // An unregistered name is the atom `undefined`.
    let before = call(&mut rt, "whereis", 1, &[name]).expect("whereis/1");
    assert!(is_atom_named(&rt, before, "undefined"), "{before:?}");

    // Registering succeeds; `whereis/1` then answers the very pid term.
    let ok = call(&mut rt, "register", 2, &[name, pid]).expect("register/2");
    assert!(is_atom_named(&rt, ok, "true"), "{ok:?}");
    let after = call(&mut rt, "whereis", 1, &[name]).expect("whereis/1");
    assert_eq!(after, pid, "whereis/1 answers the registered pid");

    // A taken name, the atom `undefined`, a non-pid id and a non-atom name are all `badarg`.
    let init = rt.intern("init");
    assert!(is_badarg(call(&mut rt, "register", 2, &[init, pid])));
    assert!(is_badarg(call(&mut rt, "register", 2, &[undefined, pid])));
    assert!(is_badarg(call(&mut rt, "register", 2, &[name, make_small(42)])));
    assert!(is_badarg(call(&mut rt, "register", 2, &[make_small(42), pid])));
    assert!(is_badarg(call(&mut rt, "whereis", 1, &[make_small(42)])));

    // Unregistering succeeds once; the second attempt and the non-atom name are `badarg`, and
    // the name really is gone afterwards.
    let gone = call(&mut rt, "unregister", 1, &[name]).expect("unregister/1");
    assert!(is_atom_named(&rt, gone, "true"), "{gone:?}");
    assert!(is_badarg(call(&mut rt, "unregister", 1, &[name])));
    assert!(is_badarg(call(&mut rt, "unregister", 1, &[make_small(42)])));
    let nowhere = call(&mut rt, "whereis", 1, &[name]).expect("whereis/1");
    assert!(is_atom_named(&rt, nowhere, "undefined"), "{nowhere:?}");
}

// ---------------------------------------------------------------------------------------
// processes/0, ports/0, group_leader/0
// ---------------------------------------------------------------------------------------

/// `processes/0` is a list of pids naming the live processes; `ports/0` is a list (empty for a
/// runtime that has opened nothing); `group_leader/0` is this runtime's root pid — the same pid
/// the deleted runtime-native floor arm reported.
#[test]
fn processes_ports_and_group_leader_have_the_oracle_shapes() {
    let mut rt = runtime();
    let self_pid = rt.self_pid();

    let processes = call(&mut rt, "processes", 0, &[]).expect("processes/0");
    assert!(is_erlang_list(processes), "processes/0 is a list: {processes:?}");
    let pids = terms::list_elements(processes).expect("a proper list");
    assert!(!pids.is_empty(), "the root process is live");
    assert!(
        pids.iter().all(|pid| ProcessId::is_process(*pid)),
        "every entry is a pid: {pids:?}"
    );
    assert!(pids.contains(&self_pid.to_eterm()), "the root is listed");

    let ports = call(&mut rt, "ports", 0, &[]).expect("ports/0");
    assert!(is_erlang_list(ports), "ports/0 is a list: {ports:?}");
    assert_eq!(
        terms::list_elements(ports).expect("a proper list"),
        Vec::<Eterm>::new(),
        "a fresh runtime has opened no ports"
    );

    let leader = call(&mut rt, "group_leader", 0, &[]).expect("group_leader/0");
    assert!(ProcessId::is_process(leader), "group_leader/0 is a pid");
    assert_eq!(leader, self_pid.to_eterm());
}

// ---------------------------------------------------------------------------------------
// The name registry holds a port as well as a pid
// ---------------------------------------------------------------------------------------

/// `register/2` of a **live port** round-trips through `whereis/1`, `registered/0` and
/// `unregister/1`, and the two refusals the oracle reports as `badarg` are `badarg` here: a
/// second name for a port that already has one, and a port the port registry does not hold (a
/// number nothing was opened under — the BIF layer passes any internal port through, and the
/// port registry refuses one that is not alive).
///
/// Oracle ground truth (OTP 29.1.1 / erts-17.1), with `F/1` the thunk helper that renders a
/// `catch`ed raise as its bare reason atom:
///
/// ```text
/// $ erl -noshell -eval '
///     F = fun(G) -> case catch G() of
///                       {EXIT, {R, _}} when is_atom(R) -> R;
///                       {EXIT, R} when is_atom(R) -> R;
///                       V -> V
///                   end end,
///     P = open_port({spawn_driver,"ram_file_drv"},[]),
///     io:format("~p~n",[F(fun() -> erlang:register(myp,P) end)]),
///     io:format("~p~n",[is_port(erlang:whereis(myp))]),
///     io:format("~p~n",[lists:member(myp,erlang:registered())]),
///     io:format("~p~n",[F(fun() -> erlang:register(myp2,P) end)]),
///     io:format("~p~n",[F(fun() -> erlang:unregister(myp) end)]),
///     io:format("~p~n",[erlang:whereis(myp)]), halt(0).'
/// true
/// true
/// true
/// badarg
/// true
/// undefined
/// ```
///
/// (`EXIT` above abbreviates the quoted atom `'EXIT'` the probe matches on.)
#[test]
fn registering_a_live_port_round_trips_and_refuses_the_rest() {
    let mut rt = runtime();
    let name = rt.intern("myp");
    let second = rt.intern("myp2");
    let third = rt.intern("myp3");

    // A real `{spawn_driver, "ram_file_drv"}` port of this runtime (`open_port/2` is the
    // runtime-native floor's, so the evaluator is the shortest honest way to open one).
    let port = rt
        .eval(r#"open_port({spawn_driver,"ram_file_drv"},[])"#)
        .expect("open_port({spawn_driver,\"ram_file_drv\"},[])");
    assert!(PortId::is_port(port), "a live port, got {port:?}");

    // `register/2` → `whereis/1`: the name now resolves to the port itself.
    let ok = call(&mut rt, "register", 2, &[name, port]).expect("register/2");
    assert!(is_atom_named(&rt, ok, "true"), "{ok:?}");
    let found = call(&mut rt, "whereis", 1, &[name]).expect("whereis/1");
    assert!(PortId::is_port(found), "whereis/1 answers a port, got {found:?}");
    assert_eq!(
        PortId::from_eterm(found).expect("whereis/1 answers an internal port"),
        PortId::from_eterm(port).expect("a port immediate")
    );

    // ... and `registered/0` lists the name.
    let registered = call(&mut rt, "registered", 0, &[]).expect("registered/0");
    let names = terms::list_elements(registered).expect("a proper list");
    assert!(names.contains(&name), "`myp` is registered: {names:?}");

    // A second name for the same port is `badarg` (the C's "port already registered"), as is
    // naming a port the registry does not hold.
    assert!(is_badarg(call(&mut rt, "register", 2, &[second, port])));
    let unheld = PortId::new(7).to_eterm();
    assert!(is_badarg(call(&mut rt, "register", 2, &[third, unheld])));

    // `unregister/1` removes the name; `whereis/1` is then the atom `undefined`.
    let gone = call(&mut rt, "unregister", 1, &[name]).expect("unregister/1");
    assert!(is_atom_named(&rt, gone, "true"), "{gone:?}");
    let nowhere = call(&mut rt, "whereis", 1, &[name]).expect("whereis/1");
    assert!(is_atom_named(&rt, nowhere, "undefined"), "{nowhere:?}");
}

// ---------------------------------------------------------------------------------------
// The live-oracle cross-check
// ---------------------------------------------------------------------------------------

/// The exact expression the comparison test runs under the oracle. Kept as one literal so the
/// probe quoted in this file's header and the executed one can never drift.
const ORACLE_PROBE: &str = r#"
R1 = erlang:register(zz_seams_name, self()),
W1 = is_pid(erlang:whereis(zz_seams_name)),
R2 = (catch erlang:register(zz_seams_name, self())),
U1 = erlang:unregister(zz_seams_name),
U2 = (catch erlang:unregister(zz_seams_name)),
R3 = (catch erlang:register(init, self())),
R4 = (catch erlang:register(undefined, self())),
Parent = self(),
spawn(fun() ->
    T1 = erlang:process_flag(trap_exit, true),
    T2 = erlang:process_flag(trap_exit, false),
    P1 = erlang:process_flag(priority, high),
    S1 = erlang:process_flag(sensitive, true),
    C1 = erlang:process_flag(save_calls, 100),
    Parent ! {flags, T1, T2, P1, S1, C1}
end),
{T1, T2, P1, S1, C1} = receive {flags, A, B, C, D, E} -> {A, B, C, D, E} end,
Flags = [
    is_pid(erlang:whereis(init)),
    erlang:whereis(zz_seams_absent) =:= undefined,
    R1 =:= true,
    W1,
    element(1, R2) =:= 'EXIT',
    U1 =:= true,
    element(1, U2) =:= 'EXIT',
    element(1, R3) =:= 'EXIT',
    element(1, R4) =:= 'EXIT',
    erlang:is_process_alive(self()),
    erlang:is_process_alive(erlang:list_to_pid("<0.9999.0>")) =:= false,
    element(1, catch erlang:is_process_alive(42)) =:= 'EXIT',
    is_list(erlang:processes()) andalso is_pid(hd(erlang:processes())),
    is_list(erlang:ports()),
    is_pid(erlang:group_leader()),
    is_list(erlang:registered()) andalso lists:member(init, erlang:registered()),
    T1 =:= false,
    T2 =:= true,
    P1 =:= normal,
    S1 =:= false,
    C1 =:= 0,
    element(1, catch erlang:process_flag(zz_seams_frob, 1)) =:= 'EXIT',
    element(1, catch erlang:process_flag(trap_exit, zz_seams_not_bool)) =:= 'EXIT',
    element(1, catch erlang:whereis(42)) =:= 'EXIT'
],
io:format("~s~n",[lists:join("|", [atom_to_list(F) || F <- Flags])]),
halt(0).
"#;

/// The same 24 checks, computed against this runtime, in the probe's own order.
///
/// The five `process_flag/2` previous-value checks run against this runtime's root process,
/// which — like the oracle probe's freshly `spawn`ed worker — starts with the defaults
/// (`trap_exit`/`sensitive` `false`, `priority` `normal`, `save_calls` `0`).
fn our_flags(rt: &mut Runtime) -> Vec<bool> {
    let name = rt.intern("zz_seams_name");
    let absent = rt.intern("zz_seams_absent");
    let init = rt.intern("init");
    let undefined = rt.intern("undefined");
    let frob = rt.intern("zz_seams_frob");
    let not_a_bool = rt.intern("zz_seams_not_bool");
    let v_true = rt.intern("true");
    let v_false = rt.intern("false");
    let high = rt.intern("high");
    let trap = rt.intern("trap_exit");
    let priority = rt.intern("priority");
    let sensitive = rt.intern("sensitive");
    let save_calls = rt.intern("save_calls");

    let self_pid = rt.self_pid().to_eterm();
    let dead = ProcessId::new(9999, 0).to_eterm();

    let r1 = call(rt, "register", 2, &[name, self_pid]).expect("register/2");
    let w1 = call(rt, "whereis", 1, &[name]).expect("whereis/1");
    let r2 = call(rt, "register", 2, &[name, self_pid]).is_err();
    let u1 = call(rt, "unregister", 1, &[name]).expect("unregister/1");
    let u2 = call(rt, "unregister", 1, &[name]).is_err();
    let r3 = call(rt, "register", 2, &[init, self_pid]).is_err();
    let r4 = call(rt, "register", 2, &[undefined, self_pid]).is_err();
    let whereis_init = call(rt, "whereis", 1, &[init]).expect("whereis/1");
    let whereis_absent = call(rt, "whereis", 1, &[absent]).expect("whereis/1");
    let alive_self = call(rt, "is_process_alive", 1, &[self_pid]).expect("is_process_alive/1");
    let alive_dead = call(rt, "is_process_alive", 1, &[dead]).expect("is_process_alive/1");
    let alive_non_pid = call(rt, "is_process_alive", 1, &[make_small(42)]).is_err();
    let processes = call(rt, "processes", 0, &[]).expect("processes/0");
    let ports = call(rt, "ports", 0, &[]).expect("ports/0");
    let leader = call(rt, "group_leader", 0, &[]).expect("group_leader/0");
    let registered = call(rt, "registered", 0, &[]).expect("registered/0");
    let flag_t1 = call(rt, "process_flag", 2, &[trap, v_true]).expect("process_flag/2");
    let flag_t2 = call(rt, "process_flag", 2, &[trap, v_false]).expect("process_flag/2");
    let flag_p1 = call(rt, "process_flag", 2, &[priority, high]).expect("process_flag/2");
    let flag_s1 = call(rt, "process_flag", 2, &[sensitive, v_true]).expect("process_flag/2");
    let flag_c1 =
        call(rt, "process_flag", 2, &[save_calls, make_small(100)]).expect("process_flag/2");
    let flag_unknown = call(rt, "process_flag", 2, &[frob, make_small(1)]).is_err();
    let flag_bad_value = call(rt, "process_flag", 2, &[trap, not_a_bool]).is_err();
    let whereis_non_atom = call(rt, "whereis", 1, &[make_small(42)]).is_err();

    let processes_shape = is_erlang_list(processes)
        && terms::list_elements(processes)
            .map(|pids| pids.first().copied().map(ProcessId::is_process).unwrap_or(false))
            .unwrap_or(false);
    let registered_shape = is_erlang_list(registered)
        && terms::list_elements(registered)
            .map(|names| names.contains(&init))
            .unwrap_or(false);

    vec![
        ProcessId::is_process(whereis_init),
        is_atom_named(rt, whereis_absent, "undefined"),
        is_atom_named(rt, r1, "true"),
        ProcessId::is_process(w1),
        r2,
        is_atom_named(rt, u1, "true"),
        u2,
        r3,
        r4,
        is_atom_named(rt, alive_self, "true"),
        is_atom_named(rt, alive_dead, "false"),
        alive_non_pid,
        processes_shape,
        is_erlang_list(ports),
        ProcessId::is_process(leader),
        registered_shape,
        is_atom_named(rt, flag_t1, "false"),
        is_atom_named(rt, flag_t2, "true"),
        is_atom_named(rt, flag_p1, "normal"),
        is_atom_named(rt, flag_s1, "false"),
        is_small(flag_c1) && signed_value(flag_c1) == 0,
        flag_unknown,
        flag_bad_value,
        whereis_non_atom,
    ]
}

/// Every one of the 24 observable outcomes above must match the system `erl`'s, which proves the
/// registry + `ProcessBridge` path (including `registered/0`, `is_process_alive/1` and
/// `process_flag/2`) behaves like the oracle. Skips **loudly** without a toolchain.
#[test]
fn the_observable_outcomes_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("proc_seams") else {
        return;
    };
    let Some(rendered) = support::oracle_eval(&oracle, None, ORACLE_PROBE) else {
        eprintln!("SKIP the live-oracle comparison: the erl probe could not answer");
        return;
    };
    let oracle_flags: Vec<String> = rendered
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        oracle_flags.len(),
        1,
        "the probe prints exactly one line of flags, got {oracle_flags:?}"
    );

    let mut rt = runtime();
    let ours: Vec<String> = our_flags(&mut rt)
        .into_iter()
        .map(|flag| flag.to_string())
        .collect();
    assert_eq!(
        ours.join("|"),
        oracle_flags[0],
        "every observable outcome must match the live oracle"
    );
}
