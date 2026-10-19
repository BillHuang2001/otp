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

//! Units for the per-port inet option mirror: the record walker, the success rule and the
//! [`InetOptions`] latches (including the two an accepted socket inherits).
//!
//! What a *real* socket does with these records is pinned end to end by
//! [`completion`](crate::port::completion)'s accept path
//! (`accept_inherit_tests.rs`); these units pin the wire decode, which is where a mis-split record
//! list would silently corrupt every following record — and the latch semantics the two surviving
//! consumers ([`PortRegistry::data_mode`] and the `peer_closed` gates) read.

use super::*;

/// `INET_LOPT_HEADER` (21) — `desc->inet.hsz`, a plain fixed-length record the walker must step
/// over. Declared here because only the walk needs it, not the latches.
const INET_LOPT_HEADER: u8 = 21;
/// `INET_LOPT_PACKET` (23) — `desc->inet.htype`, another plain fixed-length record.
const INET_LOPT_PACKET: u8 = 23;

/// One fixed-length record: `[code, val_be32]`.
fn rec(code: u8, value: u32) -> Vec<u8> {
    let mut out = vec![code];
    out.extend_from_slice(&value.to_be_bytes());
    out
}

/// A bare registry — the latch accessors are [`PortRegistry`] methods.
fn bare_registry() -> PortRegistry {
    PortRegistry::new().expect("an empty port registry")
}

// -- the walker ---------------------------------------------------------------------------

/// [`walk`] refuses an argument with a leftover tail, so a single-record walk that **succeeds** is
/// itself the proof that the record's span was stepped exactly: a span one byte too short leaves a
/// tail and a span one byte too long runs out of bytes.
#[test]
fn a_walk_splits_fixed_records_and_reads_their_values() {
    let arg = [rec(INET_LOPT_PACKET, 2), rec(INET_LOPT_MODE, 1)].concat();
    let records = walk(&arg).expect("two fixed records walk");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].code, INET_LOPT_PACKET);
    assert_eq!(records[0].value, 2);
    assert_eq!(records[1].code, INET_LOPT_MODE);
    assert_eq!(records[1].value, INET_MODE_BINARY);
}

#[test]
fn an_active_counter_record_steps_over_its_two_trailing_bytes() {
    // `[active, INET_MULTI, N_be16]` then a following packet record: the second record is only
    // reachable at the right offset if the two counter bytes were stepped over.
    let mut active = rec(INET_LOPT_ACTIVE, INET_MULTI as u32);
    active.extend_from_slice(&3u16.to_be_bytes());
    let arg = [active, rec(INET_LOPT_PACKET, 2)].concat();
    let records = walk(&arg).expect("the counter record is walked");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].code, INET_LOPT_ACTIVE);
    assert_eq!(records[0].value, INET_MULTI);
    assert_eq!(records[1].code, INET_LOPT_PACKET);
    assert_eq!(records[1].value, 2);
}

