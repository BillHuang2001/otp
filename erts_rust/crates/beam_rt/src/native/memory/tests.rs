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

//! Unit tests for the `erlang:process_info/1,2` surface.
//!
//! The oracle values are pinned against the system `erl` (OTP 29.1.1 / erts-17.1); the
//! oracle-comparison test skips **loudly** when `erl` is absent.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use beam_bifs::NullSink;
use beam_common::BeamError;
use beam_engine::terms;
use beam_engine::ExecutionContext;
use beam_gc::HeapObservable;
use beam_process::{
    MonitorKind, PidTarget, ProcessId, ProcessTable, RemotePid, RemoteRef, SignalAtoms,
};
use beam_term::{atom_val, is_atom, make_small, signed_value, AtomTable, Eterm, NIL};

use crate::context::RuntimeContext;
use crate::native::test_support::{atom, call, runtime};
use crate::runner::{ProcessEntry, RunnerState};

/// The runtime's own root process — a live process in the process table.
fn root_pid(runtime: &mut crate::Runtime) -> Eterm {
    runtime.self_pid().to_eterm()
}

/// `whereis(init)` — the runtime's **`init` stand-in**: a live process, registered under
/// `init`, that is *not* the root process (`Runtime::init_pid`).
fn init_pid(runtime: &mut crate::Runtime) -> Eterm {
    let name = atom(runtime, "init");
    call(runtime, "erlang", "whereis", &[name]).expect("whereis(init)")
}

/// A pid the process table never allocated.
fn dead_pid() -> Eterm {
    ProcessId::new(999_999, 0).to_eterm()
}

/// `items` as an Erlang list (the `terms::*` builders need an [`ExecutionContext`]).
fn make_list(runtime: &mut crate::Runtime, items: &[Eterm]) -> Eterm {
    let mut sink = NullSink;
    let mut context = RuntimeContext::with_output(runtime, &mut sink);
    terms::make_list(&mut context, items).expect("a list")
}

/// `items` as an Erlang tuple.
fn make_tuple(runtime: &mut crate::Runtime, items: &[Eterm]) -> Eterm {
    let mut sink = NullSink;
    let mut context = RuntimeContext::with_output(runtime, &mut sink);
    terms::make_tuple(&mut context, items).expect("a tuple")
}

/// `car` consed onto `cdr` (possibly an improper list).
fn make_cons(runtime: &mut crate::Runtime, car: Eterm, cdr: Eterm) -> Eterm {
    let mut sink = NullSink;
    let mut context = RuntimeContext::with_output(runtime, &mut sink);
    terms::make_cons(&mut context, car, cdr).expect("a cons")
}

/// Looks an item up on `pid`.
fn info(runtime: &mut crate::Runtime, pid: Eterm, name: &str) -> beam_common::Result<Eterm> {
    let it = atom(runtime, name);
    call(runtime, "erlang", "process_info", &[pid, it])
}

/// The `(key, value)` of a `{Item, Value}` reply.
fn pair(reply: Eterm) -> (Eterm, Eterm) {
    assert_eq!(terms::tuple_arity(reply).expect("a tuple"), 2);
    (
        terms::tuple_element(reply, 0).expect("key"),
        terms::tuple_element(reply, 1).expect("value"),
    )
}

/// The integer `process_info(Pid, trace)` answers.
fn trace_of(runtime: &mut crate::Runtime, pid: Eterm) -> i64 {
    let (key, value) = pair(info(runtime, pid, "trace").expect("process_info/2 trace"));
    assert_eq!(runtime.atom_name_of(key), Some("trace"));
    signed_value(value)
}

/// The key names of a `process_info/1` reply, in order.
fn keys_of(runtime: &mut crate::Runtime, reply: Eterm) -> Vec<String> {
    terms::list_elements(reply)
        .expect("a list")
        .iter()
        .map(|entry| {
            runtime
                .atom_name_of(terms::tuple_element(*entry, 0).expect("key"))
                .expect("a key atom")
                .to_string()
        })
        .collect()
}

/// The `[{Item, Value}]` entries of a `process_info/2` list reply.
fn entries_of(reply: Eterm) -> Vec<(Eterm, Eterm)> {
    terms::list_elements(reply)
        .expect("a list")
        .iter()
        .map(|entry| pair(*entry))
        .collect()
}

#[test]
fn process_info_1_is_the_oracle_order_with_registered_name_first() {
    let mut runtime = runtime();
    let init = init_pid(&mut runtime);
    let reply = call(&mut runtime, "erlang", "process_info", &[init]).expect("process_info/1");
    let keys = keys_of(&mut runtime, reply);
    // The `init` stand-in is registered as `init`, so `registered_name` is emitted (wrapped)
    // first. Its four engine-derived items are honestly omitted (it never runs, so it has no
    // engine — see the module docs) — leaving these 15, in the oracle's relative order
    // (checked below).
    assert_eq!(
        keys,
        vec![
            "registered_name",
            "status",
            "message_queue_len",
            "links",
            "dictionary",
            "trap_exit",
            "error_handler",
            "priority",
            "group_leader",
            "total_heap_size",
            "heap_size",
            "stack_size",
            "reductions",
            "garbage_collection",
            "suspending",
        ]
    );
    // The root process is a live **unregistered** process, so the same list omits
    // `registered_name` and nothing else — the C's `nil` skip, which is why the oracle emits
    // 16 items for a registered process and 15 for an unregistered one.
    let root = root_pid(&mut runtime);
    let reply = call(&mut runtime, "erlang", "process_info", &[root]).expect("process_info/1");
    assert_eq!(
        keys_of(&mut runtime, reply),
        keys[1..].to_vec(),
        "an unregistered process omits `registered_name` and keeps the rest"
    );
}

