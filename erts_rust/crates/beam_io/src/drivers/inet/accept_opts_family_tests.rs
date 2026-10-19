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
//! The **whole** `prim_inet:accept_opts/3` option list — the eleven codes, not the seven the
//! sibling [`accept_opts_tests`](super::accept_opts_tests) suite replays.
//!
//! `gen_tcp:accept/1` (`lib/kernel/src/gen_tcp.erl:778` → `inet_tcp:accept/1` →
//! `prim_inet:accept/3` → `prim_inet:accept_opts/3`) sends:
//!
//! ```erlang
//! %% erts/preloaded/src/prim_inet.erl:502-518
//! accept_opts(L, S, FamilyOpts) ->
//!     case getopts(L, [active, nodelay, keepalive, delay_send, priority, linger, reuseaddr]
//!                     ++ FamilyOpts) of
//!         {ok, Opts} -> case setopts(S, Opts) of ok -> {ok, S}; Error1 -> close(S), Error1 end;
//!         Error2 -> close(S), Error2
//!     end.
//! ```
//!
//! and the family half is `inet_tcp:accept_family_opts/0` (`lib/kernel/src/inet_tcp.erl:249`):
//!
//! ```erlang
//! accept_family_opts() -> [tos, ttl, recvtos, recvttl].
//! ```
//!
//! so the request is **eleven** codes, in this order:
//!
//! | # | option | code | C arm | our table |
//! |---|---|---|---|---|
//! | 1 | `active` | 22 | `INET_LOPT_ACTIVE` (`inet_drv.c:9177`) | `opt_state` (store) |
//! | 2 | `nodelay` | 10 | `TCP_OPT_NODELAY` (`:9447`) | `opt_sys` (`TCP_NODELAY`) |
//! | 3 | `keepalive` | 1 | `INET_OPT_KEEPALIVE` (`:9429`) | `opt_sys` (`SO_KEEPALIVE`) |
//! | 4 | `delay_send` | 31 | `INET_LOPT_TCP_DELAY_SEND` (`:9256`, stream-gated) | `opt_state` (store) |
//! | 5 | `priority` | 8 | `INET_OPT_PRIORITY` (`:9316`, `propagate = 1`) | `opt_sys` (`SO_PRIORITY`) |
//! | 6 | `linger` | 3 | `INET_OPT_LINGER` (`:9505`, nine-byte reply) | `opt_sys` (special shape) |
//! | 7 | `reuseaddr` | 0 | `INET_OPT_REUSEADDR` (`:9380`) | `opt_sys` (`SO_REUSEADDR`) |
//! | 8 | `tos` | 9 | `INET_OPT_TOS` (`:9325`, `propagate = 1`) | `opt_sys` (`IP_TOS`) |
//! | 9 | `ttl` | 46 | `INET_OPT_TTL` (`:9344`, `propagate = 1`) | `opt_sys` (`IP_TTL`) |
//! | 10 | `recvtos` | 43 | `INET_OPT_RECVTOS` (`:9353`, `propagate = 1`) | `opt_sys` (`IP_RECVTOS`) |
//! | 11 | `recvttl` | 47 | `INET_OPT_RECVTTL` (`:9371`, `propagate = 1`) | `opt_sys` (`IP_RECVTTL`) |
//!
//! # Why the seven-code replay was not enough
//!
//! Both halves have to hold on a **real** socket for `gen_tcp:accept/1` to answer `{ok, Socket}`:
//! the `getopts` half is all-or-nothing (a single unserved code is the C's
//! `default: RETURN_ERROR()`, `inet_drv.c:9719`-`:9720`, i.e. the whole reply `[0]einval`, which
//! `prim_inet:getopts/2` turns into a failed accept), and the `setopts` half runs into
//! `propagate = 1` on **four** of the five family codes: a failed `setsockopt(2)` for `tos`/`ttl`/
//! `recvtos`/`recvttl` — or for `priority` — replaces the whole reply with `[0]einval`
//! (`inet_set_opts`'s `if (propagate) return -1`, `:7923`-`:7942`). So the family codes are the
//! ones that can turn a listener's option block into a failed accept, and they were untested.
//!
//! # What this suite pins
//!
//! * The eleven-code `getopts` **on a bound-and-listening stream socket** — the state
//!   `gen_tcp:listen/2` leaves the listener in — record by record in request order, and the
//!   identical bytes from the system driver (`tcp_inet`).
//! * The eleven-record `setopts` back onto a **really accepted** socket (a real loopback
//!   `connect(2)`, the driver's own `accept(26)`/`accept(2)`, the descriptor adopted through
//!   [`TcpInetDriver::from_accepted`] and opened as a second registry port), which is where
//!   `prim_inet:accept_opts/3` applies them.
//! * The state matrix — no socket, opened, bound-and-listening, connected, post-`close` — for
//!   `tcp_inet`, and the datagram twin for `udp_inet`, each byte-compared against the oracle.
//! * The upstream contract itself: a real `gen_tcp:listen/2` + `gen_tcp:accept/1`, with
//!   `inet:getopts/2` pinned to exactly those eleven `{Opt,Value}` pairs — and `prim_inet:getopts/2`
//!   run on a **raw** `{spawn_driver, "tcp_inet"}` port, which is what proves the *byte* reply this
//!   crate produces is accepted by `prim_inet`'s own decoder (`decode_opt_val/1`,
//!   `erts/preloaded/src/prim_inet.erl:2486`, reached from `getopts/2` at `:1049`-`:1064`).
//!
//! Every `erl` half **skips loudly** when the system Erlang is absent, and no case asserts a
//! wall-clock value.
use super::oracle::*;
use super::*;
use super::super::op_queue::async_ok_reply;
use super::super::opcode_tests::op;
use crate::completion::{CompletionEvent, InetAsyncResult};
use crate::drivers::inet::{make_tcp_inet, TcpInetDriver, DRIVER_NAME, INET_AF_INET};
use crate::poller::Poller;
use crate::port::PortRegistry;
use beam_common::BeamError;
use beam_process::PortId;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

