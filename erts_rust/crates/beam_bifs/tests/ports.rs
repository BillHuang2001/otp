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

//! Integration tests for `erlang:open_port/2` and the port BIFs, driven through
//! [`BifRegistry::call`] over a mock [`PortHook`].
//!
//! The crate owns only the seam: creating a real port (the `spawn` driver, an OS
//! process, a port-table slot) is `beam_io`/`beam_rt`'s job. So the tests install a
//! mock hook that hands out a fixed [`PortId`] and pin three contracts:
//!
//! 1. `open_port/2` returns an *internal port* — `is_port/1` is `true` for it;
//! 2. `port_close/1` / `port_info/1,2` decode the port and delegate to the hook;
//! 3. with **no** hook installed every one of those BIFs raises `unsupported_bif`
//!    (never a fabricated port id, never a silently dropped close).
//!
//! Structural comparisons use `beam_term::compare::eq`; booleans are resolved by
//! looking the `true`/`false` atom up in the atom table.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, PortHook};
use beam_common::{BeamError, ExitReason, Result};
use beam_process::{PortId, ProcessId};
use beam_term::{atom_val, is_list, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "undefined",
        "name",
        "cat",
        "spawn",
        "connected",
        "links",
        "id",
        "input",
        "output",
        "os_pid",
        "monitors",
        "stream",
        "no_such_item",
    ]);
    atoms
}

/// A `PortHook` test double: it allocates the port numbers `7`, `8`, … in order,
/// records every open/close and answers a fixed `port_info` pair list.
struct MockPorts {
    info: Vec<(Eterm, Eterm)>,
    opened: Vec<(Eterm, Eterm)>,
    closed: Vec<PortId>,
    next: u64,
}

impl MockPorts {
    fn new(info: Vec<(Eterm, Eterm)>) -> MockPorts {
        MockPorts {
            info,
            opened: Vec::new(),
            closed: Vec::new(),
            next: 7,
        }
    }
}

impl PortHook for MockPorts {
    fn open_port(&mut self, name: Eterm, options: Eterm) -> Result<PortId> {
        self.opened.push((name, options));
        let id = PortId::new(self.next);
        self.next += 1;
        Ok(id)
    }

    fn close_port(&mut self, port: PortId) -> Result<()> {
        self.closed.push(port);
        Ok(())
    }

    fn port_info(&mut self, _port: PortId) -> Result<Vec<(Eterm, Eterm)>> {
        Ok(self.info.clone())
    }
}

/// Whether `term` is the atom `true`.
fn is_true(atoms: &AtomTable, term: Eterm) -> bool {
    atoms.name(atom_val(term)) == Some("true")
}

/// An `unsupported_bif` error raise.
fn assert_unsupported(result: Result<Eterm>) {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, beam_common::ExceptionClass::Error);
            assert_eq!(raise.reason, ExitReason::atom("unsupported_bif"));
        }
        other => panic!("expected an unsupported_bif raise, got {other:?}"),
    }
}

#[test]
fn open_port_yields_an_internal_port_that_is_port_recognises() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut hook = MockPorts::new(Vec::new());

    let (port, is_port) = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_port_hook(&mut hook);

        // `open_port({spawn, "cat"}, [])` — the hook validates Name/Options, this BIF
        // only forwards them.
        let spawn = ctx.atom("spawn").expect("seeded");
        let cat = ctx.atom("cat").expect("seeded");
        let name = ctx.tuple(&[spawn, cat]).expect("fits");
        let options = NIL;
        let port = reg
            .call("erlang", "open_port", 2, &mut ctx, &[name, options])
            .expect("the hook answers");

        // `erlang:is_port/1` agrees, and the term decodes back to the hook's id.
        let is_port = reg
            .call("erlang", "is_port", 1, &mut ctx, &[port])
            .expect("is_port");
        (port, is_port)
    };

    assert_eq!(
        PortId::from_eterm(port).expect("an internal port"),
        PortId::new(7)
    );
    assert!(is_true(&atoms, is_port));
    assert_eq!(hook.opened.len(), 1, "the hook saw exactly one open");
}

