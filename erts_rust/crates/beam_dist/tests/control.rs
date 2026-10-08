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

//! `DOP_*` control-message encode→decode round-trips (arity-checked) over a
//! `beam_alloc` heap + `AtomTable`; unknown-opcode, non-tuple, truncated-tuple
//! and wrong-arity negatives.
//!
//! Every case is hermetic and exercises only the value layer. The control message
//! is the **real ETF tuple** `{DOP_*, …}` — `SMALL_TUPLE_EXT`/`LARGE_TUPLE_EXT` +
//! arity + the opcode as a `SMALL_INTEGER_EXT` term + the arguments — and, for a
//! message-carrying opcode, **one further term** (the message) after the tuple. It
//! is encoded **body-form** (no `VERSION_MAGIC`); the version byte is carried once
//! by the enclosing distribution header. Ground truth for the opcodes, their
//! arities and the trailing message is `erts/doc/guides/erl_dist_protocol.md`
//! §"Distribution Message" and `erts/emulator/beam/dist.c` (`erts_dist_command`,
//! the `{DOP_*, …}` tuple shapes and the `/* {DOP_SEND, From, To} -- Message */`
//! term).

use beam_alloc::Heap;
use beam_codec::{
    AtomCache, ATOM_CACHE_REF_EXT, ATOM_UTF8_EXT, INTEGER_EXT, SMALL_INTEGER_EXT,
    SMALL_TUPLE_EXT, VERSION_MAGIC,
};
use beam_common::BeamError;
use beam_dist::control::{decode_control, encode_control, encode_control_cached};
use beam_dist::flags::{Dop, DOP_REG_SEND, DOP_SEND};
use beam_term::prelude::{make_atom, make_small, Eterm, NIL};
use beam_term::{Atom, AtomTable};

/// A decoding fixture: the production-shaped `beam_alloc` heap plus the node's
/// `AtomTable`. Test code may `unwrap` (a failure to allocate 1024 words is a
/// broken test environment, not wire data).
struct Fixture {
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            heap: Heap::new(1024).expect("1024-word test heap"),
            atoms: AtomTable::new(),
        }
    }

    /// Interns `name` into the fixture's table.
    fn intern(&mut self, name: &str) -> Atom {
        self.atoms.intern(name)
    }

    /// Interns `name` and returns the atom term for it.
    fn atom(&mut self, name: &str) -> Eterm {
        make_atom(self.intern(name))
    }

    /// Encodes `terms` under `dop`, decodes the result and asserts the round-trip
    /// (the opcode is tuple element 1, same opcode, same arguments). Returns the
    /// body so callers can additionally assert on the bytes.
    fn round_trip(&mut self, dop: Dop, terms: &[Eterm]) -> Vec<u8> {
        let body = encode_control(dop, terms, &self.atoms).expect("encode_control");
        assert_eq!(
            body.first().copied(),
            Some(SMALL_TUPLE_EXT),
            "{dop:?}: the control message must be a tuple"
        );
        assert_eq!(
            body.get(2).copied(),
            Some(SMALL_INTEGER_EXT),
            "{dop:?}: tuple element 1 must be a small-integer opcode"
        );
        assert_eq!(
            body.get(3).copied(),
            Some(dop.as_u8()),
            "{dop:?}: tuple element 1 is the opcode"
        );
        let message = decode_control(&body, &mut self.heap, &mut self.atoms, None)
            .unwrap_or_else(|err| panic!("{dop:?}: decode_control failed: {err:?}"));
        assert_eq!(message.dop, dop, "{dop:?}: opcode round-trips");
        assert_eq!(message.terms, terms, "{dop:?}: arguments round-trip");
        body
    }
}

// ---------------------------------------------------------------------------
// Byte-exactness: the real `{DOP_*, …}` tuple form, no version byte.
// ---------------------------------------------------------------------------

