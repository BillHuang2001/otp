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

//! **The two-VM payload matrix** (contract A.5, the inbound *heap-fragment read* path): a
//! payload far larger than a receiver's default nursery crosses a real distribution link and
//! is **read back intact**.
//!
//! [`tests/dist_two_vm.rs`](../dist_two_vm.rs) proves the link (a `pong`, one message each
//! way, `nodedown` both ways) with payloads of a handful of words. This suite is about the
//! *size* of what the link carries. A `beam_rt` process starts with a **233-word** nursery,
//! so any payload past a few hundred bytes cannot be copied into the receiver's heap at
//! delivery time at all: it arrives through
//! `LocalDeliverySink::enqueue_fragmented` → `beam_message::send_fragmented` →
//! [`HeapFragment`], and is only materialized by the fragment **read** path when the
//! receiving process matches it in `receive` (`beam_engine`'s `loop_rec` over
//! `Mailbox::receive_next`). Every leg below therefore exercises that read path, and a leg
//! fails if the fragment path hands back a flattened, truncated, empty or reordered payload.
//!
//! # How a leg is asserted (the digest, and why there is no magic constant here)
//!
//! Each leg runs **two VMs** with a **private `epmd`** ([`support::two_vm`], the shared
//! harness): the runtime under test, running the compiled fixture [`FIXTURE_ERL`] through
//! the `run` action, and a system-`erl` peer.
//! The fixture registers `two_vm_peer`, prints `READY <node>`, then takes the message with a
//! **deadline-bounded** `receive`; the peer pings the runtime until it answers `pong`, prints
//! its own digest of the payload it is about to send (`TX digest=…`), sends it, and exits.
//!
//! Both sides digest what they hold with the *same* pair of primitives, so the test compares
//! two independently computed terms and bakes **no** expected value into itself:
//!
//! * a binary `B` → `{byte_size(B), binary:decode_unsigned(erlang:md5(B))}`;
//! * an integer list `L` → `{length(L), lists:sum(L), binary:decode_unsigned(erlang:md5(term_to_binary(L)))}`.
//!
//! Asserting `TX digest=<D>` equals `RX digest=<D>` is what makes a mangled payload
//! detectable: an empty, flattened or truncated copy has a different size, a different
//! length/sum and a different MD5, so no collision is plausible. Each leg additionally
//! asserts the digest's *leading* component (`{1024,`, `{10000,50005000,`, …), so a leg that
//! accidentally sent the wrong payload cannot pass by agreeing with itself.
//!
//! # The legs
//!
//! | Test | Payload | What it pins |
//! |---|---|---|
//! | [`a_1_kib_binary_is_delivered_intact`] | 1 KiB binary | the smallest size past the nursery: the fragment path replaces an on-heap copy |
//! | [`a_2_kib_binary_is_delivered_intact`] | 2 KiB binary | a second size, so a lucky 1 KiB fit cannot pass for a working path |
//! | [`a_64_kib_binary_is_delivered_intact`] | 64 KiB binary | a payload that is a *refc* binary on arrival (off-heap payload, on-heap reference) |
//! | [`a_1_mib_binary_is_delivered_intact`] | 1 MiB binary | the size the distribution decode arena is sized for; a message this large used to retire the link (`allocation failure: heap`) |
//! | [`a_100_element_integer_list_is_delivered_intact`] | 100-element integer list | a **list** payload: 200 words of cons cells, i.e. just past the nursery |
//! | [`a_10_000_element_integer_list_is_delivered_intact`] | 10 000-element integer list | a list two orders of magnitude past the nursery, still read back element for element through the digest |
//! | [`a_20_message_burst_of_64_byte_binaries_is_delivered_intact`] | 20 separate messages of `{burst, I, <<64 bytes of I>>}` | **many** fragment-carrying messages, verified element by element, counted, and digested in arrival order |
//! | [`the_differential_corpus_shape_delivers_a_payload_intact`] | 1 KiB binary | the *shape* `tools/differential`'s `dist_extra` corpus uses (`-eval` client, `-eval` peer), here as a **cross-runtime** pair |
//! | [`the_two_erl_oracle_delivers_a_multi_megabyte_binary_and_a_100k_list`] | 2 MiB binary + 100 000-element list | the **oracle's own** behaviour at those sizes, which is what anchors the matrix's expectations |
//!
//! # Running it
//!
//! Every test is `#[ignore]`d: each really spawns two VMs (plus a private `epmd`), needs a
//! whole system Erlang toolchain (`erl`, `erlc`, `epmd`) and `setsid`/`kill`, and takes a
//! few seconds. The exact command:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_rt --test dist_payload_matrix -- --ignored --nocapture
//! ```
//!
//! To run one leg: append `--exact <test-name>` after the `--`. `--nocapture` matters: both
//! VMs' transcripts are echoed on failure *and* on every loud skip. A missing toolchain, an
//! `epmd` that never comes up or a runtime that never prints `READY` is a **loud skip**,
//! never a silent pass (see [`support::two_vm`]'s module docs for the isolation rules).
//!
//! # Assumptions the implementation must satisfy
//!
//! * a `DOP_REG_SEND` payload of any size the wire carries is delivered **whole** — the
//!   decode arena is sized from the reassembly limit
//!   (`crate::dist::DIST_DISPATCH_HEAP_WORDS`), not from one frame;
//! * a payload that does not fit the receiver's heap is queued as a heap fragment and the
//!   fragment **read** path materializes it at `loop_rec` time;
//! * a message that arrives while the receiver is parked in `receive … after T` wakes it (the
//!   deadline is also what keeps the run action's scheduler pump stepping: a bare `receive`
//!   leaves the pump with nothing to poll, `tests/live_inbound.rs` pins the same rule).

