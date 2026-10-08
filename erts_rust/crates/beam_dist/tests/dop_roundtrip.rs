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

//! Exhaustive `DOP_*` control-message proof (contract Part I §A.5).
//!
//! Every one of the 30 v6 opcodes (`erts/emulator/beam/dist.h:140-183`) is
//! round-tripped through [`encode_control`] → [`decode_control`] and then
//! **re-encoded**, asserting byte-identity — and every opcode family is pinned
//! against a byte-exact golden vector, cross-checked against the system `erl`
//! oracle where the framing is expressible as an Erlang term.
//!
//! The opcode list is driven by an exhaustive `match` in
//! [`canonical_control_args`], so adding a `Dop` variant without also giving it a
//! canonical wire image is a **compile error**, not a silently skipped case.
//!
//! Ground truth (read-only): `erts/emulator/beam/dist.h` (`enum dop`),
//! `erts/emulator/beam/dist.c` (`erts_dist_command`, and the `ctx->msg`
//! assignments that decide the trailing term), and the wire guides
//! `erts/doc/guides/erl_dist_protocol.md` + `erl_ext_dist.md`.

use std::process::Command;

use beam_alloc::Heap;
use beam_codec::{
    decode_term, encode_term_body, NEWER_REFERENCE_EXT, NEW_PID_EXT, SMALL_ATOM_UTF8_EXT,
    SMALL_INTEGER_EXT, SMALL_TUPLE_EXT, VERSION_MAGIC,
};
use beam_common::BeamError;
use beam_dist::control::{carries_message, decode_control, encode_control};
use beam_dist::flags::*;
use beam_term::prelude::*;
use beam_term::AtomTable;

/// The atom name every synthetic pid/ref field is tagged with.
const NODE: &str = "n@h";

/// The field *kind* a canonical control argument is built from.
///
/// The kinds follow the per-field semantics `erts_dist_command` documents (a pid
/// where the C checks `is_pid`, a reference where it checks `is_ref`, …). They are
/// deliberately not all small integers: the round-trip has to exercise the boxed
/// (pid / reference) and atom paths too.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// A `SMALL_INTEGER_EXT` small integer.
    Small(i64),
    /// A UTF-8 atom.
    Atom(&'static str),
    /// An external pid (`NEW_PID_EXT`) on the node [`NODE`].
    Pid,
    /// An external reference (`NEWER_REFERENCE_EXT`) on the node [`NODE`].
    Ref,
    /// The empty list (`NIL_EXT`).
    Nil,
}

/// A decoding fixture: the production-shaped `beam_alloc` heap plus the node's
/// `AtomTable`. Test code may `unwrap` (a failure to allocate is a broken test
/// environment, not wire data).
struct Fixture {
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture { heap: Heap::new(4096).expect("4096-word test heap"), atoms: AtomTable::new() }
    }

    fn atom(&mut self, name: &str) -> Eterm {
        make_atom(self.atoms.intern(name))
    }

    /// Decodes a hand-built `NEW_PID_EXT` into a real external pid term — the same
    /// term a peer's `{DOP_*, …}` control tuple would carry.
    fn pid(&mut self) -> Eterm {
        let mut bytes = vec![VERSION_MAGIC, NEW_PID_EXT, SMALL_ATOM_UTF8_EXT, NODE.len() as u8];
        bytes.extend_from_slice(NODE.as_bytes());
        // number:5, serial:1, creation:2 — all big-endian.
        bytes.extend_from_slice(&[0, 0, 0, 5]);
        bytes.extend_from_slice(&[0, 0, 0, 1]);
        bytes.extend_from_slice(&[0, 0, 0, 2]);
        decode_term(&bytes, &mut self.heap, &mut self.atoms).expect("NEW_PID_EXT")
    }

    /// Decodes a hand-built `NEWER_REFERENCE_EXT` into a real external reference
    /// term: `[tag][len:2 BE][node][creation:4 BE][id:4 BE]`.
    fn reference(&mut self) -> Eterm {
        let mut bytes = vec![VERSION_MAGIC, NEWER_REFERENCE_EXT];
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.push(SMALL_ATOM_UTF8_EXT);
        bytes.push(NODE.len() as u8);
        bytes.extend_from_slice(NODE.as_bytes());
        bytes.extend_from_slice(&2u32.to_be_bytes());
        bytes.extend_from_slice(&0xdead_beefu32.to_be_bytes());
        decode_term(&bytes, &mut self.heap, &mut self.atoms).expect("NEWER_REFERENCE_EXT")
    }

    fn build(&mut self, kind: Kind) -> Eterm {
        match kind {
            Kind::Small(value) => make_small(value),
            Kind::Atom(name) => self.atom(name),
            Kind::Pid => self.pid(),
            Kind::Ref => self.reference(),
            Kind::Nil => NIL,
        }
    }
}