#[test]
fn port_close_returns_true_and_closes_the_hooks_port() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut hook = MockPorts::new(Vec::new());
    let port = PortId::new(7).to_eterm();

    let result = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_port_hook(&mut hook);
        reg.call("erlang", "port_close", 1, &mut ctx, &[port])
    };

    let closed = result.expect("port_close returns true");
    assert_eq!(atoms.name(atom_val(closed)), Some("true"));
    assert_eq!(hook.closed, vec![PortId::new(7)]);
}

#[test]
fn port_info_1_is_the_pair_list_and_port_info_2_selects_or_is_badarg() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let port = PortId::new(7).to_eterm();

    // The pair list the mock hook holds: `{name, "cat"}` and `{connected, 1}`. Both
    // keys are `/1` items, so `/1` keeps both.
    let info = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let name = ctx.atom("name").expect("seeded");
        let cat = ctx.list(&[ctx.int(99).expect("fits")]).expect("fits");
        let connected = ctx.atom("connected").expect("seeded");
        let one = ctx.int(1).expect("fits");
        vec![(name, cat), (connected, one)]
    };
    let mut hook = MockPorts::new(info);

    let (all, named, missing) = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_port_hook(&mut hook);

        let all = reg
            .call("erlang", "port_info", 1, &mut ctx, &[port])
            .expect("port_info/1");
        assert!(is_list(all));

        let name = ctx.atom("name").expect("seeded");
        let named = reg
            .call("erlang", "port_info", 2, &mut ctx, &[port, name])
            .expect("port_info/2");

        let nosuch = ctx.atom("no_such_item").expect("seeded");
        let missing = reg.call("erlang", "port_info", 2, &mut ctx, &[port, nosuch]);
        (all, named, missing)
    };

    // `port_info/1` is `[{name, "cat"}, {connected, 1}]`.
    {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let name = ctx.atom("name").expect("seeded");
        let cat = ctx.list(&[ctx.int(99).expect("fits")]).expect("fits");
        let connected = ctx.atom("connected").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let pair_1 = ctx.tuple(&[name, cat]).expect("fits");
        let pair_2 = ctx.tuple(&[connected, one]).expect("fits");
        let expected = ctx.list(&[pair_1, pair_2]).expect("fits");
        assert!(beam_term::compare::eq(all, expected, ctx.atoms()));
    }

    // `port_info/2` returns the 2-tuple `{Item, Value}`, not the bare value.
    {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let name = ctx.atom("name").expect("seeded");
        let cat = ctx.list(&[ctx.int(99).expect("fits")]).expect("fits");
        let expected = ctx.tuple(&[name, cat]).expect("fits");
        assert!(beam_term::compare::eq(named, expected, ctx.atoms()));
        assert_eq!(beam_codec::term::tuple_arity(named).expect("a 2-tuple"), 2);
    }

    // An item matching no pair is `badarg` (never the atom `undefined`).
    assert!(matches!(missing, Err(BeamError::BadArg)));
}

