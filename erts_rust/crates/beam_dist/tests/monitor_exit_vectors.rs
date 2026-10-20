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

#![deny(unsafe_code)]

//! **Golden-vector** pins for the inbound monitor control messages, hand-written
//! byte for byte (no sockets, no `erl`, no encoder):
//!
//! * `DOP_MONITOR_P_EXIT` — `{DOP_MONITOR_P_EXIT, Remote pid or name, Local pid,
//!   Ref, Reason}`, the wire's **first** argument being the *watched* process and
//!   the **second** the *watcher* (`dist.c:2701-2740`, `tuple[2]`/`tuple[3]`);
//! * `DOP_PAYLOAD_MONITOR_P_EXIT` — the same argument order in a 4-tuple, with the
//!   reason arriving as the **trailing** term (`dist.c:2706-2713`);
//! * `DOP_MONITOR_P` — the **reverse** order, `{DOP_MONITOR_P, Remote pid, local
//!   pid or name, Ref}` (watcher first, `dist.c:2341-2350`), pinned next to the
//!   exit forms so the two orders are locked against each other and a future swap
//!   of `watched`/`watcher` cannot pass unnoticed.
//!
//! Every vector below is the exact bytes a real node writes for the control body:
//! the ETF **tuple** (`SMALL_TUPLE_EXT` + arity), the opcode as a
//! `SMALL_INTEGER_EXT` term, then one term per argument (`dist.c:2205-2210`), with
//! the distributions' own atom/pid/ref forms (`SMALL_ATOM_UTF8_EXT`, `NEW_PID_EXT`,
//! `NEWER_REFERENCE_EXT`). Atom, pid and reference terms make the arguments
//! *distinguishable*, which a small-integer-only vector cannot do: `watched`,
//! `watcher` and `ref` each identify a different endpoint, so a mis-ordered decode
//! cannot accidentally compare equal.
//!
//! The decode is the crate's real [`decode_control`]; each vector also round-trips
//! through [`encode_control`], so the hand-written bytes are pinned against the
//! encoder as well as against the C.

use beam_alloc::Heap;
use beam_codec::term::{
    external_pid_parts, external_ref_parts, make_external_pid, tuple_arity, tuple_element,
};
use beam_common::BeamError;
use beam_dist::flags::Dop;
use beam_dist::{decode_control, encode_control, ControlMessage, Delivery, LinkIdentity};
use beam_term::prelude::{atom_val, make_atom, make_internal_pid, make_pid_data, make_small, Eterm};
use beam_term::AtomTable;
/// The node the peer runs on — the node of every `watched`/`ref` endpoint here.
const PEER: &str = "peer@host";
/// This node — the node of the `watcher` pid, and the `local_node` of the links.
const LOCAL: &str = "me@host";
/// A third node, used for the anti-spoof negatives.
const OTHER: &str = "other@host";

// ---------------------------------------------------------------------------
// The hand-written control bodies, one hex string per argument.
//
// `NEW_PID_EXT` (88)   = tag, node atom, id:4, serial:4, creation:4.
// `NEWER_REFERENCE_EXT` (90) = tag, id count:2, node atom, creation:4, ids:4 each.
// `SMALL_ATOM_UTF8_EXT` (119) = tag, byte length:1, UTF-8 bytes.
// ---------------------------------------------------------------------------

/// `watched` — the peer's process that died: `NEW_PID_EXT` on [`PEER`], id 1.
const WATCHED_HEX: &str = concat!(
    "58",                     // NEW_PID_EXT
    "77097065657240686f7374", // SMALL_ATOM_UTF8_EXT (9) "peer@host"
    "00000001",               // id   1
    "00000000",               // serial 0
    "00000000",               // creation 0
);

/// `watcher` — the local (this node's) process that armed the monitor: `NEW_PID_EXT`
/// on [`LOCAL`], id 2.
const WATCHER_HEX: &str = concat!(
    "58",                 // NEW_PID_EXT
    "77076d6540686f7374", // SMALL_ATOM_UTF8_EXT (7) "me@host"
    "00000002",           // id   2
    "00000000",           // serial 0
    "00000000",           // creation 0
);

/// `ref` — the monitor reference *created by the watcher*, echoed back by the peer:
/// `NEWER_REFERENCE_EXT` on [`PEER`] with the single id 7.
const REF_HEX: &str = concat!(
    "5a",                     // NEWER_REFERENCE_EXT
    "0001",                   // one id
    "77097065657240686f7374", // SMALL_ATOM_UTF8_EXT (9) "peer@host"
    "00000000",               // creation 0
    "00000007",               // id 7
);

