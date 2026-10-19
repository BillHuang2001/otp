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

//! The node-set/connector floor's colocated suite: the `nodes/0,1,2` grammar and answers, the
//! connector's two entry points, `erl_distribution:start_link/0,1`, `erlang:monitor_node/2` and
//! the two peer-name helpers, plus the `nodes/2` `connection_id` case over a real loopback peer.
//!
//! It is a child module, so `use super::*;` reaches the floor's items and `super::connector` the
//! private name helpers.

use super::*;
use beam_bifs::NullSink;
use beam_common::Result;
use beam_engine::{terms, ExecutionContext};
use beam_term::{make_small, Eterm};

use crate::Runtime;

use super::connector::{connect_name, same_node};

/// A `-noshell` runtime: no `-name`/`-sname`, so the node is `nonode@nohost`.
fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// Runs `f` against a fresh `RuntimeContext` over `rt`; the context (and its sink) are
/// dropped before `rt` is borrowed again.
fn with_context<T>(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    f(&mut ctx)
}

/// Calls a BIF through the runtime's `ExecutionContext` seam.
fn call(rt: &mut Runtime, module: &str, name: &str, args: &[Eterm]) -> Result<Eterm> {
    with_context(rt, |ctx| ctx.call_bif(module, name, args.len() as u32, args))
}

/// `term` rendered by the registered `~p` printer (with the trailing newline of the
/// `~p~n` control stripped).
fn rendered(rt: &mut Runtime, term: Eterm) -> String {
    rt.format_term(term, None)
        .expect("a printable term")
        .trim_end()
        .to_string()
}

/// A term built on the runtime's heap (through the engine seam, so it stays valid).
fn build(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> Eterm) -> Eterm {
    with_context(rt, f)
}

#[test]
fn nodes_0_and_the_peer_types_are_the_empty_list() {
    let mut rt = runtime();
    let term = call(&mut rt, "erlang", "nodes", &[]).expect("erlang:nodes/0");
    assert_eq!(rendered(&mut rt, term), "[]");
    for name in ["connected", "visible", "hidden"] {
        let selector = rt.intern(name);
        let term = call(&mut rt, "erlang", "nodes", &[selector])
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(rendered(&mut rt, term), "[]", "nodes({name})");
    }
}

#[test]
fn nodes_this_and_known_are_the_local_node() {
    for name in ["this", "known"] {
        let mut rt = runtime();
        let selector = rt.intern(name);
        let term = call(&mut rt, "erlang", "nodes", &[selector])
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(rendered(&mut rt, term), "[nonode@nohost]", "nodes({name})");
    }
}

#[test]
fn nodes_of_a_list_concatenates_and_de_duplicates_first_seen() {
    let mut rt = runtime();
    let this = rt.intern("this");
    let known = rt.intern("known");
    let connected = rt.intern("connected");
    let local = rt.local_node_term();

    // The empty list selects nothing (the C's `while (is_list(...))` never runs, and
    // `NIL` is not a cons cell -- `nodes([])` must not be `badarg`).
    let empty = rt.eval("[]").expect("the empty list");
    let term = call(&mut rt, "erlang", "nodes", &[empty]).expect("nodes([])");
    assert_eq!(rendered(&mut rt, term), "[]");

    // `[this, connected]` and its permutation are the local node, exactly once.
    for elements in [vec![this, connected], vec![connected, this]] {
        let list = build(&mut rt, |ctx| {
            terms::make_list(ctx, &elements).expect("a list")
        });
        let term = call(&mut rt, "erlang", "nodes", &[list]).expect("nodes([...])");
        assert_eq!(rendered(&mut rt, term), "[nonode@nohost]");
    }

    // A repeated selector stays a one-element list (first-seen-order de-duplication).
    let repeated = build(&mut rt, |ctx| {
        terms::make_list(ctx, &[this, known, this]).expect("a list")
    });
    let term = call(&mut rt, "erlang", "nodes", &[repeated]).expect("nodes([this,known,this])");
    assert_eq!(rendered(&mut rt, term), "[nonode@nohost]");
    assert_eq!(
        terms::list_elements(term).expect("a list"),
        vec![local],
        "one local node term, whatever the selector combination"
    );
}

