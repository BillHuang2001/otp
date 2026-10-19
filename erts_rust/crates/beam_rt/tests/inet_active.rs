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
//! The **`{active, …}` socket-input channel**, end to end over real loopback traffic
//! (contract A.4).
//!
//! `src/port/active_input.rs` turns a socket driver's active-mode read into a message the
//! owning process receives — `{tcp, S, Data}` / `{udp, S, IP, Port, Data}` for a delivery and
//! `{tcp_passive, S}` / `{udp_passive, S}` for an exhausted grant — with the port's **effective**
//! data mode (`PortRegistry::data_mode`) deciding whether `Data` is a binary or a charlist. The
//! colocated unit suite (`src/port/active_input_e2e_tests.rs`) pins that rendering against the
//! oracle; *this* file pins the whole path through the real `beam_rt` executable: a `.beam`
//! fixture parks in a compiled `receive` for the socket message, the runtime's scheduler pass
//! drains the driver's active-input queue (`Runtime::deliver_port_messages`), the message lands
//! in the process's mailbox and `resume-on-send` re-enqueues it — so the value the fixture gets
//! back is the one a peer actually wrote over a loopback socket.
//!
//! # Why the `run` route, not `-eval`
//!
//! A socket in `{active, …}` mode blocks its owner in an ordinary `receive` until the driver
//! delivers. Under `-eval` the evaluator is a tree-walker with no scheduler behind it, so a
//! parked receive folds into the typed capability gap (`unsupported: opcode: run: process
//! blocked`, exit 3) and nothing can resume it. Every case below therefore runs through
//! `beam_rt run <fixture.beam> inet_active:<case>/0`, which runs the module as a **scheduled**
//! process (the convention `tests/port_wake.rs` and `tests/inet_completion.rs` established).
//!
//! # The transport: the driver directly, exactly as `prim_inet` drives it
//!
//! Every socket is `erlang:open_port({spawn_driver, "tcp_inet" | "udp_inet"}, [binary])` plus
//! **numeric** `erlang:port_control/3` requests — `INET_REQ_OPEN` 1, `INET_REQ_CONNECT` 3,
//! `INET_REQ_BIND` 6, `INET_REQ_SETOPTS` 7, `INET_REQ_ACCEPT` 26, `INET_REQ_LISTEN` 27, each
//! offset by `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` = `0x03f1a300`. That is precisely what
//! `prim_inet:open/5`/`setopts/3`/`accept/1`/`send/3` do underneath, so the *identical* fixture
//! source runs under both runtimes with no `gen_tcp`/`gen_udp`/`prim_inet` in the picture, and
//! no library-level rewriting can differ between the two sides. Option records are
//! `[code, value_be32]`, with the trailing 16-bit counter **only** for `{active, N}`
//! (`prim_inet:enc_opt_val/2`), and a send is `prim_inet:send/3`'s own wire shape — a 16-bit
//! tagged caller prefix (the `term_to_binary` image of a reference) followed by the payload, the
//! tag being echoed back in `{inet_reply, S, ok, Tag}`.
//!
//! Each case is **self-contained in one VM**: the fixture binds a listener on `127.0.0.1:0`,
//! reads the kernel-assigned port out of the `bind` reply (`[1] ++ <<Port:16/big>>`), and opens
//! its **own peer** socket in the same run — so there is no external peer to coordinate, and the
//! bytes the owner receives are bytes a real socket carried.
//!
//! # The cases, and the oracle's own rendering of each
//!
//! Every row was measured on this host (OTP 29.1.1 / erts-17.1). `'$port'` stands for the port
//! term — a per-runtime table slot, so the fixture normalises it — and `'$source'` for a UDP
//! source port, which is per-run; **everything else is compared byte for byte**, and the last
//! line (`ok`) is the `run` action's printed result on one side and the fixture's own return
//! value on the other.
//!
//! | Test | Fixture | Shape it pins | The oracle's rendering |
//! |---|---|---|---|
//! | [`active_true_delivers_every_peer_write_as_a_tcp_message`] | `t1` | `{active,true}` `{mode,binary}`: every write becomes a `{tcp, S, Data}` | `t1_first={tcp,'$port',<<104,101,108,108,111>>}` then `t1_second={tcp,'$port',<<97,103,97,105,110>>}` |
//! | [`active_n_delivers_n_then_exactly_one_passive`] | `t2` | `{active,2}`: two deliveries, one `{tcp_passive, S}`, silence, then a re-armed delivery | `t2_passive={tcp_passive,'$port'}`, `t2_silent=timeout`, `t2_rearmed={tcp,'$port',<<116,104,114,101,101>>}` |
//! | [`active_once_delivers_one_then_needs_a_rearm`] | `t3` | `{active,once}`: silence after the spent grant, a re-arm, one more delivery | `t3_silent=timeout`, `t3_second={tcp,'$port',<<98,98,98,98>>}`, `t3_silent_again=timeout` |
//! | [`mode_list_and_mode_binary_render_the_same_payload_differently`] | `t4` | `{mode,binary}` ⇒ binary, `{mode,list}` ⇒ charlist for the same `<<"hello">>` | `t4_binary={tcp,'$port',<<104,101,108,108,111>>}` / `t4_list={tcp,'$port',[104,101,108,108,111]}` |
//! | [`packet_2_strips_the_framing_header_before_delivery`] | `t5` | `{packet,2}`: the peer's own header is stripped | `t5_stripped={tcp,'$port',<<104,101,108,108,111>>}` from a `<<5:16,"hello">>` write |
//! | [`udp_active_delivers_the_datagram_with_its_real_source`] | `t6` | `{udp, S, {127,0,0,1}, Port, Data}` with the peer's real bound source port | `t6_datagram={udp,'$port',{127,0,0,1},'$source',<<104,105>>}`, `t6_source_is_real=true` |
//! | [`an_accepted_socket_inherits_its_listeners_options`] | `t7` | an accepted socket inherits `{packet,2}` + `{mode,list}` (not `{active,_}`) and delivers charlists with the header stripped | `t7_first={tcp,'$port',[104,101,108,108,111]}`, `t7_passive={tcp_passive,'$port'}` |
//!
//! # The one measured difference this suite does not depend on
//!
//! This runtime harvests the bytes that arrive in a **passive window** out of the socket when it
//! emits `{tcp_passive, S}`, and they are never delivered; the oracle keeps them queued and hands
//! them over after the next re-arm, so a peer write made *while* the grant is spent appears as
//! `<<"threethree">>` there and `<<"three">>` here. That is `src/port/active_input_e2e_tests.rs`'s
//! recorded fidelity note, not something this suite may paper over — so `t2` deliberately writes
//! **nothing** during its passive window (its `t2_silent=timeout` is the absence of a delivery,
//! not the absence of a write) and `t3`'s spent-grant write is never read back. Both runtimes
//! agree on every line as a result. Closing the gap belongs to the port provider, not here.
//!
//! # Bounds and skips
//!
//! Nothing can hang: every child is run through [`support::run_bounded`] (a 30 s poll/kill
//! bound; the oracle is always given `-s init stop`), the fixtures' own waits are 5 s for a
//! delivery and 500 ms for a silence, and a run that outlives the bound is a **failure** with its
//! captured output, never a skip. A missing `erlc` or `erl` is a **loud skip**
//! ([`support::skip_loudly`]), never a silent pass. `tests/fixtures/` is never named on a `-pa`
//! (it holds a `lists.beam` that would shadow stdlib): each case compiles the fixture into its
//! own scratch directory, and only that directory is passed to the oracle.