use support::two_vm::{
    unique, unique_cookie, ChildVm, Harness, EXIT_TIMEOUT, LINE_TIMEOUT, READY_TIMEOUT,
};

// The shared harness is compiled into every test target; this target only uses a subset of
// it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

/// The suite name every loud skip reports.
const BANNER: &str = "dist_payload_matrix";

// ---------------------------------------------------------------------------------------
// The fixture (compiled at test time with the system `erlc`; never committed)
// ---------------------------------------------------------------------------------------

/// The runtime-side fixture, shared by every leg.
///
/// `run/2` takes the leg's **shape** atom and the peer's `node@host` charlist (accepted for
/// symmetry with the harness's `run`-argument convention; the fixture never needs to address
/// the peer, so it is ignored). It registers `two_vm_peer` **before** printing `READY`, so a
/// payload the peer sends after seeing that line always lands in a live mailbox.
///
/// Two shapes:
///
/// * `burst` — the peer sends twenty separate `{burst, I, <<64 bytes of I>>}` messages. Each
///   one is verified **element by element** against `list_to_binary(lists:duplicate(64, I))`
///   (a count alone would not catch a payload whose elements were permuted or corrupted),
///   then the twenty received payloads are concatenated **in arrival order** and digested, so
///   a reordered burst fails the digest too. The line is
///   `BURST count=<N> ok=<true|false> digest={1280, <md5>}`.
/// * anything else — a single message, digested by [`FIXTURE_ERL`]'s `digest/1`:
///   `RX digest={byte_size, md5}` for a binary, `{length, sum, md5(term_to_binary)}` for a
///   list.
///
/// Every wait is deadline-bounded (`after 10000`), which is load-bearing rather than
/// defensive: a bare `receive` leaves the `run` action's scheduler pump with an empty run
/// queue, nothing to poll and no pending timer, so `Runtime::run_scheduled` stops with its
/// honest `blocked` diagnostic before the peer's message can arrive.
const FIXTURE_ERL: &str = r#"%% Generated by `beam_rt/tests/dist_payload_matrix.rs` (never committed).
-module(payload_matrix).
-export([run/2]).

run(Shape, _PeerNode) ->
    true = register(two_vm_peer, self()),
    io:format("READY ~p~n", [node()]),
    case Shape of
        burst -> burst(20, 0, true, []);
        _ -> single()
    end.

%% One message, digested by shape. `timeout` is the honest verdict: the test then reports a
%% `TIMEOUT` line instead of waiting for this runtime to be killed.
single() ->
    receive
        Msg ->
            io:format("RX digest=~w~n", [digest(Msg)])
    after 10000 ->
        io:format("TIMEOUT~n", []),
        halt(4)
    end.

burst(0, Count, Ok, Acc) ->
    Concat = iolist_to_binary(Acc),
    io:format("BURST count=~w ok=~p digest={~w,~w}~n",
              [Count, Ok, byte_size(Concat),
               binary:decode_unsigned(erlang:md5(Concat))]);
burst(Left, Count, Ok, Acc) ->
    receive
        {burst, I, Bin} ->
            Expected = list_to_binary(lists:duplicate(64, I)),
            Good = (Bin =:= Expected)
                andalso is_integer(I)
                andalso (I >= 1)
                andalso (I =< 20),
            burst(Left - 1, Count + 1, Ok andalso Good, Acc ++ [Bin])
    after 10000 ->
        io:format("BURST count=~w ok=false digest=timeout~n", [Count]),
        halt(6)
    end.

digest(B) when is_binary(B) ->
    {byte_size(B), binary:decode_unsigned(erlang:md5(B))};
digest(L) when is_list(L) ->
    {length(L), lists:sum(L),
     binary:decode_unsigned(erlang:md5(term_to_binary(L)))}.
"#;

/// The fixture's module name (it must be what the `run` spec names).
const FIXTURE_MODULE: &str = "payload_matrix";

/// The fixture's entry point: the leg's shape atom + the peer's node charlist.
const FIXTURE_SPEC: &str = "payload_matrix:run/2";

// ---------------------------------------------------------------------------------------
// The peer expressions
// ---------------------------------------------------------------------------------------