#[test]
fn process_info_1_of_a_dead_pid_is_undefined_and_of_a_non_pid_is_badarg() {
    let mut runtime = runtime();
    for pid in [dead_pid(), ProcessId::new(4_000_000, 0).to_eterm()] {
        let reply = call(&mut runtime, "erlang", "process_info", &[pid]).expect("process_info/1");
        assert_eq!(runtime.atom_name_of(reply), Some("undefined"));
    }
    for not_a_pid in [make_small(42), atom(&mut runtime, "erlang"), NIL] {
        assert!(matches!(
            call(&mut runtime, "erlang", "process_info", &[not_a_pid]),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn the_list_form_returns_wrapped_pairs_in_order_with_duplicates() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    let status = atom(&mut runtime, "status");
    let heap = atom(&mut runtime, "heap_size");
    let list = make_list(&mut runtime, &[status, heap, status]);
    let reply = call(&mut runtime, "erlang", "process_info", &[pid, list]).expect("process_info/2");
    let entries = entries_of(reply);
    assert_eq!(entries.len(), 3, "duplicates are repeated");
    let names: Vec<Option<&str>> = entries
        .iter()
        .map(|(key, _)| runtime.atom_name_of(*key))
        .collect();
    assert_eq!(names, vec![Some("status"), Some("heap_size"), Some("status")]);
    // The status value really is one of the oracle's state atoms.
    assert!(is_atom(entries[0].1));
    assert!(matches!(
        runtime.atom_name_of(entries[0].1),
        Some("running" | "runnable" | "waiting" | "suspended" | "exiting")
    ));
}

#[test]
fn an_empty_list_on_a_live_pid_is_the_empty_list_and_on_a_dead_pid_is_undefined() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    let reply = call(&mut runtime, "erlang", "process_info", &[pid, NIL]).expect("process_info/2");
    assert_eq!(reply, NIL);
    let reply =
        call(&mut runtime, "erlang", "process_info", &[dead_pid(), NIL]).expect("process_info/2");
    assert_eq!(runtime.atom_name_of(reply), Some("undefined"));
}

#[test]
fn registered_name_is_wrapped_when_registered_and_bare_empty_when_unregistered() {
    let mut runtime = runtime();
    // The `init` stand-in is registered as `init`: the single-atom form is the WRAPPED pair.
    let init = init_pid(&mut runtime);
    let (key, value) = pair(info(&mut runtime, init, "registered_name").expect("process_info/2"));
    assert_eq!(runtime.atom_name_of(key), Some("registered_name"));
    assert_eq!(runtime.atom_name_of(value), Some("init"));
    // The list form is wrapped too.
    let name = atom(&mut runtime, "registered_name");
    let list = make_list(&mut runtime, &[name]);
    let reply = call(&mut runtime, "erlang", "process_info", &[init, list]).expect("process_info/2");
    let (key, value) = pair(terms::list_elements(reply).expect("a list")[0]);
    assert_eq!(runtime.atom_name_of(key), Some("registered_name"));
    assert_eq!(runtime.atom_name_of(value), Some("init"));
    // The root process is a LIVE, unregistered pid: the single-atom form is the BARE `[]` (the
    // oracle's `always_wrap = 0` nil case), while the list form wraps it.
    let root = root_pid(&mut runtime);
    assert_eq!(info(&mut runtime, root, "registered_name").expect("process_info/2"), NIL);
    let list = make_list(&mut runtime, &[name]);
    let reply = call(&mut runtime, "erlang", "process_info", &[root, list]).expect("process_info/2");
    let (key, value) = pair(terms::list_elements(reply).expect("a list")[0]);
    assert_eq!(runtime.atom_name_of(key), Some("registered_name"));
    assert_eq!(value, NIL);
    // Unregister `init` so the stand-in becomes a live, unregistered pid as well: the same bare
    // `[]` / wrapped `{registered_name, []}` split applies to it.
    let init_name = atom(&mut runtime, "init");
    call(&mut runtime, "erlang", "unregister", &[init_name]).expect("unregister");
    assert_eq!(info(&mut runtime, init, "registered_name").expect("process_info/2"), NIL);
    let list = make_list(&mut runtime, &[name]);
    let reply = call(&mut runtime, "erlang", "process_info", &[init, list]).expect("process_info/2");
    let (key, value) = pair(terms::list_elements(reply).expect("a list")[0]);
    assert_eq!(runtime.atom_name_of(key), Some("registered_name"));
    assert_eq!(value, NIL);
}

/// `registered_name` is the truth of the **queried** process, never the caller's: the name is
/// looked up by the pid argument, so asking about a *second* process registered under another
/// name reports that name while the caller's own item keeps its own.
///
/// Oracle probe (OTP 29.1.1 / erts-17.1), the very sequence this test mirrors — run as
/// `erl -noshell -eval '…'`, stdout shown verbatim:
///
/// ```text
/// A     = process_info(whereis(init), registered_name)  => {registered_name,init}
/// P     = spawn(fun() -> receive stop -> ok end end),
/// register(probe_reg, P),
/// B     = process_info(P, registered_name)             => {registered_name,probe_reg}
/// Blist = process_info(P, [registered_name])           => [{registered_name,probe_reg}]
/// unregister(probe_reg),
/// C     = process_info(P, registered_name)             => []
/// Clist = process_info(P, [registered_name])           => [{registered_name,[]}]
/// ```
///
/// The one route where this runtime answers differently for *its own* process is the
/// `-eval`/`-s`/`-run` action routes (see the module docs): those run the action in a spawned
/// child, so `process_info(self(), registered_name)` is the bare `[]` there — exactly the
/// oracle's answer for its own `-eval` process.
#[test]
fn registered_name_is_the_queried_processes_name_not_the_callers() {
    let mut runtime = runtime();
    let query = atom(&mut runtime, "registered_name");

    // A: `whereis(init)` resolves the runtime's `init` stand-in — a live process distinct from
    // the root one — and its item is `init`.
    let init_pid = init_pid(&mut runtime);
    assert_ne!(
        init_pid,
        root_pid(&mut runtime),
        "the `init` name belongs to the stand-in, not to the root process"
    );
    let (key, value) = pair(
        call(&mut runtime, "erlang", "process_info", &[init_pid, query]).expect("process_info/2"),
    );
    assert_eq!(runtime.atom_name_of(key), Some("registered_name"));
    assert_eq!(runtime.atom_name_of(value), Some("init"));

    // A **second**, distinct live process (never the caller), registered under another name.
    let other = runtime.processes.spawn_default().expect("a fresh process");
    let other_pid = other.to_eterm();
    assert_ne!(other_pid, init_pid, "a distinct pid");
    let probe = atom(&mut runtime, "probe_reg");
    let answer =
        call(&mut runtime, "erlang", "register", &[probe, other_pid]).expect("register/2");
    assert_eq!(runtime.atom_name_of(answer), Some("true"));

    // B: the queried process's name — NOT the other process's.
    let (key, value) = pair(info(&mut runtime, other_pid, "registered_name").expect("B"));
    assert_eq!(runtime.atom_name_of(key), Some("registered_name"));
    assert_eq!(runtime.atom_name_of(value), Some("probe_reg"));
    // ... while the `init` stand-in's own item still answers `init`.
    let (_, own) = pair(info(&mut runtime, init_pid, "registered_name").expect("init item"));
    assert_eq!(runtime.atom_name_of(own), Some("init"));
    // Blist: the list form wraps the name too.
    let list = make_list(&mut runtime, &[query]);
    let reply =
        call(&mut runtime, "erlang", "process_info", &[other_pid, list]).expect("process_info/2");
    let (key, value) = pair(terms::list_elements(reply).expect("a list")[0]);
    assert_eq!(runtime.atom_name_of(key), Some("registered_name"));
    assert_eq!(runtime.atom_name_of(value), Some("probe_reg"));

    // C/Clist: a live but **unregistered** process — the single-atom form is the bare `[]`, the
    // list form wraps it (`{registered_name, []}`), the oracle's `always_wrap` split.
    let answer = call(&mut runtime, "erlang", "unregister", &[probe]).expect("unregister/1");
    assert_eq!(runtime.atom_name_of(answer), Some("true"));
    assert_eq!(
        info(&mut runtime, other_pid, "registered_name").expect("C"),
        NIL,
        "the single-atom form is the bare []"
    );
    let list = make_list(&mut runtime, &[query]);
    let reply =
        call(&mut runtime, "erlang", "process_info", &[other_pid, list]).expect("process_info/2");
    let (key, value) = pair(terms::list_elements(reply).expect("a list")[0]);
    assert_eq!(runtime.atom_name_of(key), Some("registered_name"));
    assert_eq!(value, NIL, "the list form wraps the []");
}

#[test]
fn the_dictionary_tuple_form_echoes_the_key_and_answers_undefined_when_absent() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    let tag = atom(&mut runtime, "dictionary");
    let key = atom(&mut runtime, "no_such_key");
    let option = make_tuple(&mut runtime, &[tag, key]);
    let reply = call(&mut runtime, "erlang", "process_info", &[pid, option]).expect("process_info");
    let (echo, value) = pair(reply);
    // The echo is the `{dictionary, Key}` tuple itself.
    assert_eq!(terms::tuple_arity(echo).expect("a tuple"), 2);
    assert_eq!(
        runtime.atom_name_of(terms::tuple_element(echo, 0).expect("tag")),
        Some("dictionary")
    );
    assert_eq!(runtime.atom_name_of(value), Some("undefined"));
}

#[test]
fn the_reachable_items_have_their_oracle_shapes() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);

    let (key, value) = pair(info(&mut runtime, pid, "message_queue_len").expect("mq len"));
    assert_eq!(runtime.atom_name_of(key), Some("message_queue_len"));
    assert_eq!(signed_value(value), 0);
    // Unlike `registered_name`, `messages` is wrapped: the oracle answers `{messages, []}`.
    let (key, value) = pair(info(&mut runtime, pid, "messages").expect("messages"));
    assert_eq!(runtime.atom_name_of(key), Some("messages"));
    assert_eq!(value, NIL);

    for empty_list_item in ["links", "monitors", "monitored_by", "dictionary", "suspending"] {
        let (key, value) = pair(info(&mut runtime, pid, empty_list_item).expect("empty item"));
        assert_eq!(runtime.atom_name_of(key), Some(empty_list_item));
        assert_eq!(value, NIL, "{empty_list_item} starts empty");
    }
    for (name, expected) in [
        ("trap_exit", "false"),
        ("error_handler", "error_handler"),
        ("priority", "normal"),
        ("message_queue_data", "on_heap"),
        ("last_calls", "false"),
    ] {
        let (key, value) = pair(info(&mut runtime, pid, name).expect("atom item"));
        assert_eq!(runtime.atom_name_of(key), Some(name));
        assert!(is_atom(value), "{name} is an atom");
        assert_eq!(runtime.atom_name_of(value), Some(expected), "{name}");
    }
    // `trace` is an integer bitmask, `0` here because the root process is untraced; the live
    // per-process values (`[call]` ⇒ 1024, `[all]` ⇒ 32763895) are pinned by
    // `the_trace_item_is_the_live_erts_tracee_flag_bitmask`.
    for (name, expected) in [("trace", 0), ("min_heap_size", 233), ("min_bin_vheap_size", 46_422)] {
        let (key, value) = pair(info(&mut runtime, pid, name).expect("integer item"));
        assert_eq!(runtime.atom_name_of(key), Some(name));
        assert_eq!(signed_value(value), expected, "{name}");
    }
    // `fullsweep_after` comes from the policy; `max_heap_size` is a map.
    let (_, fullsweep) = pair(info(&mut runtime, pid, "fullsweep_after").expect("fullsweep"));
    assert_eq!(signed_value(fullsweep), 65_535);
    let (_, heap_map) = pair(info(&mut runtime, pid, "max_heap_size").expect("max_heap_size"));
    assert!(terms::map_entries(heap_map).is_ok(), "max_heap_size is a map");
    // The measured items all answer a non-negative integer.
    for name in ["stack_size", "reductions", "memory", "heap_size", "total_heap_size"] {
        let (key, value) = pair(info(&mut runtime, pid, name).expect("measured item"));
        assert_eq!(runtime.atom_name_of(key), Some(name));
        assert!(signed_value(value) >= 0, "{name} is a non-negative measurement");
    }
    // The group leader is a live pid.
    let (_, group_leader) = pair(info(&mut runtime, pid, "group_leader").expect("group leader"));
    assert!(ProcessId::from_eterm(group_leader).is_ok());

    // The sequential-trace token is the **bare** `[]` while tracing is inactive for the
    // process (`erl_bif_info.c:2162-2166` copies the token verbatim) ...
    let (key, value) = pair(
        info(&mut runtime, pid, "sequential_trace_token").expect("token item"),
    );
    assert_eq!(runtime.atom_name_of(key), Some("sequential_trace_token"));
    assert_eq!(value, NIL);
    // ... and the 5-tuple `{Flags,Label,Serial,From,Lastcnt}` once the process has one.
    let internal = ProcessId::from_eterm(pid).expect("an internal pid");
    runtime
        .seq_trace_mut()
        .set_label(internal, internal.to_eterm(), make_small(7));
    let (_, value) = pair(
        info(&mut runtime, pid, "sequential_trace_token").expect("token item"),
    );
    assert_eq!(terms::tuple_arity(value).expect("a tuple"), 5);
    assert_eq!(
        terms::tuple_element(value, 1).expect("label"),
        make_small(7)
    );
}