/// The plain form's `Reason` — the atom `boom`.
const REASON_HEX: &str = concat!(
    "7704",     // SMALL_ATOM_UTF8_EXT (4)
    "626f6f6d", // "boom"
);

/// The payload form's trailing reason — the tuple `{shutdown, 9}`, so the payload
/// slot is a *composite* term (the payload can be any term, not just an atom).
const PAYLOAD_HEX: &str = concat!(
    "6802",                 // SMALL_TUPLE_EXT, arity 2
    "770873687574646f776e", // SMALL_ATOM_UTF8_EXT (8) "shutdown"
    "6109",                 // SMALL_INTEGER_EXT 9
);

/// Decodes a hex control body with the crate's real decoder.
fn decode(hex: &str, heap: &mut Heap, atoms: &mut AtomTable) -> ControlMessage {
    let body = unhex(hex);
    decode_control(&body, heap, atoms, None).expect("the hand-written control body decodes")
}

/// Decodes a hex control body that must be **rejected** (`dist.c`'s
/// `goto invalid_message`).
fn decode_err(hex: &str, heap: &mut Heap, atoms: &mut AtomTable) {
    let body = unhex(hex);
    assert!(
        matches!(decode_control(&body, heap, atoms, None), Err(BeamError::BadArg)),
        "a control body with the wrong tuple arity for the opcode must be BadArg",
    );
}

/// `"5857…"` → the bytes it denotes. Refuses an odd length rather than truncating.
fn unhex(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len() % 2, 0, "a hex string must have an even length: {hex:?}");
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digits = core::str::from_utf8(pair).expect("hex digits are ASCII");
            u8::from_str_radix(digits, 16).expect("a pair of hex digits")
        })
        .collect()
}

/// The `watched` endpoint of a decoded pid term, as
/// `(node name, number, serial, creation)`.
fn pid_parts(term: Eterm, atoms: &mut AtomTable) -> (String, u32, u32, u32) {
    let (node, number, serial, creation) = external_pid_parts(term).expect("an external pid");
    let name = atoms.name(atom_val(node)).expect("an interned node name");
    (name.to_string(), number, serial, creation)
}

// ---------------------------------------------------------------------------
// The plain form: `{DOP_MONITOR_P_EXIT, Watched, Watcher, Ref, Reason}`.
// ---------------------------------------------------------------------------

#[test]
fn plain_monitor_p_exit_pins_watched_first_and_watcher_second() {
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    // `SMALL_TUPLE_EXT` arity 5, then the opcode as a `SMALL_INTEGER_EXT` term — 21
    // is `DOP_MONITOR_P_EXIT` (`dist.h:156`) — then `watched`, `watcher`, `ref`,
    // `reason`, exactly as `dist.c:2701-2719` reads them (`tuple[2]`…`tuple[5]`).
    let hex =
        ["68056115", WATCHED_HEX, WATCHER_HEX, REF_HEX, REASON_HEX].concat();
    let body = unhex(&hex);
    assert_eq!(body[0], 104, "SMALL_TUPLE_EXT");
    assert_eq!(body[1], 5, "the C's `tuple_arity` counts the opcode: 5");
    assert_eq!(body[2], 97, "the opcode is a SMALL_INTEGER_EXT term");
    assert_eq!(
        body[3],
        Dop::MonitorPExit.as_u8(),
        "DOP_MONITOR_P_EXIT is 21 (`dist.h:156`)",
    );
    assert_eq!(Dop::MonitorPExit.arity(), 4, "4 control args = the C's `tuple_arity == 5`");

    let message = decode(&hex, &mut heap, &mut atoms);
    assert_eq!(message.dop, Dop::MonitorPExit);
    assert_eq!(message.terms.len(), 4, "no trailing term for the plain form");

    // The control body round-trips through the encoder byte for byte.
    assert_eq!(
        encode_control(message.dop, &message.terms, &atoms).expect("re-encode"),
        body,
        "the hand-written vector is exactly what the encoder writes",
    );

    let Delivery::MonitorExit { watched, watcher, reference, reason } =
        Delivery::classify(&message).expect("classify")
    else {
        panic!("a DOP_MONITOR_P_EXIT must classify as Delivery::MonitorExit");
    };
    // The *positions*: term 0 is `watched`, term 1 `watcher` (`dist.c:2705-2706`).
    assert_eq!(watched, message.terms[0], "the wire's FIRST argument is `watched`");
    assert_eq!(watcher, message.terms[1], "the wire's SECOND argument is `watcher`");
    assert_eq!(reference, message.terms[2], "the third argument is the reference");
    assert_eq!(reason, message.terms[3], "the fourth argument is the reason");
    // The *identities*: swapping the two would put the local pid where the peer's
    // must be, which these two asserts catch.
    assert_eq!(
        pid_parts(watched, &mut atoms),
        (PEER.to_string(), 1, 0, 0),
        "`watched` is the peer's process (the one that died)",
    );
    assert_eq!(
        pid_parts(watcher, &mut atoms),
        (LOCAL.to_string(), 2, 0, 0),
        "`watcher` is this node's process (the one that armed the monitor)",
    );
    let (ref_node, ids, creation) = external_ref_parts(reference).expect("an external ref");
    assert_eq!(
        atoms.name(atom_val(ref_node)),
        Some(PEER),        "the reference was created by the peer (so it names the peer's node)",
    );
    assert_eq!((ids, creation), (&[7u32][..], 0));
    assert_eq!(reason, make_atom(atoms.intern("boom")), "the reason rides in the tuple");
}