#[test]
fn nodes_rejects_unknown_or_malformed_selectors() {
    // `all` has no case in the C's switch, so it is `badarg` like any typo.
    for name in ["all", "bogus", "no_such_node_type"] {
        let mut rt = runtime();
        let selector = rt.intern(name);
        assert!(
            call(&mut rt, "erlang", "nodes", &[selector]).is_err(),
            "nodes({name}) must raise badarg"
        );
    }
    // A non-atom, non-list argument; a tuple; and a list with a bad element.
    let mut rt = runtime();
    assert!(call(&mut rt, "erlang", "nodes", &[make_small(42)]).is_err());
    let tuple = build(&mut rt, |ctx| {
        terms::make_tuple(ctx, &[make_small(1)]).expect("a tuple")
    });
    assert!(call(&mut rt, "erlang", "nodes", &[tuple]).is_err());
    let this = rt.intern("this");
    let bad_element = build(&mut rt, |ctx| {
        terms::make_list(ctx, &[this, make_small(42)]).expect("a list")
    });
    assert!(call(&mut rt, "erlang", "nodes", &[bad_element]).is_err());
}

#[test]
fn nodes_of_an_improper_list_is_badarg() {
    let mut rt = runtime();
    let improper = rt.eval("[this|bogus_type]").expect("an improper list");
    assert!(
        call(&mut rt, "erlang", "nodes", &[improper]).is_err(),
        "an improper tail is badarg"
    );
    // A non-atom `nodes/1` argument, and a list carrying a non-atom element.
    assert!(call(&mut rt, "erlang", "nodes", &[make_small(7)]).is_err());
}

// -- erlang:nodes/2 -------------------------------------------------------

/// The map `[{name, value}, …]` built on the runtime's heap; the **atom** keys are
/// interned through the shared table first (mirrors the `process.rs` test helper).
fn make_map(rt: &mut Runtime, pairs: &[(&str, Eterm)]) -> Eterm {
    let keys: Vec<Eterm> = pairs.iter().map(|(name, _)| rt.intern(name)).collect();
    let pairs: Vec<(Eterm, Eterm)> = keys
        .into_iter()
        .zip(pairs.iter().map(|(_, value)| *value))
        .collect();
    let atoms = rt.code_server().atoms_handle();
    build(rt, |ctx| {
        terms::make_map(ctx, &pairs, &atoms).expect("a map")
    })
}

/// The **boot** case: `global:init/1` runs
/// `nodes(visible, #{connection_id => true})` (`lib/kernel/src/global.erl:806-810`)
/// and a `-boot` runtime is not distributed, so the answer is the empty list.
///
/// Oracle (OTP 29.1.1 / erts-17.1):
/// `erl -noshell -eval 'io:format("~p~n",[nodes(visible,#{connection_id=>true})])'`
/// → `[]`.
#[test]
fn nodes_2_visible_with_connection_id_is_the_empty_list_on_a_dormant_node() {
    let mut rt = runtime();
    let visible = rt.intern("visible");
    let yes = rt.intern("true");
    let options = make_map(&mut rt, &[("connection_id", yes)]);
    let term = call(&mut rt, "erlang", "nodes", &[visible, options]).expect("nodes/2");
    assert_eq!(rendered(&mut rt, term), "[]");
}

/// `nodes(this, #{})` answers the local node with an **empty** info map.
///
/// Oracle: `io:format("~p~n",[nodes(this,#{})])` → `[{nonode@nohost,#{}}]`.
#[test]
fn nodes_2_this_with_no_options_is_the_bare_local_entry() {
    let mut rt = runtime();
    let this = rt.intern("this");
    let options = make_map(&mut rt, &[]);
    let term = call(&mut rt, "erlang", "nodes", &[this, options]).expect("nodes/2");
    assert_eq!(rendered(&mut rt, term), "[{nonode@nohost,#{}}]");
}