/// The `async_dist` item is the process's **live** `process_flag(async_dist, …)` value — the
/// C's `F_ASYNC_DIST` PCB bit (`erl_bif_info.c:986,2981`) — read out of the runtime's own
/// per-pid store ([`crate::Runtime`]'s `async_dist`), the very store the `process_flag/2`
/// bridge writes, so the read and the write can never disagree. A fresh process reports the
/// oracle's default `false` (probed: `element(2, process_info(P, async_dist))` ⇒ `false` on
/// OTP 29.1.1 / erts-17.1).
#[test]
fn the_async_dist_item_tracks_process_flag() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    let internal = ProcessId::from_eterm(pid).expect("an internal pid");

    let (key, value) = pair(info(&mut runtime, pid, "async_dist").expect("async_dist item"));
    assert_eq!(runtime.atom_name_of(key), Some("async_dist"));
    assert_eq!(runtime.atom_name_of(value), Some("false"), "a fresh process is false");

    runtime.set_async_dist(internal, true);
    let (_, value) = pair(info(&mut runtime, pid, "async_dist").expect("async_dist item"));
    assert_eq!(runtime.atom_name_of(value), Some("true"), "the flag round-trips");

    runtime.set_async_dist(internal, false);
    let (_, value) = pair(info(&mut runtime, pid, "async_dist").expect("async_dist item"));
    assert_eq!(runtime.atom_name_of(value), Some("false"), "clearing round-trips");
}

/// The `trace` item is the process's **live** ERTS tracee-flag **bitmask** — an integer, not
/// the `trace_info(Pid, flags)` atom list — and it tracks `erlang:trace/3` for real.
///
/// Oracle (OTP 29.1.1 / erts-17.1), the very commands this test mirrors:
///
/// ```text
/// P = spawn(fun() -> receive stop -> ok end end),
/// process_info(P, trace)                               => {trace,0}
/// erlang:trace(P,true,[call]), process_info(P, trace)  => {trace,1024}
/// erlang:trace(P,false,[call]),
/// erlang:trace(P,true,[all]),  process_info(P, trace)  => {trace,32763895}
/// ```
///
/// C ground truth: `res = make_small(ERTS_IS_P_TRACED_FL(rp, TRACEE_FLAGS))`
/// (`erl_bif_info.c:2133`).
#[test]
fn the_trace_item_is_the_live_erts_tracee_flag_bitmask() {
    use crate::trace::flags_from_atoms;

    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    let internal = ProcessId::from_eterm(pid).expect("an internal pid");

    // Untraced: the zero bitmask (the oracle's `{trace,0}`).
    assert_eq!(trace_of(&mut runtime, pid), 0, "an untraced process is 0");

    // A real `erlang:trace(P, true, [call])` is 1024.
    runtime
        .trace_mut()
        .set(internal, flags_from_atoms(&["call"]).expect("call"), None);
    assert_eq!(trace_of(&mut runtime, pid), 1024);

    // `[all]` is 32763895 — the oracle's own `all` bitmask.
    runtime
        .trace_mut()
        .set(internal, flags_from_atoms(&["all"]).expect("all"), None);
    assert_eq!(trace_of(&mut runtime, pid), 32_763_895);

    // ... and turning tracing off puts it back to 0.
    runtime.trace_mut().clear(internal);
    assert_eq!(trace_of(&mut runtime, pid), 0, "cleared ⇒ back to 0");
}

/// `trace_bitmask` maps **every** canonical trace-flag atom to the ERTS bit the oracle sets for
/// an isolated `erlang:trace(P, true, [Atom])`.
///
/// Each expected value below was observed on OTP 29.1.1 / erts-17.1 (one `erl` per atom:
/// `P = spawn(fun() -> receive stop -> ok end end), erlang:trace(P,true,[Atom]),
/// io:format("~p~n",[element(2, process_info(P,trace))])`); they sum to the oracle's `[all]`
/// bitmask, 32763895. The coverage assertion is the **drift guard**: if
/// [`crate::trace::flags_to_atoms`] ever grows a canonical atom this table lacks (or the table
/// loses one), the lengths differ and the test fails rather than silently dropping a bit.
#[test]
fn trace_bitmask_maps_every_canonical_atom_to_the_oracles_bit() {
    use super::{trace_bitmask, ERTS_TRACE_FLAG_BITS};
    use crate::trace::{flags_from_atoms, flags_to_atoms};

    for (name, expected) in [
        ("timestamp", 1),
        ("strict_monotonic_timestamp", 2),
        ("monotonic_timestamp", 4),
        ("send", 16),
        ("receive", 32),
        ("set_on_spawn", 64),
        ("set_on_first_spawn", 128),
        ("set_on_link", 256),
        ("set_on_first_link", 512),
        ("call", 1024),
        ("procs", 2048),
        ("running", 8192),
        ("garbage_collection", 16384),
        ("arity", 32768),
        ("return_to", 65536),
        ("silent", 131_072),
        ("running_ports", 1_048_576),
        ("running_procs", 2_097_152),
        ("ports", 4_194_304),
        ("scheduler_id", 8_388_608),
        ("exiting", 16_777_216),
    ] {
        let flags = flags_from_atoms(&[name]).expect("a canonical trace flag");
        assert_eq!(trace_bitmask(flags), expected, "{name}");
    }

    // The canonical atom set and the table are the same set, so no bit can be dropped.
    let all = flags_from_atoms(&["all"]).expect("all");
    let canonical = flags_to_atoms(all);
    assert_eq!(
        canonical.len(),
        ERTS_TRACE_FLAG_BITS.len(),
        "the canonical atom set drifted from the ERTS bit table: {canonical:?}"
    );
    for name in &canonical {
        assert!(
            ERTS_TRACE_FLAG_BITS.iter().any(|(known, _)| known == name),
            "{name} has no ERTS bit in the table"
        );
    }
    // And the whole set is exactly the oracle's `[all]` bitmask.
    assert_eq!(trace_bitmask(all), 32_763_895);
}
/// The `monitors` item carries **both** monitor families, each entry as `{process, Pid}` or
/// `{port, Port}` — the oracle's shape (verified against OTP 29.1.1 / erts-17.1:
/// `monitors => [{port,#Port<0.3>},{process,<0.84.0>}]`).
///
/// Process monitors live in `Process::monitors` and port monitors in
/// `Process::port_monitors`, which `ProcessTable::monitor_port` populates — which is exactly
/// what `erlang:monitor/2,3` on a port does, over the shared `beam_io` port table. The
/// rendering is pinned here by populating both sets directly on a `Process` and inserting it
/// into the runtime's table.
#[test]
fn monitors_carries_the_port_monitors_alongside_the_process_monitors() {
    use beam_process::{Monitor, MonitorKind, PortId, PortWatch, Process, RefAllocator};

    let mut runtime = runtime();
    let pid = ProcessId::new(4242, 0);
    let watched = ProcessId::new(4243, 0);
    let port = PortId::new(7);

    let mut process = Process::new(pid).expect("a fresh process");
    let mut refs = RefAllocator::new();
    // One outgoing process monitor ...
    process.monitors_mut().push(Monitor {
        reference: refs.next(),
        target: watched,
        kind: MonitorKind::Process,
    });
    // ... and one outgoing port monitor.
    process.port_monitors_mut().push(PortWatch {
        reference: refs.next(),
        port,
    });
    {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
        context
            .process_table_mut()
            .insert(process)
            .expect("the process table accepts the pid");
    }

    let reply = info(&mut runtime, pid.to_eterm(), "monitors").expect("process_info/2 monitors");
    let (key, value) = pair(reply);
    assert_eq!(runtime.atom_name_of(key), Some("monitors"));
    let entries = terms::list_elements(value).expect("a list");
    assert_eq!(entries.len(), 2, "both families are reported: {entries:?}");

    // The process monitor first ...
    let (kind, target) = pair(entries[0]);
    assert_eq!(runtime.atom_name_of(kind), Some("process"));
    assert_eq!(target, watched.to_eterm());

    // ... then the port monitor, as `{port, Port}` with the port's own id.
    let (kind, target) = pair(entries[1]);
    assert_eq!(runtime.atom_name_of(kind), Some("port"));
    assert_eq!(target, port.to_eterm());
    assert!(
        PortId::from_eterm(target).is_ok(),
        "the port entry's target is a real port id"
    );
}

/// The valid oracle items this runtime cannot answer are **only** [`UNSUPPORTED_ITEMS`], and each
/// raises the typed capability gap — never a fabricated value and never a false `badarg`.
///
/// The five items that used to be gaps (`parent`, `label`, `priority_messages`, `magic_ref`,
/// `garbage_collection_info`) are now served out of this runtime's own state, so they are pinned
/// here as *answers*: a regression that silently moved one back into the gap list would fail both
/// halves.
#[test]
fn every_unsupported_item_raises_the_typed_capability_gap() {
    use super::UNSUPPORTED_ITEMS;

    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    assert_eq!(
        UNSUPPORTED_ITEMS,
        ["backtrace"],
        "`backtrace` is the one remaining valid oracle item this runtime cannot answer"
    );
    for name in UNSUPPORTED_ITEMS {
        let error = info(&mut runtime, pid, name).expect_err("must be a capability gap");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "{name} must be a typed `unsupported` gap, got {error}"
        );
    }
    // …and the five formerly-gapped items answer their own `{Item, Value}` pair.
    for name in [
        "parent",
        "label",
        "priority_messages",
        "magic_ref",
        "garbage_collection_info",
    ] {
        let reply = info(&mut runtime, pid, name).unwrap_or_else(|error| {
            panic!("{name} is served now, but process_info/2 answered {error}")
        });
        let (key, _) = pair(reply);
        assert_eq!(runtime.atom_name_of(key), Some(name));
    }
}