/// `prim_inet:accept_opts/3`'s whole request list, in its request order: the seven codes it names
/// itself (`erts/preloaded/src/prim_inet.erl:504`-`:505`) followed by
/// `inet_tcp:accept_family_opts/0`'s four (`lib/kernel/src/inet_tcp.erl:249`).
///
/// Spelled from the code constants rather than as bare integers so a numbering slip in
/// [`super`] cannot silently turn this suite into a different request.
const ACCEPT_OPTS_FAMILY_CODES: [u8; 11] = [
    INET_LOPT_ACTIVE,
    TCP_OPT_NODELAY,
    INET_OPT_KEEPALIVE,
    INET_LOPT_TCP_DELAY_SEND,
    INET_OPT_PRIORITY,
    INET_OPT_LINGER,
    INET_OPT_REUSEADDR,
    INET_OPT_TOS,
    INET_OPT_TTL,
    INET_OPT_RECVTOS,
    INET_OPT_RECVTTL,
];

/// The eleven atoms of the same list, as `inet:getopts/2` reports them — the pin the upstream case
/// asserts.
const ACCEPT_OPTS_FAMILY_ATOMS: &str =
    "[active,nodelay,keepalive,delay_send,priority,linger,reuseaddr,tos,ttl,recvtos,recvttl]";

/// `accept(26)`'s timeout operand: 5000 ms, the C's `get_int32` of `Timeout(4)`
/// (`inet_drv.c:12189`-`:12194`).
const ACCEPT_TIMEOUT: [u8; 4] = [0, 0, 0x13, 0x88];

/// The poll budget for one accept completion — a **condition** wait, never a sleep-then-assert.
const ACCEPT_WAIT: Duration = Duration::from_secs(10);

/// The packed `bind` operand naming `127.0.0.1` with a zero port, so the kernel picks the
/// ephemeral one (`inet_get_address`, `inet_drv.c:4890`).
fn loopback_port_zero() -> Vec<u8> {
    vec![INET_AF_INET, 0, 0, 127, 0, 0, 1]
}

/// A `tcp_inet` port over a real [`PortRegistry`] + [`Poller`], plus — when a test needs one — the
/// **accepted** connection opened as a second port, both owned here so one poller drives both.
///
/// The control calls go through [`PortRegistry::port_control`] with the **decimal** command a live
/// runtime forwards (`op(code)` = `code + CONTROL_MAGIC`), i.e. exactly the route `beam_rt` uses,
/// so the numeric-opcode bridge is in the path of every row.
struct TestPort {
    poller: Poller,
    ports: PortRegistry,
    /// The listener (or the only port, for the state-matrix cases).
    id: PortId,
}

impl TestPort {
    /// Opens the built-in `tcp_inet` driver as a port and runs its `open` verb, so the port holds a
    /// real stream socket.
    fn open() -> TestPort {
        let mut poller = Poller::new().expect("a poll set");
        let driver = make_tcp_inet().expect("the tcp_inet driver");
        let mut ports = PortRegistry::new();
        let id = ports.open(&mut poller, driver).expect("the port opens");
        let mut port = TestPort { poller, ports, id };
        assert_eq!(
            port.control(port.id, INET_REQ_OPEN, &open_arg()),
            ctl_ok(),
            "`open` answers `[1]`"
        );
        port
    }

    /// One control call on `id`, panicking on a typed error — the reply is the C's raw byte string.
    fn control(&mut self, id: PortId, code: u32, arg: &[u8]) -> Vec<u8> {
        let command = op(code);
        self.ports
            .port_control(&mut self.poller, id, &command, arg)
            .unwrap_or_else(|err| panic!("the `{}` control call failed: {err:?}", verb_name(code)))
    }

    /// Binds to the loopback address with the kernel's ephemeral port and starts listening — the
    /// state `gen_tcp:listen(0, …)` leaves the listen socket in. Returns the assigned port.
    fn bind_loopback_and_listen(&mut self) -> u16 {
        let bound = self.control(self.id, INET_REQ_BIND, &loopback_port_zero());
        assert_eq!(bound.len(), 3, "`bind` answers `[1] ++ port`: {bound:?}");
        assert_eq!(bound[0], 1, "`bind`'s result tag: {bound:?}");
        let port = u16::from_be_bytes([bound[1], bound[2]]);
        assert_ne!(port, 0, "the kernel assigned an ephemeral port");
        assert_eq!(
            self.control(self.id, INET_REQ_LISTEN, &[0, 5]),
            ctl_ok(),
            "`listen` answers `[1]`"
        );
        port
    }