#[test]
fn the_variable_length_records_step_over_their_payloads() {
    // linger: `[opt, val, l_linger]`, so five plus four bytes and nothing left over.
    let mut linger = rec(INET_OPT_LINGER, 1);
    linger.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(walk(&linger).expect("linger walks").len(), 1);
    // bind_to_device: the value *is* the payload length.
    let mut device = rec(INET_OPT_BIND_TO_DEVICE, 4);
    device.extend_from_slice(b"eth0");
    assert_eq!(walk(&device).expect("bind_to_device walks").len(), 1);
    // raw: 5 + type(4) + len(4) + payload. The *second* extra word is the length word, so a
    // one-byte payload is the whole 14 bytes and no tail.
    let mut raw = rec(INET_OPT_RAW, 6);
    raw.extend_from_slice(&0u32.to_be_bytes());
    raw.extend_from_slice(&1u32.to_be_bytes());
    raw.push(0x01);
    assert_eq!(walk(&raw).expect("raw walks").len(), 1);
    // Reading the length word at the right offset is what distinguishes the two: a large `type`
    // (the first extra word) with an empty payload is still `RECORD + 8`, not `RECORD + 8 + 99`.
    let mut empty_raw = rec(INET_OPT_RAW, 6);
    empty_raw.extend_from_slice(&99u32.to_be_bytes());
    empty_raw.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(walk(&empty_raw).expect("empty raw walks").len(), 1);
    // multicast_if is not variable-length at all — the C's arm re-reads the value it already has
    // and steps over nothing — so it must leave the next record at the very next byte.
    let mif = [rec(INET_OPT_MULTICAST_IF, 0x7f00_0001), rec(INET_LOPT_PACKET, 2)].concat();
    let mif_records = walk(&mif).expect("multicast_if walks");
    assert_eq!(mif_records.len(), 2);
    assert_eq!(mif_records[1].code, INET_LOPT_PACKET);
    // The membership payload is the record's own domain: 12 bytes for inet, 20 for inet6.
    let mut mreq4 = rec(INET_OPT_ADD_MEMBERSHIP, INET_AF_INET as u32);
    mreq4.extend_from_slice(&[0; 12]);
    assert_eq!(walk(&mreq4).expect("ipv4 membership walks").len(), 1);
    let mut mreq6 = rec(INET_OPT_DROP_MEMBERSHIP, INET_AF_INET6 as u32);
    mreq6.extend_from_slice(&[0; 20]);
    assert_eq!(walk(&mreq6).expect("ipv6 membership walks").len(), 1);
    // ... so 12 bytes suffice for inet but truncate an inet6 membership record, and a domain the
    // driver itself rejects leaves the span undeterminable.
    let mut short6 = rec(INET_OPT_ADD_MEMBERSHIP, INET_AF_INET6 as u32);
    short6.extend_from_slice(&[0; 12]);
    assert!(walk(&short6).is_none());
    assert!(walk(&rec(INET_OPT_ADD_MEMBERSHIP, 7)).is_none());
}

#[test]
fn a_truncated_argument_walks_to_nothing_rather_than_a_mis_split_list() {
    // A fixed record with its value word cut short.
    assert!(walk(&[INET_LOPT_PACKET, 0, 0]).is_none());
    // A whole record followed by a 3-byte tail: the tail cannot be attributed to a record, so the
    // argument is refused rather than the head record being recorded on its own.
    assert!(walk(&[rec(INET_LOPT_PACKET, 2), vec![INET_LOPT_HEADER, 0, 0]].concat()).is_none());
    // A length-word record whose payload is absent.
    assert!(walk(&rec(INET_OPT_BIND_TO_DEVICE, 9)).is_none());
    // A negative length word.
    assert!(walk(&rec(INET_LOPT_NETNS, u32::MAX)).is_none());
    // A raw record with no length word at all.
    assert!(walk(&rec(INET_OPT_RAW, 0)).is_none());
    // Nothing at all is a valid, empty walk.
    assert_eq!(walk(&[]).expect("empty walks").len(), 0);
}

// -- the success rule and the verb test ----------------------------------------------------

#[test]
fn only_an_inet_rep_ok_reply_counts_as_success() {
    assert!(setopts_succeeded(&[INET_REP_OK]));
    assert!(setopts_succeeded(&[INET_REP_OK, 0, 0]));
    // The C's `[0]einval` and an empty reply are both rejections.
    assert!(!setopts_succeeded(&[0]));
    assert!(!setopts_succeeded(&[]));
}

#[test]
fn the_setopts_request_is_recognised_by_verb_and_by_numeric_opcode() {
    assert!(is_setopts_control(SETOPTS_VERB));
    assert!(is_setopts_control(&(INET_REQ_SETOPTS + INET_CONTROL_MAGIC).to_string()));
    // Neighbouring requests and a driver's private verb are not the mirror's business.
    assert!(!is_setopts_control("open"));
    assert!(!is_setopts_control(&(INET_REQ_SETOPTS + INET_CONTROL_MAGIC + 1).to_string()));
    assert!(!is_setopts_control(""));
    assert!(!is_setopts_control("setopts "));
}