// ---------------------------------------------------------------------------
// The payload form: `{DOP_PAYLOAD_MONITOR_P_EXIT, Watched, Watcher, Ref} -- Reason`.
// ---------------------------------------------------------------------------

#[test]
fn payload_monitor_p_exit_pins_watched_first_and_watcher_second() {
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    // `SMALL_TUPLE_EXT` arity 4 (`dist.c:2707-2710` rejects anything else), opcode 28
    // (`DOP_PAYLOAD_MONITOR_P_EXIT`, `dist.h:163`), three control arguments, and the
    // reason as the **trailing** term — `reason = THE_NON_VALUE` in the C.
    let hex = ["6804611c", WATCHED_HEX, WATCHER_HEX, REF_HEX, PAYLOAD_HEX].concat();
    let body = unhex(&hex);
    assert_eq!(body[1], 4, "the C's `tuple_arity` counts the opcode: 4");
    assert_eq!(body[3], Dop::PayloadMonitorPExit.as_u8());
    assert_eq!(Dop::PayloadMonitorPExit.arity(), 3, "3 control args = the C's `tuple_arity == 4`");

    let message = decode(&hex, &mut heap, &mut atoms);
    assert_eq!(message.dop, Dop::PayloadMonitorPExit);
    assert_eq!(message.terms.len(), 4, "3 control args + the trailing reason");

    assert_eq!(
        encode_control(message.dop, &message.terms, &atoms).expect("re-encode"),
        body,
        "the hand-written vector is exactly what the encoder writes",
    );

    let Delivery::PayloadMonitorExit { watched, watcher, reference, payload } =
        Delivery::classify(&message).expect("classify")
    else {
        panic!("a DOP_PAYLOAD_MONITOR_P_EXIT must classify as Delivery::PayloadMonitorExit");
    };
    assert_eq!(watched, message.terms[0], "the wire's FIRST argument is `watched`");
    assert_eq!(watcher, message.terms[1], "the wire's SECOND argument is `watcher`");
    assert_eq!(reference, message.terms[2], "the third argument is the reference");
    assert_eq!(payload, message.terms[3], "the payload is the trailing term");
    assert_eq!(pid_parts(watched, &mut atoms), (PEER.to_string(), 1, 0, 0));
    assert_eq!(pid_parts(watcher, &mut atoms), (LOCAL.to_string(), 2, 0, 0));
    // The trailing reason is a composite term and is carried whole.
    assert_eq!(tuple_arity(payload).expect("a tuple"), 2);
    assert_eq!(tuple_element(payload, 0).expect("element 0"), make_atom(atoms.intern("shutdown")));
    assert_eq!(tuple_element(payload, 1).expect("element 1"), make_small(9));
}

// ---------------------------------------------------------------------------
// `DOP_MONITOR_P` — the reverse order, locked side by side with the exit form.
// ---------------------------------------------------------------------------