#[test]
fn send_encodes_to_the_exact_tuple_wire_bytes() {
    // `{DOP_SEND, 1, 2}` -> `[SMALL_TUPLE_EXT, 3, SMALL_INTEGER_EXT, DOP_SEND,
    // SMALL_INTEGER_EXT, 1, SMALL_INTEGER_EXT, 2]`.
    let mut fixture = Fixture::new();
    let body = encode_control(Dop::Send, &[make_small(1), make_small(2)], &fixture.atoms)
        .expect("encode_control");
    assert_eq!(
        body,
        vec![
            SMALL_TUPLE_EXT,
            3,
            SMALL_INTEGER_EXT,
            DOP_SEND,
            SMALL_INTEGER_EXT,
            1,
            SMALL_INTEGER_EXT,
            2,
        ]
    );
    // Body-form: the version byte is carried once by the distribution header.
    assert!(
        !body.contains(&VERSION_MAGIC),
        "a control body must carry no version byte: {body:?}"
    );

    let message = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None)
        .expect("decode_control");
    assert_eq!(message.dop, Dop::Send);
    assert_eq!(message.terms, vec![make_small(1), make_small(2)]);
}

#[test]
fn reg_send_encodes_to_the_exact_tuple_wire_bytes() {
    // `{DOP_REG_SEND, 1, 2, 3}` -> arity 4 (opcode + three arguments).
    let mut fixture = Fixture::new();
    let body =
        encode_control(Dop::RegSend, &[make_small(1), make_small(2), make_small(3)], &fixture.atoms)
            .expect("encode_control");
    assert_eq!(
        body,
        vec![
            SMALL_TUPLE_EXT,
            4,
            SMALL_INTEGER_EXT,
            DOP_REG_SEND,
            SMALL_INTEGER_EXT,
            1,
            SMALL_INTEGER_EXT,
            2,
            SMALL_INTEGER_EXT,
            3,
        ]
    );
    assert_eq!(DOP_REG_SEND, 6, "the opcode keeps its wire value");

    let message = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None)
        .expect("decode_control");
    assert_eq!(message.dop, Dop::RegSend);
    assert_eq!(message.terms, vec![make_small(1), make_small(2), make_small(3)]);
}

#[test]
fn a_send_message_carries_the_trailing_message_term() {
    // `{DOP_SEND, From, To} -- Message`: the tuple holds three elements (the opcode
    // and two control arguments) and the message is **one further term** after it.
    let mut fixture = Fixture::new();
    let body = encode_control(
        Dop::Send,
        &[make_small(1), make_small(2), make_small(3)],
        &fixture.atoms,
    )
    .expect("encode_control");
    assert_eq!(
        body,
        vec![
            SMALL_TUPLE_EXT,
            3,
            SMALL_INTEGER_EXT,
            DOP_SEND,
            SMALL_INTEGER_EXT,
            1,
            SMALL_INTEGER_EXT,
            2,
            // --- the tuple ends here; the message (3) follows it ---
            SMALL_INTEGER_EXT,
            3,
        ]
    );

    let message = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None)
        .expect("decode_control");
    assert_eq!(message.dop, Dop::Send);
    assert_eq!(
        message.terms,
        vec![make_small(1), make_small(2), make_small(3)],
        "From, To and the trailing message"
    );

    // `{DOP_REG_SEND, From, Cookie, ToName} -- Message`: arity 4 + the message.
    let body = encode_control(
        Dop::RegSend,
        &[make_small(1), make_small(2), make_small(3), make_small(4)],
        &fixture.atoms,
    )
    .expect("encode_control");
    assert_eq!(
        body,
        vec![
            SMALL_TUPLE_EXT,
            4,
            SMALL_INTEGER_EXT,
            DOP_REG_SEND,
            SMALL_INTEGER_EXT,
            1,
            SMALL_INTEGER_EXT,
            2,
            SMALL_INTEGER_EXT,
            3,
            // --- the message term follows the tuple ---
            SMALL_INTEGER_EXT,
            4,
        ]
    );
    let message = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None)
        .expect("decode_control");
    assert_eq!(message.dop, Dop::RegSend);
    assert_eq!(
        message.terms,
        vec![make_small(1), make_small(2), make_small(3), make_small(4)]
    );
}