#[test]
fn a_mode_value_maps_onto_the_two_stream_types_and_nothing_else() {
    assert_eq!(mode_of(INET_MODE_BINARY), Some(StreamType::Binary));
    assert_eq!(mode_of(0), Some(StreamType::List));
    assert_eq!(mode_of(7), None);
}

// -- the latches ---------------------------------------------------------------------------

/// The three latched codes are the **only** thing an argument leaves behind, and the last accepted
/// record wins — exactly as the driver's own `desc->inet.mode`/`.active`/`.exitf` fields do.
#[test]
fn the_latches_follow_the_last_accepted_record() {
    let mut options = InetOptions::default();
    let first = [rec(INET_LOPT_PACKET, 2), rec(INET_LOPT_MODE, 0)].concat();
    assert!(options.record(7, &first), "the argument was walked");
    let mirror = options.get(7).expect("port 7 has a mirror");
    assert_eq!(mirror.mode(), Some(StreamType::List));
    // A record naming no latched code (here `{header, 2}`) changes nothing at all — and an
    // argument with only such records leaves no mirror behind (see the next case).
    assert!(options.record(7, &rec(INET_LOPT_HEADER, 2)));
    assert_eq!(options.get(7), Some(&PortInetOptions {
        mode: Some(StreamType::List),
        active: None,
        active_count: 0,
        exit_on_close: None,
    }));    // Last accepted record wins, exactly as the driver's field does.
    assert!(options.record(7, &rec(INET_LOPT_MODE, INET_MODE_BINARY as u32)));
    assert_eq!(
        options.get(7).and_then(PortInetOptions::mode),
        Some(StreamType::Binary)
    );
}

#[test]
fn an_active_record_moves_the_active_latch_and_never_the_mode() {
    let mut options = InetOptions::default();
    let mut active = rec(INET_LOPT_ACTIVE, INET_MULTI as u32);
    active.extend_from_slice(&3u16.to_be_bytes());
    assert!(options.record(1, &active));
    let mirror = options.get(1).expect("recorded");
    assert_eq!(
        mirror.active_mode(),
        Some(INET_MULTI),
        "`{{active,3}}`'s operand is the C's `desc->inet.active`"
    );
    assert_eq!(mirror.mode(), None, "`active` never touches the mode latch");
    assert_eq!(
        mirror.exit_on_close(),
        None,
        "…nor the `exit_on_close` latch"
    );
    // `{active, false}` is the last word again.
    assert!(options.record(1, &rec(INET_LOPT_ACTIVE, 0)));
    assert_eq!(options.get(1).and_then(PortInetOptions::active_mode), Some(0));
}

#[test]
fn an_exit_on_close_record_moves_its_own_latch() {
    let mut options = InetOptions::default();
    assert!(options.record(2, &rec(INET_LOPT_EXITONCLOSE, 0)));
    assert_eq!(
        options.get(2).and_then(PortInetOptions::exit_on_close),
        Some(false),
        "the record's value word is the flag itself (`{{exit_on_close,false}}` is `0`)"
    );
    assert!(options.record(2, &rec(INET_LOPT_EXITONCLOSE, 1)));
    assert_eq!(
        options.get(2).and_then(PortInetOptions::exit_on_close),
        Some(true)
    );
}

#[test]
fn a_rejected_or_empty_or_truncated_argument_records_nothing() {
    let mut options = InetOptions::default();
    assert!(!options.record(3, &[]), "no options inherit nothing");
    assert!(!options.record(3, &[INET_LOPT_PACKET, 0]), "truncated");
    assert!(options.get(3).is_none());
    // A walked argument that names no latched code applies nothing, so no mirror is created: the
    // `None` from `get` keeps meaning "nothing about this port is latched".
    assert!(
        options.record(3, &rec(INET_LOPT_PACKET, 2)),
        "the argument was walked …"
    );
    assert!(options.get(3).is_none(), "… but it named no latch");
}