#![deny(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

// The shared harness is compiled into every test target; this one uses a subset of it, so the
// unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

use support::{find_on_path, require_oracle, run_bounded, skip_loudly};

/// The loud-skip banner: a missing toolchain is reported under this suite's name.
const BANNER: &str = "inet_active";

/// The built runtime — the `run` action is what this suite drives.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The committed fixture (`tests/fixtures/inet_active.erl`, module `inet_active`), compiled at
/// test time with the system `erlc`. It exports `t1/0` … `t7/0`, one function per case.
const FIXTURE: &str = include_str!("fixtures/inet_active.erl");

/// The module the fixture defines (`-module(inet_active).`).
const MODULE: &str = "inet_active";

// ---------------------------------------------------------------------------------------------
// A scratch directory
// ---------------------------------------------------------------------------------------------

/// A per-case, self-removing scratch directory.
///
/// The name carries the label, the pid, a per-process nonce and a per-call sequence number, so
/// two cases (or two concurrently running test *processes*, or a leftover directory from an
/// earlier run) can never share a path — the rule the crate's own `context::test_support::
/// scratch_dir` follows, re-stated here because that helper is `pub(crate)`.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// The directory this guard owns.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Creates a fresh scratch directory for `label`.
fn scratch_dir(label: &str) -> Scratch {
    static NONCE: OnceLock<u64> = OnceLock::new();
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let nonce = *NONCE.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        nanos ^ (u64::from(std::process::id()) << 32)
    });
    let sequence = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "beamrt_inet_active_{label}_{}_{nonce:016x}_{sequence}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap_or_else(|error| {
        panic!(
            "cannot create the scratch directory {}: {error}",
            path.display()
        )
    });
    Scratch { path }
}