/// The peer preamble, shared by every single-payload script: resolve `@TARGET@` (the
/// runtime's full `node@host` atom), `net_adm:ping/1` it until it answers `pong` (a real
/// `gen:call` to the *remote* `net_kernel`, so the runtime must serve that endpoint), and
/// report `PONG <own node>`.
///
/// The ping is retried on a bounded schedule (100 × 200 ms) because the runtime's `epmd`
/// publication and its fixture's `register/2` are two separate steps; the retries only
/// absorb that window, and a peer that never gets a `pong` prints `PINGFAIL` and halts 2
/// rather than hanging.
const PEER_PREAMBLE: &str = r#"
Target = list_to_atom("@TARGET@"),
Wait = fun F(0) -> io:format("PINGFAIL~n", []), halt(2);
          F(N) -> case net_adm:ping(Target) of
                      pong -> ok;
                      _ -> timer:sleep(200), F(N - 1)
                  end
       end,
Wait(100),
io:format("PONG ~p~n", [node()]),
"#;

/// The peer epilogue: send `PAYLOAD_EXPR`'s message to the runtime's registered name and
/// keep the VM alive long enough for the delivery (and the receiver's `RX` line) to land,
/// then `halt(0)` — the peer never depends on a reply.
const PEER_EPILOGUE: &str = r#"
{two_vm_peer, Target} ! Payload,
receive _ -> ok after 3000 -> ok end,
halt(0).
"#;

/// The peer script for a **binary** payload built by `payload`: prints its own digest
/// (`TX digest={byte_size, md5}`) *before* sending, so a leg that never reaches the receiver
/// still reports the sender's figure in the failure transcript.
fn binary_peer_eval(payload: &str) -> String {
    format!(
        r#"{PEER_PREAMBLE}
Payload = {payload},
io:format("TX digest=~w~n",
          [{{byte_size(Payload), binary:decode_unsigned(erlang:md5(Payload))}}]),
{PEER_EPILOGUE}"#
    )
}

/// The peer script for an **integer-list** payload built by `payload`, digested as
/// `{length, sum, md5(term_to_binary)}`.
fn list_peer_eval(payload: &str) -> String {
    format!(
        r#"{PEER_PREAMBLE}
Payload = {payload},
io:format("TX digest=~w~n",
          [{{length(Payload), lists:sum(Payload),
             binary:decode_unsigned(erlang:md5(term_to_binary(Payload)))}}]),
{PEER_EPILOGUE}"#
    )
}

/// The peer script for the twenty-message burst: it builds `{burst, I, <<64 bytes of I>>}`
/// for `I` in `1..=20` **in order**, sends each one as its own message, and prints the
/// ordering-sensitive digest of the concatenation (`TX count=20 digest={1280, md5}`).
const PEER_BURST_EVAL: &str = r#"
Target = list_to_atom("@TARGET@"),
Wait = fun F(0) -> io:format("PINGFAIL~n", []), halt(2);
          F(N) -> case net_adm:ping(Target) of
                      pong -> ok;
                      _ -> timer:sleep(200), F(N - 1)
                  end
       end,
Wait(100),
io:format("PONG ~p~n", [node()]),
Send = fun(I) ->
               Bin = list_to_binary(lists:duplicate(64, I)),
               {two_vm_peer, Target} ! {burst, I, Bin},
               Bin
       end,
Bins = lists:map(Send, lists:seq(1, 20)),
Concat = iolist_to_binary(Bins),
io:format("TX count=20 digest={~w,~w}~n",
          [byte_size(Concat), binary:decode_unsigned(erlang:md5(Concat))]),
receive _ -> ok after 3000 -> ok end,
halt(0).
"#;

/// The corpus-shaped **peer** (`tools/differential/src/corpus/dist_extra.rs`'s `PEER_PAYLOAD`
/// shape, read-only reference): register a name, announce readiness, block in a
/// deadline-bounded `receive`, digest what arrived, and exit on its own.
///
/// The names (`dxpeer`, `PEER-READY`) follow the corpus so the shape is recognisable; the
/// digest replaces the corpus's `phash2`/ETF-size reply because this leg asserts payload
/// *identity* rather than a relational classification.
const CORPUS_PEER_EVAL: &str = r#"
try register(dxpeer, self()),
    io:format("PEER-READY ~p~n", [node()]),
    receive
        {payload, _From, Term} ->
            io:format("RX digest=~w~n",
                      [{byte_size(Term), binary:decode_unsigned(erlang:md5(Term))}])
    after 8000 ->
        io:format("PEER-TIMEOUT~n", [])
    end,
    halt(0)
catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end.
"#;

/// The corpus-shaped **client**, run by the runtime under test under `-eval` (exactly the
/// corpus's argv shape, `-noshell -sname <node> -eval <script>`): register, ping until
/// `pong`, print the digest of the one payload it will send, send it to the peer's registered
/// name, then wait out its own deadline and halt on its own (no `init:stop`).
///
/// `@TARGET@` is replaced with the *peer's* full `node@host` atom.
const CORPUS_CLIENT_EVAL: &str = r#"
Target = list_to_atom("@TARGET@"),
Pong = case lists:any(fun(_) -> case net_adm:ping(Target) of
                                   pong -> true;
                                   _ -> timer:sleep(100), false
                               end
                      end, lists:seq(1, 50)) of
           true -> pong;
           false -> pang
       end,
