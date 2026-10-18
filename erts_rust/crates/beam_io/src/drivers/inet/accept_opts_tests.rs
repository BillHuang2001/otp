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
//! The `prim_inet:accept_opts/3` replay — the exact option sequence `gen_tcp:accept/1` sends.
//!
//! `gen_tcp:accept/1` ends in `prim_inet:accept_opts/3` (`lib/erlang/lib/erts-17.1/src/
//! prim_inet.erl:502`-`:517`):
//!
//! ```erlang
//! accept_opts(L, S, FamilyOpts) ->
//!     case getopts(L, [active, nodelay, keepalive, delay_send, priority, linger, reuseaddr]
//!                     ++ FamilyOpts) of
//!         {ok, Opts} -> case setopts(S, Opts) of ok -> {ok, S}; Error1 -> close(S), Error1 end;
//!         Error2 -> close(S), Error2
//!     end.
//! ```
//!
//! So `accept_opts` needs **two** things to hold, and this suite pins both:
//!
//! 1. `getopts` of the seven codes `[active(22), nodelay(10), keepalive(1), delay_send(31),
//!    priority(8), linger(3), reuseaddr(0)]` on the **listening** port must answer `[1]` followed
//!    by one record per code, in request order. Any unserved code makes the C's
//!    `default: RETURN_ERROR()` (`inet_drv.c:9719`) discard the whole buffer and answer
//!    `[0]einval`, which is what `prim_inet:getopts/2` turns into a failed `gen_tcp:accept/1`.
//! 2. Feeding that record body straight back as a `setopts` — the `setopts(S, Opts)` half — must
//!    answer `[1]`. `linger` is the record whose **set** form is nine bytes (the five-byte record
//!    plus `l_linger`), so the replay also pins that the walk consumes it correctly.
//!
//! Both replies are byte-compared against the live `erl` oracle; every oracle half **skips
//! loudly** when `erl` is absent and no case asserts a wall-clock value. Two further cases cover
//! the semantics `accept_opts` leans on — the seven codes across the no-socket/opened/post-`close`
//! states (`priority`'s `propagate = 1`, `delay_send`'s stream gate, `linger`'s eight-byte
//! read-back and its "sticky" `l_linger`), and the whole served option surface in one request —
//! and the last one runs the high-level `gen_tcp:listen/2` + `gen_tcp:accept/1` pair under the
//! real `erl` to show the upstream sequence is exactly what a plain accept needs.
use super::oracle::*;
use super::*;
use crate::driver::DriverRegistry;
use crate::drivers::inet::{DRIVER_NAME, INET_AF_INET};
use crate::poller::Poller;
use crate::port::PortRegistry;

/// The seven option codes `prim_inet:accept_opts/3` requests, in its request order
/// (`inet_drv.c:874`-`:930`: `active`=22, `nodelay`=10, `keepalive`=1, `delay_send`=31,
/// `priority`=8, `linger`=3, `reuseaddr`=0).
const ACCEPT_OPTS_CODES: [u8; 7] = [22, 10, 1, 31, 8, 3, 0];

/// The packed loopback address `bind` takes: `[AF_INET, port_hi, port_lo, 127, 0, 0, 1]` with a
/// zero port, so the kernel picks the ephemeral one (no fixed port is ever required).
fn loopback_port_zero() -> Vec<u8> {
    vec![INET_AF_INET, 0, 0, 127, 0, 0, 1]
}

/// A live `tcp_inet` port over a real [`PortRegistry`] + [`Poller`], kept alive across steps so a
/// multi-step sequence (and a second port) can be driven by hand.
///
/// This is the [`rust_replies`](super::oracle::rust_replies) setup with the port handed back
/// instead of being closed, because `accept_opts` needs a *bound, listening* port for the
/// `getopts` half and a *second* port for the `setopts` half.
struct TestPort {
    poller: Poller,
    ports: PortRegistry,
    id: beam_process::PortId,
}

impl TestPort {
    /// Opens the built-in `tcp_inet` driver as a port and runs `open` on it, so it holds a real
    /// stream socket (needed for the syscall-backed option records: with no socket the C's
    /// `getsockopt` fails and `TRUNCATE_TO(0, ptr)` drops the record).
    fn open() -> TestPort {
        let mut poller = Poller::new().expect("a poll set");
        let driver = DriverRegistry::with_builtins()
            .create(DRIVER_NAME)
            .expect("the built-in tcp_inet driver is registered");
        let mut ports = PortRegistry::new();
        let id = ports.open(&mut poller, driver).expect("the port opens");
        let mut port = TestPort { poller, ports, id };
        assert_eq!(port.control("open", &open_arg()), ctl_ok(), "open");
        port
    }