// ---------------------------------------------------------------------------------------------
// Toolchain and fixture compilation
// ---------------------------------------------------------------------------------------------

/// The system `erlc` — the fixture is an `.erl` source, so a missing compiler is a loud skip.
fn require_erlc() -> Option<PathBuf> {
    match find_on_path("erlc") {
        Some(erlc) => Some(erlc),
        None => {
            skip_loudly(BANNER, "no `erlc` on $PATH (the fixture cannot be compiled)");
            None
        }
    }
}

/// Writes the fixture into `scratch` under its module name and compiles it there, returning the
/// `.beam`'s path.
///
/// The committed fixture is named after the *suite* (`inet_active.erl`) and defines the module
/// `inet_active`; it is nevertheless materialised under `<module>.erl` because `erlc` requires the
/// file name to match the module name. The output goes to the scratch directory, and nothing ever
/// names `tests/fixtures/` on a `-pa`/`-o` — it holds a `lists.beam` that would shadow stdlib.
/// A compiler failure is a **failure** (the fixture is part of this repository), never a skip.
fn compile_fixture(erlc: &Path, scratch: &Scratch) -> PathBuf {
    let source_path = scratch.path().join(format!("{MODULE}.erl"));
    std::fs::write(&source_path, FIXTURE).unwrap_or_else(|error| {
        panic!("cannot write {}: {error}", source_path.display())
    });
    let out_dir = scratch.path().to_string_lossy().into_owned();
    let source_arg = source_path.to_string_lossy().into_owned();
    let Some(output) = run_bounded(erlc, &["-o", &out_dir, &source_arg]) else {
        panic!(
            "`erlc` could not compile {} (bounded probe)",
            source_path.display()
        );
    };
    assert!(
        output.status.success(),
        "`erlc` failed on {} with {:?}:\n{}",
        source_path.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "`erlc` warned about {} (the committed fixture must compile silently):\n{}",
        source_path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let beam = scratch.path().join(format!("{MODULE}.beam"));
    assert!(beam.is_file(), "`erlc` produced no {}", beam.display());
    beam
}

// ---------------------------------------------------------------------------------------------
// One case, run under both runtimes
// ---------------------------------------------------------------------------------------------

/// The two runs of one case: this runtime's trimmed stdout and the oracle's.
struct CaseOutput {
    /// The fixture's name (`t1` … `t7`).
    function: String,
    /// `beam_rt run <beam> inet_active:<function>/0`
    rust: Vec<String>,
    /// `erl -noshell -pa <scratch> -eval 'io:format("~w~n",[inet_active:<function>()])' -s init stop`
    oracle: Vec<String>,
}

impl CaseOutput {
    /// The `rust` lines, after asserting the oracle printed exactly the same ones.
    ///
    /// Both sides are non-empty by construction (`ok` is always the last line), but the check is
    /// stated anyway: a case that compared two empty vectors would report a pass that pinned
    /// nothing.
    fn assert_agrees(&self) {
        assert!(
            !self.rust.is_empty() && !self.oracle.is_empty(),
            "case {} produced no output at all (rust {} line(s), oracle {} line(s))",
            self.function,
            self.rust.len(),
            self.oracle.len()
        );
        assert_eq!(
            self.rust, self.oracle,
            "case {}: this runtime's output differs from the oracle's",
            self.function
        );
        assert_eq!(
            self.rust.last().map(String::as_str),
            Some("ok"),
            "case {}: the fixture must finish by returning `ok`\n{:#?}",
            self.function,
            self.rust
        );
    }
}

/// Compiles the fixture into a per-case scratch directory and runs case `function` under **both**
/// runtimes, returning their trimmed stdout lines.
///
/// `None` means the host cannot host the case (no `erlc` or no `erl`): the skip has already been
/// printed loudly. Everything else — a compiler failure, a non-zero exit, a run that outlives the
/// bound — panics with the captured output, because those are defects, not absences.
fn run_case(function: &str) -> Option<CaseOutput> {
    let erlc = require_erlc()?;
    let oracle = require_oracle(BANNER)?;
    let scratch = scratch_dir(function);
    let beam = compile_fixture(&erlc, &scratch);

    let beam_arg = beam.to_string_lossy().into_owned();
    let mfa = format!("{MODULE}:{function}/0");
    let rust = run_bounded(
        Path::new(BINARY),
        &["run", beam_arg.as_str(), mfa.as_str()],
    )
    .unwrap_or_else(|| {
        panic!(
            "case {function}: `beam_rt run` outlived the {}s probe bound",
            support::PROBE_TIMEOUT.as_secs()
        )
    });
    assert!(
        rust.status.success(),
        "case {function}: `beam_rt run` exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        rust.status.code(),
        String::from_utf8_lossy(&rust.stdout),
        String::from_utf8_lossy(&rust.stderr)
    );

    // The oracle is driven with the same fixture and the same `~w` rendering of the result, so the
    // `run` action's trailing `ok` line has a counterpart on both sides. `-pa` goes *before*
    // `-eval` (erl evaluates command-line switches in order) and `-s init stop` makes the node
    // shut down whatever the case does.
    let dir_arg = scratch.path().to_string_lossy().into_owned();
    let expr = format!("io:format(\"~w~n\",[{MODULE}:{function}()])");
    let oracle_output = run_bounded(
        &oracle,
        &[
            "-noshell",
            "-pa",
            dir_arg.as_str(),
            "-eval",
            expr.as_str(),
            "-s",
            "init",
            "stop",
        ],
    )
    .unwrap_or_else(|| {
        panic!(
            "case {function}: the oracle outlived the {}s probe bound",
            support::PROBE_TIMEOUT.as_secs()
        )
    });
    assert!(
        oracle_output.status.success(),
        "case {function}: the oracle exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        oracle_output.status.code(),
        String::from_utf8_lossy(&oracle_output.stdout),
        String::from_utf8_lossy(&oracle_output.stderr)
    );

    Some(CaseOutput {
        function: function.to_string(),
        rust: trimmed_lines(&rust.stdout),
        oracle: trimmed_lines(&oracle_output.stdout),
    })
}

/// `bytes` as trimmed, non-empty lines — the comparison unit for both sides.
fn trimmed_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Printed-line assertions
// ---------------------------------------------------------------------------------------------

/// The value of the `<prefix><value>` line in `lines`, or a panic naming the whole output.
///
/// Each printed line of the fixture is `prefix=value` with a unique prefix, so "the value this
/// runtime printed for this observation" is unambiguous.
fn value_of<'a>(lines: &'a [String], prefix: &str) -> &'a str {
    let mut found: Option<&str> = None;
    for line in lines {
        if let Some(value) = line.strip_prefix(prefix) {
            assert!(
                found.is_none(),
                "the fixture printed {prefix:?} more than once\n{lines:#?}"
            );
            found = Some(value);
        }
    }
    match found {
        Some(value) => value,
        None => panic!("the fixture printed no {prefix:?} line\n{lines:#?}"),
    }
}