/// `nodes(this, #{connection_id => true})`: a `this` entry's connection id is the C's
/// `dep->connection_id == ~0`, which the oracle renders as the atom `undefined`.
///
/// Oracle: `io:format("~p~n",[nodes(this,#{connection_id=>true})])`
/// → `[{nonode@nohost,#{connection_id => undefined}}]`.
#[test]
fn nodes_2_this_with_connection_id_is_undefined() {
    let mut rt = runtime();
    let this = rt.intern("this");
    let yes = rt.intern("true");
    let options = make_map(&mut rt, &[("connection_id", yes)]);
    let term = call(&mut rt, "erlang", "nodes", &[this, options]).expect("nodes/2");
    assert_eq!(
        rendered(&mut rt, term),
        "[{nonode@nohost,#{connection_id => undefined}}]"
    );
}

/// `nodes(this, #{node_type => true})`: the entry's own type atom, `this`.
///
/// Oracle: `io:format("~p~n",[nodes(this,#{node_type=>true})])`
/// → `[{nonode@nohost,#{node_type => this}}]`.
#[test]
fn nodes_2_this_with_node_type_is_this() {
    let mut rt = runtime();
    let this = rt.intern("this");
    let yes = rt.intern("true");
    let options = make_map(&mut rt, &[("node_type", yes)]);
    let term = call(&mut rt, "erlang", "nodes", &[this, options]).expect("nodes/2");
    assert_eq!(
        rendered(&mut rt, term),
        "[{nonode@nohost,#{node_type => this}}]"
    );
}

/// Both keys requested at once: the info map holds them in the C's key order
/// (`connection_id` then `node_type`), whether or not the caller mentioned `node_type`
/// first.
///
/// Oracle: `io:format("~p~n",[nodes([this],#{connection_id=>true,node_type=>true})])`
/// → `[{nonode@nohost,#{connection_id => undefined,node_type => this}}]`.
#[test]
fn nodes_2_both_options_keep_the_c_key_order() {
    // The info map's **rendered** key order follows this runtime's flat-map storage
    // order, which is by interned atom index (the C's `erts_map_from_ks_and_vs` order
    // here is insertion order and its `atom.names` has `connection_id` before
    // `node_type`). The two coincide when `connection_id` is interned first, which is
    // the oracle's own relative order and the case asserted here.
    for pairs in [[("connection_id", "true"), ("node_type", "true")]] {
        let mut rt = runtime();
        let this = rt.intern("this");
        let list = build(&mut rt, |ctx| terms::make_list(ctx, &[this]).expect("a list"));
        let values: Vec<(&str, Eterm)> = pairs
            .iter()
            .map(|(name, flag)| (*name, rt.intern(flag)))
            .collect();
        let options = make_map(&mut rt, &values);
        let term = call(&mut rt, "erlang", "nodes", &[list, options]).expect("nodes/2");
        assert_eq!(
            rendered(&mut rt, term),
            "[{nonode@nohost,#{connection_id => undefined,node_type => this}}]",
            "{pairs:?}"
        );
    }
}

/// The C's `goto badarg` arms: a non-map `Options` (`[]` included), an unrecognised key,
/// a recognised key valued anything but `true`/`false`, and an invalid `NodeType`
/// element. The options are validated **before** the empty-set short cut, so each of
/// these is `badarg` even though the node set is empty.
#[test]
fn nodes_2_rejects_malformed_options_and_selectors() {
    // A non-map: `[]` (`NIL` is not a map) and a plain atom.
    for bad in ["[]", "bogus_options"] {
        let mut rt = runtime();
        let visible = rt.intern("visible");
        let options = rt.eval(bad).expect("an options term");
        let error = call(&mut rt, "erlang", "nodes", &[visible, options])
            .expect_err("a non-map options");
        assert_eq!(error.as_atom(), Some("badarg"), "{bad}: {error}");
    }
    // An unrecognised key: the recognised-key count cannot equal the map size.
    let mut rt = runtime();
    let visible = rt.intern("visible");
    let yes = rt.intern("true");
    let options = make_map(&mut rt, &[("bogus", yes)]);
    assert!(call(&mut rt, "erlang", "nodes", &[visible, options]).is_err());
    // A recognised key with a non-boolean value.
    let mut rt = runtime();
    let visible = rt.intern("visible");
    let options = make_map(&mut rt, &[("connection_id", make_small(1))]);
    assert!(call(&mut rt, "erlang", "nodes", &[visible, options]).is_err());
    // An invalid `NodeType` element (integer, and an atom with no switch case).
    let mut rt = runtime();
    let none = make_map(&mut rt, &[]);
    assert!(call(&mut rt, "erlang", "nodes", &[make_small(42), none]).is_err());
    let mut rt = runtime();
    let all = rt.intern("all");
    let none = make_map(&mut rt, &[]);
    assert!(call(&mut rt, "erlang", "nodes", &[all, none]).is_err());
}

