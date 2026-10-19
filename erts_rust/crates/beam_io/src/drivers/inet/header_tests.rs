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

//! The `{header, N}` split's own suite — the C's `desc->hsz` rule
//! ([`super::header::split_payload`]).
//!
//! These are the **shape** rows of the module doc's table, one per `(hsz, len)` relation, plus the
//! `unsigned int` cast that makes a negative `{header, N}` the C's huge `hsz`. The end-to-end
//! deliveries that consume them are `active_tests.rs` (the `{active, …}` path) and `async_tests.rs`
//! (the passive `TCP_REQ_RECV` reply).
use super::header::split_payload;

/// The `hsz < len` row: `[H1..Hh | <<rest>>]` (`tcp_message`, `inet_drv.c:3993`-`:3997`).
#[test]
fn a_narrow_header_is_consed_onto_the_binary_tail() {
    let read = split_payload(2, b"abcdef".to_vec());
    assert_eq!(read.header, b"ab".to_vec(), "the first hsz bytes");
    assert_eq!(read.body.as_deref(), Some(&b"cdef"[..]), "the remainder, as a tail");
}

/// The `hsz == len > 0` row: the body is a **present**, empty binary (`LOAD_BUF2BINARY(buf+hsz, 0)`
/// ⇒ `<<>>`), so `[binary]` mode renders `[H1,H2|<<>>]` — not a bare proper list.
#[test]
fn a_header_as_wide_as_the_payload_leaves_an_empty_binary_tail() {
    let read = split_payload(2, b"ab".to_vec());
    assert_eq!(read.header, b"ab".to_vec(), "the whole payload is the header");
    assert_eq!(
        read.body.as_deref(),
        Some(&b""[..]),
        "a present, empty body — not the C's NULL buf"
    );
    assert!(read.has_body(), "the tail exists, it is just empty");
}

/// The `hsz > len` row: the C's flat-list branch (`(desc->mode == INET_MODE_LIST) || (hsz > len)`,
/// `:3985`) reports the whole payload as one **proper** list, i.e. no binary tail at all — the
/// `body: None` half of [`super::header::split_payload`]'s shape table.
#[test]
fn a_header_wider_than_the_payload_is_one_flat_header_with_no_tail() {
    let read = split_payload(10, b"abc".to_vec());
    assert_eq!(read.header, b"abc".to_vec(), "the whole payload, as the header");
    assert_eq!(read.body, None, "no binary tail (`buf == NULL`)");
    assert!(!read.has_body(), "a proper list, never `[H1,H2,H3|<<>>]`");
    assert_eq!(read.flattened(), b"abc".to_vec(), "the bytes are unchanged");
}

/// The `hsz == 0 && len == 0` row: the binary branch with a zero-length body ⇒ `<<>>`, so
/// `body: Some(vec![])` — a present empty body, never the flat-list shape.
#[test]
fn an_empty_payload_without_a_header_is_a_present_empty_body() {
    let read = split_payload(0, Vec::new());
    assert!(read.header.is_empty(), "no header bytes");
    assert_eq!(read.body.as_deref(), Some(&b""[..]), "`LOAD_BUF2BINARY(buf, 0)`");
    assert!(read.is_empty(), "it still carries no bytes");
}

/// The `hsz > 0 && len == 0` row: `hsz > len` holds, so the flat-list branch gives the empty list
/// — an empty header and **no** body.
#[test]
fn an_empty_payload_with_a_header_is_the_empty_flat_list() {
    let read = split_payload(4, Vec::new());
    assert!(read.header.is_empty(), "the flat list of nothing");
    assert_eq!(read.body, None, "and no binary tail");
}

/// The `hsz == 0 && len > 0` row: no cons at all, so the read is a bare body — the header-less
/// `driver_output` shape the runtime renders as the bare binary.
#[test]
fn a_zero_header_is_a_body_only_read() {
    let read = split_payload(0, b"abc".to_vec());
    assert!(read.header.is_empty(), "nothing is split off");
    assert_eq!(read.body.as_deref(), Some(&b"abc"[..]), "the whole payload");
}

/// A **negative** `{header, N}` is the C's huge `unsigned int` (`desc->hsz = ival`, `:6931`), so it
/// always takes the flat-list branch — `-1 as u32` is larger than any payload.
#[test]
fn a_negative_header_is_the_unsigned_value_the_c_stores() {
    let read = split_payload(-1, b"abc".to_vec());
    assert_eq!(read.header, b"abc".to_vec(), "the whole payload");
    assert_eq!(read.body, None, "the flat-list branch, never a split");

    let huge = split_payload(i32::MIN, b"abc".to_vec());
    assert_eq!(huge, read, "every negative value is the same huge hsz");
}

/// The split never loses or reorders a byte: `flattened()` is the payload for every `hsz`, which
/// is what an `[list]` port's `{data, D}` and the `recv_oct` accounting both see.
#[test]
fn every_split_preserves_the_payload_bytes_in_order() {
    let payload = b"abcdefgh".to_vec();
    for hsz in -1..=12 {
        let read = split_payload(hsz, payload.clone());
        assert_eq!(
            read.flattened(),
            payload,
            "hsz {hsz} must not lose or reorder bytes"
        );
        assert_eq!(
            read.len(),
            payload.len(),
            "hsz {hsz} carries exactly the payload's bytes"
        );
    }
}