io:format("PONG ~p~n", [Pong]),
Payload = binary:copy(<<"0123456789abcdef">>, 64),
io:format("TX digest=~w~n",
          [{byte_size(Payload), binary:decode_unsigned(erlang:md5(Payload))}]),
erlang:send({dxpeer, Target}, {payload, self(), Payload}),
receive _ -> ok after 3000 -> ok end,
halt(0).
"#;

/// The **oracle-side receiver** of
/// [`the_two_erl_oracle_delivers_a_multi_megabyte_binary_and_a_100k_list`]: register a name,
/// announce readiness, take two messages in order and digest each by shape.
const TWO_ERL_SINK_EVAL: &str = r#"
register(matrix_sink, self()),
io:format("READY ~p~n", [node()]),
Bin = receive M1 -> M1 after 20000 -> timeout end,
case Bin of
    timeout -> io:format("RX1 TIMEOUT~n", []);
    _ -> io:format("RX1 digest=~w~n",
                   [{byte_size(Bin), binary:decode_unsigned(erlang:md5(Bin))}])
end,
List = receive M2 -> M2 after 20000 -> timeout end,
case List of
    timeout -> io:format("RX2 TIMEOUT~n", []);
    _ -> io:format("RX2 digest=~w~n",
                   [{length(List), lists:sum(List),
                     binary:decode_unsigned(erlang:md5(term_to_binary(List)))}])
end,
halt(0).
"#;

/// The **oracle-side sender**: ping the sink until `pong`, print its own digests, send the
/// two payloads to the sink's registered name in order, then exit on its own.
const TWO_ERL_CLIENT_EVAL: &str = r#"
Target = list_to_atom("@TARGET@"),
Pong = case lists:any(fun(_) -> case net_adm:ping(Target) of
                                   pong -> true;
                                   _ -> timer:sleep(100), false
                               end
                      end, lists:seq(1, 50)) of
           true -> pong;
           false -> pang
       end,
io:format("PONG ~p~n", [Pong]),
Bin = binary:copy(<<"0123456789abcdef">>, 131072),
io:format("TX1 digest=~w~n",
          [{byte_size(Bin), binary:decode_unsigned(erlang:md5(Bin))}]),
erlang:send({matrix_sink, Target}, Bin),
List = lists:seq(1, 100000),
io:format("TX2 digest=~w~n",
          [{length(List), lists:sum(List),
            binary:decode_unsigned(erlang:md5(term_to_binary(List)))}]),
erlang:send({matrix_sink, Target}, List),
receive _ -> ok after 3000 -> ok end,
halt(0).
"#;

// ---------------------------------------------------------------------------------------
// One leg: the harness, the runtime fixture and the peer
// ---------------------------------------------------------------------------------------

/// One running leg: the shared harness (private `epmd` + scratch directory), the runtime
/// under test, the system-`erl` peer, and both nodes' full names.
///
/// Declared so the children drop **before** the harness: the VMs are killed, then the
/// private `epmd` is stopped, then the scratch directory (the children's cwd) is removed.
struct Leg {
    runtime: ChildVm,
    peer: ChildVm,
    /// Held, never read: it owns the private `epmd` and the scratch directory (the children's
    /// cwd), so it must outlive both children — which is why it is declared last.
    #[allow(dead_code)]
    harness: Harness,
    rust_node: String,
    peer_node: String,
}

/// Starts one leg: the harness, the compiled fixture, the runtime (waits for its `READY`
/// line, which is what proves `two_vm_peer` is registered) and the peer (waits for its
/// `PONG` line).
///
/// `None` after a loud skip (the harness's own preconditions, or a runtime that never came
/// up). A peer that never gets a `pong` is a **failure**, not a skip: the runtime was up and
/// its fixture registered, so the link is what broke.
fn start_leg(label: &str, shape: &str, peer_eval: &str) -> Option<Leg> {
    let harness = Harness::new(BANNER, label)?;
    let fixture = harness.compile_fixture(FIXTURE_MODULE, FIXTURE_ERL)?;

    let rust_name = unique('r');
    let peer_name = unique('o');
    let cookie = unique_cookie();
    let rust_node = format!("{rust_name}@{}", harness.host);
    let peer_node = format!("{peer_name}@{}", harness.host);

    let runtime = harness
        .spawn_runtime_with(
            &rust_name,
            &cookie,
            &fixture,
            FIXTURE_SPEC,
            &[shape.to_string(), format!("\"{peer_node}\"")],
        )
        .unwrap_or_else(|error| panic!("cannot start the runtime under setsid: {error}"));

    let Some(ready) = runtime.wait_for(|line| line.starts_with("READY "), READY_TIMEOUT) else {
        support::skip_loudly(
            BANNER,
            &format!(
                "the runtime printed no `READY` line within {READY_TIMEOUT:?}\n  command: {}\n  \
                 stdout: {:?}\n  stderr: {}",
                runtime.command,
                runtime.transcript(),
                runtime.stderr_text()
            ),
        );
        return None;
    };
    assert_eq!(
        ready.trim(),
        format!("READY {rust_node}"),
        "the runtime's fixture reported its own full node name\n  stderr: {}",
        runtime.stderr_text()
    );
    harness.report_registration(&rust_name);

    let peer = harness
        .spawn_peer(&peer_name, &cookie, peer_eval, &rust_node)
        .unwrap_or_else(|error| panic!("cannot start the system `erl` peer under setsid: {error}"));

    let Some(pong) = peer.wait_for(|line| line.starts_with("PONG "), LINE_TIMEOUT) else {
        panic!(
            "`net_adm:ping('{rust_node}')` never answered `pong`\n  peer command: {}\n  peer \
             stdout: {:?}\n  peer stderr: {}\n  runtime command: {}\n  runtime stdout: {:?}\n  \
             runtime stderr: {}",
            peer.command,
            peer.transcript(),
            peer.stderr_text(),
            runtime.command,
            runtime.transcript(),
            runtime.stderr_text(),
        )
    };
    assert_eq!(
        pong.trim(),
        format!("PONG {peer_node}"),
        "the peer answered only after a real `pong` from {rust_node}"
    );

    Some(Leg {
        runtime,
        peer,
        harness,
        rust_node,
        peer_node,
    })
}