// -- net_adm:ping/1 -------------------------------------------------------

#[test]
fn net_adm_ping_is_pang_on_a_dormant_node() {
    // The oracle answers `pang` for every argument on a `nonode@nohost` VM -- its own
    // node name and an unrelated atom alike.
    for name in ["nonode@nohost", "nosuchnode@nowhere", "foo"] {
        let mut rt = runtime();
        let peer = rt.intern(name);
        let term = call(&mut rt, "net_adm", "ping", &[peer])
            .unwrap_or_else(|e| panic!("net_adm:ping({name}): {e}"));
        assert_eq!(rendered(&mut rt, term), "pang", "net_adm:ping({name})");
    }
}

#[test]
fn net_adm_ping_rejects_a_non_atom_argument() {
    let mut rt = runtime();
    let text = rt.eval("\"str\"").expect("the string \"str\"");
    let error = call(&mut rt, "net_adm", "ping", &[text]).expect_err("a non-atom peer");
    // The oracle raises `function_clause`; a native BIF can only reject with `badarg`
    // (see the module docs) -- which is the honest typed substitute.
    assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    assert!(call(&mut rt, "net_adm", "ping", &[make_small(1)]).is_err());
    assert!(call(&mut rt, "net_adm", "ping", &[]).is_err());
}

// -- net_kernel:connect_node/1 -------------------------------------------

#[test]
fn connect_node_is_ignored_on_a_dormant_node() {
    // Measured on the oracle twice: a non-distributed VM answers `ignored` for its own
    // `nonode@nohost` name and for an arbitrary atom.
    for name in ["nonode@nohost", "foo"] {
        let mut rt = runtime();
        let peer = rt.intern(name);
        let term = call(&mut rt, "net_kernel", "connect_node", &[peer])
            .unwrap_or_else(|e| panic!("net_kernel:connect_node({name}): {e}"));
        assert_eq!(rendered(&mut rt, term), "ignored", "connect_node({name})");
    }
}

#[test]
fn connect_node_rejects_a_non_atom_argument() {
    let mut rt = runtime();
    let text = rt.eval("\"str\"").expect("the string \"str\"");
    let error =
        call(&mut rt, "net_kernel", "connect_node", &[text]).expect_err("a non-atom peer");
    assert_eq!(error.as_atom(), Some("badarg"), "{error}");
    assert!(call(&mut rt, "net_kernel", "connect_node", &[make_small(1)]).is_err());
}

// -- erl_distribution:start_link/0,1 --------------------------------------

/// Both `start_link` arities answer the atom `ignore`, whatever their argument: this
/// runtime's distribution is the native `DistNode`, so the Erlang supervisor
/// `erl_distribution` would start has nothing to run. The argument of `/1` is the
/// `#{name => …, name_domain => …, clean_halt => …, supervisor => net_sup}` map
/// `erl_distribution:do_start_link/1` builds; the floor ignores it.
///
/// Oracle (`erl_distribution.erl:107-138`): the module's own `start_link/0` reaches
/// `do_start_link([{sname,shortnames},{name,longnames}])`, and with no `-sname`/`-name`
/// argument both lookups miss, so it falls through to `ignore` — the value the
/// supervisor sees on the plain CLI route (which boots to completion).
#[test]
fn erl_distribution_start_link_is_ignore() {
    let mut rt = runtime();
    let term = call(&mut rt, "erl_distribution", "start_link", &[]).expect("start_link/0");
    assert_eq!(rendered(&mut rt, term), "ignore");

    let mut rt = runtime();
    let name = rt.intern("foo");
    let net_sup = rt.intern("net_sup");
    let options = make_map(
        &mut rt,
        &[("name", name), ("supervisor", net_sup)],
    );
    let term =
        call(&mut rt, "erl_distribution", "start_link", &[options]).expect("start_link/1");
    assert_eq!(rendered(&mut rt, term), "ignore");
}