    /// Runs one control verb on the live port.
    fn control(&mut self, op: &str, arg: &[u8]) -> Vec<u8> {
        self.ports
            .get_mut(self.id)
            .expect("the port is live")
            .control(&mut self.poller, op, arg)
            .unwrap_or_else(|err| panic!("the `{op}` verb failed: {err:?}"))
    }

    /// Binds to the loopback address with the kernel's ephemeral port and starts listening —
    /// the state `gen_tcp:listen(0, …)` puts the listen socket in before `accept`.
    fn bind_loopback_and_listen(&mut self) -> Vec<u8> {
        let bound = self.control("bind", &loopback_port_zero());
        assert_eq!(bound[0], 1, "bind answers `[1] ++ port`: {bound:?}");
        assert_eq!(bound.len(), 3, "bind carries the ephemeral port: {bound:?}");
        let listened = self.control("listen", &[0, 5]);
        assert_eq!(listened, ctl_ok(), "listen");
        bound
    }

    /// Closes the port (the driver's `stop` runs, releasing the socket).
    fn close(mut self) {
        let closed = self.ports.close(&mut self.poller, self.id);
        assert!(closed.is_ok(), "the port closes: {closed:?}");
    }
}

/// The record body the `accept_opts` `getopts` half produces, as the `setopts` request:
/// `reply[1..]` (the leading `[1]` is the C's `INET_REP_OK` tag, not a record byte).
fn setopts_request(getopts_reply: &[u8]) -> Vec<u8> {
    assert_eq!(getopts_reply.first(), Some(&1), "a `[1]`-tagged reply");
    getopts_reply[1..].to_vec()
}

/// The `prim_inet:accept_opts/3` replay against OUR driver, end to end: `open` → `bind` to a
/// loopback ephemeral port → `listen` → `getopts` of the seven codes → `setopts` of the record
/// body on a **second** port.
///
/// The `getopts` reply must begin with `1`: an unserved code among the seven would make the whole
/// reply `[0]einval` (the C's `default: RETURN_ERROR()`), and `gen_tcp:accept/1` would fail there.
#[test]
fn the_accept_opts_option_pair_is_served_and_round_trips() {
    let mut listener = TestPort::open();
    listener.bind_loopback_and_listen();

    let request: Vec<u8> = ACCEPT_OPTS_CODES.to_vec();
    let getopts = listener.control("getopts", &request);

    // `[1]` + 22,10,1,31,8 (five-byte records) + 3 (a nine-byte linger record) + 0.
    assert_eq!(
        getopts,
        vec![
            1, // INET_REP_OK
            22, 0, 0, 0, 0, // {active, false}
            10, 0, 0, 0, 0, // {nodelay, false}
            1, 0, 0, 0, 0, // {keepalive, false}
            31, 0, 0, 0, 0, // {delay_send, false}
            8, 0, 0, 0, 0, // {priority, 0}
            3, 0, 0, 0, 0, 0, 0, 0, 0, // {linger, {false, 0}} — eight payload bytes
            0, 0, 0, 0, 0, // {reuseaddr, false}
        ],
        "the accept_opts getopts record set"
    );
    assert_eq!(getopts[0], 1, "the C's INET_REP_OK, not `[0]einval`");
    assert_eq!(getopts.len(), 1 + 6 * 5 + 9, "seven records, linger's is nine");

    // The `setopts` half: `prim_inet:accept_opts/3` re-encodes the records onto the accepted
    // socket. The listener's own records are replayed onto a second, freshly opened port.
    let mut accepted = TestPort::open();
    let request = setopts_request(&getopts);
    assert_eq!(
        accepted.control("setopts", &request),
        ctl_ok(),
        "the getopts record body is a valid setopts request"
    );
    // The round trip leaves every plain-int value where it started.
    assert_eq!(
        accepted.control("getopts", &ACCEPT_OPTS_CODES[..5]),
        vec![1, 22, 0, 0, 0, 0, 10, 0, 0, 0, 0, 1, 0, 0, 0, 0, 31, 0, 0, 0, 0, 8, 0, 0, 0, 0],
        "the values read back unchanged"
    );

    accepted.close();
    listener.close();
}

