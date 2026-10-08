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

//! 2-/4-byte packet framing plus the zero-length tick round-trip over
//! `MemoryTransport`.
//!
//! Every test is hermetic (no sockets): the write side is inspected
//! byte-for-byte and the read side is served from a pre-loaded
//! [`MemoryTransport`].

use beam_common::BeamError;
use beam_dist::framing::{
    is_tick, read_length, read_packet, write_packet, write_tick, PacketHeaderSize,
};
use beam_dist::transport::{MemoryTransport, Read};

#[test]
fn two_byte_framing_round_trip() {
    let mut tx = MemoryTransport::new();
    write_packet(&mut tx, PacketHeaderSize::Two, b"hello").expect("write_packet");
    let wire = tx.into_written();

    // 2-byte big-endian length prefix followed by the body.
    assert_eq!(&wire[..2], &[0x00, 0x05]);
    assert_eq!(&wire[2..], b"hello");

    let mut rx = MemoryTransport::with_input(wire);
    let body = read_packet(&mut rx, PacketHeaderSize::Two).expect("read_packet");
    assert_eq!(body, b"hello");
}

#[test]
fn four_byte_framing_round_trip() {
    let mut tx = MemoryTransport::new();
    write_packet(&mut tx, PacketHeaderSize::Four, b"hello").expect("write_packet");
    let wire = tx.into_written();

    // 4-byte big-endian length prefix followed by the body.
    assert_eq!(&wire[..4], &[0x00, 0x00, 0x00, 0x05]);
    assert_eq!(&wire[4..], b"hello");

    let mut rx = MemoryTransport::with_input(wire);
    let body = read_packet(&mut rx, PacketHeaderSize::Four).expect("read_packet");
    assert_eq!(body, b"hello");
}

#[test]
fn tick_is_a_zero_length_packet() {
    // A tick is written as a length prefix of 0, nothing else.
    let mut t2 = MemoryTransport::new();
    write_tick(&mut t2, PacketHeaderSize::Two).expect("write_tick");
    assert_eq!(t2.written(), &[0x00, 0x00]);

    let mut t4 = MemoryTransport::new();
    write_tick(&mut t4, PacketHeaderSize::Four).expect("write_tick");
    assert_eq!(t4.written(), &[0x00, 0x00, 0x00, 0x00]);

    // A received zero-length packet yields an empty body and is recognised as a tick.
    let mut rx = MemoryTransport::with_input(vec![0, 0]);
    let body = read_packet(&mut rx, PacketHeaderSize::Two).expect("read_packet");
    assert!(body.is_empty());
    assert!(is_tick(&body));
}

#[test]
fn write_packet_rejects_oversize_two_byte_body() {
    let mut tx = MemoryTransport::new();
    // 0x10000 > 0xffff (the 2-byte ceiling): must be rejected, not truncated.
    let big = vec![0u8; 0x10000];
    let res = write_packet(&mut tx, PacketHeaderSize::Two, &big);
    assert!(matches!(res, Err(BeamError::SystemLimit)));
    // Nothing must be written on rejection.
    assert!(tx.written().is_empty());
}

#[test]
fn write_packet_accepts_exactly_two_byte_ceiling() {
    let mut tx = MemoryTransport::new();
    let max = vec![0xa5u8; 0xffff];
    write_packet(&mut tx, PacketHeaderSize::Two, &max).expect("write_packet");
    assert_eq!(&tx.written()[..2], &[0xff, 0xff]);
    assert_eq!(tx.written().len(), 2 + 0xffff);
}

#[test]
fn read_packet_truncated_body_is_badarg() {
    // Advertises a 5-byte body but supplies only 3 body bytes.
    let mut rx = MemoryTransport::with_input(vec![0x00, 0x05, b'a', b'b', b'c']);
    let res = read_packet(&mut rx, PacketHeaderSize::Two);
    assert!(matches!(res, Err(BeamError::BadArg)));
}

#[test]
fn read_packet_truncated_length_prefix_is_badarg() {
    // A 2-byte header with only one length byte available.
    let mut rx = MemoryTransport::with_input(vec![0x00]);
    let res = read_packet(&mut rx, PacketHeaderSize::Two);
    assert!(matches!(res, Err(BeamError::BadArg)));
}

#[test]
fn read_length_two_byte_parses_big_endian() {
    let mut rx = MemoryTransport::with_input(vec![0x01, 0x02]);
    let len = read_length(&mut rx, PacketHeaderSize::Two).expect("read_length");
    assert_eq!(len, 258);
}

#[test]
fn read_length_four_byte_parses_big_endian() {
    let mut rx = MemoryTransport::with_input(vec![0x00, 0x01, 0x00, 0x02]);
    let len = read_length(&mut rx, PacketHeaderSize::Four).expect("read_length");
    assert_eq!(len, 0x0001_0002);
}

#[test]
fn read_exact_short_read_is_badarg_and_consumes_nothing() {
    let mut rx = MemoryTransport::with_input(vec![1, 2, 3]);
    let mut buf = [0u8; 5];
    let res = rx.read_exact(&mut buf);
    assert!(matches!(res, Err(BeamError::BadArg)));
    // No partial consumption: all 3 input bytes remain available.
    assert_eq!(rx.input_remaining(), 3);
}

#[test]
fn read_exact_then_write_all_round_trip() {
    use beam_dist::transport::Write;

    let mut rx = MemoryTransport::with_input(vec![0xde, 0xad]);
    let mut buf = [0u8; 2];
    rx.read_exact(&mut buf).expect("read_exact");
    assert_eq!(buf, [0xde, 0xad]);
    assert_eq!(rx.input_remaining(), 0);
    // A subsequent read past the end is a short read.
    let res = rx.read_exact(&mut buf);
    assert!(matches!(res, Err(BeamError::BadArg)));

    let mut tx = MemoryTransport::new();
    tx.write_all(b"ab").expect("write_all");
    tx.write_all(b"cd").expect("write_all");
    tx.flush().expect("flush");
    assert_eq!(tx.written(), b"abcd");
}