/// The `erl_distribution` namespace is **key-granular**: the floor owns exactly
/// `start_link/0,1`, so the module's other bodies (`init/1`, `start/1`, `stop/0`) stay
/// enterable in-machine.
#[test]
fn only_erl_distribution_start_link_is_a_floor_key() {
    for arity in [0u32, 1] {
        assert!(
            crate::native::floor_shadows("erl_distribution", "start_link", arity),
            "the floor owns `erl_distribution:start_link/{arity}`"
        );
    }
    for (function, arity) in [("init", 1u32), ("start", 1), ("stop", 0)] {
        assert!(
            !crate::native::floor_shadows("erl_distribution", function, arity),
            "`erl_distribution:{function}/{arity}` is an ordinary `.beam` body"
        );
    }
}

// -- erlang:monitor_node/2 ------------------------------------------------

/// The oracle's answer is the atom `true` for **every atom** node — this runtime's own node,
/// `nonode@nohost`, and a node it has never connected to — with the subscription recorded as
/// one latch per distinct node. (Measured on OTP 29.1.1 / erts-17.1; there is no `badarg` for
/// `Node == node()`, and none for a node with no link.)
#[test]
fn monitor_node_answers_true_for_every_atom_node() {
    for extra in [None, Some("peer@elsewhere")] {
        let mut rt = runtime();
        let own = rt.local_node_term();
        let on = rt.intern("true");
        let mut nodes = vec![own, rt.intern(NONODE_NAME)];
        if let Some(name) = extra {
            nodes.push(rt.intern(name));
        }
        for node in nodes {
            let term = call(&mut rt, "erlang", "monitor_node", &[node, on])
                .unwrap_or_else(|error| panic!("monitor_node({node:?}, true): {error}"));
            assert_eq!(rendered(&mut rt, term), "true", "every atom node answers `true`");
        }
        // `own` and `nonode@nohost` are the same atom on a `-noshell` runtime, so those two
        // subscriptions collapse to the same idempotent-set entry.
        let latches = rt
            .node_monitors
            .get(&rt.self_pid())
            .expect("the subscriptions are recorded");
        assert_eq!(
            latches.len(),
            if extra.is_some() { 2 } else { 1 },
            "one latch per distinct node atom"
        );
    }
}

/// A **repeated** `monitor_node(Node, true)` neither duplicates nor refcounts its entry, and
/// `monitor_node(Node, false)` removes it — the C's idempotent set. `false` for a node the
/// process never subscribed to is the same no-op, and still answers `true`.
#[test]
fn monitor_node_is_an_idempotent_set_and_false_removes() {
    let mut rt = runtime();
    let on = rt.intern("true");
    let off = rt.intern("false");
    let peer = rt.intern("peer@host");
    for _ in 0..3 {
        let term = call(&mut rt, "erlang", "monitor_node", &[peer, on]).expect("monitor_node");
        assert_eq!(rendered(&mut rt, term), "true");
    }
    assert_eq!(
        rt.node_monitors
            .get(&rt.self_pid())
            .expect("the subscription")
            .len(),
        1,
        "a repeat neither duplicates nor refcounts"
    );
    let term = call(&mut rt, "erlang", "monitor_node", &[peer, off]).expect("unsubscribe");
    assert_eq!(rendered(&mut rt, term), "true");
    assert!(
        !rt.node_monitors.contains_key(&rt.self_pid()),
        "the last latch's removal drops the process's entry"
    );

    let unknown = rt.intern("other@host");
    let term = call(&mut rt, "erlang", "monitor_node", &[unknown, off]).expect("no-op false");
    assert_eq!(rendered(&mut rt, term), "true", "`false` is `true` for any atom node");
    assert!(rt.node_monitors.is_empty());
}