/// The byte-for-byte gate: the identical `open`/`getopts`/`setopts` sequence must answer the same
/// bytes on the real system `tcp_inet` driver.
///
/// The comparison deliberately covers only the three steps whose replies are deterministic: the
/// `bind` reply carries the kernel's ephemeral port, so it is asserted to start with `[1]` on both
/// sides instead of being byte-compared (see
/// [`the_full_accept_opts_sequence_agrees_with_the_oracle`] for that half).
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_accept_opts_option_pair_agrees_with_the_oracle() {
    let mut listener = TestPort::open();
    listener.bind_loopback_and_listen();
    let request: Vec<u8> = ACCEPT_OPTS_CODES.to_vec();
    let getopts = listener.control("getopts", &request);

    let mut accepted = TestPort::open();
    let setopts = accepted.control("setopts", &setopts_request(&getopts));

    let sequence = vec![
        (INET_REQ_OPEN, open_arg()),
        (INET_REQ_GETOPTS, request),
        (INET_REQ_SETOPTS, setopts_request(&getopts)),
    ];
    let rust = vec![ctl_ok(), getopts.clone(), setopts.clone()];
    assert_eq!(setopts, ctl_ok(), "the setopts half on our side");

    assert_agrees_with_oracle(DRIVER_NAME, &sequence, &rust);

    accepted.close();
    listener.close();
}

/// The full `prim_inet:accept_opts/3` sequence — including the `bind`/`listen` that put the
/// listen socket in the state `gen_tcp:listen/2` leaves it in — replayed on both sides.
///
/// The `bind` reply is the one step that cannot be byte-compared (it names the kernel's ephemeral
/// port, a different number in each process), so it is asserted to carry the `[1]` tag on both
/// sides and the `getopts`/`setopts` replies are compared byte-for-byte against the oracle port
/// that ran the very same steps.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_full_accept_opts_sequence_agrees_with_the_oracle() {
    let mut listener = TestPort::open();
    let rust_bind = listener.bind_loopback_and_listen();
    let request: Vec<u8> = ACCEPT_OPTS_CODES.to_vec();
    let rust_getopts = listener.control("getopts", &request);

    let mut accepted = TestPort::open();
    let rust_setopts = accepted.control("setopts", &setopts_request(&rust_getopts));

    let sequence = vec![
        (INET_REQ_OPEN, open_arg()),
        (INET_REQ_BIND, loopback_port_zero()),
        (INET_REQ_LISTEN, vec![0, 5]),
        (INET_REQ_GETOPTS, request),
        (INET_REQ_SETOPTS, setopts_request(&rust_getopts)),
    ];
    let rust = vec![
        ctl_ok(),
        rust_bind.clone(),
        ctl_ok(),
        rust_getopts.clone(),
        rust_setopts.clone(),
    ];

    let Some(oracle) = oracle_replies(&sequence) else {
        return; // the oracle is unavailable; the skip was reported loudly above
    };
    assert_eq!(oracle.len(), sequence.len(), "one reply per step: {oracle:?}");
    eprintln!("oracle accept_opts replies: {oracle:?}");
    eprintln!("rust accept_opts replies:   {rust:?}");

    // Both binds carry the result tag and an ephemeral port; the numbers necessarily differ.
    assert_eq!(rust[1][0], 1, "our bind answers `[1] ++ port`: {:?}", rust[1]);
    assert_eq!(oracle[1][0], 1, "the oracle's bind answers `[1] ++ port`");
    assert_eq!(oracle[1].len(), 3, "the oracle's bind carries a port: {oracle:?}");
    // Every other step is deterministic and must agree byte for byte.
    for step in [0usize, 2] {
        assert_eq!(
            rust[step], oracle[step],
            "step {step} (`{}`) disagrees with the oracle",
            verb_name(sequence[step].0)
        );
    }
    // The two replies `accept_opts` actually depends on are byte-identical.
    assert_eq!(
        rust[3], oracle[3],
        "the accept_opts `getopts` reply disagrees with the oracle"
    );
    assert_eq!(
        rust[4], oracle[4],
        "the accept_opts `setopts` reply disagrees with the oracle"
    );

    accepted.close();
    listener.close();
}