/// Asserts this runtime printed `prefix` with exactly `want` — the content pin that makes a case
/// fail on an empty, missing or substituted result (never a pass over nothing).
fn expect(lines: &[String], prefix: &str, want: &str) {
    assert_eq!(
        value_of(lines, prefix),
        want,
        "the {prefix:?} observation is not the oracle's own rendering\n{lines:#?}"
    );
}

// ---------------------------------------------------------------------------------------------
// 1. {active, true}
// ---------------------------------------------------------------------------------------------

/// `{active, true}` on a real TCP socket: **every** byte the peer writes is delivered to the owner
/// as `{tcp, S, Data}`, continuously, with no re-arm in between.
///
/// The oracle prints `t1_first={tcp,'$port',<<104,101,108,108,111>>}` for the peer's `<<"hello">>`
/// and `t1_second={tcp,'$port',<<97,103,97,105,110>>}` for `<<"again">>`; the payload predicates
/// (`t1_first_is_hello`, `t1_second_is_again`) and `t1_socket_is_port` are printed by the fixture
/// so the case cannot pass on any other message (or on the atom `timeout`).
#[test]
fn active_true_delivers_every_peer_write_as_a_tcp_message() {
    let Some(case) = run_case("t1") else { return };
    expect(
        &case.rust,
        "t1_first=",
        "{tcp,'$port',<<104,101,108,108,111>>}",
    );
    expect(&case.rust, "t1_socket_is_port=", "true");
    expect(&case.rust, "t1_first_is_hello=", "true");
    expect(
        &case.rust,
        "t1_second=",
        "{tcp,'$port',<<97,103,97,105,110>>}",
    );
    expect(&case.rust, "t1_second_is_again=", "true");
    case.assert_agrees();
}