#[test]
fn every_opcode_is_tuple_element_one() {
    let fixture = Fixture::new();
    let body = encode_control(Dop::RegSend, &[make_small(7)], &fixture.atoms).unwrap();
    assert_eq!(body[0], SMALL_TUPLE_EXT);
    assert_eq!(body[2], SMALL_INTEGER_EXT);
    assert_eq!(body[3], DOP_REG_SEND);
}

// ---------------------------------------------------------------------------
// Round-trips, one per `{DOP_*, …}` shape (`dist.c`).
// ---------------------------------------------------------------------------

#[test]
fn link_round_trips() {
    // `{DOP_LINK, FromPid, ToPid}` (`dist.c:2228`, `tuple_arity == 3`): 2 terms.
    let mut fixture = Fixture::new();
    let from = fixture.atom("from");
    let to = fixture.atom("to");
    fixture.round_trip(Dop::Link, &[from, to]);
}

#[test]
fn reg_send_round_trips() {
    // `{DOP_REG_SEND, From, Cookie, ToName}` (`dist.c:2476`): 3 terms.
    let mut fixture = Fixture::new();
    let from = fixture.atom("from");
    let cookie = fixture.atom("cookie");
    let to_name = fixture.atom("to_name");
    fixture.round_trip(Dop::RegSend, &[from, cookie, to_name]);
}

#[test]
fn monitor_p_round_trips() {
    // `{DOP_MONITOR_P, RemotePid, LocalPidOrName, Ref}` (`dist.c:2348`): 3 terms.
    let mut fixture = Fixture::new();
    let watcher = fixture.atom("watcher");
    let watched = fixture.atom("watched");
    let reference = fixture.atom("reference");
    fixture.round_trip(Dop::MonitorP, &[watcher, watched, reference]);
}

#[test]
fn exit_tt_round_trips() {
    // `{DOP_EXIT_TT, FromPid, ToPid, Token, Reason}` (`dist.c:2774`): 4 terms.
    let mut fixture = Fixture::new();
    let from = fixture.atom("from");
    let to = fixture.atom("to");
    let token = fixture.atom("token");
    let reason = fixture.atom("noproc");
    fixture.round_trip(Dop::ExitTt, &[from, to, token, reason]);
}

#[test]
fn send_tt_round_trips() {
    // `{DOP_SEND_TT, FromPid, ToPid, Token}` (`dist.c:2515`): 3 terms.
    let mut fixture = Fixture::new();
    let from = fixture.atom("from");
    let to = fixture.atom("to");
    let token = fixture.atom("token");
    fixture.round_trip(Dop::SendTt, &[from, to, token]);
}

#[test]
fn spawn_request_round_trips() {
    // `{DOP_SPAWN_REQUEST, Ref, From, GL, MFA, Opts}` (`dist.c:2921`): 5 terms.
    let mut fixture = Fixture::new();
    let reference = fixture.atom("reference");
    let from = fixture.atom("from");
    let group_leader = fixture.atom("group_leader");
    let mfa = fixture.atom("mfa");
    let opts = fixture.atom("opts");
    fixture.round_trip(
        Dop::SpawnRequest,
        &[reference, from, group_leader, mfa, opts],
    );
}

#[test]
fn unlink_id_round_trips() {
    // `{DOP_UNLINK_ID, Id, FromPid, ToPid}` (`dist.c:2287`): 3 terms.
    let mut fixture = Fixture::new();
    let id = make_small(17);
    let from = fixture.atom("from");
    let to = fixture.atom("to");
    fixture.round_trip(Dop::UnlinkId, &[id, from, to]);
}

