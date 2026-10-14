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

//! Port BIFs for `erlang`: `open_port/2`, `port_close/1`, `port_info/1,2`,
//! `list_to_port/1` and `port_to_list/1`.
//!
//! Ground truth (read-only): the `open_port/2`, `port_close/1`, `port_info/1,2`,
//! `list_to_port/1` and `port_to_list/1` roster in
//! `../../erts/emulator/beam/bif.tab` — where the first three are the
//! `erts_internal:*` entries the `erlang` module's own `erlang.erl` wrappers call —
//! plus `../../erts/emulator/beam/erl_bif_port.c`
//! (`erts_internal_open_port_2`/`erts_internal_port_close_1`/
//! `erts_internal_port_info_1,2`) and `../../erts/emulator/beam/bif.c`
//! (`port_to_list_1`/`list_to_port_1`).
//!
//! # A port is runtime machinery, not a BIF
//!
//! Opening a port spawns an OS process through a driver (`spawn`, `spawn_executable`,
//! `fd`, …) and allocates a port-table slot. None of that belongs in a BIF, so every
//! port operation routes through the optional
//! [`PortHook`](crate::context::PortHook) seam —
//! [`BifContext::open_port`](crate::context::BifContext::open_port),
//! [`BifContext::close_port`](crate::context::BifContext::close_port) and
//! [`BifContext::port_info`](crate::context::BifContext::port_info) — which `beam_rt`
//! supplies over `beam_io`/`beam_process`. With no hook installed all four
//! seam-backed BIFs raise `unsupported_bif` (workspace `CONTEXT.md` §3): never a
//! fabricated port id, never a silently dropped close, never an invented `port_info`
//! pair.
//!
//! # `list_to_port/1` / `port_to_list/1` are pure
//!
//! The two deprecated textual conversions never touch the seam — they only map a
//! [`PortId`] to/from `#Port<0.Num>` (the shape `port_to_list_1` renders and
//! `list_to_port_1` parses). The node part is this node's *node number*, which is
//! `0` for the `nonode@nohost` node Milestone-1 always is (`convert::node_0`); a port
//! naming any other node cannot exist here and is `badarg`. Both shapes are pinned
//! against `erl -noshell` (`#Port<1.3>`, `#Port<0.>`, `#Port<0.3` are all `badarg`
//! in the oracle, `#Port<0.24>` round-trips).
//!
//! Failure is return-based: a non-port argument is `BeamError::BadArg`, a malformed
//! `#Port<…>` list is `BeamError::BadArg`, and an unset seam is `unsupported_bif`.

use beam_common::Result;
use beam_process::PortId;
use beam_term::compare;
use beam_term::{atom_val, signed_value, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error::badarg;
use crate::print::proper_list;
use crate::registry::BifRegistry;

/// The `erlang:open_port/2` BIF.
///
/// The Name/Options validation (`{spawn, …}`/`{spawn_executable, …}`/`{fd, …}` and
/// the option list) belongs to the driver set the runtime installed, so it is the
/// [`PortHook`](crate::context::PortHook)'s; this BIF only forwards and encodes the
/// resulting id.
fn open_port(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let port = ctx.open_port(args[0], args[1])?;
    Ok(port.to_eterm())
}

/// The `erlang:port_close/1` BIF.
///
/// Returns the atom `true` — the value the `erlang.erl` wrapper of
/// `erts_internal:port_close/1` produces (the oracle's
/// `erlang:port_close(open_port({spawn,"true"},[]))` ⇒ `true`), *not* `ok`.
///
/// A non-port argument is `badarg` *before* the seam is consulted, matching
/// `erts_internal_port_close_1`.
fn port_close(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let port = PortId::from_eterm(args[0])?;
    ctx.close_port(port)?;
    ctx.boolean(true)
}

/// The `Item` keys `erlang:port_info/1` reports — the oracle's `port_info/1` roster.
///
/// `erts_internal:port_info/1` emits exactly these seven pairs; every other port item
/// (e.g. `monitors`) is reachable only through `/2`.
const PORT_INFO_1_ITEMS: [&str; 7] = [
    "name",
    "links",
    "id",
    "connected",
    "input",
    "output",
    "os_pid",
];

/// The `erlang:port_info/1` BIF: the seven `/1`-item `{Item, Value}` pairs.
///
/// The oracle's `port_info/1` is *not* the whole `/2` item set — it reports only
/// `name`, `links`, `id`, `connected`, `input`, `output` and `os_pid`. So the hook's
/// pair list is filtered: a pair survives iff its key is an atom naming one of those
/// seven items; every other pair (a `/2`-only item such as `monitors`, or a non-atom
/// key) is dropped. The surviving pairs keep their hook order.
fn port_info_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let port = PortId::from_eterm(args[0])?;
    let pairs = ctx.port_info(port)?;
    let mut tuples = Vec::with_capacity(pairs.len());
    for (item, value) in pairs {
        let keep = access::is_atom(item)
            && ctx
                .atoms()
                .name(atom_val(item))
                .is_some_and(|name| PORT_INFO_1_ITEMS.contains(&name));
        if !keep {
            continue;
        }
        tuples.push(ctx.tuple(&[item, value])?);
    }
    ctx.list(&tuples)
}