/// Runs `sequence` on a real system `tcp_inet` port and returns one byte vector per reply, or
/// `None` (after a loud skip) when `erl` is absent.
///
/// This is [`oracle_expression`](super::oracle::oracle_expression) +
/// [`erl_stdout`](super::oracle::erl_stdout) + [`parse_reply_dump`](super::oracle::parse_reply_dump)
/// composed for the cases whose replies cannot all be compared byte-wise (the ephemeral-port
/// `bind`). Every other case uses the shared
/// [`assert_agrees_with_oracle`](super::oracle::assert_agrees_with_oracle).
fn oracle_replies(sequence: &[(u32, Vec<u8>)]) -> Option<Vec<Vec<u8>>> {
    let stdout = erl_stdout(&oracle_expression(DRIVER_NAME, sequence))?;
    match parse_reply_dump(&stdout) {
        Some(replies) => Some(replies),
        None => panic!("could not parse the oracle reply dump: {stdout:?}"),
    }
}

/// The upstream half: `gen_tcp:listen/2` + `gen_tcp:accept/1` under the real `erl`, i.e. the
/// sequence `prim_inet:accept_opts/3` exists to serve, run against a real socket.
///
/// A spawned process connects back to the listener's own port, so the accept completes without
/// any fixed port and without a second node. **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn gen_tcp_listen_and_accept_complete_under_the_oracle() {
    // `accept/1` (arity 1) is the `accept_opts` caller: it inherits the listen socket's options.
    let expr = "{ok, L} = gen_tcp:listen(0, [binary, {active, false}, {reuseaddr, true}]), \
                {ok, P} = inet:port(L), \
                Self = self(), \
                spawn(fun() -> \
                    R = gen_tcp:connect({127,0,0,1}, P, [binary, {active, false}]), \
                    Self ! {connected, R} end), \
                A = gen_tcp:accept(L), \
                C = receive {connected, R0} -> R0 after 10000 -> timeout end, \
                case {A, C} of \
                    {{ok, S}, {ok, CS}} when is_port(S), is_port(CS) -> \
                        gen_tcp:close(CS), gen_tcp:close(S), gen_tcp:close(L), \
                        io:format(\"accepted {ok,~p}~n\", [true]); \
                    Other -> \
                        io:format(\"FAILED ~p~n\", [Other]) \
                end";
    let Some(stdout) = erl_stdout(expr) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the gen_tcp listen/accept oracle check");
        return;
    };
    eprintln!("gen_tcp listen/accept oracle run: {stdout:?}");
    assert!(
        stdout.contains("accepted") && stdout.contains("{ok,"),
        "gen_tcp:listen/2 + gen_tcp:accept/1 must complete: {stdout:?}"
    );
    assert!(
        !stdout.contains("FAILED"),
        "the accept pair failed on the oracle: {stdout:?}"
    );
}