#[test]
fn mixed_value_kinds_round_trip() {
    // Every immediate family at once — atoms, `NIL` and both integer wire forms
    // (`SMALL_INTEGER_EXT` for 7 and `INTEGER_EXT` for 100_000) — so the codec
    // seam is exercised beyond a single tag. `{DOP_MONITOR_P_EXIT,
    // RemotePidOrName, LocalPid, Ref, Reason}` (`dist.c:2716`): 4 terms.
    let mut fixture = Fixture::new();
    let watched = fixture.atom("watched");
    let watcher = fixture.atom("watcher");
    let body = fixture.round_trip(
        Dop::MonitorPExit,
        &[watched, watcher, NIL, make_small(100_000)],
    );
    assert!(body.len() > 2 + 4, "every argument is on the wire");

    // The integer wire form is chosen by value, never narrowed: 7 is one byte
    // after its tag, 100_000 (`0x0001_86A0`) is a 4-byte `INTEGER_EXT`.
    let small = encode_control(Dop::Send, &[make_small(7), make_small(1)], &fixture.atoms).unwrap();
    assert_eq!(
        small,
        vec![
            SMALL_TUPLE_EXT,
            3,
            SMALL_INTEGER_EXT,
            DOP_SEND,
            SMALL_INTEGER_EXT,
            7,
            SMALL_INTEGER_EXT,
            1,
        ]
    );
    let big =
        encode_control(Dop::Send, &[make_small(100_000), make_small(1)], &fixture.atoms).unwrap();
    assert_eq!(
        big,
        vec![
            SMALL_TUPLE_EXT,
            3,
            SMALL_INTEGER_EXT,
            DOP_SEND,
            INTEGER_EXT,
            0x00,
            0x01,
            0x86,
            0xA0,
            SMALL_INTEGER_EXT,
            1,
        ]
    );
}

// ---------------------------------------------------------------------------
// Arity checking.
// ---------------------------------------------------------------------------