/// The `erlang:port_info/2` BIF: the 2-tuple `{Item, Value}` whose item is *exactly*
/// (`=:=`) `args[1]`.
///
/// Unlike `/1`, `/2` searches **every** hook pair — including the items `/1` filters
/// out (e.g. `monitors`). An item matching no pair (in particular a non-atom item, or
/// an item the oracle does not know such as `stream`) is `error:badarg`, matching
/// erts-17.1.
fn port_info_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let port = PortId::from_eterm(args[0])?;
    let item = args[1];
    let pairs = ctx.port_info(port)?;
    for (key, value) in pairs {
        if compare::eq(key, item, ctx.atoms()) {
            return ctx.tuple(&[key, value]);
        }
    }
    Err(badarg())
}

/// The largest internal port number.
///
/// Ports spend their whole 60-bit payload on the number
/// (`_PORT_NUM_SIZE == _PORT_DATA_SIZE` in `erl_term.h`), so anything above this
/// cannot be encoded as an internal port and must not be accepted.
const MAX_INTERNAL_PORT_NUMBER: u64 = (1 << 60) - 1;

/// The node number of the `nonode@nohost` node — the only node Milestone-1 ever is
/// (`../../erts/emulator/beam/bif.c` compares the parsed node against the local
/// dist entry; `0` is the non-distributed node's number).
const LOCAL_NODE_NUMBER: u64 = 0;

/// The `erlang:list_to_port/1` BIF: parses the legacy textual form `#Port<0.Num>`.
///
/// The result is an internal-port immediate, so nothing is allocated and the
/// context is unused.
fn list_to_port(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = byte_list(args[0])?;
    let port = parse_port_text(&bytes)?;
    Ok(port.to_eterm())
}

/// The `erlang:port_to_list/1` BIF: renders `#Port<0.Num>` for an internal port —
/// the shared `%T` text of
/// [`crate::term_conv::port_text_str`](crate::term_conv::port_text_str).
///
/// # Errors
///
/// A non-port is `badarg`; an **external** port is the typed capability gap
/// `error:unsupported_bif` (its channel number is not derivable from the term) —
/// both per
/// [`port_text_str`](crate::term_conv::port_text_str).
fn port_to_list(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let text = crate::term_conv::port_text_str(args[0])?;
    let mut elems = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        elems.push(ctx.int(i64::from(byte))?);
    }
    ctx.list(&elems)
}

/// Collects `term` into the bytes of a proper list of byte-sized small integers.
///
/// Mirrors `intlist_to_buf` in `bif.c`: a non-list, an improper list, a non-integer
/// element or an element outside `0..=255` is `badarg`.
fn byte_list(term: Eterm) -> Result<Vec<u8>> {
    let elems = proper_list(term).ok_or_else(badarg)?;
    let mut bytes = Vec::with_capacity(elems.len());
    for elem in elems {
        if !access::is_small(elem) {
            return Err(badarg());
        }
        bytes.push(u8::try_from(signed_value(elem)).map_err(|_| badarg())?);
    }
    Ok(bytes)
}