impl Leg {
    /// Waits for the peer's first line starting with `prefix`, or fails with both VMs'
    /// transcripts.
    fn expect_peer(&self, prefix: &str) -> String {
        self.peer
            .wait_for(|line| line.starts_with(prefix), LINE_TIMEOUT)
            .unwrap_or_else(|| {
                panic!(
                    "the peer never printed a `{prefix}` line\n  peer command: {}\n  peer stdout: \
                     {:?}\n  peer stderr: {}\n  runtime command: {}\n  runtime stdout: {:?}\n  \
                     runtime stderr: {}",
                    self.peer.command,
                    self.peer.transcript(),
                    self.peer.stderr_text(),
                    self.runtime.command,
                    self.runtime.transcript(),
                    self.runtime.stderr_text(),
                )
            })
    }

    /// Waits for the runtime's first line starting with `prefix`, or fails with both VMs'
    /// transcripts (a receiver that reports `TIMEOUT` is named in the message).
    fn expect_runtime(&self, prefix: &str) -> String {
        self.runtime
            .wait_for(|line| line.starts_with(prefix), LINE_TIMEOUT)
            .unwrap_or_else(|| {
                panic!(
                    "the runtime {} never printed a `{prefix}` line\n  runtime command: {}\n  \
                     runtime stdout: {:?}\n  runtime stderr: {}\n  peer {} command: {}\n  peer \
                     stdout: {:?}\n  peer stderr: {}",
                    self.rust_node,
                    self.runtime.command,
                    self.runtime.transcript(),
                    self.runtime.stderr_text(),
                    self.peer_node,
                    self.peer.command,
                    self.peer.transcript(),
                    self.peer.stderr_text(),
                )
            })
    }

    /// The single-payload assertion: the peer's `TX digest=…` and the runtime's
    /// `RX digest=…` are both present, both carry `expected_prefix` (so the leg really sent
    /// the payload it claims), and their digest terms are **equal**.
    fn assert_round_trip(&self, expected_prefix: &str) -> (String, String) {
        let tx = self.expect_peer("TX ");
        let rx = self.expect_runtime("RX ");
        assert_same_digest(
            &tx,
            &rx,
            &format!("TX digest={expected_prefix}"),
            &format!("RX digest={expected_prefix}"),
        );
        (tx.trim().to_string(), rx.trim().to_string())
    }

    /// Both VMs exit **0** on their own: the runtime once its fixture returns, the peer once
    /// its own bounded wait expires.
    fn assert_clean_exit(&mut self) {
        let status = self
            .runtime
            .wait_exit(EXIT_TIMEOUT)
            .unwrap_or_else(|| panic!("the runtime did not exit within {EXIT_TIMEOUT:?}"));
        assert_eq!(
            status.code(),
            Some(0),
            "the `run` action exits 0 once the fixture returns\n  stdout: {:?}\n  stderr: {}",
            self.runtime.transcript(),
            self.runtime.stderr_text(),
        );
        let status = self
            .peer
            .wait_exit(EXIT_TIMEOUT)
            .unwrap_or_else(|| panic!("the peer did not exit within {EXIT_TIMEOUT:?}"));
        assert_eq!(
            status.code(),
            Some(0),
            "the peer halted 0 on its own\n  stdout: {:?}\n  stderr: {}",
            self.peer.transcript(),
            self.peer.stderr_text(),
        );
    }
}

/// The part of a digest report line after `digest=`.
fn digest_of(line: &str) -> &str {
    line.split_once("digest=")
        .unwrap_or_else(|| panic!("`{line}` carries no `digest=` report"))
        .1
        .trim()
}

/// Asserts both report lines carry the expected shape and that their digest terms are
/// **byte-identical** — the sender's own copy and the receiver's delivered copy.
fn assert_same_digest(tx: &str, rx: &str, tx_prefix: &str, rx_prefix: &str) {
    let tx = tx.trim();
    let rx = rx.trim();
    assert!(
        tx.starts_with(tx_prefix),
        "the sender's report has the expected shape ({tx_prefix:?}): {tx:?}"
    );
    assert!(
        rx.starts_with(rx_prefix),
        "the receiver's report has the expected shape ({rx_prefix:?}): {rx:?}"
    );
    assert_eq!(
        digest_of(tx),
        digest_of(rx),
        "the delivered payload's digest equals the sender's: sender {tx:?}, receiver {rx:?}"
    );
}