    /// Queues the driver's own `accept(26)` on the listener, connects a real loopback peer, waits
    /// for the completion, adopts the descriptor through [`TcpInetDriver::from_accepted`] (the C's
    /// `tcp_inet_copy`) and opens it as a **second** port. Returns that port and the held peer.
    fn accept(&mut self, listener_port: u16) -> (PortId, TcpStream) {
        assert_eq!(
            self.control(self.id, INET_REQ_ACCEPT, &ACCEPT_TIMEOUT),
            async_ok_reply(0),
            "the accept queues and answers its id"
        );
        let peer =
            TcpStream::connect(("127.0.0.1", listener_port)).expect("a real loopback peer connects");
        let fd = self.drive_until_accepted(0);
        let child = {
            let listener = self
                .ports
                .driver_as::<TcpInetDriver>(self.id)
                .expect("the listener port's own tcp_inet driver");
            TcpInetDriver::from_accepted(fd, listener)
                .expect("the accepted descriptor is adopted with the listener's options")
        };
        let child = self
            .ports
            .open(&mut self.poller, Box::new(child))
            .expect("the accepted connection opens as a second port");
        (child, peer)
    }

    /// Pumps the reactor until the accept queued as `id` completes, answering the descriptor it
    /// handed over — the runtime's own `poller.poll` → `handle_events` → `take_completions` loop,
    /// bounded by a **condition** under [`ACCEPT_WAIT`].
    fn drive_until_accepted(&mut self, id: u16) -> RawFd {
        let deadline = Instant::now() + ACCEPT_WAIT;
        loop {
            let events = match self.poller.poll(Some(Duration::from_millis(100))) {
                Ok(events) => events,
                Err(BeamError::Io(err)) if err.kind() == ErrorKind::Interrupted => Vec::new(),
                Err(err) => panic!("the poll failed: {err:?}"),
            };
            self.ports
                .handle_events(&mut self.poller, &events)
                .expect("the ready events are dispatched");
            for done in self.ports.take_completions() {
                if let CompletionEvent::InetAsync {
                    id: got,
                    result: InetAsyncResult::Accepted { fd },
                } = done.event
                {
                    assert_eq!(got, id, "the completion carries the accept's own id");
                    return fd;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the accept never completed within {ACCEPT_WAIT:?}"
            );
        }
    }
}

/// Decodes a `getopts` reply into `(code, payload)` records — the C's `[opt, value…]` walk
/// (`inet_fill_opts`'s `*ptr++ = opt; put_int32(...)`).
///
/// The payload widths are the ones the eleven `accept_opts` codes can produce: eight bytes for
/// `linger` (the C's `struct linger`, `inet_drv.c:9744`-`:9751`) and four for every other code in
/// this list (`active`'s `{active, N}` counter cannot appear here, because the listener is never
/// put into `{active, N}`). Any other code in the reply is a panic, not a silent skip: this helper
/// exists to prove the record set, so an unexpected member must fail loudly.
fn records(reply: &[u8]) -> Vec<(u8, Vec<u8>)> {
    assert_eq!(reply.first(), Some(&1), "a `[1]`-tagged reply: {reply:?}");
    let mut rest = &reply[1..];
    let mut out = Vec::new();
    while let Some(&code) = rest.first() {
        let width = match code {
            INET_OPT_LINGER => 8,
            other if ACCEPT_OPTS_FAMILY_CODES.contains(&other) => 4,
            other => panic!("unexpected option code {other} in the reply {reply:?}"),
        };
        let payload = rest
            .get(1..1 + width)
            .unwrap_or_else(|| panic!("a truncated record for code {code}: {reply:?}"));
        out.push((code, payload.to_vec()));
        rest = &rest[1 + width..];
    }
    out
}

/// The `i32` a four-byte record payload carries (`get_int32`, big endian).
fn int(payload: &[u8]) -> i32 {
    i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
}

/// The `getopts` reply body as the `setopts` request `prim_inet:accept_opts/3` builds — the
/// leading `[1]` is the C's `INET_REP_OK` tag, not a record byte.
fn setopts_request(getopts_reply: &[u8]) -> Vec<u8> {
    assert_eq!(getopts_reply.first(), Some(&1), "a `[1]`-tagged reply");
    getopts_reply[1..].to_vec()
}

/// The `bind` reply for `port`: `[1, port_hi, port_lo]` (`inet_ctl_bind`'s `put_int16` pair).
/// The number is the kernel's ephemeral assignment, so it is the one step the oracle comparison
/// cannot byte-compare — but it is still the real reply, not a placeholder.
fn bind_reply(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![1, hi, lo]
}

// ---------------------------------------------------------------------------
// The oracle side — a hex-encoded runner (no byte can be mangled or mis-split)
// ---------------------------------------------------------------------------

/// Runs `sequence` on a real system `tcp_inet`/`udp_inet` port and decodes the replies from a
/// **hex-encoded, `|`-separated** dump.
///
/// Hex rather than raw length-framed bytes for two reasons, both of which a
/// `bind` reply exposes on a busy host: an `erl` reply byte above `0x7f` — the ephemeral port's
/// high byte is one on most of this host's assignments — cannot survive the oracle harness's
/// UTF-8 stdout capture ([`erl_stdout`] is lossy, so `0xC3` becomes two bytes), and a reply byte
/// equal to the delimiter would corrupt the split. Hex is ASCII, so neither can happen.
///
/// `None` — after a loud skip — when `erl` is absent or over budget.
fn framed_oracle_replies(driver: &str, sequence: &[(u32, Vec<u8>)]) -> Option<Vec<Vec<u8>>> {
    let calls: Vec<String> = sequence
        .iter()
        .map(|(code, arg)| {
            let bytes: Vec<String> = arg.iter().map(u8::to_string).collect();
            format!("F({code}, <<{}>>)", bytes.join(","))
        })
        .collect();
    let expr = format!(
        "M = {CONTROL_MAGIC}, \
         P = open_port({{spawn_driver, \"{driver}\"}}, [binary]), \
         F = fun(C, A) -> iolist_to_binary(erlang:port_control(P, C + M, A)) end, \
         R = [{}], \
         H = fun(L) -> iolist_to_binary([io_lib:format(\"~2.16.0b\", [B]) || <<B>> <= L]) end, \
         io:format(\"~s\\n\", [lists:join(\"|\", [H(L) || L <- R])])",
        calls.join(", ")
    );
    let stdout = erl_stdout(&expr)?;
    let replies = decode_hex_replies(&stdout).unwrap_or_else(|| {
        panic!("could not decode the oracle's hex dump: {stdout:?}");
    });
    assert_eq!(
        replies.len(),
        sequence.len(),
        "one reply per step: {replies:?}"
    );
    Some(replies)
}

/// Decodes the `0102|`-style hex dump into one byte vector per reply: `|` separates replies and an
/// empty field is a zero-length reply.
fn decode_hex_replies(stdout: &str) -> Option<Vec<Vec<u8>>> {
    let line = stdout
        .lines()
        .find(|line| !line.is_empty() && line.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '|'))?;
    let mut replies = Vec::new();
    for field in line.trim_end().split('|') {
        if field.len() % 2 != 0 {
            return None;
        }
        let mut bytes = Vec::with_capacity(field.len() / 2);
        for pair in field.as_bytes().chunks(2) {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            bytes.push((hi * 16 + lo) as u8);
        }
        replies.push(bytes);
    }
    Some(replies)
}