/// The seven `accept_opts` codes across every socket state a `tcp_inet` port can be in — no
/// socket, an opened stream socket, post-`close` — with the three misses the C's get switch
/// distinguishes kept apart:
///
/// * `priority` (`8`) is one of the nine `propagate = 1` codes, so a `setsockopt` failure replaces
///   the **whole** reply with `[0]einval` — which is why `set 8` on a socket-less (and on a
///   post-`close`) port is `[0]einval` while `set 3`/`set 31` there are `[1]`;
/// * `delay_send` (`31`) is `stype == SOCK_STREAM`-gated on **both** sides, so a bare port
///   contributes no record and stores nothing, an opened one reports a boolean;
/// * `linger` (`3`) is the eight-byte read-back/ nine-byte set record, and the kernel keeps
///   `l_linger` when `l_onoff` is cleared (the C's "sticky" form) — a bare port truncates it.
///
/// The sequence is identical on both sides (no ephemeral port anywhere), so the whole thing is
/// byte-compared through the shared oracle harness. **Skips loudly** without `erl`.
#[test]
fn the_accept_opts_codes_agree_with_the_oracle_in_every_state() {
    let linger = |onoff: i32, seconds: i32| {
        let mut record = vec![INET_OPT_LINGER, 0, 0, 0, 0];
        record[1..5].copy_from_slice(&onoff.to_be_bytes());
        record.extend_from_slice(&seconds.to_be_bytes());
        record
    };
    let all_codes: Vec<u8> = ACCEPT_OPTS_CODES.to_vec();
    let sequence = vec![
        // ---- (a) no socket at all: the C's `inet.stype` is -1 ----
        (INET_REQ_GETOPTS, all_codes.clone()),
        // `priority` propagates, so an invalid descriptor fails the whole reply ...
        (INET_REQ_SETOPTS, record(INET_OPT_PRIORITY, -1)),
        // ... while every other accept_opts code answers `[1]` (the C's `propagate = 0`).
        (INET_REQ_SETOPTS, linger(1, 7)),
        (INET_REQ_SETOPTS, record(INET_LOPT_TCP_DELAY_SEND, 1)),
        (INET_REQ_SETOPTS, record(INET_OPT_KEEPALIVE, 1)),
        (INET_REQ_SETOPTS, record(INET_OPT_REUSEADDR, 1)),
        (INET_REQ_SETOPTS, record(TCP_OPT_NODELAY, 1)),
        (INET_REQ_SETOPTS, record(INET_LOPT_ACTIVE, 0)),
        // The gated/truncated read-backs stay empty on a bare port.
        (INET_REQ_GETOPTS, vec![INET_OPT_LINGER]),
        (INET_REQ_GETOPTS, vec![INET_LOPT_TCP_DELAY_SEND]),
        (INET_REQ_GETOPTS, vec![INET_OPT_PRIORITY]),
        // ---- (b) an opened stream socket: every record reads back ----
        (INET_REQ_OPEN, open_arg()),
        (INET_REQ_GETOPTS, all_codes.clone()),
        (INET_REQ_SETOPTS, linger(1, 7)),
        (INET_REQ_GETOPTS, vec![INET_OPT_LINGER]),
        // Clearing `l_onoff` leaves `l_linger` where it was (the kernel's own rule).
        (INET_REQ_SETOPTS, linger(0, 0)),
        (INET_REQ_GETOPTS, vec![INET_OPT_LINGER]),
        (INET_REQ_SETOPTS, record(INET_LOPT_TCP_DELAY_SEND, 1)),
        (INET_REQ_GETOPTS, vec![INET_LOPT_TCP_DELAY_SEND]),
        (INET_REQ_SETOPTS, record(INET_LOPT_TCP_DELAY_SEND, 0)),
        (INET_REQ_GETOPTS, vec![INET_LOPT_TCP_DELAY_SEND]),
        (INET_REQ_SETOPTS, record(INET_OPT_PRIORITY, 0)),
        (INET_REQ_GETOPTS, vec![INET_OPT_PRIORITY]),
        (INET_REQ_SETOPTS, record(INET_OPT_KEEPALIVE, 1)),
        (INET_REQ_GETOPTS, vec![INET_OPT_KEEPALIVE]),
        (INET_REQ_SETOPTS, record(INET_OPT_REUSEADDR, 1)),
        (INET_REQ_GETOPTS, vec![INET_OPT_REUSEADDR]),
        (INET_REQ_SETOPTS, record(TCP_OPT_NODELAY, 1)),
        (INET_REQ_GETOPTS, vec![TCP_OPT_NODELAY]),
        // ---- (c) post-`close`: the cached `stype` survives, the socket does not ----
        (INET_REQ_CLOSE, vec![]),
        (INET_REQ_GETOPTS, all_codes),
        (INET_REQ_SETOPTS, record(INET_OPT_PRIORITY, 0)),
        (INET_REQ_SETOPTS, record(INET_LOPT_TCP_DELAY_SEND, 1)),
        (INET_REQ_GETOPTS, vec![INET_LOPT_TCP_DELAY_SEND]),
    ];
    let rust = rust_replies(DRIVER_NAME, &sequence);
    assert_eq!(rust.len(), sequence.len(), "one reply per step");

    // The bare-port rows, on the record: only the ungated driver-state code is reported.
    assert_eq!(
        rust[0],
        vec![1, 22, 0, 0, 0, 0],
        "a bare port truncates every syscall-backed record, keeping `active`"
    );
    assert_eq!(
        rust[1],
        ctl_xerror("einval"),
        "`priority` is one of the nine propagate=1 codes"
    );
    assert_eq!(rust[2], ctl_ok(), "`linger` never propagates a failure");
    assert_eq!(rust[3], ctl_ok(), "the stream-gated `delay_send` still answers `[1]`");
    assert_eq!(rust[8], ctl_ok(), "the bare port drops the `linger` record");
    assert_eq!(rust[9], ctl_ok(), "`delay_send` is gated out with no cached `stype`");
    assert_eq!(rust[10], ctl_ok(), "the bare port drops the `priority` record");
    // The opened rows.
    assert_eq!(
        rust[12],
        vec![
            1, 22, 0, 0, 0, 0, // {active, false}
            10, 0, 0, 0, 0, // {nodelay, false}
            1, 0, 0, 0, 0, // {keepalive, false}
            31, 0, 0, 0, 0, // {delay_send, false}
            8, 0, 0, 0, 0, // {priority, 0}
            3, 0, 0, 0, 0, 0, 0, 0, 0, // {linger, {false, 0}}
            0, 0, 0, 0, 0, // {reuseaddr, false}
        ],
        "the accept_opts record set on an opened stream socket"
    );
    assert_eq!(
        rust[14],
        vec![1, 3, 0, 0, 0, 1, 0, 0, 0, 7],
        "eight linger payload bytes: `l_onoff` then `l_linger`"
    );
    assert_eq!(
        rust[16],
        vec![1, 3, 0, 0, 0, 0, 0, 0, 0, 7],
        "clearing `l_onoff` keeps the kernel's `l_linger`"
    );
    assert_eq!(rust[18], vec![1, 31, 0, 0, 0, 1], "`delay_send` reads back as a boolean");
    // The post-`close` rows.
    assert_eq!(
        rust[30],
        vec![1, 22, 0, 0, 0, 0, 31, 0, 0, 0, 0],
        "the cached `stype` survives the close, the socket does not"
    );
    assert_eq!(rust[31], ctl_xerror("einval"), "`priority` propagates post-close too");

    assert_agrees_with_oracle(DRIVER_NAME, &sequence, &rust);
}