/// Parses `#Port<0.Num>` into the internal port it names.
///
/// Every deviation from the exact shape (missing `#Port<`/`.`/`>`, a non-digit,
/// trailing characters, another node number, an out-of-range number) is `badarg` —
/// the acceptance rule of `list_to_port_1` in `bif.c`.
fn parse_port_text(bytes: &[u8]) -> Result<PortId> {
    let text = core::str::from_utf8(bytes).map_err(|_| badarg())?;
    let rest = text.strip_prefix("#Port<").ok_or_else(badarg)?;
    let (node, rest) = rest.split_once('.').ok_or_else(badarg)?;
    let number = rest.strip_suffix('>').ok_or_else(badarg)?;

    let node: u64 = node.parse().map_err(|_| badarg())?;
    if node != LOCAL_NODE_NUMBER {
        return Err(badarg());
    }
    let number: u64 = number.parse().map_err(|_| badarg())?;
    if number > MAX_INTERNAL_PORT_NUMBER {
        return Err(badarg());
    }
    Ok(PortId::new(number))
}

/// Registers `erlang:open_port/2`, `erlang:port_close/1`, `erlang:port_info/1,2`,
/// `erlang:list_to_port/1` and `erlang:port_to_list/1`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "open_port", 2, open_port);
    reg.register("erlang", "port_close", 1, port_close);
    reg.register("erlang", "port_info", 1, port_info_1);
    reg.register("erlang", "port_info", 2, port_info_2);
    reg.register("erlang", "list_to_port", 1, list_to_port);
    reg.register("erlang", "port_to_list", 1, port_to_list);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{atom_val, is_list, make_small, AtomTable, NIL};

    /// A `PortHook` test double: it hands out `PortId::new(3)` and answers a fixed
    /// pair list.
    struct StubPorts {
        info: Vec<(Eterm, Eterm)>,
        opened: Vec<(Eterm, Eterm)>,
        closed: Vec<PortId>,
    }

    impl StubPorts {
        fn new(info: Vec<(Eterm, Eterm)>) -> StubPorts {
            StubPorts {
                info,
                opened: Vec::new(),
                closed: Vec::new(),
            }
        }
    }

    impl crate::context::PortHook for StubPorts {
        fn open_port(&mut self, name: Eterm, options: Eterm) -> Result<PortId> {
            self.opened.push((name, options));
            Ok(PortId::new(3))
        }

        fn close_port(&mut self, port: PortId) -> Result<()> {
            self.closed.push(port);
            Ok(())
        }

        fn port_info(&mut self, _port: PortId) -> Result<Vec<(Eterm, Eterm)>> {
            Ok(self.info.clone())
        }
    }

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true",
            "false",
            "ok",
            "undefined",
            "name",
            "links",
            "monitors",
        ]);
        atoms
    }

    #[test]
    fn open_port_returns_the_hooks_id_and_forwards_the_arguments() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded_atoms();
        let mut hook = StubPorts::new(Vec::new());
        let name = make_small(9);
        {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_port_hook(&mut hook);
            let term = open_port(&mut ctx, &[name, NIL]).expect("the hook answers");
            assert!(PortId::is_port(term));
            assert_eq!(PortId::from_eterm(term).expect("a port"), PortId::new(3));
        }
        assert_eq!(hook.opened, vec![(name, NIL)]);
    }

    #[test]
    fn open_port_without_a_hook_is_unsupported() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match open_port(&mut ctx, &[make_small(9), NIL]) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(
                    raise.reason,
                    beam_common::ExitReason::atom("unsupported_bif")
                );
            }
            other => panic!("expected an unsupported_bif raise, got {other:?}"),
        }
    }

    #[test]
    fn port_close_and_port_info_decode_the_port_first() {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = seeded_atoms();
        let port = PortId::new(5).to_eterm();

        // The pair list the stub answers with: two `/1` items, in hook order.
        let info = {
            let ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
            let name = ctx.atom("name").expect("seeded");
            let links = ctx.atom("links").expect("seeded");
            let one = make_small(1);
            let two = make_small(2);
            vec![(name, two), (links, one)]
        };
        let mut hook = StubPorts::new(info);
        {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_port_hook(&mut hook);

            let closed = port_close(&mut ctx, &[port]).expect("port_close");
            assert_eq!(ctx.atoms().name(atom_val(closed)), Some("true"));

            // `/1` keeps the two `/1` items, in hook order.
            let all = port_info_1(&mut ctx, &[port]).expect("port_info/1");
            let elems = proper_list(all).expect("a proper list");
            assert_eq!(elems.len(), 2);
            // `tuple_element` is 0-based (`tuple_val(x)[i]`).
            assert_eq!(access::tuple_element(elems[0], 0), ctx.atom("name").unwrap());
            assert_eq!(access::tuple_element(elems[0], 1), make_small(2));
            assert_eq!(access::tuple_element(elems[1], 0), ctx.atom("links").unwrap());
            assert_eq!(access::tuple_element(elems[1], 1), make_small(1));

            // `/2` of a matching item is the 2-tuple `{Item, Value}`.
            let name = ctx.atom("name").expect("seeded");
            let named = port_info_2(&mut ctx, &[port, name]).expect("port_info/2");
            assert_eq!(access::tuple_size(named), 2);
            assert_eq!(access::tuple_element(named, 0), name);
            assert_eq!(access::tuple_element(named, 1), make_small(2));

            // An item matching no pair is `badarg`.
            let unknown = ctx.atom("undefined").expect("seeded");
            assert!(matches!(
                port_info_2(&mut ctx, &[port, unknown]),
                Err(BeamError::BadArg)
            ));
            // A non-atom item never matches, so it is `badarg` too.
            assert!(matches!(
                port_info_2(&mut ctx, &[port, make_small(1)]),
                Err(BeamError::BadArg)
            ));

            // A non-port is `badarg`, before the seam is consulted.
            assert!(matches!(
                port_close(&mut ctx, &[make_small(1)]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                port_info_1(&mut ctx, &[make_small(1)]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                port_info_2(&mut ctx, &[make_small(1), make_small(1)]),
                Err(BeamError::BadArg)
            ));
        }
        assert_eq!(hook.closed, vec![PortId::new(5)]);
    }

    #[test]
    fn port_to_list_and_list_to_port_round_trip() {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let text = port_to_list(&mut ctx, &[PortId::new(3).to_eterm()]).expect("port_to_list");
        assert!(is_list(text));
        let rendered: String = proper_list(text)
            .expect("a proper list")
            .into_iter()
            .map(|e| char::from(u8::try_from(signed_value(e)).expect("a byte")))
            .collect();
        assert_eq!(rendered, "#Port<0.3>");

        let port = list_to_port(&mut ctx, &[text]).expect("list_to_port");
        assert_eq!(PortId::from_eterm(port).expect("a port"), PortId::new(3));

        // A malformed textual form is `badarg`.
        assert!(matches!(
            parse_port_text(b"#Port<1.3>"),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            parse_port_text(b"#Port<0.>"),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            parse_port_text(b"#Port<0.3"),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(parse_port_text(b"nope"), Err(BeamError::BadArg)));
        assert_eq!(
            parse_port_text(b"#Port<0.0>").expect("port 0 is valid"),
            PortId::new(0)
        );
    }

    #[test]
    fn register_installs_every_port_bif() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (name, arity) in [
            ("open_port", 2),
            ("port_close", 1),
            ("port_info", 1),
            ("port_info", 2),
            ("list_to_port", 1),
            ("port_to_list", 1),
        ] {
            assert!(reg.contains("erlang", name, arity), "{name}/{arity}");
        }
        assert_eq!(reg.len(), 6);
    }
}
