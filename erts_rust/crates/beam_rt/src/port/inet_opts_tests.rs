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

//! Units for the per-port inet option mirror: the record walker, the inherited-code table, the
//! success rule and the [`InetOptions`] recorder.
//!
//! What a *replayed* socket really does with these records is pinned end to end by
//! [`completion`](crate::port::completion)'s accept path; these units pin the wire decode, which
//! is where a mis-split record list would silently corrupt every following record.

use super::*;

/// One fixed-length record: `[code, val_be32]`.
fn rec(code: u8, value: u32) -> Vec<u8> {
    let mut out = vec![code];
    out.extend_from_slice(&value.to_be_bytes());
    out
}

// -- the walker ---------------------------------------------------------------------------

#[test]
fn a_walk_splits_fixed_records_and_reads_their_values() {
    let arg = [rec(INET_LOPT_PACKET, 2), rec(INET_LOPT_MODE, 1)].concat();
    let records = walk(&arg).expect("two fixed records walk");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].code, INET_LOPT_PACKET);
    assert_eq!(records[0].value, 2);
    assert_eq!(records[0].bytes, &rec(INET_LOPT_PACKET, 2)[..]);
    assert_eq!(records[1].code, INET_LOPT_MODE);
    assert_eq!(records[1].value, INET_MODE_BINARY);
}

#[test]
fn an_active_counter_record_steps_over_its_two_trailing_bytes() {
    // `[active, INET_MULTI, N_be16]` then a following packet record.
    let mut active = rec(INET_LOPT_ACTIVE, INET_MULTI as u32);
    active.extend_from_slice(&3u16.to_be_bytes());
    let arg = [active, rec(INET_LOPT_PACKET, 2)].concat();
    let records = walk(&arg).expect("the counter record is walked");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].bytes.len(), RECORD + ACTIVE_COUNTER);
    assert_eq!(records[1].code, INET_LOPT_PACKET);
}