#[test]
fn a_pid_not_in_the_table_is_undefined_for_every_item() {
    let mut runtime = runtime();
    let dead = dead_pid();
    for name in [
        "registered_name",
        "status",
        "message_queue_len",
        "messages",
        "links",
        "monitors",
        "dictionary",
        "trap_exit",
        "garbage_collection",
        "heap_size",
        "memory",
        "current_function",
    ] {
        let reply = info(&mut runtime, dead, name)
            .unwrap_or_else(|error| panic!("process_info(dead, {name}): {error}"));
        assert_eq!(
            runtime.atom_name_of(reply),
            Some("undefined"),
            "{name} of a dead pid is undefined"
        );
    }
}

#[test]
fn an_unknown_item_is_badarg_even_for_a_bogus_pid() {
    let mut runtime = runtime();
    let live = root_pid(&mut runtime);
    let dead = dead_pid();
    let nonsense = atom(&mut runtime, "nonsense");
    let status = atom(&mut runtime, "status");
    for pid in [live, dead] {
        assert!(matches!(
            call(&mut runtime, "erlang", "process_info", &[pid, nonsense]),
            Err(BeamError::BadArg)
        ));
        // An unknown item inside a list is badarg too, even for a dead pid.
        let list = make_list(&mut runtime, &[status, nonsense]);
        assert!(matches!(
            call(&mut runtime, "erlang", "process_info", &[pid, list]),
            Err(BeamError::BadArg)
        ));
        // A non-atom item term, an improper list and a bogus 2-tuple are all badarg.
        assert!(matches!(
            call(&mut runtime, "erlang", "process_info", &[pid, make_small(7)]),
            Err(BeamError::BadArg)
        ));
        let improper = make_cons(&mut runtime, status, make_small(7));
        assert!(matches!(
            call(&mut runtime, "erlang", "process_info", &[pid, improper]),
            Err(BeamError::BadArg)
        ));
        let tag = atom(&mut runtime, "status");
        let bogus = make_tuple(&mut runtime, &[tag, make_small(1)]);
        assert!(matches!(
            call(&mut runtime, "erlang", "process_info", &[pid, bogus]),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn a_non_pid_first_argument_is_badarg() {
    let mut runtime = runtime();
    let it = atom(&mut runtime, "status");
    for not_a_pid in [make_small(42), atom(&mut runtime, "erlang"), NIL] {
        assert!(
            matches!(
                call(&mut runtime, "erlang", "process_info", &[not_a_pid, it]),
                Err(BeamError::BadArg)
            ),
            "a non-pid process must be badarg"
        );
    }
}

#[test]
fn garbage_collection_matches_the_oracle_for_a_live_process() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    let reply = info(&mut runtime, pid, "garbage_collection").expect("process_info/2");
    let (key, value) = pair(reply);
    assert_eq!(runtime.atom_name_of(key), Some("garbage_collection"));
    let pairs: Vec<Eterm> = terms::list_elements(value).expect("a proplist");
    let expected = [
        ("max_heap_size", None),
        ("min_bin_vheap_size", Some(46_422_i64)),
        ("min_heap_size", Some(233)),
        ("fullsweep_after", Some(65_535)),
        ("minor_gcs", Some(0)),
    ];
    assert_eq!(pairs.len(), expected.len());
    for (entry, (name, expected_value)) in pairs.iter().zip(expected) {
        let (key, value) = pair(*entry);
        assert_eq!(runtime.atom_name_of(key), Some(name));
        if let Some(expected_value) = expected_value {
            assert_eq!(signed_value(value), expected_value, "{name}");
        }
    }
}

#[test]
fn max_heap_size_is_the_oracle_flag_map() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    let (_, map) = pair(info(&mut runtime, pid, "max_heap_size").expect("max_heap_size"));
    let atoms = runtime.code_server().atoms();
    let entries = beam_term::map::map_pairs(map, atoms).expect("a map");
    let names: Vec<Option<&str>> = entries
        .iter()
        .map(|(key, _)| atoms.name(atom_val(*key)))
        .collect();
    assert_eq!(
        names,
        vec![
            Some("error_logger"),
            Some("include_shared_binaries"),
            Some("kill"),
            Some("size"),
        ]
    );
    let value_of = |name: &str| -> Eterm {
        entries
            .iter()
            .find(|(key, _)| atoms.name(atom_val(*key)) == Some(name))
            .expect("present")
            .1
    };
    assert_eq!(atoms.name(atom_val(value_of("error_logger"))), Some("true"));
    assert_eq!(atoms.name(atom_val(value_of("kill"))), Some("true"));
    assert_eq!(signed_value(value_of("size")), 0);
}

#[test]
fn heap_size_and_total_heap_size_are_the_processes_real_capacities() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    let (heap_size, old_heap_size) = {
        let process = runtime
            .processes()
            .get(runtime.self_pid())
            .expect("the root process is live");
        (
            process.collector().heap_size_words(),
            process.collector().old_heap_size_words(),
        )
    };
    assert_eq!(
        signed_value(pair(info(&mut runtime, pid, "heap_size").expect("heap_size")).1),
        heap_size as i64
    );
    // `total_heap_size` is the **C-shaped** sum `heap_size + old_heap_size`
    // (`erts/emulator/beam/erl_bif_info.c:1023-1030`), not the collector's summed capacity:
    // the oracle's old heap is created by a collection (`erl_gc.c:1470-1478`), so a fresh
    // process reports `total_heap_size == heap_size` (measured on the oracle: 233 / 233).
    assert_eq!(
        signed_value(pair(info(&mut runtime, pid, "total_heap_size").expect("total_heap_size")).1),
        heap_size as i64 + old_heap_size as i64
    );
    assert_eq!(heap_size, 233);
    assert_eq!(old_heap_size, 0, "no collection has created an old heap yet");
}
/// Runs `erl` and returns `process_info/1`'s item names in order, or `None` when the system
/// `erl` is unavailable (the test then skips **loudly**).
///
/// The probe queries a **registered** process, so the oracle emits all 17 `pi_1_keys` (with
/// `{registered_name, probe_reg}` first) rather than omitting `registered_name`.
fn oracle_pi_1_keys() -> Option<Vec<String>> {
    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "P = spawn(fun() -> receive stop -> ok end end), register(probe_reg, P), \
             io:format(\"~p~n\",[[element(1,X) || X <- process_info(P)]]), halt().",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let inner = text.trim().trim_start_matches('[').trim_end_matches(']');
    Some(
        inner
            .split(',')
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect(),
    )
}

#[test]
fn the_implemented_item_order_is_a_subsequence_of_the_oracles() {
    let mut runtime = runtime();
    // A **registered** process with no engine: the `init` stand-in. It emits the oracle's full
    // item set minus the two engine-derived items, which is the shape this comparison needs
    // (`registered_name` is present, so the oracle's 17 items are the yardstick).
    let pid = init_pid(&mut runtime);
    let reply = call(&mut runtime, "erlang", "process_info", &[pid]).expect("process_info/1");
    let ours = keys_of(&mut runtime, reply);

    let Some(oracle) = oracle_pi_1_keys() else {
        eprintln!("SKIPPING process_info oracle test: the system `erl` is unavailable");
        return;
    };
    assert_eq!(
        oracle.len(),
        17,
        "the oracle's process_info/1 emits 17 items for a registered process: {oracle:?}"
    );
    // Every item this runtime emits must appear in the oracle's list, in the same relative
    // order — the oracle order is the contract.
    let mut last: Option<usize> = None;
    for key in &ours {
        let position = oracle
            .iter()
            .position(|name| name == key)
            .unwrap_or_else(|| panic!("{key} is not an oracle process_info/1 item"));
        if let Some(last) = last {
            assert!(position > last, "{key} is out of the oracle's order");
        }
        last = Some(position);
    }
    assert_eq!(
        ours.len(),
        oracle.len() - 2,
        "the `init` stand-in has no engine (it never runs), so its current_function/initial_call \
         are omitted (see the module docs)"
    );
}

/// The four **execution-position** items are recognized (never `badarg`) and are the honest
/// typed capability gap for the process with no readable position: the `-eval` root process
/// making the call has no engine of its own (see the module docs).
#[test]
fn the_position_items_are_the_typed_capability_gap_for_the_callers_own_process() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    for name in [
        "current_function",
        "initial_call",
        "current_location",
        "current_stacktrace",
    ] {
        let error = info(&mut runtime, pid, name).expect_err("no engine ⇒ capability gap");
        let reason = crate::error::unsupported_reason(&error)
            .unwrap_or_else(|| panic!("{name} must be a typed `unsupported` gap, got {error}"));
        assert!(
            reason.contains(name),
            "the gap names the item it cannot answer: {reason}"
        );
    }
}

// -- `catchlevel` (the one gap closed) ------------------------------------

/// The committed vertical-slice module, reused by relative path (the entry engine of the
/// positive `catchlevel` case needs a real [`beam_loader::Module`]).
fn slice_fixture() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/slice.beam"
    )
}