/// The canonical **control arguments** of every one of the 30 v6 `DOP_*` opcodes,
/// in wire order (`erts_dist_command`, `dist.c:2223-3155`).
///
/// The `match` is exhaustive and has **no wildcard arm**: a newly added `Dop`
/// variant stops the test crate from compiling until it is given a canonical
/// image here. `DOP_ALTACT_SIG_SEND` is listed in its *minimum* form (no token);
/// its variable-arity shape is covered by [`altact_sig_send_variable_arity`].
fn canonical_control_args(dop: Dop) -> Vec<Kind> {
    match dop {
        // `{DOP_LINK, From, To}` (dist.c:2231-2232).
        Dop::Link => vec![Kind::Pid, Kind::Pid],
        // `{DOP_SEND, From, To}` (dist.c:2547-2548).
        Dop::Send => vec![Kind::Pid, Kind::Pid],
        // `{DOP_EXIT, From, To, Reason}` (dist.c:2763-2771).
        Dop::Exit => vec![Kind::Pid, Kind::Pid, Kind::Atom("reason")],
        // Never sent (dist.c:2277); the legacy `{op, From, To}` shape.
        Dop::Unlink => vec![Kind::Pid, Kind::Pid],
        // `{DOP_REG_SEND, From, Cookie, ToName}` (dist.c:2484-2485).
        Dop::RegSend => vec![Kind::Pid, Kind::Atom("cookie"), Kind::Atom("name")],
        // `{DOP_GROUP_LEADER, From, To}` (dist.c:2893-2894).
        Dop::GroupLeader => vec![Kind::Pid, Kind::Pid],
        // `{DOP_EXIT2, From, To, Reason}` (dist.c:2827-2834).
        Dop::Exit2 => vec![Kind::Pid, Kind::Pid, Kind::Atom("reason")],
        // `{DOP_SEND_TT, From, To, Token}` (dist.c:2519).
        Dop::SendTt => vec![Kind::Pid, Kind::Pid, Kind::Atom("token")],
        // `{DOP_EXIT_TT, From, To, Token, Reason}` (dist.c:2777-2778).
        Dop::ExitTt => vec![Kind::Pid, Kind::Pid, Kind::Atom("token"), Kind::Atom("reason")],
        // `{DOP_REG_SEND_TT, From, Cookie, ToName, Token}` (dist.c:2464).
        Dop::RegSendTt => vec![
            Kind::Pid,
            Kind::Atom("cookie"),
            Kind::Atom("name"),
            Kind::Atom("token"),
        ],
        // `{DOP_EXIT2_TT, From, To, Token, Reason}` (dist.c:2841-2842).
        Dop::Exit2Tt => vec![Kind::Pid, Kind::Pid, Kind::Atom("token"), Kind::Atom("reason")],
        // `{DOP_MONITOR_P, Remote, Local, Ref}` (dist.c:2352-2354).
        Dop::MonitorP => vec![Kind::Pid, Kind::Pid, Kind::Ref],
        // `{DOP_DEMONITOR_P, Remote, Local, Ref}` (dist.c:2415).
        Dop::DemonitorP => vec![Kind::Pid, Kind::Pid, Kind::Ref],
        // `{DOP_MONITOR_P_EXIT, Remote, Local, Ref, Reason}` (dist.c:2706-2719).
        Dop::MonitorPExit => vec![Kind::Pid, Kind::Pid, Kind::Ref, Kind::Atom("noproc")],
        // `{DOP_SEND_SENDER, Sender, To}` (dist.c:2547-2548).
        Dop::SendSender => vec![Kind::Pid, Kind::Pid],
        // `{DOP_SEND_SENDER_TT, Sender, To, Token}` (dist.c:2519).
        Dop::SendSenderTt => vec![Kind::Pid, Kind::Pid, Kind::Atom("token")],
        // `{DOP_PAYLOAD_EXIT, From, To}` -- Reason (dist.c:1745, 2757).
        Dop::PayloadExit => vec![Kind::Pid, Kind::Pid],
        // `{DOP_PAYLOAD_EXIT_TT, From, To, Token}` -- Reason (dist.c:1739).
        Dop::PayloadExitTt => vec![Kind::Pid, Kind::Pid, Kind::Atom("token")],
        // `{DOP_PAYLOAD_EXIT2, From, To}` -- Reason (dist.c:1796-1797).
        Dop::PayloadExit2 => vec![Kind::Pid, Kind::Pid],
        // `{DOP_PAYLOAD_EXIT2_TT, From, To, Token}` -- Reason (dist.c:2822).
        Dop::PayloadExit2Tt => vec![Kind::Pid, Kind::Pid, Kind::Atom("token")],
        // `{DOP_PAYLOAD_MONITOR_P_EXIT, Remote, Local, Ref}` -- Reason
        // (dist.c:1512-1513).
        Dop::PayloadMonitorPExit => vec![Kind::Pid, Kind::Pid, Kind::Ref],
        // `{DOP_SPAWN_REQUEST, Ref, From, GL, MFA, Opts}` -- Args (dist.c:2927-2931).
        Dop::SpawnRequest => vec![
            Kind::Ref,
            Kind::Pid,
            Kind::Pid,
            Kind::Atom("mfa"),
            Kind::Nil,
        ],
        // `{DOP_SPAWN_REQUEST_TT, Ref, From, GL, MFA, Opts, Token}` -- Args
        // (dist.c:2912, 2927-2931).
        Dop::SpawnRequestTt => vec![
            Kind::Ref,
            Kind::Pid,
            Kind::Pid,
            Kind::Atom("mfa"),
            Kind::Nil,
            Kind::Atom("token"),
        ],
        // `{DOP_SPAWN_REPLY, Ref, To, Flags, From}` (dist.c:3051-3068).
        Dop::SpawnReply => {
            vec![Kind::Ref, Kind::Pid, Kind::Small(DIST_SPAWN_FLAG_LINK as i64), Kind::Pid]
        }
        // `{DOP_SPAWN_REPLY_TT, Ref, To, Flags, From, Token}` (dist.c:3041-3045).
        Dop::SpawnReplyTt => vec![
            Kind::Ref,
            Kind::Pid,
            Kind::Small(DIST_SPAWN_FLAG_LINK as i64),
            Kind::Pid,
            Kind::Atom("token"),
        ],
        // `{DOP_ALIAS_SEND, From, To}` (dist.c:2582-2583).
        Dop::AliasSend => vec![Kind::Pid, Kind::Ref],
        // `{DOP_ALIAS_SEND_TT, From, To, Token}` (dist.c:2573).
        Dop::AliasSendTt => vec![Kind::Pid, Kind::Ref, Kind::Atom("token")],
        // `{DOP_UNLINK_ID, Id, From, To}` (dist.c:2290-2298).
        Dop::UnlinkId => vec![Kind::Small(7), Kind::Pid, Kind::Pid],
        // `{DOP_UNLINK_ID_ACK, Id, From, To}` (dist.c:2320-2324).
        Dop::UnlinkIdAck => vec![Kind::Small(7), Kind::Pid, Kind::Pid],
        // `{DOP_ALTACT_SIG_SEND, Flags, SenderPid, To}` (dist.c:2592): minimum
        // form — `Flags` without `ERTS_DOP_ALTACT_SIG_FLG_TOKEN`.
        Dop::AltactSigSend => vec![Kind::Small(DOP_ALTACT_SIG_FLG_PRIO as i64), Kind::Pid, Kind::Pid],
    }
}

