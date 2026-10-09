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

use std::process::Command;

use beam_bifs::NullSink;
use beam_common::BeamError;
use beam_engine::terms;
use beam_gc::HeapObservable;
use beam_process::ProcessId;
use beam_term::{atom_val, is_atom, make_small, signed_value, Eterm, NIL};

use crate::context::RuntimeContext;
use crate::native::test_support::{atom, call, runtime};

/// The runtime's own root process — a live process in the process table.
fn root_pid(runtime: &mut crate::Runtime) -> Eterm {
    runtime.self_pid().to_eterm()
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
    let pid = root_pid(&mut runtime);
    let reply = call(&mut runtime, "erlang", "process_info", &[pid]).expect("process_info/1");
    let keys = keys_of(&mut runtime, reply);
    // The root process is registered as `init`, so `registered_name` is emitted (wrapped)
    // first. Its four engine-derived items are honestly omitted (the `-eval` root process has
    // no engine — see the module docs) — leaving these 15, in the oracle's relative order
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
    let pid = root_pid(&mut runtime);
    // The root process is registered as `init`: the single-atom form is the WRAPPED pair.
    let (key, value) = pair(info(&mut runtime, pid, "registered_name").expect("process_info/2"));
    assert_eq!(runtime.atom_name_of(key), Some("registered_name"));
    assert_eq!(runtime.atom_name_of(value), Some("init"));
    // The list form is wrapped too.
    let name = atom(&mut runtime, "registered_name");
    let list = make_list(&mut runtime, &[name]);
    let reply = call(&mut runtime, "erlang", "process_info", &[pid, list]).expect("process_info/2");
    let (key, value) = pair(terms::list_elements(reply).expect("a list")[0]);
    assert_eq!(runtime.atom_name_of(key), Some("registered_name"));
    assert_eq!(runtime.atom_name_of(value), Some("init"));
    // Unregister `init` so the root becomes a LIVE, unregistered pid: the single-atom form is
    // then the BARE `[]` (the oracle's `always_wrap = 0` nil case), while the list form wraps it.
    let init = atom(&mut runtime, "init");
    call(&mut runtime, "erlang", "unregister", &[init]).expect("unregister");
    assert_eq!(info(&mut runtime, pid, "registered_name").expect("process_info/2"), NIL);
    let name = atom(&mut runtime, "registered_name");
    let list = make_list(&mut runtime, &[name]);
    let reply = call(&mut runtime, "erlang", "process_info", &[pid, list]).expect("process_info/2");
    let (key, value) = pair(terms::list_elements(reply).expect("a list")[0]);
    assert_eq!(runtime.atom_name_of(key), Some("registered_name"));
    assert_eq!(value, NIL);
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

#[test]
fn every_unsupported_item_raises_the_typed_capability_gap() {
    let mut runtime = runtime();
    let pid = root_pid(&mut runtime);
    for name in [
        "catchlevel",
        "backtrace",
        "magic_ref",
        "parent",
        "label",
        "async_dist",
        "priority_messages",
        "garbage_collection_info",
        "binary",
    ] {
        let error = info(&mut runtime, pid, name).expect_err("must be a capability gap");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "{name} must be a typed `unsupported` gap, got {error}"
        );
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
    let (heap_size, total_heap_size) = {
        let process = runtime
            .processes()
            .get(runtime.self_pid())
            .expect("the root process is live");
        (
            process.collector().heap_size_words(),
            process.collector().total_heap_size_words(),
        )
    };
    assert_eq!(
        signed_value(pair(info(&mut runtime, pid, "heap_size").expect("heap_size")).1),
        heap_size as i64
    );
    assert_eq!(
        signed_value(pair(info(&mut runtime, pid, "total_heap_size").expect("total_heap_size")).1),
        total_heap_size as i64
    );
    // `Process::new` uses `HeapPolicy::default()`: a 233-word nursery, so `heap_size` agrees
    // with the oracle's 233, while the pre-allocated 233-word mature space makes
    // `total_heap_size` 466 — see this module's documented divergence.
    assert_eq!(heap_size, 233);
    assert_eq!(total_heap_size, 466);
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
    let pid = root_pid(&mut runtime);
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
        "the `-eval` root process has no engine, so its current_function/initial_call are \
         omitted (see the module docs)"
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