/// A process table independent of the runtime (what a scheduler pass owns) holding **one**
/// scheduled process, plus that process's engine state — the shape that makes the
/// engine-derived items readable (see [`RuntimeContext::for_process_with_engines`]).
fn pass_table_with_one_engine() -> (ProcessTable, RunnerState, ProcessId) {
    let mut atoms = AtomTable::new();
    let mut table = ProcessTable::new(SignalAtoms::from_table(&mut atoms));
    let pid = table.spawn_default().expect("a fresh process");
    let mut engines = RunnerState::default();
    let module = Arc::new(beam_loader::load_file(slice_fixture()).expect("slice.beam"));
    let function = module
        .atom_table
        .lookup("add")
        .expect("`add` is interned in slice.beam");
    engines.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 2,
        },
        Vec::new(),
    );
    (table, engines, pid)
}

/// The same table shape, but the process's entry point is a **fun value**.
fn pass_table_with_one_fun_engine() -> (ProcessTable, RunnerState, ProcessId) {
    let mut atoms = AtomTable::new();
    let mut table = ProcessTable::new(SignalAtoms::from_table(&mut atoms));
    let pid = table.spawn_default().expect("a fresh process");
    let mut engines = RunnerState::default();
    engines.spawn_fun(pid, make_small(0), Vec::new());
    (table, engines, pid)
}

/// `catchlevel` is a **valid** oracle item, so a process whose engine this runtime does not
/// hold answers the typed capability gap — never `badarg` and never a fabricated `0`.
///
/// The `-eval` root process making the call is exactly such a process: its evaluator has no
/// engine of its own (see the module docs).
#[test]
fn catchlevel_is_the_typed_capability_gap_without_a_readable_engine() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    let error = info(&mut runtime, pid, "catchlevel").expect_err("no engine ⇒ capability gap");
    let reason = crate::error::unsupported_reason(&error)
        .unwrap_or_else(|| panic!("catchlevel must be a typed `unsupported` gap, got {error}"));
    assert!(
        reason.contains("catchlevel"),
        "the gap names the item it cannot answer: {reason}"
    );
    assert!(
        reason.contains("no engine"),
        "the gap names the missing piece (the engine), not a placeholder: {reason}"
    );
}

/// With a readable engine the item is answered **for real**: a process whose machine has run
/// no `try`/`catch` reports the integer `0`.
///
/// `0` is the oracle's own answer for a process parked outside any catch frame (see the
/// oracle cross-check below), and the value is tied to the engine's own
/// [`Machine::catch_frame_depth`](beam_engine::Machine::catch_frame_depth) — the C's
/// `catchlevel(rp)` (`beam_common.c:2347-2350`) — so a future regression in the wiring shows
/// up as a mismatch rather than a silent constant.
#[test]
fn catchlevel_with_a_readable_engine_is_the_live_catch_frame_depth() {
    let mut runtime = runtime();
    let (mut table, engines, pid) = pass_table_with_one_engine();
    let target = pid.to_eterm();
    let item = atom(&mut runtime, "catchlevel");
    let reply = {
        let mut context =
            RuntimeContext::for_process_with_engines(&mut runtime, &mut table, pid, &engines);
        context
            .call_bif("erlang", "process_info", 2, &[target, item])
            .expect("process_info/2 catchlevel")
    };
    let (key, value) = pair(reply);
    assert_eq!(runtime.atom_name_of(key), Some("catchlevel"));
    assert_eq!(
        signed_value(value),
        engines
            .engine_at(pid)
            .expect("the pid has an engine")
            .1
            .catch_frame_depth() as i64,
        "the item is the engine's own catch-frame depth"
    );
    assert_eq!(signed_value(value), 0, "a machine that ran no try/catch is at depth 0");
}

/// A **fun-bodied** process has an engine, but no module to name a position from — so
/// `catchlevel` stays the honest capability gap there too (the engine's depth would belong to
/// a launch that has not happened yet, so `0` would be a guess).
#[test]
fn catchlevel_is_the_gap_for_a_fun_bodied_process() {
    let mut runtime = runtime();
    let (mut table, engines, pid) = pass_table_with_one_fun_engine();
    let target = pid.to_eterm();
    let item = atom(&mut runtime, "catchlevel");
    let error = {
        let mut context =
            RuntimeContext::for_process_with_engines(&mut runtime, &mut table, pid, &engines);
        context
            .call_bif("erlang", "process_info", 2, &[target, item])
            .expect_err("a fun-bodied process has no readable engine")
    };
    let reason = crate::error::unsupported_reason(&error)
        .unwrap_or_else(|| panic!("catchlevel must be a typed `unsupported` gap, got {error}"));
    assert!(reason.contains("catchlevel"), "{reason}");
}

// -- the `binary` item ----------------------------------------------------

/// A process holding no off-heap refc binary reports the empty list `[]` — the oracle's own
/// answer, and the honest value for this runtime's root process (nothing registers an off-heap
/// payload for it).
#[test]
fn the_binary_item_is_the_empty_list_for_a_process_with_no_off_heap_binaries() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    let (key, value) = pair(info(&mut runtime, pid, "binary").expect("process_info/2 binary"));
    assert_eq!(runtime.atom_name_of(key), Some("binary"));
    assert_eq!(value, NIL, "a process with no off-heap binary reports []");
}

/// The `binary` roster is the process's **owned off-heap refc binaries** as `[{Ptr, Size,
/// Refc}, …]`, in slot order: one 3-tuple per payload, `Size` the payload's byte length and
/// `Refc` its total reference count. Every element is a non-negative integer (the payload
/// pointer materializes as an exact integer, never a truncated one).
#[test]
fn the_binary_item_lists_the_registered_off_heap_payloads() {
    let mut runtime = runtime();
    let pid_term = root_pid(&mut runtime);
    let pid = ProcessId::from_eterm(pid_term).expect("a pid");
    let sizes = [100usize, 300];
    let expected: Vec<(i64, i64, i64)> = {
        let process = runtime
            .processes
            .get_mut(pid)
            .expect("the root process is live");
        for size in sizes {
            process.register_off_heap_binary(
                beam_alloc::RefcBinary::new(&vec![0x5Au8; size]).expect("a refc binary"),
            );
        }
        process
            .off_heap_binaries()
            .map(|binary| {
                (
                    binary.data_ptr() as usize as i64,
                    binary.len() as i64,
                    binary.refcount() as i64,
                )
            })
            .collect()
    };
    assert_eq!(expected.len(), sizes.len(), "one entry per registered payload");

    let (key, value) = pair(info(&mut runtime, pid_term, "binary").expect("process_info/2 binary"));
    assert_eq!(runtime.atom_name_of(key), Some("binary"));
    let entries = terms::list_elements(value).expect("a list");
    assert_eq!(entries.len(), expected.len(), "one tuple per owned payload");
    for (entry, (ptr, size, refcount)) in entries.iter().zip(&expected) {
        assert_eq!(terms::tuple_arity(*entry).expect("a tuple"), 3, "a 3-tuple");
        let fields: Vec<i64> = (0..3)
            .map(|index| {
                signed_value(terms::tuple_element(*entry, index).expect("a tuple field"))
            })
            .collect();
        assert_eq!(fields, vec![*ptr, *size, *refcount]);
        assert!(fields[0] > 0, "the pointer is a positive integer: {fields:?}");
        assert!(fields[1] > 0, "the size is the payload's byte length: {fields:?}");
        assert!(fields[2] >= 1, "the refcount is at least 1: {fields:?}");
    }
    // The roster is in slot order, so the byte lengths come back in registration order.
    let listed: Vec<i64> = entries
        .iter()
        .map(|entry| signed_value(terms::tuple_element(*entry, 1).expect("size")))
        .collect();
    assert_eq!(listed, vec![100, 300]);
}

/// The roster follows the slot-stable release semantics: declaring an owned payload dead and
/// draining that declaration drops exactly that entry, and a second sweep releases nothing
/// further (a released slot is never reused).
#[test]
fn the_binary_item_drops_a_payload_once_its_death_is_swept() {
    let mut runtime = runtime();
    let pid_term = root_pid(&mut runtime);
    let pid = ProcessId::from_eterm(pid_term).expect("a pid");
    let (count, released) = {
        let process = runtime
            .processes
            .get_mut(pid)
            .expect("the root process is live");
        for size in [64usize, 128, 256] {
            process.register_off_heap_binary(
                beam_alloc::RefcBinary::new(&vec![0x11u8; size]).expect("a refc binary"),
            );
        }
        let before = process.off_heap_binary_count();
        assert!(process.declare_off_heap_binary_dead(1), "slot 1 is live");
        (before, process.sweep_off_heap_binaries())
    };
    assert_eq!(count, 3);
    assert_eq!(released, 1, "exactly the declared-dead payload is released");

    let (_, value) = pair(info(&mut runtime, pid_term, "binary").expect("process_info/2 binary"));
    let entries = terms::list_elements(value).expect("a list");
    assert_eq!(entries.len(), count - 1, "the released slot is gone");
    // The survivors are the two payloads that were never declared dead.
    let listed: Vec<i64> = entries
        .iter()
        .map(|entry| signed_value(terms::tuple_element(*entry, 1).expect("size")))
        .collect();
    assert_eq!(listed, vec![64, 256]);
}

// -- the remaining gaps ---------------------------------------------------

/// Every remaining gap names **what is missing** — the accessor/hook a future implementation
/// would need — rather than a vague placeholder.
#[test]
fn every_remaining_gap_names_its_missing_accessor() {
    use super::{UNSUPPORTED_ITEMS, UNSUPPORTED_ITEM_REASONS};

    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    // The reason table covers the gap list exactly: no gap without a reason, no stale reason.
    assert_eq!(
        UNSUPPORTED_ITEM_REASONS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>(),
        UNSUPPORTED_ITEMS.to_vec(),
        "every gap has exactly one reason row, and no row outlives its gap"
    );
    for (name, missing) in [("backtrace", "stack-dump")] {
        let error = info(&mut runtime, pid, name).expect_err("must be a capability gap");
        let reason = crate::error::unsupported_reason(&error)
            .unwrap_or_else(|| panic!("{name} must be a typed `unsupported` gap, got {error}"));
        assert!(reason.contains(name), "the gap names the item: {reason}");
        assert!(
            reason.contains(missing),
            "the gap names the missing piece ({missing:?}): {reason}"
        );
    }
}