/// `erlang:port_info/1` reports **only** the seven `/1` items — `name`, `links`, `id`,
/// `connected`, `input`, `output`, `os_pid` — dropping every `/2`-only extra
/// (`monitors`), an item the oracle does not know (`stream`) and a non-atom key.
#[test]
fn port_info_1_keeps_only_the_seven_items() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let port = PortId::new(7).to_eterm();

    // The hook's pair list in the oracle's `/1` order, then the three keys `/1` must
    // drop.
    let info = {
        let ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let mut pairs = Vec::new();
        for (i, item) in [
            "name", "links", "id", "connected", "input", "output", "os_pid", "monitors",
            "stream",
        ]
        .into_iter()
        .enumerate()
        {
            let key = ctx.atom(item).expect("seeded");
            let value = ctx.int(i as i64 + 1).expect("fits");
            pairs.push((key, value));
        }
        let one = ctx.int(1).expect("fits");
        pairs.push((one, one));
        pairs
    };
    let mut hook = MockPorts::new(info);

    let all = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_port_hook(&mut hook);
        reg.call("erlang", "port_info", 1, &mut ctx, &[port])
            .expect("port_info/1")
    };

    // Exactly the seven `/1` items, in hook order.
    {
        let mut heap = Heap::new(65536).expect("test heap");
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let mut pairs = Vec::new();
        for (i, item) in [
            "name",
            "links",
            "id",
            "connected",
            "input",
            "output",
            "os_pid",
        ]
        .into_iter()
        .enumerate()
        {
            let key = ctx.atom(item).expect("seeded");
            let value = ctx.int(i as i64 + 1).expect("fits");
            pairs.push(ctx.tuple(&[key, value]).expect("fits"));
        }
        let expected = ctx.list(&pairs).expect("fits");
        assert!(
            beam_term::compare::eq(all, expected, ctx.atoms()),
            "port_info/1 must be exactly the seven /1 items"
        );
    }
}

/// `erlang:port_info/2` searches **every** hook pair (including the `/2`-only items
/// `/1` filters out) and is `error:badarg` when nothing matches — for an unknown
/// atom, for an item the oracle does not know, and for a non-atom item.
#[test]
fn port_info_2_searches_every_pair_and_is_badarg_when_nothing_matches() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let port = PortId::new(7).to_eterm();

    // Two `/1` items and one `/2`-only item.
    let info = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let name = ctx.atom("name").expect("seeded");
        let cat = ctx.list(&[ctx.int(99).expect("fits")]).expect("fits");
        let connected = ctx.atom("connected").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let monitors = ctx.atom("monitors").expect("seeded");
        vec![(name, cat), (connected, one), (monitors, NIL)]
    };
    let mut hook = MockPorts::new(info);

    let (monitors_pair, all) = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_port_hook(&mut hook);

        // A `/2`-only key is found by `/2` ...
        let monitors = ctx.atom("monitors").expect("seeded");
        let monitors_pair = reg
            .call("erlang", "port_info", 2, &mut ctx, &[port, monitors])
            .expect("port_info/2 monitors");
        // ... but `/1` drops it.
        let all = reg
            .call("erlang", "port_info", 1, &mut ctx, &[port])
            .expect("port_info/1");

        // An item the oracle does not know, an unknown atom and a non-atom item are
        // all `badarg` (the registry surfaces the BIF's `error:badarg`).
        let stream = ctx.atom("stream").expect("seeded");
        assert!(matches!(
            reg.call("erlang", "port_info", 2, &mut ctx, &[port, stream]),
            Err(BeamError::BadArg)
        ));
        let nosuch = ctx.atom("no_such_item").expect("seeded");
        assert!(matches!(
            reg.call("erlang", "port_info", 2, &mut ctx, &[port, nosuch]),
            Err(BeamError::BadArg)
        ));
        let one = ctx.int(1).expect("fits");
        assert!(matches!(
            reg.call("erlang", "port_info", 2, &mut ctx, &[port, one]),
            Err(BeamError::BadArg)
        ));
        (monitors_pair, all)
    };

    // `/2` of `monitors` is the 2-tuple `{monitors, []}`.
    {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let monitors = ctx.atom("monitors").expect("seeded");
        let expected = ctx.tuple(&[monitors, NIL]).expect("fits");
        assert!(beam_term::compare::eq(monitors_pair, expected, ctx.atoms()));
    }

    // `/1` is `[{name, "cat"}, {connected, 1}]` — `monitors` is absent.
    {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let name = ctx.atom("name").expect("seeded");
        let cat = ctx.list(&[ctx.int(99).expect("fits")]).expect("fits");
        let connected = ctx.atom("connected").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let pair_1 = ctx.tuple(&[name, cat]).expect("fits");
        let pair_2 = ctx.tuple(&[connected, one]).expect("fits");
        let expected = ctx.list(&[pair_1, pair_2]).expect("fits");
        assert!(beam_term::compare::eq(all, expected, ctx.atoms()));
    }
}