/// Asserts `rust` agrees with the oracle step for step, except the steps in `skip` (the `bind`
/// reply carries the kernel's ephemeral port, a different number in each process) which must still
/// carry the C's `INET_REP_OK` tag on both sides. **Skips loudly** without `erl`.
fn assert_agrees_except(
    driver: &str,
    sequence: &[(u32, Vec<u8>)],
    rust: &[Vec<u8>],
    skip: &[usize],
) {
    let Some(oracle) = framed_oracle_replies(driver, sequence) else {
        return; // the oracle is unavailable; the skip was reported loudly above
    };
    eprintln!("oracle accept_opts family replies: {oracle:?}");
    eprintln!("rust accept_opts family replies:   {rust:?}");
    for (index, (rust_reply, oracle_reply)) in rust.iter().zip(oracle.iter()).enumerate() {
        if skip.contains(&index) {
            assert_eq!(
                rust_reply.first(),
                Some(&1),
                "step {index} (`{}`) carries `[1]` on our side: {rust_reply:?}",
                verb_name(sequence[index].0)
            );
            assert_eq!(
                oracle_reply.first(),
                Some(&1),
                "step {index} (`{}`) carries `[1]` on the oracle side: {oracle_reply:?}",
                verb_name(sequence[index].0)
            );
            continue;
        }
        assert_eq!(
            rust_reply,
            oracle_reply,
            "reply {index} (`{}` of {:?}) disagrees with the oracle",
            verb_name(sequence[index].0),
            sequence[index].1
        );
    }
}

// ---------------------------------------------------------------------------
// The eleven codes on the listening port
// ---------------------------------------------------------------------------