// ---------------------------------------------------------------------------------------
// The binary legs
// ---------------------------------------------------------------------------------------

/// **1 KiB binary.** The smallest size past the receiver's 233-word nursery: the delivery
/// cannot be an on-heap copy, so the fragment path is what makes it arrive at all.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_1_kib_binary_is_delivered_intact() {
    let peer = binary_peer_eval("binary:copy(<<\"0123456789abcdef\">>, 64)");
    let Some(mut leg) = start_leg("kb1", "binary", &peer) else {
        return;
    };
    let (tx, rx) = leg.assert_round_trip("{1024,");
    eprintln!("1 KiB: {tx} | {rx}");
    leg.assert_clean_exit();
}

/// **2 KiB binary.** A second size, so a payload that happens to fit some threshold cannot
/// pass for a working fragment path.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_2_kib_binary_is_delivered_intact() {
    let peer = binary_peer_eval("binary:copy(<<\"0123456789abcdef\">>, 128)");
    let Some(mut leg) = start_leg("kb2", "binary", &peer) else {
        return;
    };
    let (tx, rx) = leg.assert_round_trip("{2048,");
    eprintln!("2 KiB: {tx} | {rx}");
    leg.assert_clean_exit();
}

/// **64 KiB binary.** Past `ERL_ONHEAP_BINARY_LIMIT`, so the delivered term is a *refc*
/// binary — an on-heap 3-word `BinRef` into an off-heap payload — and the digest proves the
/// payload bytes survived the fragment read, not only the reference.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_64_kib_binary_is_delivered_intact() {
    let peer = binary_peer_eval("binary:copy(<<\"0123456789abcdef\">>, 4096)");
    let Some(mut leg) = start_leg("kb64", "binary", &peer) else {
        return;
    };
    let (tx, rx) = leg.assert_round_trip("{65536,");
    eprintln!("64 KiB: {tx} | {rx}");
    leg.assert_clean_exit();
}

/// **1 MiB binary.** The leg the decode arena is sized for: before
/// `crate::dist::DIST_DISPATCH_HEAP_WORDS` was derived from the reassembly limit, a message
/// this large exhausted the arena and **retired the link** (`allocation failure: heap`), so
/// the payload vanished. It must genuinely arrive and digest equal.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_1_mib_binary_is_delivered_intact() {
    let peer = binary_peer_eval("binary:copy(<<\"0123456789abcdef\">>, 65536)");
    let Some(mut leg) = start_leg("mib1", "binary", &peer) else {
        return;
    };
    let (tx, rx) = leg.assert_round_trip("{1048576,");
    eprintln!("1 MiB: {tx} | {rx}");
    leg.assert_clean_exit();
}

// ---------------------------------------------------------------------------------------
// The list legs
// ---------------------------------------------------------------------------------------

/// **100-element integer list.** A list just past the nursery (200 cons-cell words): the
/// digest's `length`/`sum` components also prove the elements were neither truncated nor
/// replaced.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_100_element_integer_list_is_delivered_intact() {
    let peer = list_peer_eval("lists:seq(1, 100)");
    let Some(mut leg) = start_leg("list100", "list", &peer) else {
        return;
    };
    let (tx, rx) = leg.assert_round_trip("{100,5050,");
    eprintln!("100-element list: {tx} | {rx}");
    leg.assert_clean_exit();
}

/// **10 000-element integer list.** Two orders of magnitude past the nursery, read back
/// element for element through the `term_to_binary` MD5 (a single wrong, dropped or duplicated
/// element changes it).
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_10_000_element_integer_list_is_delivered_intact() {
    let peer = list_peer_eval("lists:seq(1, 10000)");
    let Some(mut leg) = start_leg("list10k", "list", &peer) else {
        return;
    };
    let (tx, rx) = leg.assert_round_trip("{10000,50005000,");
    eprintln!("10 000-element list: {tx} | {rx}");
    leg.assert_clean_exit();
}

// ---------------------------------------------------------------------------------------
// The burst leg
// ---------------------------------------------------------------------------------------

/// **Twenty separate 64-byte messages.** Many fragment-carrying deliveries rather than one,
/// which is where a mailbox that reuses or drops a fragment slot shows up.
///
/// The fixture verifies each payload **element by element** against
/// `list_to_binary(lists:duplicate(64, I))` and counts what it verified; the digest is over
/// the twenty payloads **in arrival order**, so a permuted burst fails it. The test asserts
/// `count=20`, `ok=true` and digest equality with the peer's own figure.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn a_20_message_burst_of_64_byte_binaries_is_delivered_intact() {
    let Some(mut leg) = start_leg("burst", "burst", PEER_BURST_EVAL) else {
        return;
    };
    let tx = leg.expect_peer("TX ");
    let rx = leg.expect_runtime("BURST ");
    assert_same_digest(
        &tx,
        &rx,
        "TX count=20 digest={1280,",
        "BURST count=20 ok=true digest={1280,",
    );
    eprintln!("burst: {} | {}", tx.trim(), rx.trim());
    leg.assert_clean_exit();
}