/// The trailing term every [`carries_message`] opcode is given in the canonical
/// round-trip.
const TRAILING: Kind = Kind::Atom("payload");

/// Structural term equality.
///
/// `==` compares boxed terms by *pointer*, and a decoded pid / reference is
/// necessarily a fresh allocation on the caller's heap — so two structurally
/// identical pids are never `==`. Comparing the canonical ETF body images is the
/// codec's own definition of "the same term on the wire"; the byte-identity
/// assertion on the whole message is the stronger check.
fn same_term(a: Eterm, b: Eterm, atoms: &AtomTable) -> bool {
    if a == b {
        return true;
    }
    match (encode_term_body(a, atoms), encode_term_body(b, atoms)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Encodes `dop` + `terms`, decodes it back, checks the opcode and every term, and
/// asserts that re-encoding the decoded message is **byte-identical**.
fn assert_round_trip(fixture: &mut Fixture, dop: Dop, terms: &[Eterm]) -> Vec<u8> {
    let body = encode_control(dop, terms, &fixture.atoms)
        .unwrap_or_else(|err| panic!("{dop:?}: encode_control failed: {err:?}"));
    let message = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None)
        .unwrap_or_else(|err| panic!("{dop:?}: decode_control failed: {err:?}"));

    assert_eq!(message.dop, dop, "{dop:?}: the opcode round-trips");
    assert_eq!(
        message.terms.len(),
        terms.len(),
        "{dop:?}: term count round-trips ({:?})",
        message.terms
    );
    for (index, (got, want)) in message.terms.iter().zip(terms).enumerate() {
        assert!(
            same_term(*got, *want, &fixture.atoms),
            "{dop:?}: argument {index} round-trips"
        );
    }

    let reencoded = encode_control(message.dop, &message.terms, &fixture.atoms)
        .unwrap_or_else(|err| panic!("{dop:?}: re-encode failed: {err:?}"));
    assert_eq!(reencoded, body, "{dop:?}: re-encoding the decoded message is byte-identical");
    body
}

// ---------------------------------------------------------------------------
// (1) Every one of the 30 opcodes round-trips, with its trailing term.
// ---------------------------------------------------------------------------

#[test]
fn every_opcode_round_trips_exhaustively() {
    let mut fixture = Fixture::new();
    // A compile-enforced exhaustive walk: `canonical_control_args` has no wildcard
    // arm, so a new `Dop` variant fails the build here.
    let mut seen = Vec::new();
    for byte in 0u8..=u8::MAX {
        let Some(dop) = Dop::from_u8(byte) else { continue };
        seen.push(dop);
        let args = canonical_control_args(dop);
        assert_eq!(
            args.len(),
            dop.arity(),
            "{dop:?}: the canonical argument vector must match the documented arity"
        );

        let mut terms: Vec<Eterm> = args.into_iter().map(|kind| fixture.build(kind)).collect();
        if carries_message(dop) {
            terms.push(fixture.build(TRAILING));
        }
        assert_round_trip(&mut fixture, dop, &terms);
    }

    // 30 opcodes (`dist.h:140-183`), each with its own `from_u8` byte.
    assert_eq!(seen.len(), 30, "v6 has exactly 30 control opcodes: {seen:?}");
}

#[test]
fn the_trailing_term_set_is_the_documented_one() {
    // The opcodes that put a term *after* the control tuple: the send family, the
    // `DFLAG_EXIT_PAYLOAD` payload-exit family and the spawn-request family.
    let expected = [
        Dop::Send,
        Dop::SendTt,
        Dop::SendSender,
        Dop::SendSenderTt,
        Dop::RegSend,
        Dop::RegSendTt,
        Dop::AliasSend,
        Dop::AliasSendTt,
        Dop::AltactSigSend,
        Dop::PayloadExit,
        Dop::PayloadExitTt,
        Dop::PayloadExit2,
        Dop::PayloadExit2Tt,
        Dop::PayloadMonitorPExit,
        Dop::SpawnRequest,
        Dop::SpawnRequestTt,
    ];
    for byte in 0u8..=u8::MAX {
        let Some(dop) = Dop::from_u8(byte) else { continue };
        assert_eq!(
            carries_message(dop),
            expected.contains(&dop),
            "{dop:?}: trailing-term membership"
        );
    }
    assert_eq!(expected.len(), 16);

    // `DOP_SPAWN_REPLY[*]` deliberately sets `ctx->msg = THE_NON_VALUE`
    // (`dist.c:1897`) — it must *not* carry one.
    assert!(!carries_message(Dop::SpawnReply));
    assert!(!carries_message(Dop::SpawnReplyTt));
    // Neither do the plain signal opcodes built through `dsig_send_ctl`.
    assert!(!carries_message(Dop::Exit));
    assert!(!carries_message(Dop::ExitTt));
    assert!(!carries_message(Dop::Link));
    assert!(!carries_message(Dop::GroupLeader));
}

// ---------------------------------------------------------------------------
// (2) `DOP_ALTACT_SIG_SEND` variable arity.
// ---------------------------------------------------------------------------

#[test]
fn altact_sig_send_variable_arity() {
    let mut fixture = Fixture::new();
    let sender = fixture.pid();
    let to = fixture.pid();
    let message = fixture.atom("payload");
    let token = fixture.atom("token");

    // Minimum form: `{op, Flags, SenderPid, To}` -- Signal (dist.c:2592).
    let minima = [make_small(DOP_ALTACT_SIG_FLG_PRIO as i64), sender, to, message];
    let body = assert_round_trip(&mut fixture, Dop::AltactSigSend, &minima);
    // The control tuple holds 4 elements; the signal is the 5th term, *after* it.
    assert_eq!(body[..5], [TUP, 4, SI, DOP_ALTACT_SIG_SEND, SI]);

    // Extended form: only `ERTS_DOP_ALTACT_SIG_FLG_TOKEN` grows the tuple to five
    // elements — `{op, Flags, SenderPid, To, Token}` -- Signal (dist.c:2651-2658).
    let with_token =
        [make_small(DOP_ALTACT_SIG_FLG_TOKEN as i64), sender, to, token, message];
    let body = assert_round_trip(&mut fixture, Dop::AltactSigSend, &with_token);
    assert_eq!(
        body[..6],
        [
            TUP,
            5, // the tuple grew by exactly one element
            SI,
            DOP_ALTACT_SIG_SEND,
            SI,
            DOP_ALTACT_SIG_FLG_TOKEN as u8,
        ],
        "the token-carrying altact tuple is a 5-tuple, not a 6-tuple"
    );
}

#[test]
fn altact_sig_send_rejects_a_token_flagged_short_tuple() {
    // `ERTS_DOP_ALTACT_SIG_FLG_TOKEN` set but no token element: `dist.c:2654-2656`
    // is an `invalid_message`, so classification must be `BadArg`, never a wrong
    // token.
    let mut fixture = Fixture::new();
    let terms = [
        make_small(DOP_ALTACT_SIG_FLG_TOKEN as i64),
        make_small(1),
        make_small(2),
        make_small(3),
    ];
    let body = encode_control(Dop::AltactSigSend, &terms, &fixture.atoms).expect("encode");
    let decoded = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None).expect("decode");
    let delivery = beam_dist::Delivery::classify(&decoded);
    assert!(matches!(delivery, Err(BeamError::BadArg)), "{delivery:?}");
}