#[test]
fn a_seeded_mode_never_overwrites_an_observed_one() {
    let mut options = InetOptions::default();
    options.seed_mode(9, StreamType::List);
    assert_eq!(
        options.get(9).and_then(PortInetOptions::mode),
        Some(StreamType::List)
    );
    options.record(9, &rec(INET_LOPT_MODE, INET_MODE_BINARY as u32));
    assert_eq!(
        options.get(9).and_then(PortInetOptions::mode),
        Some(StreamType::Binary),
        "the observed mode wins over the seed"
    );
    options.seed_mode(9, StreamType::List);
    assert_eq!(
        options.get(9).and_then(PortInetOptions::mode),
        Some(StreamType::Binary),
        "a later seed cannot overwrite it either"
    );
}

#[test]
fn a_seeded_exit_on_close_never_overwrites_an_observed_one() {
    let mut options = InetOptions::default();
    options.seed_exit_on_close(8, false);
    assert_eq!(
        options.get(8).and_then(PortInetOptions::exit_on_close),
        Some(false)
    );
    options.record(8, &rec(INET_LOPT_EXITONCLOSE, 1));
    assert_eq!(
        options.get(8).and_then(PortInetOptions::exit_on_close),
        Some(true),
        "the observed flag wins over the seed"
    );
    options.seed_exit_on_close(8, false);
    assert_eq!(
        options.get(8).and_then(PortInetOptions::exit_on_close),
        Some(true),
        "a later seed cannot overwrite it either"
    );
}

/// The adoption setter: exactly the two fields `tcp_inet_copy` copies and the driver cannot carry
/// across (`inet.mode` `inet_drv.c:11811`, `inet.exitf` `:11812`) land on the accepted port, and
/// `inet.active` stays **unknown** because the C's copy list omits it (`:11814`-`:11844`).
#[test]
fn seed_inet_latches_installs_mode_and_exit_on_close_but_leaves_active_unknown() {
    let mut registry = bare_registry();
    let accepted = PortId::new(11);
    registry.seed_inet_latches(accepted, StreamType::List, false);
    let mirror = registry
        .inet_options(accepted)
        .expect("an adopted port has a seeded mirror");
    assert_eq!(mirror.mode(), Some(StreamType::List));
    assert_eq!(mirror.exit_on_close(), Some(false));
    assert_eq!(
        mirror.active_mode(),
        None,
        "`inet.active` is not copied, so the accepted port starts passive"
    );
    // The seeded mode is what the payload renderers resolve through.
    assert_eq!(
        registry.data_mode(accepted, StreamType::Binary),
        StreamType::List,
        "the accepted port renders payloads in the listener's data mode"
    );
    // …and the seeded flag is what the peer-close gate reads.
    assert!(
        !registry.inet_exit_on_close(accepted),
        "the inherited `exitf = 0` keeps the socket half-open"
    );
}

#[test]
fn forgetting_a_port_drops_its_mirror_so_a_recycled_number_starts_clean() {
    let mut options = InetOptions::default();
    options.record(5, &rec(INET_LOPT_MODE, INET_MODE_BINARY as u32));
    options.record(5, &rec(INET_LOPT_ACTIVE, 1));
    assert!(options.get(5).is_some());
    options.forget_port(5);
    assert!(options.get(5).is_none());
    // A new port that gets the same number inherits nothing from the dead one.
    options.seed_mode(5, StreamType::Binary);
    let mirror = options.get(5).expect("the seed created a mirror");
    assert_eq!(mirror.mode(), Some(StreamType::Binary));
    assert_eq!(
        mirror.active_mode(),
        None,
        "the dead port's `{{active,…}}` did not survive the recycle"
    );
    assert_eq!(mirror.exit_on_close(), None);
}
