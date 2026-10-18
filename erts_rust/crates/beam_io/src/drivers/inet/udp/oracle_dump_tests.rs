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

//! The `udp_inet` oracle dump's **framing** suite: a `bind` reply carries a port, and a port's
//! low byte can be any value — including whatever byte an unframed dump would delimit replies
//! with.
//!
//! [`an_explicit_bind_port_whose_low_byte_is_the_old_delimiter_stays_three_replies`] pins the
//! bug deterministically: it binds an **explicit** loopback port whose low byte is `0x7C` (`|`,
//! the delimiter the harness used to split the oracle's dump on) and whose high byte is not, so
//! the reply is `[1, 0x7D, 0x7C]` on **both** sides. A delimiter-split dump of
//! `01 7C 01 7D 7C 7C 01` yields `[01]`, `[01,7D]`, `[]`, `[01]` — three replies (the empty part
//! is filtered out) whose middle entry is **two** bytes, i.e. exactly the `left: 3 / right: 2`
//! panic the length-framed dump removes.
//!
//! [`an_ephemeral_bind_reply_is_a_well_formed_port_on_both_sides`] pins the same reply's *shape*
//! for the ordinary `INET_AF_ANY`/port-0 bind, where the two runs legitimately bind **different**
//! ephemeral ports so only `[INET_REP_OK, hi, lo]` with a non-zero big-endian port is comparable.
//!
//! Both cases drive the real `open`/`bind`/`close` control verbs on both sides and **skip
//! loudly** when `erl` is not on `PATH`. The candidate-port loop below retries a
//! **resource-allocation** step only (an explicit port can be taken on the host); no assertion is
//! ever retried.

use super::{
    ctl_ok, erl_stdout, loopback_arg, open_arg, oracle_expression, oracle_replies, Rig,
    INET_AF_ANY, INET_REQ_BIND, INET_REQ_CLOSE, INET_REQ_OPEN, INET_REP_OK,
};

/// Explicit loopback ports whose **low** byte is `0x7C` (the old dump's delimiter) and whose
/// **high** byte is not. All are below `32768`, so they sit outside the kernel's ephemeral range
/// (`/proc/sys/net/ipv4/ip_local_port_range` is `32768 60999` on Linux) and are not
/// privilege-restricted (`>= 1024`).
const CANDIDATE_BIND_PORTS: [u16; 4] = [0x7D7C, 0x7E7C, 0x3F7C, 0x2C7C];