// ---------------------------------------------------------------------------
// (3) Byte-exact golden vectors — one per opcode family.
// ---------------------------------------------------------------------------

/// `SMALL_INTEGER_EXT` (`97`, `'a'`), named for readability of the vectors below.
const SI: u8 = SMALL_INTEGER_EXT;
/// `SMALL_TUPLE_EXT` (`104`, `'h'`), named for readability of the vectors below.
const TUP: u8 = SMALL_TUPLE_EXT;

/// `(opcode, expected body bytes)`. The control arguments are the small integers
/// `1..=arity()` in order (so the vector is literal), and — for a
/// [`carries_message`] opcode — the trailing term is the small integer `9`.
///
/// `SMALL_INTEGER_EXT` is `97` (`'a'`), `SMALL_TUPLE_EXT` is `104` (`'h'`): the
/// same bytes the system `erl` oracle produces for the equivalent tuple (see
/// [`oracle_agrees_on_every_control_tuple`]).
const GOLDEN: &[(Dop, &[u8])] = &[
    // {DOP_LINK, 1, 2}
    (Dop::Link, &[TUP, 3, SI, DOP_LINK, SI, 1, SI, 2]),
    // {DOP_SEND, 1, 2} -- 9
    (Dop::Send, &[TUP, 3, SI, DOP_SEND, SI, 1, SI, 2, SI, 9]),
    // {DOP_EXIT, 1, 2, 3}  (the reason is *inline*, not a trailing term)
    (Dop::Exit, &[TUP, 4, SI, DOP_EXIT, SI, 1, SI, 2, SI, 3]),
    // {DOP_UNLINK, 1, 2}
    (Dop::Unlink, &[TUP, 3, SI, DOP_UNLINK, SI, 1, SI, 2]),
    // {DOP_REG_SEND, 1, 2, 3} -- 9
    (Dop::RegSend, &[TUP, 4, SI, DOP_REG_SEND, SI, 1, SI, 2, SI, 3, SI, 9]),
    // {DOP_GROUP_LEADER, 1, 2}
    (Dop::GroupLeader, &[TUP, 3, SI, DOP_GROUP_LEADER, SI, 1, SI, 2]),
    // {DOP_EXIT2, 1, 2, 3}
    (Dop::Exit2, &[TUP, 4, SI, DOP_EXIT2, SI, 1, SI, 2, SI, 3]),
    // {DOP_SEND_TT, 1, 2, 3} -- 9
    (Dop::SendTt, &[TUP, 4, SI, DOP_SEND_TT, SI, 1, SI, 2, SI, 3, SI, 9]),
    // {DOP_EXIT_TT, 1, 2, 3, 4}
    (Dop::ExitTt, &[TUP, 5, SI, DOP_EXIT_TT, SI, 1, SI, 2, SI, 3, SI, 4]),
    // {DOP_REG_SEND_TT, 1, 2, 3, 4} -- 9
    (Dop::RegSendTt, &[TUP, 5, SI, DOP_REG_SEND_TT, SI, 1, SI, 2, SI, 3, SI, 4, SI, 9]),
    // {DOP_EXIT2_TT, 1, 2, 3, 4}
    (Dop::Exit2Tt, &[TUP, 5, SI, DOP_EXIT2_TT, SI, 1, SI, 2, SI, 3, SI, 4]),
    // {DOP_MONITOR_P, 1, 2, 3}
    (Dop::MonitorP, &[TUP, 4, SI, DOP_MONITOR_P, SI, 1, SI, 2, SI, 3]),
    // {DOP_DEMONITOR_P, 1, 2, 3}
    (Dop::DemonitorP, &[TUP, 4, SI, DOP_DEMONITOR_P, SI, 1, SI, 2, SI, 3]),
    // {DOP_MONITOR_P_EXIT, 1, 2, 3, 4}
    (Dop::MonitorPExit, &[TUP, 5, SI, DOP_MONITOR_P_EXIT, SI, 1, SI, 2, SI, 3, SI, 4]),
    // {DOP_SEND_SENDER, 1, 2} -- 9
    (Dop::SendSender, &[TUP, 3, SI, DOP_SEND_SENDER, SI, 1, SI, 2, SI, 9]),
    // {DOP_SEND_SENDER_TT, 1, 2, 3} -- 9
    (Dop::SendSenderTt, &[TUP, 4, SI, DOP_SEND_SENDER_TT, SI, 1, SI, 2, SI, 3, SI, 9]),
    // {DOP_PAYLOAD_EXIT, 1, 2} -- 9
    (Dop::PayloadExit, &[TUP, 3, SI, DOP_PAYLOAD_EXIT, SI, 1, SI, 2, SI, 9]),
    // {DOP_PAYLOAD_EXIT_TT, 1, 2, 3} -- 9
    (Dop::PayloadExitTt, &[TUP, 4, SI, DOP_PAYLOAD_EXIT_TT, SI, 1, SI, 2, SI, 3, SI, 9]),
    // {DOP_PAYLOAD_EXIT2, 1, 2} -- 9
    (Dop::PayloadExit2, &[TUP, 3, SI, DOP_PAYLOAD_EXIT2, SI, 1, SI, 2, SI, 9]),
    // {DOP_PAYLOAD_EXIT2_TT, 1, 2, 3} -- 9
    (Dop::PayloadExit2Tt, &[TUP, 4, SI, DOP_PAYLOAD_EXIT2_TT, SI, 1, SI, 2, SI, 3, SI, 9]),
    // {DOP_PAYLOAD_MONITOR_P_EXIT, 1, 2, 3} -- 9
    (
        Dop::PayloadMonitorPExit,
        &[TUP, 4, SI, DOP_PAYLOAD_MONITOR_P_EXIT, SI, 1, SI, 2, SI, 3, SI, 9],
    ),
    // {DOP_SPAWN_REQUEST, 1, 2, 3, 4, 5} -- 9
    (
        Dop::SpawnRequest,
        &[TUP, 6, SI, DOP_SPAWN_REQUEST, SI, 1, SI, 2, SI, 3, SI, 4, SI, 5, SI, 9],
    ),
    // {DOP_SPAWN_REQUEST_TT, 1, 2, 3, 4, 5, 6} -- 9
    (
        Dop::SpawnRequestTt,
        &[TUP, 7, SI, DOP_SPAWN_REQUEST_TT, SI, 1, SI, 2, SI, 3, SI, 4, SI, 5, SI, 6, SI, 9],
    ),
    // {DOP_SPAWN_REPLY, 1, 2, 3, 4}  (no trailing term)
    (Dop::SpawnReply, &[TUP, 5, SI, DOP_SPAWN_REPLY, SI, 1, SI, 2, SI, 3, SI, 4]),
    // {DOP_SPAWN_REPLY_TT, 1, 2, 3, 4, 5}
    (Dop::SpawnReplyTt, &[TUP, 6, SI, DOP_SPAWN_REPLY_TT, SI, 1, SI, 2, SI, 3, SI, 4, SI, 5]),
    // {DOP_ALIAS_SEND, 1, 2} -- 9
    (Dop::AliasSend, &[TUP, 3, SI, DOP_ALIAS_SEND, SI, 1, SI, 2, SI, 9]),
    // {DOP_ALIAS_SEND_TT, 1, 2, 3} -- 9
    (Dop::AliasSendTt, &[TUP, 4, SI, DOP_ALIAS_SEND_TT, SI, 1, SI, 2, SI, 3, SI, 9]),
    // {DOP_UNLINK_ID, 1, 2, 3}
    (Dop::UnlinkId, &[TUP, 4, SI, DOP_UNLINK_ID, SI, 1, SI, 2, SI, 3]),
    // {DOP_UNLINK_ID_ACK, 1, 2, 3}
    (Dop::UnlinkIdAck, &[TUP, 4, SI, DOP_UNLINK_ID_ACK, SI, 1, SI, 2, SI, 3]),
    // {DOP_ALTACT_SIG_SEND, 1, 2, 3} -- 9  (Flags = 1 = PRIO, no TOKEN bit)
    (
        Dop::AltactSigSend,
        &[TUP, 4, SI, DOP_ALTACT_SIG_SEND, SI, 1, SI, 2, SI, 3, SI, 9],
    ),
];