// ---------------------------------------------------------------------------------------
// The differential-corpus-shaped leg
// ---------------------------------------------------------------------------------------

/// **The `dist_extra` corpus shape, as a cross-runtime pair.** `tools/differential`'s
/// distribution group runs both sides from the *same* program (`CaseKind::Command` compares
/// each child's own stdout, so no case can carry a message between the compared runtimes):
/// a `-eval` client that pings, sends one payload and exits on its own, and a `-eval` peer
/// that registers a name, blocks in a bounded `receive`, and reports what arrived.
///
/// This leg keeps that shape — the same argv (`-noshell -sname … -eval …`), the same
/// `PEER-READY` handshake, the same `dxpeer` registered name, the same "client waits out its
/// own deadline then halts" rule — but pairs the runtime under test against the **system
/// `erl`**, so the payload genuinely crosses between the two implementations. The digests
/// must agree.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn the_differential_corpus_shape_delivers_a_payload_intact() {
    let Some(harness) = Harness::new(BANNER, "corpus_shape") else {
        return;
    };
    let rust_name = unique('r');
    let peer_name = unique('o');
    let cookie = unique_cookie();
    let rust_node = format!("{rust_name}@{}", harness.host);
    let peer_node = format!("{peer_name}@{}", harness.host);

    // The oracle peer first: it must have registered `dxpeer` before the client sends.
    let mut peer = harness
        .spawn_peer(&peer_name, &cookie, CORPUS_PEER_EVAL, &rust_node)
        .unwrap_or_else(|error| panic!("cannot start the system `erl` peer under setsid: {error}"));
    let Some(ready) = peer.wait_for(|line| line.starts_with("PEER-READY "), READY_TIMEOUT) else {
        panic!(
            "the oracle peer never announced `PEER-READY` within {READY_TIMEOUT:?}\n  command: \
             {}\n  stdout: {:?}\n  stderr: {}",
            peer.command,
            peer.transcript(),
            peer.stderr_text()
        )
    };
    assert_eq!(
        ready.trim(),
        format!("PEER-READY {peer_node}"),
        "the peer registered `dxpeer` on its own full node name"
    );

    // The runtime is the corpus's **client**: `-noshell -sname … -eval <script>`, exactly
    // the argv the corpus gives both sides.
    let client = CORPUS_CLIENT_EVAL.replace("@TARGET@", &peer_node);
    let mut runtime = harness
        .spawn_runtime_eval(&rust_name, &cookie, &client)
        .unwrap_or_else(|error| panic!("cannot start the runtime under setsid: {error}"));

    let pong = runtime
        .wait_for(|line| line.starts_with("PONG "), LINE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the runtime's `-eval` client never pinged the peer\n  runtime command: {}\n  \
                 runtime stdout: {:?}\n  runtime stderr: {}\n  peer command: {}\n  peer stdout: \
                 {:?}\n  peer stderr: {}",
                runtime.command,
                runtime.transcript(),
                runtime.stderr_text(),
                peer.command,
                peer.transcript(),
                peer.stderr_text(),
            )
        });
    assert_eq!(
        pong.trim(),
        "PONG pong",
        "the runtime resolved the peer through `epmd` and completed the handshake"
    );

    let tx = runtime
        .wait_for(|line| line.starts_with("TX "), LINE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the runtime never reported its `TX` digest\n  runtime stdout: {:?}\n  runtime \
                 stderr: {}\n  peer stdout: {:?}\n  peer stderr: {}",
                runtime.transcript(),
                runtime.stderr_text(),
                peer.transcript(),
                peer.stderr_text(),
            )
        });
    let rx = peer
        .wait_for(|line| line.starts_with("RX "), LINE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the peer never reported an `RX` digest\n  peer stdout: {:?}\n  peer stderr: \
                 {}\n  runtime stdout: {:?}\n  runtime stderr: {}",
                peer.transcript(),
                peer.stderr_text(),
                runtime.transcript(),
                runtime.stderr_text(),
            )
        });
    assert_same_digest(
        &tx,
        &rx,
        "TX digest={1024,",
        "RX digest={1024,",
    );
    eprintln!("corpus shape: {} | {}", tx.trim(), rx.trim());

    let status = runtime
        .wait_exit(EXIT_TIMEOUT)
        .unwrap_or_else(|| panic!("the runtime did not exit within {EXIT_TIMEOUT:?}"));
    assert_eq!(
        status.code(),
        Some(0),
        "the `-eval` client waited out its deadline and exited on its own\n  stdout: {:?}\n  \
         stderr: {}",
        runtime.transcript(),
        runtime.stderr_text(),
    );
    let status = peer
        .wait_exit(EXIT_TIMEOUT)
        .unwrap_or_else(|| panic!("the peer did not exit within {EXIT_TIMEOUT:?}"));
    assert_eq!(
        status.code(),
        Some(0),
        "the peer digested the payload and halted 0 on its own\n  stdout: {:?}\n  stderr: {}",
        peer.transcript(),
        peer.stderr_text(),
    );
}