/// The whole **served** option surface in one request, across the three `tcp_inet` states — the
/// first execution of codes whose only consumer is `accept_opts`-adjacent driver work
/// (`linger`, `priority`, `delay_send`, `keepcnt`/`keepidle`/`keepintvl`, the watermarks, …).
///
/// Two codes are deliberately **not** requested, because neither has a comparable reply:
///
/// * `multicast_if` (`11`) — the C reserves four value bytes it never writes and advances its
///   length past them, so the reported value slot is *uninitialized memory*
///   (`inet_fill_opts`, `inet_drv.c:9497`-`:9523`; measured runs differ). This floor answers the
///   deterministic zero there — a documented non-mirror, not a value the C produces;
/// * `raw` (`34`) — its reply is a caller-sized byte block, so it needs a fourteen-byte request
///   per code rather than the one-byte code list this sweep sends.
///
/// The set-only codes (`14`/`15` membership, `40` line_delim) would replace the whole reply with
/// `[0]einval` and are covered by their own rows elsewhere. **Skips loudly** without `erl`.
#[test]
fn the_whole_served_option_surface_agrees_with_the_oracle() {
    let codes: Vec<u8> = vec![
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 13, 16, 17, 18, 19, 21, 22, 23, 24, 25, 30, 31, 32,
        33, 35, 36, 37, 38, 41, 42, 43, 44, 45, 46, 47, 48, 51, 52, 53, 54,
    ];
    let sequence = vec![
        // (a) no socket at all
        (INET_REQ_GETOPTS, codes.clone()),
        // (b) an opened stream socket: every servable record appears
        (INET_REQ_OPEN, open_arg()),
        (INET_REQ_GETOPTS, codes.clone()),
        // a truncated `linger` record (no `l_linger`) is the C's `if (len < 4) return -1`
        (INET_REQ_SETOPTS, vec![INET_OPT_LINGER, 0, 0, 0, 1]),
        (INET_REQ_CLOSE, vec![]),
        // (c) post-`close`: the cached `stype` keeps the gated records alive
        (INET_REQ_GETOPTS, codes),
    ];
    let rust = rust_replies(DRIVER_NAME, &sequence);
    assert_eq!(rust.len(), sequence.len(), "one reply per step");
    // On a bare port `pktoptions` (`45`) has no cached family, which is the C's
    // `RETURN_ERROR()` — the **whole** reply is `[0]einval`, ahead of every other record.
    assert_eq!(
        rust[0],
        ctl_xerror("einval"),
        "the family-gated `pktoptions` refuses the whole reply on a bare port"
    );
    assert_eq!(rust[1], ctl_ok(), "open");
    assert_eq!(
        rust[2][0], 1,
        "no served code among the request — an unserved one would discard the whole reply"
    );
    assert_eq!(
        rust[3],
        ctl_xerror("einval"),
        "a truncated `linger` record is the C's `return -1`"
    );
    assert_eq!(rust[4], ctl_ok(), "close");
    assert_eq!(rust[5][0], 1, "the cached `stype` keeps the gated records: {:?}", rust[5]);

    assert_agrees_with_oracle(DRIVER_NAME, &sequence, &rust);
}