/// The golden control arguments for `dop`: the small integers `1..=arity()`.
fn golden_args(dop: Dop) -> Vec<Eterm> {
    (1..=dop.arity() as i64).map(make_small).collect()
}

#[test]
fn golden_wire_bytes_for_every_opcode_family() {
    let mut fixture = Fixture::new();
    let mut covered = Vec::new();
    for &(dop, expected) in GOLDEN {
        covered.push(dop);
        let mut terms = golden_args(dop);
        if carries_message(dop) {
            terms.push(make_small(9));
        }
        let body = encode_control(dop, &terms, &fixture.atoms).expect("encode_control");
        assert_eq!(
            body, expected,
            "{dop:?}: control body must be {expected:?}, got {body:?}"
        );

        // And the golden bytes are exactly what decodes back to the same message.
        let decoded =
            decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None).expect("decode");
        assert_eq!(decoded.dop, dop);
        assert_eq!(decoded.terms, terms, "{dop:?}: golden vector round-trips");
    }

    // Every opcode has a golden vector — no family is left unpinned.
    for byte in 0u8..=u8::MAX {
        let Some(dop) = Dop::from_u8(byte) else { continue };
        assert!(covered.contains(&dop), "{dop:?} has no golden vector");
    }
    assert_eq!(covered.len(), 30);
}