// ---------------------------------------------------------------------------------------
// The oracle cross-check: two system-`erl` nodes at the same sizes
// ---------------------------------------------------------------------------------------

/// **The oracle's own behaviour at multi-megabyte sizes**, over the same private-`epmd`
/// harness: two live system-`erl` nodes, a 2 MiB binary and a 100 000-element integer list,
/// both digested on both sides.
///
/// This is what anchors the matrix's expectations: the sizes the runtime legs use are sizes
/// a real ERTS delivers without ceremony, so a runtime failure there is a runtime defect
/// rather than an unreasonable test.
#[test]
#[ignore = "two VMs: a system erl/erlc/epmd peer plus setsid/kill; run with --ignored --nocapture"]
fn the_two_erl_oracle_delivers_a_multi_megabyte_binary_and_a_100k_list() {
    let Some(harness) = Harness::new(BANNER, "two_erl") else {
        return;
    };
    let sink_name = unique('s');
    let client_name = unique('c');
    let cookie = unique_cookie();
    let sink_node = format!("{sink_name}@{}", harness.host);
    let client_node = format!("{client_name}@{}", harness.host);

    let mut sink = harness
        .spawn_peer(&sink_name, &cookie, TWO_ERL_SINK_EVAL, &sink_node)
        .unwrap_or_else(|error| panic!("cannot start the sink node under setsid: {error}"));
    let Some(ready) = sink.wait_for(|line| line.starts_with("READY "), READY_TIMEOUT) else {
        panic!(
            "the sink node never announced `READY` within {READY_TIMEOUT:?}\n  command: {}\n  \
             stdout: {:?}\n  stderr: {}",
            sink.command,
            sink.transcript(),
            sink.stderr_text()
        )
    };
    assert_eq!(
        ready.trim(),
        format!("READY {sink_node}"),
        "the sink node registered `matrix_sink` on its own full node name"
    );

    let mut client = harness
        .spawn_peer(&client_name, &cookie, TWO_ERL_CLIENT_EVAL, &sink_node)
        .unwrap_or_else(|error| panic!("cannot start the client node under setsid: {error}"));
    let pong = client
        .wait_for(|line| line.starts_with("PONG "), LINE_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "the client node never pinged the sink\n  client command: {}\n  client stdout: \
                 {:?}\n  client stderr: {}\n  sink stdout: {:?}\n  sink stderr: {}",
                client.command,
                client.transcript(),
                client.stderr_text(),
                sink.transcript(),
                sink.stderr_text(),
            )
        });
    assert_eq!(
        pong.trim(),
        "PONG pong",
        "the two oracle nodes really connected through the private `epmd` ({client_node} → \
         {sink_node})"
    );

    for (tx_prefix, rx_prefix, label) in [
        ("TX1 digest={2097152,", "RX1 digest={2097152,", "2 MiB binary"),
        (
            "TX2 digest={100000,5000050000,",
            "RX2 digest={100000,5000050000,",
            "100 000-element list",
        ),
    ] {
        let tx = client
            .wait_for(|line| line.starts_with(&tx_prefix[..3]), LINE_TIMEOUT)
            .unwrap_or_else(|| {
                panic!(
                    "the client never reported the {label}'s digest\n  client stdout: {:?}\n  \
                     client stderr: {}\n  sink stdout: {:?}\n  sink stderr: {}",
                    client.transcript(),
                    client.stderr_text(),
                    sink.transcript(),
                    sink.stderr_text(),
                )
            });
        let rx = sink
            .wait_for(|line| line.starts_with(&rx_prefix[..3]), LINE_TIMEOUT)
            .unwrap_or_else(|| {
                panic!(
                    "the sink never reported the {label}'s digest\n  sink stdout: {:?}\n  sink \
                     stderr: {}\n  client stdout: {:?}\n  client stderr: {}",
                    sink.transcript(),
                    sink.stderr_text(),
                    client.transcript(),
                    client.stderr_text(),
                )
            });
        assert_same_digest(&tx, &rx, tx_prefix, rx_prefix);
        eprintln!("oracle {label}: {} | {}", tx.trim(), rx.trim());
    }

    let status = sink
        .wait_exit(EXIT_TIMEOUT)
        .unwrap_or_else(|| panic!("the sink node did not exit within {EXIT_TIMEOUT:?}"));
    assert_eq!(
        status.code(),
        Some(0),
        "the sink digested both payloads and halted 0\n  stdout: {:?}\n  stderr: {}",
        sink.transcript(),
        sink.stderr_text(),
    );
    let status = client
        .wait_exit(EXIT_TIMEOUT)
        .unwrap_or_else(|| panic!("the client node did not exit within {EXIT_TIMEOUT:?}"));
    assert_eq!(
        status.code(),
        Some(0),
        "the client halted 0 on its own\n  stdout: {:?}\n  stderr: {}",
        client.transcript(),
        client.stderr_text(),
    );
}