// ---------------------------------------------------------------------------------------------
// 2. {active, N}
// ---------------------------------------------------------------------------------------------

/// `{active, 2}`: exactly **two** deliveries, then exactly **one** `{tcp_passive, S}` at
/// exhaustion, then silence until a re-arm, and a delivery after it.
///
/// The re-arm is `{active, true}` and nothing is written during the passive window (see the module
/// docs: this runtime harvests those bytes, the oracle keeps them, so writing there would make the
/// two sides legitimately differ). `t2_silent=timeout` is therefore the pinned state — a socket
/// still granting deliveries would have delivered the peer's next write without one.
#[test]
fn active_n_delivers_n_then_exactly_one_passive() {
    let Some(case) = run_case("t2") else { return };
    expect(&case.rust, "t2_one=", "{tcp,'$port',<<111,110,101>>}");
    expect(&case.rust, "t2_two=", "{tcp,'$port',<<116,119,111>>}");
    expect(&case.rust, "t2_passive=", "{tcp_passive,'$port'}");
    expect(&case.rust, "t2_silent=", "timeout");
    expect(
        &case.rust,
        "t2_rearmed=",
        "{tcp,'$port',<<116,104,114,101,101>>}",
    );
    // Exactly one passive notification: the prefix appears once (a second one would also fail
    // [`value_of`], which rejects a repeated observation).
    assert_eq!(
        case.rust
            .iter()
            .filter(|line| line.starts_with("t2_passive="))
            .count(),
        1,
        "case t2 must deliver exactly one {{tcp_passive,S}}\n{:#?}",
        case.rust
    );
    case.assert_agrees();
}

// ---------------------------------------------------------------------------------------------
// 3. {active, once}
// ---------------------------------------------------------------------------------------------

/// `{active, once}`: one delivery, then **silence** (the peer's next write must not be delivered
/// while the grant is spent), then a `setopts` re-arm and a second delivery, then silence again.
#[test]
fn active_once_delivers_one_then_needs_a_rearm() {
    let Some(case) = run_case("t3") else { return };
    expect(
        &case.rust,
        "t3_first=",
        "{tcp,'$port',<<104,101,108,108,111>>}",
    );
    expect(&case.rust, "t3_silent=", "timeout");
    expect(&case.rust, "t3_second=", "{tcp,'$port',<<98,98,98,98>>}");
    expect(&case.rust, "t3_silent_again=", "timeout");
    case.assert_agrees();
}

// ---------------------------------------------------------------------------------------------
// 4. {mode, list} vs {mode, binary}
// ---------------------------------------------------------------------------------------------