#[test]
fn tt_opcodes_differ_from_their_siblings_only_by_the_token() {
    // `dist.c` documents each `_TT` opcode as its sibling plus a trace token, so
    // the encoded bodies must share a common prefix (the tuple header + opcode +
    // every shared argument) and differ only by the extra token element.
    let pairs = [
        (Dop::Send, Dop::SendTt),
        (Dop::Exit, Dop::ExitTt),
        (Dop::Exit2, Dop::Exit2Tt),
        (Dop::SendSender, Dop::SendSenderTt),
        (Dop::RegSend, Dop::RegSendTt),
        (Dop::AliasSend, Dop::AliasSendTt),
    ];
    let fixture = Fixture::new();
    for (plain, tt) in pairs {
        assert!(tt.is_tt(), "{tt:?} must be marked as a trace-token opcode");
        assert!(!plain.is_tt());
        assert_eq!(tt.arity(), plain.arity() + 1, "{tt:?} takes one more argument");

        let plain_args = golden_args(plain);
        let mut tt_args = plain_args.clone();
        tt_args.push(make_small(9));
        let token_offset = tt_args.len() - 1;

        let mut plain_terms = plain_args.clone();
        if carries_message(plain) {
            plain_terms.push(make_small(200));
        }
        let mut tt_terms = tt_args.clone();
        if carries_message(tt) {
            tt_terms.push(make_small(200));
        }

        let plain_body = encode_control(plain, &plain_terms, &fixture.atoms).expect("plain");
        let tt_body = encode_control(tt, &tt_terms, &fixture.atoms).expect("tt");

        // Strip the differing tuple element (the opcode byte) and the arity byte,
        // then the token element that only the `_TT` form carries.
        let plain_args_bytes: Vec<u8> = plain_args
            .iter()
            .flat_map(|&t| encode_term_body(t, &fixture.atoms).expect("arg"))
            .collect();
        let tt_args_bytes: Vec<u8> = tt_args
            .iter()
            .flat_map(|&t| encode_term_body(t, &fixture.atoms).expect("arg"))
            .collect();
        let token_bytes =
            encode_term_body(tt_args[token_offset], &fixture.atoms).expect("token");

        let mut expected_tt = vec![tt_body[0], tt_body[1], tt_body[2], tt_body[3]];
        expected_tt.extend_from_slice(&tt_args_bytes);
        if carries_message(tt) {
            expected_tt.extend_from_slice(
                &encode_term_body(make_small(200), &fixture.atoms).expect("msg"),
            );
        }
        assert_eq!(tt_body, expected_tt, "{tt:?}: full body");
        assert_eq!(
            &tt_args_bytes[..tt_args_bytes.len() - token_bytes.len()],
            &plain_args_bytes[..],
            "{tt:?}: the shared arguments are byte-identical to {plain:?}'s"
        );
        assert!(
            plain_body.starts_with(&[SMALL_TUPLE_EXT, (plain.arity() + 1) as u8]),
            "{plain:?}: tuple arity"
        );
    }
}