#[test]
fn an_explicit_bind_port_whose_low_byte_is_the_old_delimiter_stays_three_replies() {
    // ---- the Rust side: `open`, `bind` to one of the candidates, `close` ----
    // A candidate whose bind is refused (the port is taken on this host) moves to the next one:
    // an allocation retry, never an assertion retry.
    let mut chosen: Option<(u16, Vec<Vec<u8>>)> = None;
    for port in CANDIDATE_BIND_PORTS {
        let [hi, lo] = port.to_be_bytes();
        let mut rig = Rig::new();
        let opened = rig.control("open", &open_arg()).expect("open");
        assert_eq!(opened, ctl_ok(), "the `open` verb answers `[1]`");
        let bound = rig.control("bind", &loopback_arg(port)).expect("bind");
        if bound.first().copied() != Some(INET_REP_OK) {
            eprintln!(
                "note: candidate loopback port {port} (0x{port:04X}) is unavailable on this \
                 host ({bound:02x?}) — trying the next candidate",
            );
            continue;
        }
        assert_eq!(
            bound,
            vec![INET_REP_OK, hi, lo],
            "an explicit bind reports the port it bound: rust(bind)={bound:02x?}",
        );
        let closed = rig.control("close", &[]).expect("close");
        chosen = Some((port, vec![opened, bound, closed]));
        break;
    }
    let Some((port, rust)) = chosen else {
        panic!("no candidate bind port from {CANDIDATE_BIND_PORTS:?} is available on this host");
    };
    let [hi, lo] = port.to_be_bytes();

    // ---- the oracle side: the very same three calls ----
    let calls = vec![
        (INET_REQ_OPEN, open_arg().to_vec()),
        (INET_REQ_BIND, loopback_arg(port)),
        (INET_REQ_CLOSE, vec![]),
    ];
    let Some(dump) = erl_stdout(&oracle_expression(&calls)) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the framed udp_inet bind-oracle check");
        return;
    };
    let oracle = oracle_replies(&dump);

    // The dump must frame **three** replies — the middle one's `0x7C` low port byte is payload,
    // never a separator.
    assert_eq!(
        oracle.len(),
        3,
        "the oracle answered all three calls (open / bind / close): dump={dump:02x?}, \
         parsed={oracle:02x?}",
    );
    assert_eq!(
        rust[0],
        oracle[0],
        "open replies disagree: rust(open)={:02x?} oracle(open)={:02x?} dump={dump:02x?}",
        rust[0],
        oracle[0],
    );
    // Both sides bound the **same** explicit port, so this comparison is byte-exact — the reply
    // is where the delimiter byte hid.
    assert_eq!(
        rust[1],
        oracle[1],
        "bind replies disagree: rust(bind)={:02x?} oracle(bind)={:02x?} dump={dump:02x?} \
         (explicit port {port} = 0x{hi:02X}0x{lo:02X})",
        rust[1],
        oracle[1],
    );
    assert_eq!(
        rust[1],
        vec![INET_REP_OK, hi, lo],
        "the shared bind reply is `[INET_REP_OK, hi, lo]` on both sides: \
         rust(bind)={:02x?} oracle(bind)={:02x?}",
        rust[1],
        oracle[1],
    );
    assert_eq!(
        rust[2],
        oracle[2],
        "close replies disagree: rust(close)={:02x?} oracle(close)={:02x?} dump={dump:02x?}",
        rust[2],
        oracle[2],
    );
}

#[test]
fn an_ephemeral_bind_reply_is_a_well_formed_port_on_both_sides() {
    let calls = vec![
        (INET_REQ_OPEN, open_arg().to_vec()),
        (INET_REQ_BIND, vec![INET_AF_ANY, 0, 0]),
        (INET_REQ_CLOSE, vec![]),
    ];

    let mut rig = Rig::new();
    let opened = rig.control("open", &open_arg()).expect("open");
    assert_eq!(opened, ctl_ok(), "the `open` verb answers `[1]`");
    let bound = rig.control("bind", &[INET_AF_ANY, 0, 0]).expect("bind");
    let closed = rig.control("close", &[]).expect("close");
    let rust = vec![opened, bound, closed];

    let Some(dump) = erl_stdout(&oracle_expression(&calls)) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the ephemeral udp_inet bind-oracle check");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(
        oracle.len(),
        3,
        "the oracle answered all three calls (open / bind / close): dump={dump:02x?}, \
         parsed={oracle:02x?}",
    );

    // The two runs bind **different** ephemeral ports, so the shape is the strongest honest
    // comparison: a successful three-byte reply naming a real (non-zero) big-endian port.
    for (side, reply) in [("rust", &rust[1]), ("oracle", &oracle[1])] {
        assert_eq!(
            reply.len(),
            3,
            "{side} bind reply is `[INET_REP_OK, hi, lo]`: {reply:02x?} dump={dump:02x?}",
        );
        assert_eq!(
            reply[0],
            INET_REP_OK,
            "{side} bind reply reports success: {reply:02x?} dump={dump:02x?}",
        );
        assert_ne!(
            u16::from_be_bytes([reply[1], reply[2]]),
            0,
            "{side} bind reply names the kernel-assigned port: {reply:02x?} dump={dump:02x?}",
        );
    }
    assert_eq!(
        rust[0],
        oracle[0],
        "open replies disagree: rust(open)={:02x?} oracle(open)={:02x?} dump={dump:02x?}",
        rust[0],
        oracle[0],
    );
    assert_eq!(
        rust[2],
        oracle[2],
        "close replies disagree: rust(close)={:02x?} oracle(close)={:02x?} dump={dump:02x?}",
        rust[2],
        oracle[2],
    );
}