/// The eleven-code `getopts` on a **bound-and-listening** stream socket — the exact request
/// `prim_inet:accept_opts/3` makes of the listener — answered record by record, in request order.
///
/// The values are the ones the store and the kernel really hold for a fresh listener: every
/// `setsockopt(2)`-backed code reports its default (`0`, `linger` `{false,0}`) and `ttl` the
/// kernel's initial hop limit, which is recorded rather than asserted so a host with a different
/// `/proc/sys/net/ipv4/ip_default_ttl` cannot fail the suite (the oracle comparison in
/// [`the_eleven_code_getopts_on_a_listening_socket_agrees_with_the_oracle`] pins it byte for byte).
#[test]
fn the_eleven_codes_answer_record_by_record_on_a_listening_socket() {
    let mut listener = TestPort::open();
    listener.bind_loopback_and_listen();

    let reply = listener.control(listener.id, INET_REQ_GETOPTS, &ACCEPT_OPTS_FAMILY_CODES);
    let records = records(&reply);
    let seen: Vec<u8> = records.iter().map(|(code, _)| *code).collect();
    assert_eq!(
        seen,
        ACCEPT_OPTS_FAMILY_CODES.to_vec(),
        "one record per requested code, in request order — an unserved code would have made the \
         whole reply `[0]einval` (the C's `default: RETURN_ERROR()`, `inet_drv.c:9719`)"
    );
    assert_eq!(records.len(), 11, "eleven records: {reply:?}");
    // `[1]` + ten five-byte records + `linger`'s nine-byte one (`1` code byte + 8 payload bytes).
    assert_eq!(reply.len(), 1 + 10 * 5 + 9, "the wire length: {reply:?}");

    let value = |code: u8| int(&records.iter().find(|(c, _)| *c == code).expect("a record").1);
    assert_eq!(value(INET_LOPT_ACTIVE), 0, "{{active, false}}");
    assert_eq!(value(TCP_OPT_NODELAY), 0, "{{nodelay, false}}");
    assert_eq!(value(INET_OPT_KEEPALIVE), 0, "{{keepalive, false}}");
    assert_eq!(value(INET_LOPT_TCP_DELAY_SEND), 0, "{{delay_send, false}}");
    assert_eq!(value(INET_OPT_PRIORITY), 0, "{{priority, 0}}");
    assert_eq!(value(INET_OPT_REUSEADDR), 0, "{{reuseaddr, false}}");
    assert_eq!(value(INET_OPT_TOS), 0, "{{tos, 0}}");
    assert_eq!(value(INET_OPT_RECVTOS), 0, "{{recvtos, false}}");
    assert_eq!(value(INET_OPT_RECVTTL), 0, "{{recvttl, false}}");
    let ttl = value(INET_OPT_TTL);
    eprintln!("the listener's I{{P_TTL}} is {ttl}");
    assert!(
        (1..=255).contains(&ttl),
        "a live socket's IP_TTL is a positive hop count, got {ttl}"
    );
    // `linger` is the one eight-byte payload: `l_onoff` then `l_linger`.
    let linger = records
        .iter()
        .find(|(c, _)| *c == INET_OPT_LINGER)
        .expect("the linger record");
    assert_eq!(linger.1.len(), 8, "the C's `struct linger` (`inet_drv.c:9744`)");
    assert_eq!(int(&linger.1[..4]), 0, "`l_onoff`");
    assert_eq!(int(&linger.1[4..]), 0, "`l_linger`");
}

/// The whole two-step contract on a **really accepted** socket: the listener's ten-plus-one
/// records fed straight back as a `setopts`, which is what `prim_inet:accept_opts/3` does, answers
/// `[1]` — and the accepted socket then reports the identical option block.
///
/// The `setopts` half is the one the four `propagate = 1` family codes can fail
/// (`inet_set_opts`'s `if (propagate) return -1`, `inet_drv.c:7923`-`:7942`), and the accepted
/// socket is a **different** descriptor from the listener, so this is the only place the pair is
/// proven end to end: a real loopback `connect(2)`, the driver's own `accept(26)`/`accept(2)`, the
/// C's `tcp_inet_copy` through [`TcpInetDriver::from_accepted`].
#[test]
fn the_listener_option_block_round_trips_onto_a_real_accepted_socket() {
    let mut rig = TestPort::open();
    let port = rig.bind_loopback_and_listen();
    let listener_reply = rig.control(rig.id, INET_REQ_GETOPTS, &ACCEPT_OPTS_FAMILY_CODES);
    let (child, _peer) = rig.accept(port);

    let request = setopts_request(&listener_reply);
    assert_eq!(
        rig.control(child, INET_REQ_SETOPTS, &request),
        ctl_ok(),
        "`prim_inet:accept_opts/3`'s `setopts(S, Opts)` half answers `ok`"
    );
    // The listener accepts the same block (a bound-and-listening socket has one too).
    assert_eq!(
        rig.control(rig.id, INET_REQ_SETOPTS, &request),
        ctl_ok(),
        "the same record block is valid on the listener"
    );

    let accepted_reply = rig.control(child, INET_REQ_GETOPTS, &ACCEPT_OPTS_FAMILY_CODES);
    eprintln!("listener getopts {listener_reply:?}");
    eprintln!("accepted getopts {accepted_reply:?}");
    assert_eq!(
        accepted_reply, listener_reply,
        "the accepted socket reports the option block `accept_opts` just applied to it"
    );
    // The driver's own store crossed the copy too, which is not a kernel property.
    let records = records(&accepted_reply);
    let store = |code: u8| int(&records.iter().find(|(c, _)| *c == code).expect("a record").1);
    assert_eq!(store(INET_LOPT_ACTIVE), 0, "the accepted socket starts passive");
    assert_eq!(
        store(INET_LOPT_TCP_DELAY_SEND),
        0,
        "`delay_send` is copied by `tcp_inet_copy` (`inet_drv.c:11809`-`:11844`)"
    );
}

// ---------------------------------------------------------------------------
// The oracle comparisons
// ---------------------------------------------------------------------------

/// The same eleven-code `getopts`, in the same state, on the real system driver: `open` → `bind`
/// (loopback, ephemeral) → `listen` → `getopts`. The `bind` reply names a different ephemeral port
/// in each process, so it is asserted to carry `[1]` on both sides and every other step is
/// byte-compared.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_eleven_code_getopts_on_a_listening_socket_agrees_with_the_oracle() {
    let mut listener = TestPort::open();
    let port = listener.bind_loopback_and_listen();
    let rust_getopts = listener.control(listener.id, INET_REQ_GETOPTS, &ACCEPT_OPTS_FAMILY_CODES);

    let sequence = vec![
        (INET_REQ_OPEN, open_arg()),
        (INET_REQ_BIND, loopback_port_zero()),
        (INET_REQ_LISTEN, vec![0, 5]),
        (INET_REQ_GETOPTS, ACCEPT_OPTS_FAMILY_CODES.to_vec()),
    ];
    let rust = vec![
        ctl_ok(),
        bind_reply(port),
        ctl_ok(),
        rust_getopts,
    ];
    assert_agrees_except(DRIVER_NAME, &sequence, &rust, &[1]);
}