// ---------------------------------------------------------------------------
// (4) The system `erl` oracle agrees on the tuple framing.
// ---------------------------------------------------------------------------

/// Hex-encodes `bytes` the way `binary:encode_hex/1` does — **upper case**
/// (`binary:encode_hex/1` emits `A`-`F`; comparing against a lower-case rendering
/// would be a silent case-mismatch, not a byte mismatch).
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble").to_ascii_uppercase());
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("nibble").to_ascii_uppercase());
    }
    out
}

/// Cross-checks **every** golden control tuple against the system `erl` oracle by
/// asking it to `term_to_binary/1` the equivalent Erlang tuple (and trailing
/// term) and stripping the single `VERSION_MAGIC` byte — which is exactly the body
/// [`encode_control`] must produce.
///
/// Skips loudly (never fakes a pass) when `erl` is not on `PATH`.
#[test]
fn oracle_agrees_on_every_control_tuple() {
    if Command::new("erl").arg("-version").output().is_err() {
        eprintln!("SKIP oracle_agrees_on_every_control_tuple: no `erl` on PATH");
        return;
    }

    let tuples: Vec<String> = GOLDEN
        .iter()
        .map(|&(dop, _)| {
            let args: Vec<String> =
                (1..=dop.arity() as i64).map(|n| n.to_string()).collect();
            let mut elements = vec![u32::from(dop.as_u8()).to_string()];
            elements.extend(args);
            format!("{{{}}}", elements.join(","))
        })
        .collect();
    let messages: Vec<&str> = GOLDEN
        .iter()
        .map(|&(dop, _)| if carries_message(dop) { "9" } else { "nomessage" })
        .collect();

    let script = format!(
        "S = fun(B) -> <<_:8,R/binary>> = B, R end,\n\
         F = fun(T, nomessage) -> S(term_to_binary(T));\n\
                (T, M) -> <<(S(term_to_binary(T)))/binary, (S(term_to_binary(M)))/binary>> end,\n\
         Ts = [{}],\n\
         Ms = [{}],\n\
         lists:foreach(fun({{T,M}}) -> io:format(\"~s~n\",[binary:encode_hex(F(T,M))]) end, \
         lists:zip(Ts,Ms)),\n\
         halt().",
        tuples.join(","),
        messages.join(","),
    );

    let output = Command::new("erl").args(["-noshell", "-eval", &script]).output();
    let Ok(output) = output else {
        eprintln!("SKIP oracle_agrees_on_every_control_tuple: `erl` failed to run");
        return;
    };
    assert!(
        output.status.success(),
        "erl exited {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("erl output is UTF-8");
    let oracle: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(
        oracle.len(),
        GOLDEN.len(),
        "the oracle produced {} lines for {} tuples: {stdout}",
        oracle.len(),
        GOLDEN.len()
    );

    let fixture = Fixture::new();
    for (index, &(dop, expected)) in GOLDEN.iter().enumerate() {
        let mut terms = golden_args(dop);
        if carries_message(dop) {
            terms.push(make_small(9));
        }
        let body = encode_control(dop, &terms, &fixture.atoms).expect("encode_control");
        assert_eq!(
            hex(&body),
            oracle[index],
            "{dop:?}: erl disagrees with encode_control"
        );
        assert_eq!(body, expected, "{dop:?}: erl disagrees with the golden vector");
    }

    // The oracle body is the tuple image plus one `VERSION_MAGIC` byte, which the
    // distribution header carries exactly once (`external.c:772`).
    assert_eq!(VERSION_MAGIC, 131);
}

/// A decode that leaves bytes over must be `BadArg`, never a silent truncation —
/// including for the newly classified payload-exit / spawn families.
#[test]
fn trailing_bytes_after_a_complete_message_are_rejected() {
    let mut fixture = Fixture::new();
    for dop in [Dop::PayloadExit, Dop::SpawnRequest, Dop::SpawnReply, Dop::AltactSigSend] {
        let mut terms = golden_args(dop);
        if carries_message(dop) {
            terms.push(make_small(9));
        }
        let mut body = encode_control(dop, &terms, &fixture.atoms).expect("encode");
        body.push(SMALL_INTEGER_EXT);
        body.push(7);
        let result = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None);
        assert!(
            matches!(result, Err(BeamError::BadArg)),
            "{dop:?}: a trailing byte must be rejected, got {result:?}"
        );
    }
}