#[test]
fn monitor_p_is_the_reverse_of_monitor_p_exit() {
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    // `{DOP_MONITOR_P, Remote pid, local pid or name, Ref}` (`dist.c:2341-2350`):
    // the **watcher** first, the **watched** second — the mirror image of the exit
    // forms. Opcode 19 (`DOP_MONITOR_P`, `dist.h:154`).
    let hex = ["68046113", WATCHER_HEX, WATCHED_HEX, REF_HEX].concat();
    assert_eq!(Dop::MonitorP.arity(), 3);
    let message = decode(&hex, &mut heap, &mut atoms);
    assert_eq!(message.dop, Dop::MonitorP);

    let Delivery::Monitor { watcher, watched, reference } =
        Delivery::classify(&message).expect("classify")
    else {
        panic!("a DOP_MONITOR_P must classify as Delivery::Monitor");
    };
    // Side by side with the exit form above: term 0 is the WATCHER here and the
    // WATCHED there. These asserts are what a swap of the two would break.
    assert_eq!(watcher, message.terms[0], "DOP_MONITOR_P puts the watcher FIRST");
    assert_eq!(watched, message.terms[1], "DOP_MONITOR_P puts the watched SECOND");
    assert_eq!(pid_parts(watcher, &mut atoms), (LOCAL.to_string(), 2, 0, 0));
    assert_eq!(pid_parts(watched, &mut atoms), (PEER.to_string(), 1, 0, 0));
    assert_eq!(
        external_ref_parts(reference).expect("an external ref").1,
        &[7u32][..],
    );
    // The two orders are genuinely different: term 0 of a `DOP_MONITOR_P` is the
    // watcher, term 0 of a `DOP_MONITOR_P_EXIT` is the watched.
    assert_eq!(
        external_pid_parts(message.terms[0]).expect("pid").1,
        2,
        "DOP_MONITOR_P term 0 is the watcher's pid (id 2)",
    );
}

// ---------------------------------------------------------------------------
// The arity rules: the tuple arity is authoritative `dist.c:2705-2713`.
// ---------------------------------------------------------------------------

#[test]
fn monitor_exit_wire_arity_rules() {
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();

    // Plain form: `tuple_arity != 5` is `invalid_message` (`dist.c:2709-2711`).
    // 4-arity tuple (three arguments — the reason is missing).
    let too_short = ["68046115", WATCHED_HEX, WATCHER_HEX, REF_HEX].concat();
    decode_err(&too_short, &mut heap, &mut atoms);
    // 6-arity tuple (five arguments — the plain form carries no trailing term).
    let too_long = ["68066115", WATCHED_HEX, WATCHER_HEX, REF_HEX, REASON_HEX, "6101"].concat();
    decode_err(&too_long, &mut heap, &mut atoms);

    // Payload form: `tuple_arity != 4` is `invalid_message` (`dist.c:2706-2709`).
    // 5-arity tuple (four positions — the reason promoted into the control tuple).
    let payload_four_args =
        ["6805611c", WATCHED_HEX, WATCHER_HEX, REF_HEX, REASON_HEX].concat();
    decode_err(&payload_four_args, &mut heap, &mut atoms);
    // 3-arity tuple (two arguments).
    let payload_too_short = ["6803611c", WATCHED_HEX, WATCHER_HEX].concat();
    decode_err(&payload_too_short, &mut heap, &mut atoms);

    // … and the *outer* tuple arity byte must agree with the elements it announces:
    // an honest arity whose elements are truncated is a short read, not a message.
    let truncated = ["68056115", WATCHED_HEX, WATCHER_HEX].concat();
    decode_err(&truncated, &mut heap, &mut atoms);
}

// ---------------------------------------------------------------------------
// The validated (per-link) view of the plain form — `dist.c:2721-2740`.
// ---------------------------------------------------------------------------