/// The `setopts` half in the listening state, against the oracle: the listener's own record block
/// fed back through `setopts(7)`. The four `propagate = 1` family codes make this the row that
/// decides whether `gen_tcp:accept/1` can complete at all.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_eleven_record_setopts_on_a_listening_socket_agrees_with_the_oracle() {
    let mut listener = TestPort::open();
    let port = listener.bind_loopback_and_listen();
    let reply = listener.control(listener.id, INET_REQ_GETOPTS, &ACCEPT_OPTS_FAMILY_CODES);
    let request = setopts_request(&reply);
    let rust_setopts = listener.control(listener.id, INET_REQ_SETOPTS, &request);
    assert_eq!(rust_setopts, ctl_ok(), "`setopts` on the listening socket");

    let sequence = vec![
        (INET_REQ_OPEN, open_arg()),
        (INET_REQ_BIND, loopback_port_zero()),
        (INET_REQ_LISTEN, vec![0, 5]),
        (INET_REQ_GETOPTS, ACCEPT_OPTS_FAMILY_CODES.to_vec()),
        (INET_REQ_SETOPTS, request),
    ];
    let rust = vec![
        ctl_ok(),
        bind_reply(port),
        ctl_ok(),
        reply,
        rust_setopts,
    ];
    assert_agrees_except(DRIVER_NAME, &sequence, &rust, &[1]);
}

/// The eleven codes across every `tcp_inet` state a listener can be in **without** a `bind` (so the
/// whole sequence is deterministic and byte-comparable): no socket, an opened stream socket, and
/// post-`close` — plus the per-code `setopts` rows that decide each code's `propagate` class.
///
/// The three misses the C distinguishes, all of which the accept path depends on:
///
/// * the five `propagate = 1` codes in this list (`priority` `8`, `tos` `9`, `ttl` `46`,
///   `recvtos` `43`, `recvttl` `47`) answer `[0]einval` from `setopts` on a port with **no** socket,
///   because the C's `setsockopt` fails and `inet_set_opts` returns `-1` (`:7923`-`:7942`);
/// * every other code answers `[1]` there (`propagate = 0`), including `linger`, whose record is
///   nine bytes;
/// * a failed `getsockopt` drops **that record only** (`TRUNCATE_TO(0, ptr); continue;`), which is
///   why the bare port reports `active` alone and the post-`close` port reports `active` and
///   `delay_send` (from the cached `stype`) but none of the syscall-backed codes.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_eleven_code_option_family_agrees_with_the_oracle_in_every_state() {
    let linger = {
        let mut record = vec![INET_OPT_LINGER, 0, 0, 0, 0];
        record.extend_from_slice(&0i32.to_be_bytes());
        record
    };
    let codes = ACCEPT_OPTS_FAMILY_CODES.to_vec();
    let sequence = vec![
        // ---- (a) no socket at all ----
        (INET_REQ_GETOPTS, codes.clone()),
        // The five propagating codes fail the whole reply there ...
        (INET_REQ_SETOPTS, record(INET_OPT_PRIORITY, 0)),
        (INET_REQ_SETOPTS, record(INET_OPT_TOS, 0)),
        (INET_REQ_SETOPTS, record(INET_OPT_TTL, 64)),
        (INET_REQ_SETOPTS, record(INET_OPT_RECVTOS, 1)),
        (INET_REQ_SETOPTS, record(INET_OPT_RECVTTL, 1)),
        // ... and every `propagate = 0` code answers `[1]`.
        (INET_REQ_SETOPTS, linger.clone()),
        (INET_REQ_SETOPTS, record(INET_OPT_KEEPALIVE, 1)),
        (INET_REQ_SETOPTS, record(INET_OPT_REUSEADDR, 1)),
        (INET_REQ_SETOPTS, record(TCP_OPT_NODELAY, 1)),
        (INET_REQ_SETOPTS, record(INET_LOPT_ACTIVE, 0)),
        (INET_REQ_SETOPTS, record(INET_LOPT_TCP_DELAY_SEND, 1)),
        // ---- (b) an opened stream socket: every record reads back ----
        (INET_REQ_OPEN, open_arg()),
        (INET_REQ_GETOPTS, codes.clone()),
        // The one nine-byte record survives a round trip through the set path.
        (INET_REQ_SETOPTS, linger),
        (INET_REQ_GETOPTS, vec![INET_OPT_LINGER]),
        (INET_REQ_SETOPTS, record(INET_OPT_TOS, 0)),
        (INET_REQ_SETOPTS, record(INET_OPT_TTL, 64)),
        (INET_REQ_SETOPTS, record(INET_OPT_RECVTOS, 0)),
        (INET_REQ_SETOPTS, record(INET_OPT_RECVTTL, 0)),
        (INET_REQ_GETOPTS, codes.clone()),
        // ---- (c) post-`close`: the cached `stype` survives, the socket does not ----
        (INET_REQ_CLOSE, vec![]),
        (INET_REQ_GETOPTS, codes),
        (INET_REQ_SETOPTS, record(INET_OPT_TTL, 64)),
    ];
    // The steps that mutate the port are driven in order through one registry port, exactly as
    // `rust_replies` does, so the sequence's state transitions are real.
    let rust = rust_replies(DRIVER_NAME, &sequence);
    assert_eq!(rust.len(), sequence.len(), "one reply per step");

    // The bare-port rows, on the record.
    assert_eq!(
        rust[0],
        vec![1, INET_LOPT_ACTIVE, 0, 0, 0, 0],
        "a bare port keeps only the driver-state code; every syscall-backed record is dropped"
    );
    for step in 1..=5 {
        assert_eq!(
            rust[step],
            ctl_xerror("einval"),
            "step {step} is a `propagate = 1` code on a socket-less port"
        );
    }
    for step in 6..=11 {
        assert_eq!(
            rust[step],
            ctl_ok(),
            "step {step} is a `propagate = 0` code, so the failure is discarded"
        );
    }
    // The opened rows: all eleven records, in request order.
    assert_eq!(
        records(&rust[13])
            .iter()
            .map(|(code, _)| *code)
            .collect::<Vec<u8>>(),
        ACCEPT_OPTS_FAMILY_CODES.to_vec(),
        "an opened stream socket serves all eleven codes"
    );
    assert_eq!(
        rust[15],
        vec![1, INET_OPT_LINGER, 0, 0, 0, 0, 0, 0, 0, 0],
        "the nine-byte `linger` read-back"
    );
    // The post-`close` rows.
    assert_eq!(
        rust[22],
        vec![1, INET_LOPT_ACTIVE, 0, 0, 0, 0, INET_LOPT_TCP_DELAY_SEND, 0, 0, 0, 0],
        "the cached `stype` keeps `delay_send`, the socket-backed records are gone"
    );
    assert_eq!(
        rust[23],
        ctl_xerror("einval"),
        "`ttl` propagates post-`close` too"
    );

    assert_agrees_except(DRIVER_NAME, &sequence, &rust, &[]);
}