// -- oracle cross-checks (bounded; skip loudly without the toolchain) -----

/// Runs `erl -noshell -eval EXPR` with a **bounded** poll/kill and returns its trimmed
/// stdout, or `None` when the system `erl` is unavailable or the probe did not finish (the
/// caller then skips **loudly**).
///
/// Every probe below ends in `halt()`, so it really terminates; the bound exists so a probe
/// that hangs (or an `erl` that never starts) can never wedge the suite.
fn oracle_eval(expression: &str) -> Option<String> {
    const ORACLE_TIMEOUT: Duration = Duration::from_secs(30);
    let mut child = Command::new("erl")
        .args(["-noshell", "-eval", expression])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + ORACLE_TIMEOUT;
    loop {
        match child.try_wait().ok()? {
            Some(status) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!(
                    "SKIPPING the process_info oracle test: `erl` did not exit within \
                     {ORACLE_TIMEOUT:?}"
                );
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    Some(text.trim().to_string())
}

/// `text` without any whitespace — `~p` wraps long terms across lines, and the comparison
/// cares about the terms, not the layout.
fn without_whitespace(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// The `catchlevel` semantics the C implements (`catchlevel(rp) = rp->catches`,
/// `beam_common.c:2347-2350`; the loader rewrites `try` into the single `catch` that
/// increments it, `emu/ops.tab:234`): a **parked process outside any `try`/`catch`** reports
/// `0`, while a process inside one reports a positive depth.
///
/// Measured on OTP 29.1.1 / erts-17.1: the probe prints `0 3` — the `3` is the `-eval`
/// *interpreter*'s own depth (`erl_eval` pushes two frames per nested `try`), so only the
/// shape (a positive integer while a handler is live) is asserted.
#[test]
fn the_oracle_reports_zero_catchlevel_outside_a_try_and_a_depth_inside_one() {
    let Some(line) = oracle_eval(
        "P = spawn(fun() -> receive stop -> ok end end), \
         io:format(\"~p ~p~n\", [element(2, process_info(P, catchlevel)), \
         (try element(2, process_info(self(), catchlevel)) catch _:_ -> err end)]), halt().",
    ) else {
        eprintln!("SKIPPING the catchlevel oracle test: the system `erl` is unavailable");
        return;
    };
    let mut fields = line.split_whitespace();
    assert_eq!(
        fields.next(),
        Some("0"),
        "a parked process outside any try has catchlevel 0: {line}"
    );
    let inside = fields
        .next()
        .and_then(|field| field.parse::<i64>().ok())
        .unwrap_or_else(|| panic!("a live catch frame reports an integer depth: {line}"));
    assert!(
        inside > 0,
        "a process inside a try reports a positive catchlevel: {line}"
    );
}

/// The **oracle values** of the remaining items this runtime cannot yet answer, so the gaps are
/// grounded in the real reference runtime rather than in a guess (OTP 29.1.1 / erts-17.1):
/// `magic_ref` is `[]`, `label` is `undefined`, `async_dist`/`priority_messages` are `false`,
/// `parent`/`backtrace` exist (a pid / a binary), and `garbage_collection_info` is the 11-key
/// per-generation proplist whose block detail this collector does not measure.
///
/// The probe also pins the `binary` item's *idle* value: a process holding no off-heap refc
/// binary answers the empty list `[]` — the shape this runtime now emits (see
/// [`the_oracle_lists_a_processes_off_heap_binaries`] for the non-empty case).
#[test]
fn the_oracle_backs_the_remaining_gaps_values() {
    let Some(line) = oracle_eval(
        "P = spawn(fun() -> receive stop -> ok end end), \
         io:format(\"~p~n\", [[element(1, process_info(P, magic_ref)), \
         element(2, process_info(P, magic_ref)), element(2, process_info(P, label)), \
         element(2, process_info(P, async_dist)), element(2, process_info(P, priority_messages)), \
         element(1, process_info(P, parent)), element(1, process_info(P, backtrace)), \
         is_binary(element(2, process_info(P, backtrace))), \
         [element(1, X) || X <- element(2, process_info(P, garbage_collection_info))], \
         element(2, process_info(P, binary))]]), halt().",
    ) else {
        eprintln!("SKIPPING the process_info gap oracle test: the system `erl` is unavailable");
        return;
    };
    assert_eq!(
        without_whitespace(&line),
        without_whitespace(
            "[magic_ref,[],undefined,false,false,parent,backtrace,true,\
             [old_heap_block_size,heap_block_size,mbuf_size,recent_size,stack_size,\
             old_heap_size,heap_size,bin_vheap_size,bin_vheap_block_size,\
             bin_old_vheap_size,bin_old_vheap_block_size],[]]"
        )
    );
}

/// The oracle's `binary` value is a **non-empty** list of `{Ptr, Size, Refc}` 3-tuples for a
/// process that really holds an off-heap refc binary — a `{binary, []}` would be a lie for that
/// reachable state. The roster is the process's off-heap binary **table**, pruned lazily at GC
/// rather than on drop, so it survives the handle going unreferenced (measured `true` on
/// OTP 29.1.1 / erts-17.1).
///
/// The comparison is **structural** — a non-empty list whose every element is a 3-tuple of
/// non-negative integers — never by `Ptr`/`Refc` value (this runtime's addresses and refcounts
/// are its own) nor by exact count (`binary:copy/2` may leave extra off-heap intermediates).
#[test]
fn the_oracle_lists_a_processes_off_heap_binaries() {
    let Some(line) = oracle_eval(
        "P = spawn(fun() -> _ = binary:copy(<<1,2,3>>,100), receive stop -> ok end end), \
         timer:sleep(100), L = element(2, process_info(P, binary)), \
         io:format(\"~p~n\", [L =/= [] andalso lists:all(fun(E) -> is_tuple(E) andalso \
         tuple_size(E) =:= 3 andalso lists:all(fun(X) -> is_integer(X) andalso X >= 0 end, \
         tuple_to_list(E)) end, L)]), halt().",
    ) else {
        eprintln!("SKIPPING the `binary` oracle test: the system `erl` is unavailable");
        return;
    };
    assert_eq!(
        line, "true",
        "a process holding a large binary lists a non-empty 3-tuple roster: {line}"
    );
}

/// The oracle's `binary` value is the **empty list** for a process that holds no off-heap refc
/// binary — the shape this runtime emits for such a process (measured `[]` on OTP 29.1.1 /
/// erts-17.1).
#[test]
fn the_oracle_reports_the_empty_list_for_a_process_with_no_off_heap_binary() {
    let Some(line) = oracle_eval(
        "P = spawn(fun() -> receive stop -> ok end end), timer:sleep(50), \
         io:format(\"~p~n\", [element(2, process_info(P, binary))]), halt().",
    ) else {
        eprintln!("SKIPPING the `binary` oracle test: the system `erl` is unavailable");
        return;
    };
    assert_eq!(line, "[]", "an idle process holds no off-heap binary: {line}");
}

// -- `monitored_by`: the local/remote rendering split ----------------------

/// Installs a **local** monitor on `victim` through the real `erlang:monitor/2` route, so the
/// caller (the runtime's root process) becomes one of the victim's incoming
/// [`MonitoredBy`](beam_process::MonitoredBy) records — the very store the `monitored_by` item
/// reads (`Process::monitored_by`). Returns the monitor reference the BIF answers.
fn arm_local_monitor(runtime: &mut crate::Runtime, victim: Eterm) -> Eterm {
    let process = atom(runtime, "process");
    call(runtime, "erlang", "monitor", &[process, victim]).expect("erlang:monitor/2")
}

/// `victim`'s `monitored_by` value as a **bare pid list** — the oracle's own shape
/// (`{monitored_by,[<0.84.0>]}`, verified against OTP 29.1.1 / erts-17.1). The `{Item, Value}`
/// wrapper belongs to [`pair`], one level up, so a value that came back wrapped would fail here.
fn watchers_of(runtime: &mut crate::Runtime, victim: Eterm) -> Vec<Eterm> {
    let (key, value) = pair(info(runtime, victim, "monitored_by").expect("monitored_by"));
    assert_eq!(runtime.atom_name_of(key), Some("monitored_by"));
    terms::list_elements(value).expect("a bare pid list, not a wrapped item")
}

/// The `monitored_by` item renders its two watcher families **through two different code
/// paths** ([`RuntimeContext::monitored_by_value`]): a **local** watcher is an internal pid
/// immediate and the plain `terms::make_list` fast path suffices, while a **wire-origin
/// (remote)** watcher must be materialized as the four-word *external* pid block
/// [`RemotePid::to_eterm`] writes on a scratch `beam_alloc::Heap`, and the finished list then
/// copied out through `rehome_container_result`.
///
/// Both halves are pinned here at the **builder** level, hermetically (no `erl`/`erlc`/`epmd`,
/// no peer VM, no socket): the two opt-in two-VM suites
/// (`tests/dist_two_rust.rs`, `tests/dist_two_vm.rs`) cover the same split only end to end, so a
/// regression in it would otherwise be invisible in the default suite.
///
/// The remote watcher is installed through the **real inbound entry point** an arriving
/// `DOP_MONITOR_P` reaches — [`ProcessTable::monitor_from_remote_ref`] with a peer-minted
/// external reference, the wire shape `dist.c:2341` carries. Every identity field is asserted
/// **separately** (node, number, serial, creation, all four mutually distinct), which is what
/// makes a swapped-field `to_eterm`/`from_eterm` regression fail instead of passing on
/// "it is an external pid".
#[test]
fn the_monitored_by_item_renders_a_local_and_a_wire_origin_watcher_apart() {
    let mut runtime = runtime();
    let victim = runtime.processes.spawn_default().expect("a victim process");
    let victim_pid = victim.to_eterm();
    let root = root_pid(&mut runtime);
    assert_ne!(victim_pid, root, "the victim is a child, never the caller");

    // (1) LOCAL ONLY — the fast path: one internal pid immediate, nothing on a scratch heap.
    let _reference = arm_local_monitor(&mut runtime, victim_pid);
    let local_only = watchers_of(&mut runtime, victim_pid);
    assert_eq!(
        local_only.len(),
        1,
        "the root process is the victim's only watcher: {local_only:?}"
    );
    assert!(
        ProcessId::is_process(local_only[0]),
        "a local watcher is an internal pid immediate"
    );
    assert_eq!(local_only[0], root, "and it is the pid that armed it");
    assert_eq!(
        PidTarget::from_eterm(local_only[0]).expect("a pid"),
        PidTarget::Local(ProcessId::from_eterm(root).expect("an internal pid")),
        "the local entry classifies as a local target"
    );

    // (2) The inbound `DOP_MONITOR_P`: another node's process watches the same victim. The node
    // is **not** this runtime's local node (a `Runtime::with_defaults()` runtime is not
    // distributed), so the entry cannot be mistaken for a local pid.
    let peer_node = atom(&mut runtime, "peer@host.example");
    let (number, serial, creation) = (7u32, 3u32, 0x1234_5678u32);
    let mut peer_heap = beam_alloc::Heap::new(64).expect("a scratch heap for the peer's terms");
    let peer_pid_term =
        beam_codec::term::make_external_pid(&mut peer_heap, peer_node, number, serial, creation)
            .expect("the peer's external pid");
    let watcher = RemotePid::from_eterm(peer_pid_term).expect("a remote watcher");
    let peer_ref_term =
        beam_codec::term::make_external_ref(&mut peer_heap, peer_node, &[7, 11], creation)
            .expect("the peer's monitor reference");
    let peer_ref = RemoteRef::from_eterm(peer_ref_term).expect("a peer-minted wire reference");
    runtime
        .processes
        .monitor_from_remote_ref(victim, watcher, peer_ref, MonitorKind::Process)
        .expect("record the inbound DOP_MONITOR_P");

    // (3) BOTH FAMILIES — the two-step path, in the documented local-then-remote order. (The
    // oracle orders this item by monitor creation, which this runtime cannot observe: it keeps
    // the two families in two stores. So this pins *this runtime's own documented choice*, not
    // an oracle-observable order — the same caveat the `monitors` item carries.)
    let both = watchers_of(&mut runtime, victim_pid);
    assert_eq!(
        both.len(),
        2,
        "the bare pid list holds one entry per watcher: {both:?}"
    );
    assert_eq!(both[0], root, "local watchers come first");
    assert!(
        ProcessId::is_process(both[0]),
        "the local watcher keeps its internal pid immediate"
    );

    let remote_entry = both[1];
    assert!(
        !ProcessId::is_process(remote_entry),
        "the remote watcher is NOT an internal pid immediate"
    );
    let decoded =
        RemotePid::from_eterm(remote_entry).expect("the remote entry is an external pid block");
    assert_eq!(decoded.node(), peer_node, "the peer's node atom survives");
    assert_eq!(decoded.number(), number, "the peer's pid number survives");
    assert_eq!(decoded.serial(), serial, "the peer's pid serial survives");
    assert_eq!(decoded.creation(), creation, "the peer's creation survives");
    assert!(
        matches!(
            PidTarget::from_eterm(remote_entry).expect("a pid"),
            PidTarget::Remote(_)
        ),
        "the remote entry classifies as a remote target"
    );

    // (4) THE TWO PATHS AGREE: an inbound `DOP_DEMONITOR_P` takes the victim back to the
    // local-only case, and the answer is *exactly* the fast path's value again — so the
    // two-step copy-out never leaves a mangled local half behind.
    runtime
        .processes
        .demonitor_from_remote_ref(victim, peer_ref)
        .expect("record the inbound DOP_DEMONITOR_P");
    assert_eq!(
        watchers_of(&mut runtime, victim_pid),
        local_only,
        "the fast path and the two-step path agree on the local-only case"
    );
}

// -- `links`: the local + inbound-remote union (contract A.5) --------------

/// `victim`'s `links` value as a **bare list of pid terms** — the oracle's own shape: the C's
/// `am_links` arm conses each collected link onto the result as a bare term, never as a tuple
/// (`erl_bif_info.c:3689-3707`). The `{Item, Value}` wrapper belongs to [`pair`], one level up,
/// so a value that came back wrapped would fail here.
fn links_of(runtime: &mut crate::Runtime, victim: Eterm) -> Vec<Eterm> {
    let (key, value) = pair(info(runtime, victim, "links").expect("links"));
    assert_eq!(runtime.atom_name_of(key), Some("links"));
    terms::list_elements(value).expect("a bare pid list, not a wrapped item")
}

/// The `links` item is the **union** of the two halves this runtime keeps in two stores, mirroring
/// the C's single link tree: the local links
/// ([`Process::links`](beam_process::Process::links)) and the ones an inbound `DOP_LINK` recorded
/// ([`ProcessTable::remote_links`](beam_process::ProcessTable::remote_links)). The C collects
/// **every** link type — `collect_one_link` (`erl_bif_info.c:575-593`) skips only an `unlinking`
/// link — including `ERTS_LNK_TYPE_DIST_PROC`, whose `other.item` is the external pid, so a
/// process a peer linked must name that peer instead of answering `[]`.
///
/// Both halves are pinned here at the **builder** level, hermetically (no `erl`/`erlc`/`epmd`, no
/// peer VM, no socket), through [`RuntimeContext::links_value`]: the fast path (immediates only)
/// and the two-step path (a scratch `beam_alloc::Heap` for the external-pid block, then
/// `rehome_container_result`). The remote link is installed through the **real inbound entry
/// point** an arriving `DOP_LINK` reaches — [`ProcessTable::record_remote_link`] — and removed
/// through the real `erlang:unlink/1` target
/// ([`ProcessTable::unlink_term`](beam_process::ProcessTable::unlink_term)).
///
/// Only the **shape** is asserted: a foreign pid cannot be printed by this runtime
/// (`beam_bifs`'s printer answers the typed gap for an external pid), and the oracle's
/// `<Channel.Creation.Serial>` text is not pinnable across VMs. Every identity field is checked
/// **separately** through [`RemotePid`], which is what makes a swapped-field
/// `to_eterm`/`from_eterm` regression fail instead of passing on "it is an external pid".
#[test]
fn the_links_item_unions_a_local_link_with_an_inbound_distribution_link() {
    let mut runtime = runtime();
    let victim = runtime.processes.spawn_default().expect("a victim process");
    let victim_pid = victim.to_eterm();
    let root = root_pid(&mut runtime);
    assert_ne!(victim_pid, root, "the victim is a child, never the caller");

    // (1) NO LINKS — the empty list, and the remote half is what makes that non-trivial: nothing
    // is recorded for the victim on either side yet.
    assert_eq!(
        links_of(&mut runtime, victim_pid),
        Vec::new(),
        "a process nothing linked answers the empty list"
    );

    // (2) LOCAL ONLY — the fast path: one internal pid immediate, nothing on a scratch heap.
    let local_peer = runtime
        .processes
        .spawn_default()
        .expect("a local peer process");
    runtime
        .processes
        .link(victim, local_peer)
        .expect("record a local link");
    let local_only = links_of(&mut runtime, victim_pid);
    assert_eq!(
        local_only.len(),
        1,
        "the local link is the victim's only link: {local_only:?}"
    );
    assert!(
        ProcessId::is_process(local_only[0]),
        "a local link is an internal pid immediate"
    );
    assert_eq!(local_only[0], local_peer.to_eterm(), "and it is the linked pid");

    // (3) The inbound `DOP_LINK`: another node's process links the victim. The node is **not**
    // this runtime's local node (a `Runtime::with_defaults()` runtime is not distributed), so the
    // entry cannot be mistaken for a local pid.
    let peer_node = atom(&mut runtime, "peer@host.example");
    let (number, serial, creation) = (11u32, 5u32, 0x0BAD_F00Du32);
    let mut peer_heap = beam_alloc::Heap::new(64).expect("a scratch heap for the peer's terms");
    let peer_pid_term =
        beam_codec::term::make_external_pid(&mut peer_heap, peer_node, number, serial, creation)
            .expect("the peer's external pid");
    let peer = RemotePid::from_eterm(peer_pid_term).expect("a remote peer");
    runtime
        .processes
        .record_remote_link(victim, peer)
        .expect("record the inbound DOP_LINK");

    // (4) BOTH HALVES — the two-step path, in the documented local-then-remote order. (The oracle
    // does not pin this item's order: the C emits the link-tree traversal's reverse and
    // `*_rbt_foreach`'s order is documented undefined, `erl_rbtree.h:164-169`. So this pins *this
    // runtime's own documented choice* — a caller must treat the item as a set, which is what
    // `lists:member/2` does — the same caveat the `monitors`/`monitored_by` items carry.)
    let both = links_of(&mut runtime, victim_pid);
    assert_eq!(
        both.len(),
        2,
        "one entry per link, the local one plus the wire-origin one: {both:?}"
    );
    assert_eq!(both[0], local_peer.to_eterm(), "local links come first");
    assert!(
        ProcessId::is_process(both[0]),
        "the local link keeps its internal pid immediate"
    );

    let remote_entry = both[1];
    assert!(
        !ProcessId::is_process(remote_entry),
        "the remote link is NOT an internal pid immediate"
    );
    let decoded =
        RemotePid::from_eterm(remote_entry).expect("the remote entry is an external pid block");
    assert_eq!(decoded.node(), peer_node, "the peer's node atom survives");
    assert_eq!(decoded.number(), number, "the peer's pid number survives");
    assert_eq!(decoded.serial(), serial, "the peer's pid serial survives");
    assert_eq!(decoded.creation(), creation, "the peer's creation survives");
    assert!(
        matches!(
            PidTarget::from_eterm(remote_entry).expect("a pid"),
            PidTarget::Remote(_)
        ),
        "the remote entry classifies as a remote target"
    );

    // (4b) `process_info/1` picks the item up **automatically**: it walks `PI_1_KEYS` through the
    // very same `raw_value` arm, so the fixed value is what that reply carries too — no separate
    // change is needed for it.
    let one = call(&mut runtime, "erlang", "process_info", &[victim_pid]).expect("process_info/1");
    let (_, from_one) = entries_of(one)
        .into_iter()
        .find(|(key, _)| runtime.atom_name_of(*key) == Some("links"))
        .expect("process_info/1 emits the links item");
    let from_one = terms::list_elements(from_one).expect("a bare pid list");
    assert_eq!(
        from_one.len(),
        both.len(),
        "process_info/1's links item is the same local-then-remote union"
    );
    assert_eq!(
        from_one[0], both[0],
        "the local link is the same pid immediate in both routes"
    );
    // The remote entry is a **copy** of the external-pid block, so it lives at a different heap
    // address and is compared part by part, never word by word.
    let from_one_remote = RemotePid::from_eterm(from_one[1]).expect("an external pid block");
    assert_eq!(from_one_remote.node(), decoded.node(), "same peer node");
    assert_eq!(from_one_remote.number(), decoded.number(), "same peer number");
    assert_eq!(from_one_remote.serial(), decoded.serial(), "same peer serial");
    assert_eq!(
        from_one_remote.creation(),
        decoded.creation(),
        "same peer creation"
    );

    // (5) THE TWO PATHS AGREE: the peer's `unlink` removes its recorded link, taking the victim
    // back to the local-only case — and the answer is *exactly* the fast path's value again, so
    // the two-step copy-out never leaves a mangled local half behind.
    runtime
        .processes
        .unlink_term(victim, peer_pid_term)
        .expect("record the peer's unlink");
    assert_eq!(
        links_of(&mut runtime, victim_pid),
        local_only,
        "the fast path and the two-step path agree on the local-only case"
    );
}

/// The `last_calls` value rendered as names: a recorded call as `Module:Function/Arity`, a pseudo
/// entry as its bare atom. An atom value (the unarmed `false`) renders as the atom alone.
fn last_calls_names(runtime: &mut crate::Runtime, pid: Eterm) -> Vec<String> {
    let (key, value) = pair(info(runtime, pid, "last_calls").expect("last_calls"));
    assert_eq!(runtime.atom_name_of(key), Some("last_calls"));
    if is_atom(value) {
        return vec![runtime.atom_name_of(value).expect("an atom").to_string()];
    }
    let entries = terms::list_elements(value).expect("the armed ring is a list");
    entries
        .iter()
        .map(|entry| {
            if is_atom(*entry) {
                return runtime.atom_name_of(*entry).expect("an atom entry").to_string();
            }
            assert_eq!(
                terms::tuple_arity(*entry).expect("a recorded call is a tuple"),
                3,
                "a recorded call is a {{Module, Function, Arity}} triple"
            );
            let module = terms::tuple_element(*entry, 0).expect("module");
            let function = terms::tuple_element(*entry, 1).expect("function");
            let arity = terms::tuple_element(*entry, 2).expect("arity");
            format!(
                "{}:{}/{}",
                runtime.atom_name_of(module).expect("module atom"),
                runtime.atom_name_of(function).expect("function atom"),
                signed_value(arity)
            )
        })
        .collect()
}

/// `last_calls` has exactly three states, and the armed one reports the ring in **recorded order**
/// (oldest first) — the order the oracle reports it in, because `ERTS_PI_IX_LAST_CALLS` walks the
/// ring newest-first but **prepends** each rendered term (`list = CONS(hp, term, list)`,
/// `erl_bif_info.c:2222`).
///
/// Measured on OTP 29.1.1 / erts-17.1, so the assertions below are the oracle's answers, not an
/// implementation preference: an armed process whose calls were `lists:seq/2` then one compiled
/// `!` then a `receive` answers `[{lists,seq,2}, {erlang,send,2}, send, 'receive']`, with the
/// querying `{erlang,process_info,2}` at the **tail**.
#[test]
fn the_last_calls_item_has_the_oracles_three_states_and_reports_the_ring_in_recorded_order() {
    use crate::native::memory::last_calls::SavedEntry;

    let mut runtime = runtime();
    let pid_term = root_pid(&mut runtime);
    let pid = ProcessId::from_eterm(pid_term).expect("a pid");

    // (1) No buffer armed: the bare atom `false` — both never-armed and explicitly disarmed.
    assert_eq!(last_calls_names(&mut runtime, pid_term), ["false"]);
    runtime.arm_saved_calls(pid, 0);
    assert_eq!(
        last_calls_names(&mut runtime, pid_term),
        ["false"],
        "process_flag(save_calls, 0) disarms, so the item is `false` again"
    );

    // (2) Armed and empty: the empty list (the oracle's answer for an idle armed process).
    runtime.arm_saved_calls(pid, 8);
    assert!(
        last_calls_names(&mut runtime, pid_term).is_empty(),
        "an armed ring with nothing recorded answers []"
    );

    // (3) Armed and filling: recorded order, oldest first, with the `send` pair.
    runtime.record_saved_call(pid, "lists", "seq", 2);
    runtime.record_saved_send(pid);
    runtime.record_saved_pseudo(pid, SavedEntry::Receive);
    assert_eq!(
        last_calls_names(&mut runtime, pid_term),
        ["lists:seq/2", "erlang:send/2", "send", "receive"],
        "the ring is chronological and a send records its call entry then the pseudo entry"
    );

    // The `erlang:send/2`-BIF route records the call entry through the ordinary external-call
    // seam *before* the send funnel runs, so the funnel must not add a second copy of it.
    runtime.record_saved_call(pid, "erlang", "send", 2);
    runtime.record_saved_send(pid);
    assert_eq!(
        last_calls_names(&mut runtime, pid_term),
        [
            "lists:seq/2",
            "erlang:send/2",
            "send",
            "receive",
            "erlang:send/2",
            "send"
        ],
        "a send whose call entry the call seam just recorded adds only the pseudo entry"
    );

    // Re-arming resets the ring (the C installs a brand-new zeroed buffer).
    runtime.arm_saved_calls(pid, 8);
    assert!(
        last_calls_names(&mut runtime, pid_term).is_empty(),
        "re-arming discards the previous entries"
    );
}

/// The `parent` item of a process the runtime never saw being spawned (the root, the action
/// child `Runtime::as_action_process` runs the body in, and any pid without a recorded
/// spawner) is a **distinct** pid standing in for OTP's `init`.
///
/// The oracle's `-eval` process answers `{parent, <0.0.0>}` — `init`, never itself (measured on OTP
/// 29.1.1 / erts-17.1: `element(2, process_info(self(), parent)) =:= self()` is `false`).
/// Answering the runtime's own root pid would name a process that is **not** OTP's `init`, so
/// [`crate::Runtime::init_pid`] — a real, never-enqueued process of its own — plays `init`'s role
/// instead, and a spawned child records its real spawner.
#[test]
fn the_parent_item_is_the_init_stand_in_and_a_spawned_child_names_its_spawner() {
    let mut runtime = runtime();
    let root_term = root_pid(&mut runtime);
    let root = ProcessId::from_eterm(root_term).expect("a pid");
    let init = runtime.init_pid;
    assert_ne!(init, root, "the init stand-in is a pid of its own");
    assert_eq!(
        init.to_eterm(),
        ProcessId::new(0, 0).to_eterm(),
        "it takes the table's first identity, so it renders as the oracle's `<0.0.0>` init"
    );

    let (key, value) = pair(info(&mut runtime, root_term, "parent").expect("parent"));
    assert_eq!(runtime.atom_name_of(key), Some("parent"));
    assert_eq!(value, init.to_eterm(), "the root's parent is the init stand-in");
    assert_ne!(value, root_term, "…which is never the process itself");

    // A pid with a recorded spawner reports it — the `{parent, SpawnerPid}` the oracle answers for
    // every spawned process, at any depth (`Runtime::set_parent` is the one writer).
    let spawner = ProcessId::new(4242, 1);
    runtime.set_parent(root, spawner);
    assert_eq!(runtime.parent_of(root), spawner);
    let (_, value) = pair(info(&mut runtime, root_term, "parent").expect("parent"));
    assert_eq!(value, spawner.to_eterm(), "the recorded spawner is what the item reports");
    assert_eq!(
        runtime.parent_of(ProcessId::new(4243, 0)),
        init,
        "an unrecorded pid still falls back to the init stand-in"
    );
}