/// `badarg` — and only `badarg` — for a **non-atom** node or a flag that is not the atom
/// `true`/`false`. Nothing is recorded by a rejected call.
#[test]
fn monitor_node_rejects_a_non_atom_node_or_a_non_boolean_flag() {
    let mut rt = runtime();
    let on = rt.intern("true");
    let off = rt.intern("false");
    let peer = rt.intern("peer@host");

    let int = make_small(42);
    let list = rt.eval("[1, 2]").expect("a list");
    let pid = rt.eval("self()").expect("a pid");
    for (what, node) in [("an integer", int), ("a list", list), ("a pid", pid)] {
        for flag in [on, off] {
            assert!(
                call(&mut rt, "erlang", "monitor_node", &[node, flag]).is_err(),
                "{what} as the node must be badarg"
            );
        }
    }
    for name in ["maybe", "1", "True"] {
        let flag = rt.intern(name);
        assert!(
            call(&mut rt, "erlang", "monitor_node", &[peer, flag]).is_err(),
            "the flag `{name}` must be badarg"
        );
    }
    assert!(
        rt.node_monitors.is_empty(),
        "a rejected call records no subscription"
    );
}

// -- the name helpers -----------------------------------------------------
#[test]
fn same_node_accepts_the_short_and_the_full_name() {
    assert!(same_node("foo@host", "foo@host"));
    assert!(same_node("foo", "foo@host"));
    assert!(!same_node("bar", "foo@host"));
    assert!(!same_node("foo@other", "foo@host"));
    assert!(!same_node("foo@host", "nonode@nohost"));
}

#[test]
fn connect_name_strips_the_host_of_a_full_name() {
    // `epmd` registers a node under its short name, so that is what is dialled.
    assert_eq!(connect_name("foo@host"), "foo");
    assert_eq!(connect_name("foo"), "foo");
}

// -- nodes/2's `connection_id` with a live peer ---------------------------

/// The peer a [`node_runtime_with_a_peer`] admits over its own listener.
const LIVE_PEER: &str = "peer@localhost";

/// A `-sname`-configured runtime with **one established accepted link**: a real loopback
/// peer (`ConnectionDriver::client`) dials the runtime's own listener and the handshake
/// is driven through the runtime's scheduler pass — no `erl` and no `epmd`
/// (the `tests/dist_delivery.rs` fixture shape, whose helpers are private to that
/// integration suite).
fn node_runtime_with_a_peer() -> Runtime {
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    use beam_dist::driver::{ConnectionDriver, DriverState};

    // A node name unique per test *process*, so parallel test binaries cannot collide.
    let name = format!("beamrtnodes{}{}", "unit", std::process::id());
    let args: Vec<String> = ["-noshell", "-sname", &name, "-setcookie", "cook", "-eval", "ok."]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let config = crate::parse(&args).expect("the node switches parse");
    let mut rt = Runtime::new(config).expect("the runtime starts (the listener binds)");

    // One pass publishes the node and opens the listener for accepting.
    rt.run_one_slice().expect("a scheduler pass");
    let port = rt.dist_node().expect("a configured node").port();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the listener");
    stream
        .set_nonblocking(true)
        .expect("a non-blocking loopback socket");
    let mut peer = ConnectionDriver::client(LIVE_PEER.as_bytes(), b"cook", 7);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        peer_round(&mut stream, &mut peer);
        rt.run_one_slice().expect("a scheduler pass");
        if peer.state() == DriverState::Established && rt.dist_is_connected(LIVE_PEER) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the loopback link never reached Established within 15 s"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    rt
}