/// The datagram twin: the same eleven codes on `udp_inet`, where three of them are **not** what a
/// stream socket reports — each for a different reason, and each measured against the oracle:
///
/// * `nodelay` (`10`) is `getsockopt(IPPROTO_TCP, TCP_NODELAY)` on a **UDP** socket, which the
///   kernel refuses with `ENOPROTOOPT`, so the C drops that record only
///   (`inet_fill_opts`'s `TRUNCATE_TO(0, ptr); continue;`, `inet_drv.c:9734`-`:9741`);
/// * `delay_send` (`31`) is `desc->stype == SOCK_STREAM`-gated on both sides
///   (`:9256`-`:9266`, `:7476`-`:7490`), and a datagram descriptor's `stype` is `SOCK_DGRAM`;
/// * `linger`, `priority`, `keepalive`, `reuseaddr`, `tos`, `ttl`, `recvtos`, `recvttl` and
///   `active` **are** served on a datagram socket.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_eleven_code_option_family_agrees_with_the_oracle_on_the_datagram_driver() {
    let codes = ACCEPT_OPTS_FAMILY_CODES.to_vec();
    let linger = {
        let mut record = vec![INET_OPT_LINGER, 0, 0, 0, 0];
        record.extend_from_slice(&0i32.to_be_bytes());
        record
    };
    let sequence = vec![
        (INET_REQ_GETOPTS, codes.clone()),
        (INET_REQ_SETOPTS, linger.clone()),
        (INET_REQ_OPEN, open_arg_dgram()),
        (INET_REQ_GETOPTS, codes.clone()),
        (INET_REQ_SETOPTS, linger),
        (INET_REQ_GETOPTS, codes),
    ];
    let rust = rust_replies("udp_inet", &sequence);
    assert_eq!(rust.len(), sequence.len(), "one reply per step");

    assert_eq!(
        rust[0],
        vec![1, INET_LOPT_ACTIVE, 0, 0, 0, 0],
        "a bare datagram port reports the driver-state code alone"
    );
    assert_eq!(rust[1], ctl_ok(), "`linger` never propagates a failure");
    assert_eq!(rust[2], ctl_ok(), "the datagram `open`");
    let opened: Vec<u8> = records(&rust[3]).iter().map(|(code, _)| *code).collect();
    assert_eq!(
        opened,
        vec![
            INET_LOPT_ACTIVE,
            INET_OPT_KEEPALIVE,
            INET_OPT_PRIORITY,
            INET_OPT_LINGER,
            INET_OPT_REUSEADDR,
            INET_OPT_TOS,
            INET_OPT_TTL,
            INET_OPT_RECVTOS,
            INET_OPT_RECVTTL,
        ],
        "`nodelay` is `ENOPROTOOPT` on a datagram socket and `delay_send` is stream-gated"
    );
    assert_eq!(rust[4], ctl_ok(), "`setopts` on the datagram socket");

    assert_agrees_except("udp_inet", &sequence, &rust, &[]);
}