/// The same payload (`<<"hello">>`) under the two data modes: `{mode, binary}` renders a binary and
/// `{mode, list}` a **charlist**, on sockets that are otherwise identical.
///
/// `{mode,binary}` is set explicitly in the binary half because the driver's effective default for
/// a `[binary]` port is a *list* (measured): the fixture asking for it is what makes the two
/// renderings a property of the option rather than of the open flags.
#[test]
fn mode_list_and_mode_binary_render_the_same_payload_differently() {
    let Some(case) = run_case("t4") else { return };
    expect(
        &case.rust,
        "t4_binary=",
        "{tcp,'$port',<<104,101,108,108,111>>}",
    );
    expect(&case.rust, "t4_binary_is_binary=", "true");
    expect(&case.rust, "t4_list=", "{tcp,'$port',[104,101,108,108,111]}");
    expect(&case.rust, "t4_list_is_list=", "true");
    expect(&case.rust, "t4_list_is_hello=", "true");
    case.assert_agrees();
}

// ---------------------------------------------------------------------------------------------
// 5. {packet, 2}
// ---------------------------------------------------------------------------------------------

/// `{packet, 2}`: the peer writes its **own** 16-bit length header (`<<5:16,"hello">>`) and the
/// owner receives the body alone — the framing is stripped before the message is built.
#[test]
fn packet_2_strips_the_framing_header_before_delivery() {
    let Some(case) = run_case("t5") else { return };
    expect(
        &case.rust,
        "t5_stripped=",
        "{tcp,'$port',<<104,101,108,108,111>>}",
    );
    expect(&case.rust, "t5_body_is_hello=", "true");
    case.assert_agrees();
}

// ---------------------------------------------------------------------------------------------
// 6. UDP
// ---------------------------------------------------------------------------------------------

/// One datagram over real loopback UDP: `{udp, S, {127,0,0,1}, SourcePort, Data}`.
///
/// The source port is the peer's own bound port — masked as `'$source'` for the comparison, and
/// asserted here as a real positive integer *and* as exactly the peer's port, so the message can
/// only be the one the peer sent.
#[test]
fn udp_active_delivers_the_datagram_with_its_real_source() {
    let Some(case) = run_case("t6") else { return };
    expect(
        &case.rust,
        "t6_datagram=",
        "{udp,'$port',{127,0,0,1},'$source',<<104,105>>}",
    );
    expect(&case.rust, "t6_source_is_peer=", "true");
    expect(&case.rust, "t6_source_is_real=", "true");
    expect(&case.rust, "t6_address=", "{127,0,0,1}");
    case.assert_agrees();
}

// ---------------------------------------------------------------------------------------------
// 7. An accepted socket
// ---------------------------------------------------------------------------------------------

/// An **accepted** socket delivers through the same channel, and it inherits its listener's option
/// block: the listener is configured `{packet, 2}` + `{mode, list}` + `{active, 2}`, and the
/// accepted port's own re-armed `{active, 2}` (the C copies everything **except** the active grant)
/// then delivers **charlists** with the framing header stripped, plus its own `{tcp_passive, S}`
/// at the listener's counter.
///
/// This is the case that proves the option inheritance reaches an accepted descriptor in a real
/// run (`PortRegistry::port_control`'s `note_inet_setopts` on the driver's accepted reply): a
/// socket that had *not* inherited `{packet,2}`/`{mode,list}` would deliver
/// `<<0,5,104,101,108,108,111>>` here.
#[test]
fn an_accepted_socket_inherits_its_listeners_options() {
    let Some(case) = run_case("t7") else { return };
    expect(&case.rust, "t7_accepted_is_port=", "true");
    expect(
        &case.rust,
        "t7_first=",
        "{tcp,'$port',[104,101,108,108,111]}",
    );
    expect(&case.rust, "t7_first_is_charlist=", "true");
    expect(&case.rust, "t7_second=", "{tcp,'$port',[119,111,114,108,100]}");
    expect(&case.rust, "t7_second_is_world=", "true");
    expect(&case.rust, "t7_passive=", "{tcp_passive,'$port'}");
    case.assert_agrees();
}