#[test]
fn too_few_terms_for_the_opcode_is_rejected() {
    // `DOP_REG_SEND` needs 3 terms; a tuple with only 2 must not decode.
    let mut fixture = Fixture::new();
    let from = fixture.atom("from");
    let cookie = fixture.atom("cookie");
    let body = encode_control(Dop::RegSend, &[from, cookie], &fixture.atoms).unwrap();
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn too_many_terms_for_the_opcode_is_rejected() {
    // `DOP_LINK` takes exactly two control arguments and carries no message; a
    // tuple with a third element is a malformed message.
    let mut fixture = Fixture::new();
    let body = encode_control(
        Dop::Link,
        &[make_small(1), make_small(2), make_small(3)],
        &fixture.atoms,
    )
    .unwrap();
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn zero_terms_for_a_non_nullary_opcode_is_rejected() {
    let mut fixture = Fixture::new();
    let body = encode_control(Dop::Link, &[], &fixture.atoms).unwrap();
    assert_eq!(body, vec![SMALL_TUPLE_EXT, 1, SMALL_INTEGER_EXT, Dop::Link.as_u8()]);
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn altact_sig_send_is_variable_arity() {
    // `dist.c:2625` only requires `tuple_arity >= 4` (>= 3 terms here) because
    // "the tuple might be extended with more elements in the future"; `arity()`
    // is therefore the *minimum*, and the minimum plus further elements (the
    // token, and anything a later release appends) must decode too.
    let mut fixture = Fixture::new();
    let minimum = Dop::AltactSigSend.arity();
    assert!(minimum > 0, "a control opcode has at least one argument");

    let shortest: Vec<Eterm> = (0..minimum as i64).map(make_small).collect();
    fixture.round_trip(Dop::AltactSigSend, &shortest);

    let mut extended = shortest.clone();
    extended.push(make_small(42));
    extended.push(make_small(43));
    fixture.round_trip(Dop::AltactSigSend, &extended);

    // Below the minimum is still malformed.
    let short = encode_control(Dop::AltactSigSend, &shortest[..minimum - 1], &fixture.atoms)
        .expect("encode_control");
    let result = decode_control(&short, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// Malformed input.
// ---------------------------------------------------------------------------

#[test]
fn empty_body_is_rejected() {
    let mut fixture = Fixture::new();
    let result = decode_control(&[], &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn a_non_tuple_body_is_rejected() {
    // `dist.c:2205` requires `is_tuple(arg)`: a body that is not a tuple — here a
    // bare small integer and an atom — must be rejected.
    let mut fixture = Fixture::new();
    for body in [
        vec![SMALL_INTEGER_EXT, DOP_SEND],      // a bare small integer
        vec![ATOM_UTF8_EXT, 1, b'x'],           // a bare atom
        vec![0x00, 0x00],                       // not an external-term tag at all
    ] {
        let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
        assert!(
            matches!(result, Err(BeamError::BadArg)),
            "a non-tuple body must be rejected: {body:?}"
        );
    }
}

#[test]
fn the_old_flattened_body_is_rejected() {
    // `dop byte ++ body-form terms` is *not* the ERTS wire form: the leading byte
    // (here `2` = `DOP_SEND`) is not a tuple tag, so it must not decode.
    let mut fixture = Fixture::new();
    let body = vec![DOP_SEND, SMALL_INTEGER_EXT, 1, SMALL_INTEGER_EXT, 2];
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn zero_arity_tuple_is_rejected() {
    // A control tuple always holds at least the opcode (`dist.c:2206`).
    let mut fixture = Fixture::new();
    let result = decode_control(&[SMALL_TUPLE_EXT, 0], &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn a_non_small_integer_opcode_is_rejected() {
    // Element 1 must be a `SMALL_INTEGER_EXT` small integer (`dist.c:2206`).
    let mut fixture = Fixture::new();
    let atom_opcode = vec![SMALL_TUPLE_EXT, 1, ATOM_UTF8_EXT, 1, b'x'];
    let result = decode_control(&atom_opcode, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));

    // A `SMALL_INTEGER_EXT` that is not an `enum dop` value (0 and 5 are unused;
    // 4 = `DOP_UNLINK`, 6 = `DOP_REG_SEND`).
    for opcode in [0u8, 5u8] {
        let body = vec![SMALL_TUPLE_EXT, 2, SMALL_INTEGER_EXT, opcode, SMALL_INTEGER_EXT, 7];
        let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
        assert!(
            matches!(result, Err(BeamError::BadArg)),
            "opcode {opcode} must be rejected"
        );
    }
}

#[test]
fn truncated_control_tuple_is_rejected() {
    let mut fixture = Fixture::new();
    // The tuple claims three elements but only the opcode and one argument follow.
    let body = vec![SMALL_TUPLE_EXT, 3, SMALL_INTEGER_EXT, DOP_SEND, SMALL_INTEGER_EXT, 1];
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));

    // A tuple header with no elements at all.
    let result = decode_control(&[SMALL_TUPLE_EXT], &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));

    // A complete tuple whose last element is an incomplete term.
    let full = encode_control(Dop::Send, &[make_small(1), make_small(2)], &fixture.atoms).unwrap();
    let truncated = &full[..full.len() - 1];
    let result = decode_control(truncated, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn trailing_bytes_after_a_message_less_opcode_are_rejected() {
    // `DOP_LINK` carries no message, so a term after the tuple is malformed.
    let mut fixture = Fixture::new();
    let mut body = encode_control(Dop::Link, &[make_small(1), make_small(2)], &fixture.atoms)
        .expect("encode_control");
    body.push(SMALL_INTEGER_EXT);
    body.push(9);
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn opcode_only_body_for_a_non_nullary_opcode_is_rejected() {
    let mut fixture = Fixture::new();
    // `{DOP_SEND}` — arity 1, no control arguments.
    let body = vec![SMALL_TUPLE_EXT, 1, SMALL_INTEGER_EXT, DOP_SEND];
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// The atom-cache seam (`ATOM_CACHE_REF`, `82`).
// ---------------------------------------------------------------------------

/// `{DOP_SEND, <cache-ref to slot 7>, 3}` — hand-built because `beam_codec` M1
/// never *emits* cache refs, so the wire image must be written out by hand.
fn send_tuple_with_ref_to_slot_7() -> [u8; 8] {
    [
        SMALL_TUPLE_EXT,
        3,
        SMALL_INTEGER_EXT,
        DOP_SEND,
        ATOM_CACHE_REF_EXT,
        7,
        SMALL_INTEGER_EXT,
        3,
    ]
}

#[test]
fn atom_cache_refs_resolve_through_the_supplied_cache() {
    let mut fixture = Fixture::new();
    let cached = fixture.intern("cached");
    let mut cache = AtomCache::new();
    cache.set(7, cached);

    let body = send_tuple_with_ref_to_slot_7();
    let message = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, Some(&cache))
        .expect("decode_control with an atom cache");
    assert_eq!(message.dop, Dop::Send);
    assert_eq!(message.terms, vec![make_atom(cached), make_small(3)]);
}

#[test]
fn atom_cache_ref_without_a_cache_is_rejected() {
    let mut fixture = Fixture::new();
    let body = send_tuple_with_ref_to_slot_7();
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn atom_cache_ref_to_an_unset_slot_is_rejected() {
    let mut fixture = Fixture::new();
    let cache = AtomCache::new();
    let body = send_tuple_with_ref_to_slot_7();
    let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, Some(&cache));
    assert!(matches!(result, Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// The outbound atom cache seam (`encode_control_cached`).
// ---------------------------------------------------------------------------

/// Records every atom the encoder offers, in order, and returns a fixed index for
/// exactly one chosen atom (the rest are written inline).
struct ChosenCache {
    /// The one atom this cache accepts.
    chosen: Atom,
    /// The payload index returned for `chosen`.
    index: u8,
    /// Every atom the encoder consulted, in payload order.
    seen: Vec<Atom>,
}

impl beam_codec::AtomCacheWriter for ChosenCache {
    fn intern(&mut self, atom: Atom, _atoms: &AtomTable) -> Option<u8> {
        self.seen.push(atom);
        (atom == self.chosen).then_some(self.index)
    }
}

#[test]
fn encode_control_cached_without_a_cache_is_byte_identical() {
    let mut fixture = Fixture::new();
    let from = fixture.atom("from");
    let to = fixture.atom("to_name");
    let terms = [from, to];

    let plain = encode_control(Dop::Link, &terms, &fixture.atoms).expect("encode_control");
    let cached =
        encode_control_cached(Dop::Link, &terms, &fixture.atoms, None).expect("no cache");
    assert_eq!(plain, cached, "the `None` path is byte-identical to `encode_control`");
}

#[test]
fn encode_control_cached_writes_a_cache_ref_for_a_cached_atom() {
    let mut fixture = Fixture::new();
    let cached_atom = fixture.intern("to_name");
    let from_atom = fixture.intern("from");

    let mut cache = ChosenCache { chosen: cached_atom, index: 7, seen: Vec::new() };
    let body = encode_control_cached(
        Dop::Link,
        &[make_atom(from_atom), make_atom(cached_atom)],
        &fixture.atoms,
        Some(&mut cache),
    )
    .expect("encode_control_cached");

    // The uncached atom is written inline; the cached one becomes `82, 7`.
    let mut expected = vec![SMALL_TUPLE_EXT, 3, SMALL_INTEGER_EXT, Dop::Link.as_u8()];
    expected.extend_from_slice(
        &beam_codec::encode_term_body(make_atom(from_atom), &fixture.atoms).unwrap(),
    );
    expected.extend_from_slice(&[ATOM_CACHE_REF_EXT, 7]);
    assert_eq!(body, expected);

    // The cache was consulted for every atom, in payload order.
    assert_eq!(cache.seen, vec![from_atom, cached_atom]);
}

#[test]
fn encode_control_cached_reaches_the_trailing_message_term() {
    let mut fixture = Fixture::new();
    let msg_atom = fixture.intern("payload");

    let mut cache = ChosenCache { chosen: msg_atom, index: 3, seen: Vec::new() };
    let body = encode_control_cached(
        Dop::Send,
        &[make_small(1), make_small(2), make_atom(msg_atom)],
        &fixture.atoms,
        Some(&mut cache),
    )
    .expect("encode_control_cached");

    // `{DOP_SEND, 1, 2}` has no atoms; the trailing message atom is the only one and
    // it was cached as `82, 3`.
    assert_eq!(
        body,
        vec![
            SMALL_TUPLE_EXT,
            3,
            SMALL_INTEGER_EXT,
            DOP_SEND,
            SMALL_INTEGER_EXT,
            1,
            SMALL_INTEGER_EXT,
            2,
            ATOM_CACHE_REF_EXT,
            3,
        ]
    );
    assert_eq!(cache.seen, vec![msg_atom], "only the trailing message carries an atom");
}