#[test]
fn every_port_bif_is_unsupported_without_a_hook() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let port = PortId::new(7).to_eterm();

    let spawn = ctx.atom("spawn").expect("seeded");
    let cat = ctx.atom("cat").expect("seeded");
    let name = ctx.tuple(&[spawn, cat]).expect("fits");
    let item = ctx.atom("name").expect("seeded");

    assert_unsupported(reg.call("erlang", "open_port", 2, &mut ctx, &[name, NIL]));
    assert_unsupported(reg.call("erlang", "port_close", 1, &mut ctx, &[port]));
    assert_unsupported(reg.call("erlang", "port_info", 1, &mut ctx, &[port]));
    assert_unsupported(reg.call("erlang", "port_info", 2, &mut ctx, &[port, item]));
}

#[test]
fn a_non_port_argument_is_badarg() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut hook = MockPorts::new(Vec::new());
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_port_hook(&mut hook);

    let not_a_port = ctx.int(1).expect("fits");
    let item = ctx.atom("name").expect("seeded");

    assert!(matches!(
        reg.call("erlang", "port_close", 1, &mut ctx, &[not_a_port]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        reg.call("erlang", "port_info", 1, &mut ctx, &[not_a_port]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        reg.call("erlang", "port_info", 2, &mut ctx, &[not_a_port, item]),
        Err(BeamError::BadArg)
    ));
    assert!(hook.closed.is_empty(), "nothing was closed");
}

#[test]
fn the_registry_registers_every_port_bif() {
    let reg = default_registry();
    assert!(reg.contains("erlang", "open_port", 2));
    for (name, arity) in [
        ("open_port", 2),
        ("port_close", 1),
        ("port_info", 1),
        ("port_info", 2),
        ("list_to_port", 1),
        ("port_to_list", 1),
    ] {
        assert!(
            reg.lookup("erlang", name, arity).is_some(),
            "{name}/{arity}"
        );
    }
    // Every key is registered exactly once (a duplicate would silently replace).
    // `is_port/1` (in `type_test`) is not a port BIF and is deliberately excluded.
    let port_names = [
        "open_port",
        "port_close",
        "port_info",
        "list_to_port",
        "port_to_list",
    ];
    let port_keys: Vec<_> = reg
        .names()
        .into_iter()
        .filter(|(m, f, _)| m == "erlang" && port_names.contains(&f.as_str()))
        .collect();
    assert_eq!(port_keys.len(), 6, "got {port_keys:?}");
}

/// `list_to_port/1` and `port_to_list/1` round-trip through the legacy textual form
/// `#Port<0.Num>` (the node number is `0` for `nonode@nohost`), and a malformed form
/// is `badarg`.
#[test]
fn list_to_port_and_port_to_list_round_trip() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let port = PortId::new(24).to_eterm();
    let text = reg
        .call("erlang", "port_to_list", 1, &mut ctx, &[port])
        .expect("port_to_list");
    let expected = {
        let elems: Vec<Eterm> = b"#Port<0.24>"
            .iter()
            .map(|b| ctx.int(i64::from(*b)).expect("a byte"))
            .collect();
        ctx.list(&elems).expect("fits")
    };
    assert!(beam_term::compare::eq(text, expected, ctx.atoms()));

    let round_tripped = reg
        .call("erlang", "list_to_port", 1, &mut ctx, &[text])
        .expect("list_to_port");
    assert_eq!(
        PortId::from_eterm(round_tripped).expect("an internal port"),
        PortId::new(24)
    );

    // A malformed textual form is `badarg`.
    let bad = ctx.list(&[ctx.int(35).expect("fits")]).expect("fits"); // "#"
    assert!(matches!(
        reg.call("erlang", "list_to_port", 1, &mut ctx, &[bad]),
        Err(BeamError::BadArg)
    ));
}