#[test]
fn the_variable_length_records_step_over_their_payloads() {
    // linger: 5 + 4.
    let mut linger = rec(INET_OPT_LINGER, 1);
    linger.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(walk(&linger).expect("linger walks")[0].bytes.len(), RECORD + 4);
    // bind_to_device: value is the payload length.
    let mut device = rec(INET_OPT_BIND_TO_DEVICE, 4);
    device.extend_from_slice(b"eth0");
    assert_eq!(walk(&device).expect("bind_to_device walks")[0].bytes.len(), 9);
    // raw: 5 + type(4) + len(4) + payload. The *second* extra word is the length word, so a
    // one-byte payload is `RECORD + 8 + 1`.
    let mut raw = rec(INET_OPT_RAW, 6);
    raw.extend_from_slice(&0u32.to_be_bytes());
    raw.extend_from_slice(&1u32.to_be_bytes());
    raw.push(0x01);
    assert_eq!(walk(&raw).expect("raw walks")[0].bytes.len(), RECORD + 8 + 1);
    // Reading the length word at the right offset is what distinguishes the two: a large `type`
    // (the first extra word) with an empty payload is still `RECORD + 8`, not `RECORD + 8 + 99`.
    let mut empty_raw = rec(INET_OPT_RAW, 6);
    empty_raw.extend_from_slice(&99u32.to_be_bytes());
    empty_raw.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(
        walk(&empty_raw).expect("empty raw walks")[0].bytes.len(),
        RECORD + 8
    );
    // multicast_if is not variable-length at all — the C's arm re-reads the value it already has
    // and steps over nothing — so it must leave the next record at the very next byte.
    let mif = [rec(INET_OPT_MULTICAST_IF, 0x7f00_0001), rec(INET_LOPT_PACKET, 2)].concat();
    let mif_records = walk(&mif).expect("multicast_if walks");
    assert_eq!(mif_records.len(), 2);
    assert_eq!(mif_records[0].bytes.len(), RECORD);
    assert_eq!(mif_records[1].code, INET_LOPT_PACKET);
    // The membership payload is the record's own domain: 12 bytes for inet, 20 for inet6.
    let mut mreq4 = rec(INET_OPT_ADD_MEMBERSHIP, INET_AF_INET as u32);
    mreq4.extend_from_slice(&[0; 12]);
    assert_eq!(
        walk(&mreq4).expect("ipv4 membership walks")[0].bytes.len(),
        RECORD + 12
    );
    let mut mreq6 = rec(INET_OPT_DROP_MEMBERSHIP, INET_AF_INET6 as u32);
    mreq6.extend_from_slice(&[0; 20]);
    assert_eq!(
        walk(&mreq6).expect("ipv6 membership walks")[0].bytes.len(),
        RECORD + 20
    );
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

// -- the inherited-code table -------------------------------------------------------------

#[test]
fn the_inherited_set_is_the_c_tcp_inet_copy_block() {
    for code in [
        INET_LOPT_BUFFER,
        INET_LOPT_HEADER,
        INET_LOPT_PACKET,
        INET_LOPT_MODE,
        INET_LOPT_DELIVER,
        INET_LOPT_EXITONCLOSE,
        INET_LOPT_TCP_HIWTRMRK,
        INET_LOPT_TCP_LOWTRMRK,
        INET_LOPT_TCP_SEND_TIMEOUT,
        INET_LOPT_PACKET_SIZE,
        INET_LOPT_TCP_SEND_TIMEOUT_CLOSE,
        INET_LOPT_MSGQ_HIWTRMRK,
        INET_LOPT_MSGQ_LOWTRMRK,
        INET_LOPT_TCP_SHOW_ECONNRESET,
        INET_LOPT_TCP_READ_AHEAD,
    ] {
        assert!(
            is_inherited_record(&rec(code, 1)),
            "code {code} is part of tcp_inet_copy"
        );
    }
    // Not inherited: `active` (the Erlang side re-applies it, and the C's arm ADDS to the
    // counter — replaying would double-arm), the kernel-backed options, and the delimiter.
    for code in [INET_LOPT_ACTIVE, INET_OPT_LINGER, 0, 10, 12, 33, 41] {
        assert!(
            !is_inherited_record(&rec(code, 1)),
            "code {code} is not part of tcp_inet_copy"
        );
    }
    assert!(!is_inherited_record(&[]), "an empty record inherits nothing");
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

// -- the recorder --------------------------------------------------------------------------

#[test]
fn a_successful_setopts_is_recorded_record_by_record_in_arrival_order() {
    let mut options = InetOptions::default();
    let first = [rec(INET_LOPT_PACKET, 2), rec(INET_LOPT_MODE, 0)].concat();
    let second = [rec(INET_LOPT_HEADER, 2)].concat();
    assert!(options.record(7, &first));
    assert!(options.record(7, &second));
    let mirror = options.get(7).expect("port 7 has a mirror");
    // `{mode, list}` was the second record of the first call, and it is the driver's mode.
    assert_eq!(mirror.mode(), Some(StreamType::List));
    // The replay source is every record, in arrival order across calls.
    let codes: Vec<u8> = mirror.records().iter().map(|record| record[0]).collect();
    assert_eq!(codes, vec![INET_LOPT_PACKET, INET_LOPT_MODE, INET_LOPT_HEADER]);
    // The inherited filter keeps the order and drops nothing here.
    let inherited: Vec<u8> = mirror
        .records()
        .iter()
        .filter(|record| is_inherited_record(record))
        .map(|record| record[0])
        .collect();
    assert_eq!(inherited, vec![INET_LOPT_PACKET, INET_LOPT_MODE, INET_LOPT_HEADER]);
}

#[test]
fn an_active_record_is_recorded_but_filtered_out_of_the_replay() {
    let mut options = InetOptions::default();
    let mut active = rec(INET_LOPT_ACTIVE, INET_MULTI as u32);
    active.extend_from_slice(&3u16.to_be_bytes());
    assert!(options.record(1, &active));
    let mirror = options.get(1).expect("recorded");
    assert_eq!(mirror.records().len(), 1);
    assert!(
        !is_inherited_record(&mirror.records()[0]),
        "active must never be replayed: the C's ACTIVE arm ADDS to the counter"
    );
    // `active` never touches the mode mirror.
    assert_eq!(mirror.mode(), None);
}

#[test]
fn a_rejected_or_empty_or_truncated_argument_records_nothing() {
    let mut options = InetOptions::default();
    assert!(!options.record(3, &[]), "no options inherit nothing");
    assert!(!options.record(3, &[INET_LOPT_PACKET, 0]), "truncated");
    assert!(options.get(3).is_none());
}

#[test]
fn a_seeded_mode_never_overwrites_an_observed_one() {
    let mut options = InetOptions::default();
    options.seed_mode(9, StreamType::List);
    assert_eq!(options.get(9).and_then(PortInetOptions::mode), Some(StreamType::List));
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
fn forgetting_a_port_drops_its_mirror_so_a_recycled_number_starts_clean() {
    let mut options = InetOptions::default();
    options.record(5, &rec(INET_LOPT_MODE, INET_MODE_BINARY as u32));
    assert!(options.get(5).is_some());
    options.forget_port(5);
    assert!(options.get(5).is_none());
    // A new port that gets the same number inherits nothing from the dead one.
    options.seed_mode(5, StreamType::Binary);
    assert_eq!(
        options.get(5).map(PortInetOptions::records).map(<[Vec<u8>]>::len),
        Some(0)
    );
}