/// The upstream contract itself, pinned as the **decoded** term rather than as bytes: a real
/// `gen_tcp:listen/2` + `gen_tcp:accept/1` under the system `erl`, then
///
/// * `inet:getopts(L, Codes)` on the listener,
/// * `inet:getopts(S, Codes)` on the **accepted** socket (which has already been through
///   `accept_opts/3`'s `setopts`),
/// * `prim_inet:getopts/2` on a **raw** `{spawn_driver, "tcp_inet"}` port — the same eleven-code
///   request against a bare driver port, decoded by `prim_inet`'s own `decode_opt_val/1`
///   (`erts/preloaded/src/prim_inet.erl:2486`). That last row is the proof that the **byte** reply
///   this crate produces is exactly what `prim_inet` decodes: the same crate-side bytes are
///   byte-identical to the oracle's (the two `getopts` comparisons above), and these are that
///   oracle's bytes going through the real decoder.
///
/// Five length-framed fields come back (`N`, `Keys`, listener `Opts`, accepted `Opts`, raw-port
/// `Opts`), each `~w`-printed iolist text, so no reply byte can be mis-split.
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_upstream_gen_tcp_accept_pins_the_eleven_decoded_options() {
    let expr = format!(
        "M = {CONTROL_MAGIC}, \
         Codes = [active,nodelay,keepalive,delay_send,priority,linger,reuseaddr,tos,ttl,recvtos,recvttl], \
         {{ok, L}} = gen_tcp:listen(0, [binary, {{active, false}}, {{reuseaddr, true}}]), \
         {{ok, P}} = inet:port(L), \
         Self = self(), \
         spawn(fun() -> \
             {{ok, C}} = gen_tcp:connect({{127,0,0,1}}, P, [binary, {{active, false}}]), \
             Self ! {{connected, C}}, \
             receive stop -> ok end end), \
         {{ok, S}} = gen_tcp:accept(L), \
         Peer = receive {{connected, R0}} -> R0 after 10000 -> timeout end, \
         {{ok, Opts}} = inet:getopts(L, Codes), \
         {{ok, Accepted}} = inet:getopts(S, Codes), \
         Probe = open_port({{spawn_driver, \"tcp_inet\"}}, [binary]), \
         <<1>> = iolist_to_binary(erlang:port_control(Probe, 1 + M, <<1,1,0>>)), \
         {{ok, ProbeOpts}} = prim_inet:getopts(Probe, Codes), \
         Keys = [K || {{K,_}} <- Opts], \
         F = fun(X) -> iolist_to_binary([io_lib:format(\"~2.16.0b\", [B]) || <<B>> <= iolist_to_binary(X)]) end, \
         Fields = [integer_to_binary(length(Opts)), \
             iolist_to_binary(io_lib:format(\"~w\", [Keys])), \
             iolist_to_binary(io_lib:format(\"~w\", [Opts])), \
             iolist_to_binary(io_lib:format(\"~w\", [Accepted])), \
             iolist_to_binary(io_lib:format(\"~w\", [ProbeOpts]))], \
         io:format(\"~s~n\", [lists:join(\"|\", [F(X) || X <- Fields])]), \
         gen_tcp:close(S), gen_tcp:close(L), \
         case Peer of timeout -> ok; _ -> Peer ! stop end"
    );
    let Some(stdout) = erl_stdout(&expr) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the upstream accept_opts contract pin");
        return;
    };
    let fields = decode_hex_replies(&stdout).unwrap_or_else(|| {
        panic!("could not decode the upstream contract dump: {stdout:?}");
    });
    assert_eq!(fields.len(), 5, "five framed fields: {fields:?}");
    let text = |index: usize| {
        String::from_utf8(fields[index].clone())
            .unwrap_or_else(|err| panic!("field {index} is not UTF-8 text: {err}"))
    };
    let (count, keys, listener, accepted, raw_port) =
        (text(0), text(1), text(2), text(3), text(4));
    eprintln!("upstream listener opts:   {listener}");
    eprintln!("upstream accepted opts:   {accepted}");
    eprintln!("upstream raw-port opts:   {raw_port}");

    assert_eq!(count, "11", "`inet:getopts/2` decodes eleven pairs");
    assert_eq!(
        keys, ACCEPT_OPTS_FAMILY_ATOMS,
        "the request really is prim_inet:accept_opts/3's eleven codes, in its order"
    );
    // `accept_opts/3`'s `setopts(S, Opts)` half really applied the listener's block: the accepted
    // socket reports the same eleven pairs, `{reuseaddr,true}` included (`accept_opts` is the
    // reason that non-default value is on the accepted socket at all).
    assert_eq!(
        accepted, listener,
        "the accepted socket carries the listener's option block"
    );
    assert!(
        listener.contains("{reuseaddr,true}") && listener.contains("{nodelay,false}"),
        "the listener's own values, not the defaults: {listener}"
    );
    // Every code decoded through `prim_inet`'s own decoder on the raw driver port, i.e. the bytes
    // this crate's table produces are what `prim_inet` accepts.
    assert!(
        raw_port.starts_with("[{active,") && raw_port.ends_with("}]"),
        "the raw port's reply decodes to a pair list: {raw_port}"
    );
    for atom in [
        "active",
        "nodelay",
        "keepalive",
        "delay_send",
        "priority",
        "linger",
        "reuseaddr",
        "tos",
        "ttl",
        "recvtos",
        "recvttl",
    ] {
        assert!(
            raw_port.contains(&format!("{{{atom},")),
            "`prim_inet:getopts/2` decoded `{atom}` from the driver's byte reply: {raw_port}"
        );
    }
}