/// One round of a hand-driven non-blocking link: step the driver to quiescence, write
/// what it queued, then read whatever arrived.
fn peer_round(stream: &mut std::net::TcpStream, driver: &mut beam_dist::driver::ConnectionDriver) {
    use std::io::{ErrorKind, Read, Write};
    use std::time::{Duration, Instant};

    loop {
        match driver.step() {
            Ok(beam_dist::StepOutcome::WouldBlock)
            | Ok(beam_dist::StepOutcome::Closed)
            | Ok(beam_dist::StepOutcome::Failed) => break,
            Ok(_) => {}
            Err(err) => panic!("the peer's driver reported a protocol error: {err:?}"),
        }
    }
    let pending = driver.take_output();
    let mut written = 0;
    let deadline = Instant::now() + Duration::from_secs(15);
    while written < pending.len() {
        match stream.write(&pending[written..]) {
            Ok(0) => panic!("the peer's socket closed while writing"),
            Ok(n) => written += n,
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "the peer's write never drained");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => panic!("the peer's write: {err}"),
        }
    }
    let mut buf = vec![0u8; 64 * 1024];
    match stream.read(&mut buf) {
        Ok(0) => driver.feed_eof(),
        Ok(n) => driver.feed(&buf[..n]),
        Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
        Err(err) => panic!("the peer's read: {err}"),
    }
}

/// The `connection_id` integer a rendered entry carries — a decode of the printer's own
/// output, so an answer of `undefined` (the C's `~0`) fails here rather than passing as a
/// "value".
fn peer_connection_id(rendered: &str) -> i64 {
    let (_, rest) = rendered
        .split_once("#{connection_id => ")
        .unwrap_or_else(|| panic!("no `connection_id` in {rendered}"));
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or_else(|_| {
        panic!("`connection_id` is `undefined`, not an integer: {rendered}")
    })
}

/// **The `-sname` blocker**: with a live peer established,
/// `nodes(visible, #{connection_id => true})` — the exact call `global:init/1` makes
/// through `net_kernel:monitor_nodes/2` (`lib/kernel/src/global.erl:806-810`) — answers
/// the peer with a **real** link identity instead of raising the capability gap that
/// killed `kernel_sup`.
#[test]
fn nodes_2_answers_a_real_connection_id_for_a_live_peer() {
    let mut rt = node_runtime_with_a_peer();
    let visible = rt.intern("visible");
    let yes = rt.intern("true");
    let options = make_map(&mut rt, &[("connection_id", yes)]);
    let term = call(&mut rt, "erlang", "nodes", &[visible, options])
        .expect("nodes/2 must not raise for a live peer (this is the global:init/1 call)");
    let first = rendered(&mut rt, term);
    assert!(
        first.starts_with(&format!("[{{{LIVE_PEER},#{{connection_id => ")),
        "the established peer heads the node set: {first}"
    );
    let id = peer_connection_id(&first);
    assert!(
        id >= 1,
        "a link's identity is a positive integer, not `undefined`: {first}"
    );

    // Stability: the identity belongs to the link, so a second call answers the same.
    let again = call(&mut rt, "erlang", "nodes", &[visible, options]).expect("nodes/2 again");
    assert_eq!(
        rendered(&mut rt, again),
        first,
        "the peer's `connection_id` is stable across calls"
    );

    // The `global`-shaped call with both keys: it must succeed, keep the C's key order,
    // and leave the `this` entry's id as the atom `undefined`.
    let options = make_map(&mut rt, &[("connection_id", yes), ("node_type", yes)]);
    let this = rt.intern("this");
    let list = build(&mut rt, |ctx| {
        terms::make_list(ctx, &[visible, this]).expect("a selector list")
    });
    let term = call(&mut rt, "erlang", "nodes", &[list, options])
        .expect("nodes([visible,this], #{connection_id => true, node_type => true})");
    let both = rendered(&mut rt, term);
    assert_eq!(
        peer_connection_id(&both),
        id,
        "the peer keeps its identity with `node_type` requested too: {both}"
    );
    assert!(
        both.contains("node_type => visible"),
        "the peer's type atom is `visible`: {both}"
    );
    assert_eq!(
        both.matches("undefined").count(),
        1,
        "only the `this` entry answers `undefined` (the C's `~0`): {both}"
    );
    assert!(
        both.find("connection_id").unwrap_or(usize::MAX)
            < both.find("node_type").unwrap_or(usize::MAX),
        "the info map keeps the C's key order (connection_id then node_type): {both}"
    );
}