#[test]
fn plain_monitor_p_exit_validated_view() {
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();
    let link = LinkIdentity::intern(&mut atoms, PEER.as_bytes(), LOCAL.as_bytes())
        .expect("link identity");
    let message =
        decode(&["68056115", WATCHED_HEX, WATCHER_HEX, REF_HEX, REASON_HEX].concat(), &mut heap, &mut atoms);

    // As decoded: `watched` is a peer pid, `watcher` an external pid of **this** node
    // — the C's old-incarnation shape, whose
    // `erts_this_dist_entry == external_pid_dist_entry(watcher)` arm is
    // `monitored_process_not_alive` (a *drop*, `dist.c:2731-2733`), not
    // `invalid_message`, so validation accepts it.
    assert_eq!(
        Delivery::classify_validated(&message, &link).expect("accepted"),
        Delivery::classify(&message).expect("classify"),
        "the validated view is the semantic view once the message is well-formed",
    );

    // `watched` may also be a registered **name** (an atom, `dist.c:2727`: the remote
    // "pid or name" which died).
    let mut named = message.clone();
    named.terms[0] = make_atom(atoms.intern("a_registered_name"));
    assert!(Delivery::classify_validated(&named, &link).is_ok(), "a name is a valid `watched`");

    // `watched` must be an external pid or an atom; an internal pid is neither
    // (`dist.c:2727-2728`).
    let mut internal_watched = message.clone();
    internal_watched.terms[0] = make_internal_pid(make_pid_data(0, 3));
    assert!(
        matches!(Delivery::classify_validated(&internal_watched, &link), Err(BeamError::BadArg)),
        "an internal pid is not a valid `watched`",
    );

    // `watcher`: an **internal** pid is the C's primary path (`dist.c:2730`).
    let mut internal_watcher = message.clone();
    internal_watcher.terms[1] = make_internal_pid(make_pid_data(0, 4));
    assert!(
        Delivery::classify_validated(&internal_watcher, &link).is_ok(),
        "an internal watcher pid is accepted (`dist.c:2730-2735`)",
    );

    // … and an external watcher pid of some **third** node is `invalid_message`
    // (`dist.c:2734`): neither internal nor this node's.
    let mut third_node = message.clone();
    third_node.terms[1] =
        make_external_pid(&mut heap, make_atom(atoms.intern(OTHER)), 5, 0, 0).expect("pid");
    assert!(
        matches!(Delivery::classify_validated(&third_node, &link), Err(BeamError::BadArg)),
        "a watcher on a third node is a spoofed monitor-down",
    );

    // A watcher on the **peer's own** node is `invalid_message` too — the peer may not
    // report a monitor it owns as its own watcher (`dist.c:2733-2735`).
    let mut peer_node = message.clone();
    peer_node.terms[1] =
        make_external_pid(&mut heap, make_atom(atoms.intern(PEER)), 6, 0, 0).expect("pid");
    assert!(
        matches!(Delivery::classify_validated(&peer_node, &link), Err(BeamError::BadArg)),
        "a watcher on the peer's node is not this node's",
    );

    // An atom is not a pid at all (`is_not_internal_pid` then `is_not_external_pid`).
    let mut atom_watcher = message.clone();
    atom_watcher.terms[1] = make_atom(atoms.intern("not_a_pid"));
    assert!(matches!(
        Delivery::classify_validated(&atom_watcher, &link),
        Err(BeamError::BadArg)
    ));

    // `is_not_ref(ref)` (`dist.c:2722-2723`) — a small integer is not a reference.
    let mut bad_ref = message.clone();
    bad_ref.terms[2] = make_small(5);
    assert!(matches!(Delivery::classify_validated(&bad_ref, &link), Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// The validated view of the payload form — the same rules, one argument shorter.
// ---------------------------------------------------------------------------

#[test]
fn payload_monitor_p_exit_validated_view() {
    let mut heap = Heap::new(1 << 16).expect("heap");
    let mut atoms = AtomTable::new();
    let link = LinkIdentity::intern(&mut atoms, PEER.as_bytes(), LOCAL.as_bytes())
        .expect("link identity");
    let message = decode(
        &["6804611c", WATCHED_HEX, WATCHER_HEX, REF_HEX, PAYLOAD_HEX].concat(),
        &mut heap,
        &mut atoms,
    );

    assert!(Delivery::classify_validated(&message, &link).is_ok(), "the well-formed payload form");

    // An atom `watched` (a name) is accepted here exactly as in the plain form.
    let mut named = message.clone();
    named.terms[0] = make_atom(atoms.intern("a_registered_name"));
    assert!(Delivery::classify_validated(&named, &link).is_ok());

    let mut third_node = message.clone();
    third_node.terms[1] =
        make_external_pid(&mut heap, make_atom(atoms.intern(OTHER)), 5, 0, 0).expect("pid");
    assert!(matches!(
        Delivery::classify_validated(&third_node, &link),
        Err(BeamError::BadArg)
    ));

    let mut bad_ref = message.clone();
    bad_ref.terms[2] = make_small(5);
    assert!(matches!(Delivery::classify_validated(&bad_ref, &link), Err(BeamError::BadArg)));

    // The trailing payload is not validated (the C's payload form has
    // `reason = THE_NON_VALUE`): a small integer payload is accepted.
    let mut int_payload = message.clone();
    int_payload.terms[3] = make_small(9);
    assert!(Delivery::classify_validated(&int_payload, &link).is_ok());
}