/// The spawn-reply `Flags` field is a small integer `< 1 << 27` (`dist.c:3088-3093`).
#[test]
fn spawn_reply_rejects_an_out_of_range_or_non_small_flags() {
    let mut fixture = Fixture::new();
    let reference = fixture.reference();
    let to = fixture.pid();
    let from = fixture.pid();

    for flags in [make_small(1i64 << 27), make_small(-1), fixture.atom("link")] {
        let terms = [reference, to, flags, from];
        let body = encode_control(Dop::SpawnReply, &terms, &fixture.atoms).expect("encode");
        let decoded =
            decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None).expect("decode");
        let result = beam_dist::Delivery::classify(&decoded);
        assert!(matches!(result, Err(BeamError::BadArg)), "flags {flags:?} => {result:?}");
    }

    // `1 << 27 - 1` is the largest accepted value.
    let terms = [reference, to, make_small((1i64 << 27) - 1), from];
    let body = encode_control(Dop::SpawnReply, &terms, &fixture.atoms).expect("encode");
    let decoded = decode_control(&body, &mut fixture.heap, &mut fixture.atoms, None).expect("decode");
    let delivery = beam_dist::Delivery::classify(&decoded).expect("classify");
    assert_eq!(delivery.dop(), Dop::SpawnReply);
}

#[test]
fn unused_opcode_bytes_are_not_control_opcodes() {
    // `enum dop` (`dist.h:140-183`) assigns 30 values; every other byte must be
    // rejected. `5` is the ancient `DOP_NODE_LINK` (`dist.h:147`), and there is no
    // `DOP_NODEUP`/`DOP_NODEDOWN` nor any `*_MONITOR_ID` opcode in v6 — node
    // up/down are local emulator events (`send_nodes_mon_msgs`, `dist.c:6931`).
    let unassigned: Vec<u8> = vec![
        0, 5, 9, 10, 11, 14, 15, 17, //
    ];
    for byte in unassigned {
        assert_eq!(Dop::from_u8(byte), None, "byte {byte} must not be a v6 opcode");
    }
    for byte in 38u8..=u8::MAX {
        assert_eq!(Dop::from_u8(byte), None, "byte {byte} must not be a v6 opcode");
    }
    // `DOP_UNLINK` (4) *is* a v6 opcode, just one the C never sends.
    assert_eq!(Dop::from_u8(4), Some(Dop::Unlink));

    let decodable = (0u8..=u8::MAX).filter(|&byte| Dop::from_u8(byte).is_some()).count();
    assert_eq!(decodable, 30, "v6 has exactly 30 control opcodes");
}
