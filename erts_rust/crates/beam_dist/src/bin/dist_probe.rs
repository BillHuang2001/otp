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

#![deny(unsafe_code)]

//! `dist_probe` — a tiny companion CLI for the differential-test harness
//! (`tools/differential`, a separate crate).
//!
//! Contract A.5 (the distribution wire) can only be driven through `beam_rt`
//! today, and `beam_rt` has no connector / `net_kernel` half, so a live
//! two-node loopback is not exercisable through the runtime. This helper exposes
//! the hermetic loopback already proven by `tests/two_node_loopback.rs` as a
//! standalone command whose stdout is a **host-independent observable**, so the
//! harness can compare it against the system-`erl` oracle.
//!
//! ```text
//! dist_probe loopback <report>
//! ```
//!
//! The helper stands up two in-process [`beam_dist`] endpoints (`alpha@localhost`
//! = acceptor, `beta@localhost` = connector) over a real loopback TCP socket,
//! completes the OTP-23 `'N'` handshake both ways, and derives the report from the
//! observed state. Every report prints **exactly one** deterministic, host-independent
//! line to stdout (no timestamps, addresses or ports) and exits `0`:
//!
//! | `<report>` | stdout (one line) |
//! |---|---|
//! | `ping` | `pong` iff both handshakes completed (else `pang`) |
//! | `nodes` | the number of peers node A handshook with (`1`) |
//! | `term` | uppercase hex of the ETF image of the delivered `{1,2,3}` (`836803610161026103`) |
//! | `nodeid` | the negotiated `DFLAG_*` word plus A's peer name / creation / visibility |
//! | `handshake` | a **pinned**, byte-exact transcript of the `'N'` handshake (both directions) |
//! | `badcookie` | the observable shape of a handshake refused because the cookie was wrong |
//! | `frag` | length + checksum of a > [`DIST_FRAGMENT_SIZE`] payload round trip, plus the number of `'E'`/`'F'` frames really written |
//! | `alias` | the decoded `DOP_ALIAS_SEND` fields |
//! | `monitor` | the decoded `DOP_MONITOR_P` / `DOP_LINK` / `DOP_EXIT` fields |
//! | `regsend_missing` | the local outcome of a `DOP_REG_SEND` to an unregistered name |
//! | `utf8` | uppercase hex of the ETF image of a non-Latin-1 atom that crossed the wire |
//!
//! `ping` / `nodes` / `term` additionally perform the mutual `{1,2,3}` round trip
//! (A → B name-addressed as a `DOP_REG_SEND`, B echoes it to A's pid as a
//! `DOP_SEND(_SENDER)`, A receives it) and derive their observable from it.
//!
//! An unknown or missing subcommand prints a usage line on stderr and exits `2`. If
//! a report genuinely cannot be produced, `dist_probe: unsupported: <reason>` is
//! printed on stderr and the process exits `3` — the harness's capability-gap
//! convention. A fabricated line is never printed.

use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use beam_alloc::Heap;
use beam_codec::VERSION_MAGIC;
use beam_common::{BeamError, Result};
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::flags::DFLAG_FRAGMENTS;
use beam_dist::transport::{Read, Transport, Write};
use beam_dist::{
    connect_client_with_creation, Connection, Delivery, DeliveryOutcome, DistFlags, Dop,
    InboundSink, PeerVisibility, TcpListener, TcpTransport, DIST_FRAGMENT_SIZE,
};
use beam_term::prelude::{atom_val, is_atom, is_small, make_atom, make_small, signed_value};
use beam_term::{AtomTable, Eterm};

/// The shared cookie both endpoints authenticate with.
const COOKIE: &[u8] = b"dist-probe-cookie";
/// The cookie the `badcookie` report's connector deliberately presents instead.
const WRONG_COOKIE: &[u8] = b"someone-elses-cookie";
/// Node A — the accepting side (the distribution **server** role).
const ALPHA_NAME: &[u8] = b"alpha@localhost";
/// Node A's creation.
const ALPHA_CREATION: u32 = 1;
/// Node B — the connecting side (the distribution **client** role).
const BETA_NAME: &[u8] = b"beta@localhost";
/// Node B's creation.
const BETA_CREATION: u32 = 2;
/// A stand-in for node A's own pid (a small int — the value layer treats pids
/// opaquely, exactly as `tests/two_node_loopback.rs` does).
const ALPHA_PID: i64 = 1001;
/// A stand-in for node B's own pid.
const BETA_PID: i64 = 2002;
/// The registered name node B's message is addressed to.
const ECHO_NAME: &str = "echo";
/// The registered name the `regsend_missing` report addresses — never registered.
const MISSING_NAME: &str = "missing";
/// A finite read timeout, so a broken link fails instead of hanging.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The per-endpoint heap size in words.
const HEAP_WORDS: usize = 1 << 16;
/// The per-endpoint heap size for the `frag` report (it holds a >64 KiB binary).
const FRAG_HEAP_WORDS: usize = 1 << 17;
/// The `frag` payload length in bytes — deliberately **above** the fragment ceiling.
const FRAG_BYTES: usize = 200_000;
/// The alias id the `alias` report addresses (`DOP_ALIAS_SEND`'s `To`).
const ALIAS_ID: i64 = 3003;
/// The message value the `alias` report sends.
const ALIAS_MESSAGE: i64 = 42;
/// The local pid the `monitor` report's `DOP_MONITOR_P` watches.
const MONITORED_PID: i64 = 5005;
/// The monitor reference the `monitor` report uses.
const MONITOR_REF: i64 = 9001;
/// The local pid the `monitor` report's `DOP_LINK` links.
const LINKED_PID: i64 = 5006;
/// The local pid the `monitor` report's `DOP_EXIT` signals.
const EXITED_PID: i64 = 5007;
/// The exit reason the `monitor` report's `DOP_EXIT` carries.
const EXIT_REASON: i64 = 3003;
/// The atom name the `utf8` report sends — outside Latin-1 (U+03A9, 2 UTF-8 bytes).
const UNICODE_ATOM: &str = "Ω";
/// Pinned handshake nonce for the `handshake` report's **acceptor**.
///
/// The challenge is random by default ([`beam_dist::gen_challenge`]), so a raw
/// transcript would differ on every run; the `*_with_challenge` seams pin it.
const ALPHA_CHALLENGE: u32 = 0x0BAD_C0DE;
/// Pinned handshake nonce for the `handshake` report's **connector**.
const BETA_CHALLENGE: u32 = 0x600D_F00D;
/// The accepted report kinds (`loopback <report>`).
const REPORT_KINDS: [&str; 11] = [
    "ping",
    "nodes",
    "term",
    "badcookie",
    "nodeid",
    "frag",
    "alias",
    "monitor",
    "regsend_missing",
    "handshake",
    "utf8",
];

/// Guard: the `frag` payload must actually cross the fragmentation ceiling, or
/// `Connection::send_payload` writes one oversized packet instead of exercising
/// the `'E'`/`'F'` path.
const _: () = assert!(FRAG_BYTES > DIST_FRAGMENT_SIZE);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let report = match args.as_slice() {
        [sub, report] if sub == "loopback" && REPORT_KINDS.contains(&report.as_str()) => {
            report.as_str()
        }
        _ => {
            eprintln!("usage: dist_probe loopback <{}>", REPORT_KINDS.join("|"));
            return ExitCode::from(2);
        }
    };

    match probe(report) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("dist_probe: unsupported: {reason}");
            ExitCode::from(3)
        }
    }
}

/// Runs the loopback the requested report needs and derives its one-line
/// observable from the observed state.
fn probe(report: &str) -> Result<String> {
    match report {
        // The three original reports all drive the full `{1,2,3}` round trip.
        "ping" | "nodes" | "term" => probe_exchange(report),
        "nodeid" => probe_nodeid(),
        "handshake" => probe_handshake(),
        "badcookie" => probe_badcookie(),
        "frag" => probe_frag(),
        "alias" => probe_alias(),
        "monitor" => probe_monitor(),
        "regsend_missing" => probe_regsend_missing(),
        "utf8" => probe_utf8(),
        // `main` validated the report before calling in.
        _ => Err(BeamError::BadArg),
    }
}

/// The `ping` / `nodes` / `term` reports: handshake + the mutual `{1,2,3}` round
/// trip, then derive the observable from the observed state.
fn probe_exchange(report: &str) -> Result<String> {
    let Pair {
        mut alpha,
        mut beta,
    } = connect_pair()?;

    // (2) Both ends learned the other's name and creation from the `'N'`
    // handshake — node A completed it with exactly one peer (`beta`).
    let alpha_done = alpha.peer_name() == BETA_NAME && alpha.peer_creation() == BETA_CREATION;
    let beta_done = beta.peer_name() == ALPHA_NAME && beta.peer_creation() == ALPHA_CREATION;

    // (3) Deliver a compound term: A → B name-addressed, B echoes it to A's pid.
    let mut alpha_atoms = AtomTable::new();
    let mut beta_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(HEAP_WORDS)?;
    let mut beta_heap = Heap::new(HEAP_WORDS)?;

    let payload = alpha_heap.tuple(&[make_small(1), make_small(2), make_small(3)])?;
    alpha.send_reg_send(
        make_small(ALPHA_PID),
        make_atom(alpha_atoms.intern("")),
        make_atom(alpha_atoms.intern(ECHO_NAME)),
        payload,
        &alpha_atoms,
    )?;

    let delivery = beta.recv_delivery(&mut beta_heap, &mut beta_atoms)?;
    match delivery {
        Delivery::RegSend { from, message, .. } => {
            // B answers A's (external) pid with the message it just decoded.
            beta.send_message(make_small(BETA_PID), from, message, &beta_atoms)?;
        }
        other => {
            return Err(unexpected("a name-addressed RegSend", &other));
        }
    }

    let reply = alpha.recv_delivery(&mut alpha_heap, &mut alpha_atoms)?;
    let received = match reply {
        Delivery::Send { message, .. } => message,
        other => return Err(unexpected("a pid-addressed send", &other)),
    };

    Ok(match report {
        "ping" => if alpha_done && beta_done { "pong" } else { "pang" }.to_string(),
        "nodes" => if alpha_done { "1" } else { "0" }.to_string(),
        "term" => {
            // The ETF image of the *received* term, `VERSION_MAGIC` prepended.
            let mut image = Vec::with_capacity(1 + 7);
            image.push(VERSION_MAGIC);
            image.extend_from_slice(&beam_codec::encode_term_body(received, &alpha_atoms)?);
            hex_upper(&image)
        }
        // `probe` routed only the three exchange reports here.
        _ => return Err(BeamError::BadArg),
    })
}

/// `nodeid` — the negotiated capability word plus node A's view of its peer.
///
/// Both sides advertise [`beam_dist::preferred_flags`], so `alpha.negotiated()`
/// (the *peer's* validated flags) is that same word; the peer's creation and its
/// `publish_type/1` classification (`dist_util.erl:917-923`) are reported next to
/// it. Nothing here is host- or run-dependent.
fn probe_nodeid() -> Result<String> {
    let Pair { alpha, .. } = connect_pair()?;

    let flags = alpha.negotiated().bits();
    let visibility = match alpha.peer_visibility() {
        PeerVisibility::Normal => "normal",
        PeerVisibility::Hidden => "hidden",
    };
    Ok(format!(
        "nodeid flags={flags:X} peer={} creation={} visibility={visibility}",
        String::from_utf8_lossy(alpha.peer_name()),
        alpha.peer_creation(),
    ))
}

/// `handshake` — a byte-exact transcript of the OTP-23 `'N'` handshake.
///
/// Two [`ConnectionDriver`]s are driven against each other with **pinned**
/// challenge nonces ([`ALPHA_CHALLENGE`]/[`BETA_CHALLENGE`]), so both the
/// `ChallengeReply` and `ChallengeAck` digests — the parts that would otherwise be
/// random — are constant. The emitted `client=`/`server=` fields are the two
/// directions' raw 2-byte-framed wire bytes in send order (name, flags, creation,
/// message order and tags all included).
fn probe_handshake() -> Result<String> {
    let mut client =
        ConnectionDriver::client_with_challenge(BETA_NAME, COOKIE, BETA_CREATION, BETA_CHALLENGE);
    let mut server =
        ConnectionDriver::server_with_challenge(ALPHA_NAME, COOKIE, ALPHA_CREATION, ALPHA_CHALLENGE);

    let mut client_bytes = Vec::new();
    let mut server_bytes = Vec::new();
    let mut established = false;

    for _ in 0..64 {
        client.step()?;
        server.step()?;
        if client.state() == DriverState::Established && server.state() == DriverState::Established {
            established = true;
            break;
        }
        let from_client = client.take_output();
        let from_server = server.take_output();
        if from_client.is_empty() && from_server.is_empty() {
            break;
        }
        client_bytes.extend_from_slice(&from_client);
        server_bytes.extend_from_slice(&from_server);
        server.feed(&from_client);
        client.feed(&from_server);
    }

    if !established {
        return Err(BeamError::Io(std::io::Error::other(
            "the pinned handshake did not complete",
        )));
    }

    Ok(format!(
        "handshake flags={:X} client={} server={}",
        client.negotiated().bits(),
        hex_upper(&client_bytes),
        hex_upper(&server_bytes),
    ))
}

/// `badcookie` — the connector presents the wrong cookie, so the acceptor's
/// `ChallengeReply` digest check fails and it drops the link.
///
/// The line reports the shape of the refusal: each side's error class, plus (from
/// the recorded client traffic) which 2-byte-framed handshake messages actually
/// crossed — the acceptor's `Status` + `Challenge` (`received=sN`) and **no**
/// `ChallengeAck`, after the connector's `Name` + `ChallengeReply` (`sent=Nr`).
fn probe_badcookie() -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;

    // The acceptor authenticates with the real cookie and refuses the wrong one.
    let acceptor = thread::spawn(move || -> Result<()> {
        let mut transport = listener.accept()?;
        transport.set_read_timeout(Some(TIMEOUT))?;
        Connection::server(transport, ALPHA_NAME, COOKIE, ALPHA_CREATION).map(|_| ())
    });

    let mut transport = TcpTransport::connect(&addr.to_string())?;
    transport.set_read_timeout(Some(TIMEOUT))?;
    let mut recorder = Recorder::new(transport);
    let client = connect_client_with_creation(&mut recorder, BETA_NAME, WRONG_COOKIE, BETA_CREATION);
    let server = acceptor
        .join()
        .map_err(|_| BeamError::Io(std::io::Error::other("the acceptor thread panicked")))?;

    let client_error = client
        .err()
        .ok_or_else(|| BeamError::Io(std::io::Error::other("the wrong cookie was accepted")))?;
    let server_error = server
        .err()
        .ok_or_else(|| BeamError::Io(std::io::Error::other("the acceptor did not refuse")))?;

    Ok(format!(
        "badcookie server={} client={} sent={} received={}",
        error_class(&server_error),
        error_class(&client_error),
        frame_tags(recorder.written())?,
        frame_tags(recorder.read())?,
    ))
}

/// `frag` — a payload larger than [`DIST_FRAGMENT_SIZE`] round-trips A → B → A,
/// so [`Connection::send_payload`] really splits it into `'E'`/`'F'` fragments and
/// the peer reassembles them.
///
/// The observable is the received byte length plus a checksum of the payload; the
/// `frames` field is the number of `'E'`/`'F'` fragment frames node A actually
/// *wrote*, counted by the [`Spy`] transport — so the report fails loudly (rather
/// than reporting a one-packet send) if the payload did not take the multi-frame
/// path. `DFLAG_FRAGMENTS` is asserted negotiated as well.
fn probe_frag() -> Result<String> {
    let Pair {
        mut alpha,
        mut beta,
    } = connect_pair_with(spied)?;
    let mut alpha_atoms = AtomTable::new();
    let mut beta_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(FRAG_HEAP_WORDS)?;
    let mut beta_heap = Heap::new(FRAG_HEAP_WORDS)?;

    if !alpha.negotiated().contains(DistFlags::from_bits(DFLAG_FRAGMENTS)) {
        return Err(BeamError::Io(std::io::Error::other(
            "DFLAG_FRAGMENTS was not negotiated",
        )));
    }

    let payload: Vec<u8> = (0..FRAG_BYTES).map(|i| (i % 251) as u8).collect();
    let big = beam_codec::term::make_binary(&mut alpha_heap, &payload)?;

    alpha.send_reg_send(
        make_small(ALPHA_PID),
        make_atom(alpha_atoms.intern("")),
        make_atom(alpha_atoms.intern(ECHO_NAME)),
        big,
        &alpha_atoms,
    )?;

    let delivery = beta.recv_delivery(&mut beta_heap, &mut beta_atoms)?;
    let (from, message) = match delivery {
        Delivery::RegSend { from, message, .. } => (from, message),
        other => return Err(unexpected("a name-addressed RegSend", &other)),
    };
    beta.send_message(make_small(BETA_PID), from, message, &beta_atoms)?;

    let reply = alpha.recv_delivery(&mut alpha_heap, &mut alpha_atoms)?;
    let big = match reply {
        Delivery::Send { message, .. } => message,
        other => return Err(unexpected("a pid-addressed send", &other)),
    };

    // Node A's own view of the wire: every fragment frame it wrote (the spy
    // records one `write_all` per `write_packet` body, so this is exact).
    let frames = alpha.into_inner().fragment_frames();

    let (bytes, offset, size_bits) = beam_codec::term::bitstring_bytes(big)?;
    if offset != 0 || size_bits != FRAG_BYTES * 8 || bytes.len() != FRAG_BYTES {
        return Err(BeamError::Io(std::io::Error::other(
            "the fragmented payload came back mangled",
        )));
    }
    if frames < 2 {
        return Err(BeamError::Io(std::io::Error::other(
            "the oversized payload was not fragmented",
        )));
    }
    Ok(format!(
        "frag bytes={} sum={:08X} frames={frames}",
        bytes.len(),
        checksum(bytes),
    ))
}

/// `alias` — a `DOP_ALIAS_SEND` control message crosses the loopback and the
/// peer's decoded [`Delivery::AliasSend`] fields are summarised.
fn probe_alias() -> Result<String> {
    let Pair { mut alpha, mut beta } = connect_pair()?;
    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(HEAP_WORDS)?;
    let alpha_atoms = AtomTable::new();

    // `/* {DOP_ALIAS_SEND, From, To} -- Message */` (`dist.c:2563`).
    alpha.send_control(
        Dop::AliasSend,
        &[make_small(ALPHA_PID), make_small(ALIAS_ID), make_small(ALIAS_MESSAGE)],
        &alpha_atoms,
    )?;

    let delivery = beta.recv_delivery(&mut beta_heap, &mut beta_atoms)?;
    match delivery {
        Delivery::AliasSend { from, to, message, token: None } => Ok(format!(
            "alias from={} to={} msg={}",
            small_value(from)?,
            small_value(to)?,
            small_value(message)?,
        )),
        other => Err(unexpected("an alias send", &other)),
    }
}

/// `monitor` — the `DOP_MONITOR_P` / `DOP_LINK` / `DOP_EXIT` control messages
/// cross the loopback and each peer-decoded delivery is summarised.
fn probe_monitor() -> Result<String> {
    let Pair { mut alpha, mut beta } = connect_pair()?;
    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(HEAP_WORDS)?;
    let alpha_atoms = AtomTable::new();

    // `{DOP_MONITOR_P, Remote, Local, Ref}` (`dist.c:2341`).
    alpha.send_monitor_p(
        make_small(ALPHA_PID),
        make_small(MONITORED_PID),
        make_small(MONITOR_REF),
        &alpha_atoms,
    )?;
    let monitor = match beta.recv_delivery(&mut beta_heap, &mut beta_atoms)? {
        Delivery::Monitor { watcher, watched, reference } => format!(
            "monitor watcher={} watched={} ref={}",
            small_value(watcher)?,
            small_value(watched)?,
            small_value(reference)?,
        ),
        other => return Err(unexpected("a process monitor", &other)),
    };

    // `{DOP_LINK, From, To}` (`dist.c:2224`).
    alpha.send_link(make_small(ALPHA_PID), make_small(LINKED_PID), &alpha_atoms)?;
    let link = match beta.recv_delivery(&mut beta_heap, &mut beta_atoms)? {
        Delivery::Link { from, to } => {
            format!("link from={} to={}", small_value(from)?, small_value(to)?)
        }
        other => return Err(unexpected("a link", &other)),
    };

    // `{DOP_EXIT, From, To, Reason}` (`dist.c:2760`).
    alpha.send_exit(
        make_small(ALPHA_PID),
        make_small(EXITED_PID),
        make_small(EXIT_REASON),
        &alpha_atoms,
    )?;
    let exit = match beta.recv_delivery(&mut beta_heap, &mut beta_atoms)? {
        Delivery::Exit { from, to, reason, token: None } => format!(
            "exit from={} to={} reason={}",
            small_value(from)?,
            small_value(to)?,
            small_value(reason)?,
        ),
        other => return Err(unexpected("an exit signal", &other)),
    };

    Ok(format!("{monitor} {link} {exit}"))
}

/// `regsend_missing` — a `DOP_REG_SEND` addressed to a name the receiver has **not**
/// registered.
///
/// Ground truth (`dist.c:2484-2509`): the C looks the target name up and, on a miss,
/// *silently drops* the message — no error and nothing sent back. The receiver's
/// half of that is modelled here by the [`InboundSink`] (`Registry`): a registered
/// name is `delivered`, an unknown one is `nosuchprocess`, and (as in the C) no
/// reply is generated.
fn probe_regsend_missing() -> Result<String> {
    let Pair { mut alpha, mut beta } = connect_pair()?;
    let mut alpha_atoms = AtomTable::new();
    let mut beta_atoms = AtomTable::new();
    let mut alpha_heap = Heap::new(HEAP_WORDS)?;
    let mut beta_heap = Heap::new(HEAP_WORDS)?;
    let _ = &mut alpha_heap;

    alpha.send_reg_send(
        make_small(ALPHA_PID),
        make_atom(alpha_atoms.intern("")),
        make_atom(alpha_atoms.intern(MISSING_NAME)),
        make_small(ALIAS_MESSAGE),
        &alpha_atoms,
    )?;

    // B knows only `echo`; every other registered-name target has no recipient.
    let mut registry = Registry {
        known: make_atom(beta_atoms.intern(ECHO_NAME)),
    };
    let dispatched = beta.dispatch_inbound(&mut beta_heap, &mut beta_atoms, &mut registry)?;
    match &dispatched.delivery {
        Delivery::RegSend { name, message, .. } => {
            if beta_atoms.name(atom_val(*name)) != Some(MISSING_NAME) {
                return Err(unexpected("a RegSend to the missing name", &dispatched.delivery));
            }
            if small_value(*message)? != ALIAS_MESSAGE {
                return Err(unexpected("the sent message", &dispatched.delivery));
            }
        }
        other => return Err(unexpected("a name-addressed RegSend", other)),
    }

    Ok(format!(
        "regsend target={MISSING_NAME} outcome={}",
        outcome_name(dispatched.outcome),
    ))
}

/// `utf8` — an atom **outside Latin-1** crosses the wire A → B.
///
/// `DFLAG_UTF8_ATOMS` is mandatory, so the name travels as
/// `SMALL_ATOM_UTF8_EXT`/`ATOM_UTF8_EXT` and the payload references it through the
/// distribution atom cache; the observable is the re-encoded ETF image of the
/// **received** term (`VERSION_MAGIC` prepended), which therefore shows the UTF-8
/// atom tag.
fn probe_utf8() -> Result<String> {
    let Pair { mut alpha, mut beta } = connect_pair()?;
    let mut alpha_atoms = AtomTable::new();
    let mut beta_atoms = AtomTable::new();
    let mut beta_heap = Heap::new(HEAP_WORDS)?;

    let omega = make_atom(alpha_atoms.intern(UNICODE_ATOM));
    alpha.send_reg_send(
        make_small(ALPHA_PID),
        make_atom(alpha_atoms.intern("")),
        make_atom(alpha_atoms.intern(ECHO_NAME)),
        omega,
        &alpha_atoms,
    )?;

    let received = match beta.recv_delivery(&mut beta_heap, &mut beta_atoms)? {
        Delivery::RegSend { message, .. } => message,
        other => return Err(unexpected("a name-addressed RegSend", &other)),
    };
    if !is_atom(received) || beta_atoms.name(atom_val(received)) != Some(UNICODE_ATOM) {
        return Err(BeamError::Io(std::io::Error::other(
            "the non-Latin-1 atom did not cross the wire intact",
        )));
    }

    let mut image = Vec::with_capacity(1 + 5);
    image.push(VERSION_MAGIC);
    image.extend_from_slice(&beam_codec::encode_term_body(received, &beta_atoms)?);
    Ok(hex_upper(&image))
}

/// The two live endpoints of one loopback link.
struct Pair<T: Transport = TcpTransport> {
    /// Node A (`alpha@localhost`), the acceptor.
    alpha: Connection<T>,
    /// Node B (`beta@localhost`), the connector.
    beta: Connection<T>,
}

/// Binds a loopback listener, completes the `'N'` handshake on the acceptor
/// thread and returns both established endpoints, owned by this thread.
fn connect_pair() -> Result<Pair<TcpTransport>> {
    connect_pair_with(std::convert::identity)
}

/// Like [`connect_pair`], but each endpoint's transport is wrapped by `wrap`
/// before it enters a [`Connection`].
///
/// `wrap` is a plain `fn` pointer so the *same* wrapper builds the acceptor's
/// transport (on its thread) and the connector's; the `frag` report uses it to
/// install a [`Spy`] and inspect the bytes each side really wrote.
fn connect_pair_with<T, W>(wrap: W) -> Result<Pair<T>>
where
    T: Transport + Send + 'static,
    W: Fn(TcpTransport) -> T + Copy + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;

    let acceptor = thread::spawn(move || -> Result<Connection<T>> {
        let mut transport = listener.accept()?;
        transport.set_read_timeout(Some(TIMEOUT))?;
        Connection::server(wrap(transport), ALPHA_NAME, COOKIE, ALPHA_CREATION)
    });

    let mut transport = TcpTransport::connect(&addr.to_string())?;
    transport.set_read_timeout(Some(TIMEOUT))?;
    let beta = Connection::client_with_creation(wrap(transport), BETA_NAME, COOKIE, BETA_CREATION)?;
    let alpha = acceptor
        .join()
        .map_err(|_| BeamError::Io(std::io::Error::other("the acceptor thread panicked")))??;

    Ok(Pair { alpha, beta })
}

/// The `wrap` argument for [`connect_pair_with`] that installs a [`Spy`].
fn spied(transport: TcpTransport) -> Spy<TcpTransport> {
    Spy::new(transport)
}

/// A [`Transport`] that forwards every call but counts the `'E'`/`'F'` **fragment
/// frames** it is asked to write.
///
/// `Connection::send_payload` writes each fragment packet body with a separate
/// `write_all`, and an established frame body always begins `VERSION_MAGIC` +
/// tag (`'D'` for a normal header, `'E'`/`'F'` for a fragment). Counting the
/// writes whose body starts `131 'E'` or `131 'F'` is therefore an exact,
/// framing-independent count of the fragments on the wire — the evidence the
/// `frag` report needs that the payload really took the multi-frame path.
struct Spy<T> {
    /// The wrapped transport.
    inner: T,
    /// The number of fragment frames written so far.
    frames: usize,
}

impl<T> Spy<T> {
    /// Wraps `inner`.
    fn new(inner: T) -> Spy<T> {
        Spy { inner, frames: 0 }
    }

    /// The number of `'E'`/`'F'` fragment frames written through this transport.
    fn fragment_frames(&self) -> usize {
        self.frames
    }

    /// Whether `body` starts a fragment frame (`VERSION_MAGIC` then `'E'`/`'F'`).
    fn is_fragment_body(body: &[u8]) -> bool {
        body.len() >= 2 && body[0] == VERSION_MAGIC && matches!(body[1], b'E' | b'F')
    }
}

impl<T: Read> Read for Spy<T> {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.inner.read_exact(buf)
    }
}

impl<T: Write> Write for Spy<T> {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        if Spy::<T>::is_fragment_body(buf) {
            self.frames += 1;
        }
        self.inner.write_all(buf)
    }

    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

/// A [`beam_dist::transport::Transport`] wrapper that records every byte read from
/// and written to the wrapped transport.
///
/// The `badcookie` report needs to describe *which* handshake messages crossed the
/// wire before the link was dropped; the [`Connection`] constructors take the
/// transport by value, so the report uses the `&mut`-taking
/// [`connect_client_with_creation`] instead and inspects the recording afterwards.
struct Recorder<T> {
    /// The wrapped transport.
    inner: T,
    /// Every byte successfully read from `inner`.
    read: Vec<u8>,
    /// Every byte written to `inner`.
    written: Vec<u8>,
}

impl<T> Recorder<T> {
    /// Wraps `inner`.
    fn new(inner: T) -> Recorder<T> {
        Recorder { inner, read: Vec::new(), written: Vec::new() }
    }

    /// The bytes read from the wrapped transport.
    fn read(&self) -> &[u8] {
        &self.read
    }

    /// The bytes written to the wrapped transport.
    fn written(&self) -> &[u8] {
        &self.written
    }
}

impl<T: Read> Read for Recorder<T> {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.inner.read_exact(buf)?;
        self.read.extend_from_slice(buf);
        Ok(())
    }
}

impl<T: Write> Write for Recorder<T> {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.written.extend_from_slice(buf);
        self.inner.write_all(buf)
    }

    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

/// The one-name local registry the `regsend_missing` report routes through.
///
/// This is the local half of the C's `DOP_REG_SEND` handler (`dist.c:2489-2509`):
/// a known name is delivered, an unknown one has no recipient.
struct Registry {
    /// The one registered name this registry knows.
    known: Eterm,
}

impl InboundSink for Registry {
    fn deliver(&mut self, delivery: &Delivery) -> Result<DeliveryOutcome> {
        Ok(match delivery {
            Delivery::RegSend { name, .. } if *name == self.known => DeliveryOutcome::Delivered,
            Delivery::RegSend { .. } => DeliveryOutcome::NoSuchProcess,
            _ => DeliveryOutcome::Unsupported,
        })
    }
}

/// A typed error for a delivery whose shape was not the one the probe expects.
fn unexpected(expected: &str, got: &Delivery) -> BeamError {
    BeamError::Io(std::io::Error::other(format!(
        "expected {expected}, decoded {got:?}"
    )))
}

/// A short, host-independent classification of a [`BeamError`] (the `badcookie`
/// report's whole observable is the *shape* of the observed failure).
fn error_class(error: &BeamError) -> String {
    match error {
        BeamError::BadArg => "badarg".to_string(),
        BeamError::BadArith => "badarith".to_string(),
        BeamError::BadArity => "badarity".to_string(),
        BeamError::BadMatch => "badmatch".to_string(),
        BeamError::SystemLimit => "system_limit".to_string(),
        BeamError::Alloc(_) => "out_of_memory".to_string(),
        BeamError::TermNotFound => "not_found".to_string(),
        BeamError::Io(io) => match io.kind() {
            std::io::ErrorKind::UnexpectedEof => "eof".to_string(),
            std::io::ErrorKind::ConnectionRefused => "connection_refused".to_string(),
            std::io::ErrorKind::ConnectionAborted => "connection_aborted".to_string(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                "timeout".to_string()
            }
            _ => "io".to_string(),
        },
        other => other.as_atom().map(str::to_string).unwrap_or_else(|| "error".to_string()),
    }
}

/// The decimal value of a small-integer term.
///
/// Every term these reports exchange in a pid/ref/reason slot is a small int (the
/// same stand-ins `tests/two_node_loopback.rs` uses), so a non-small term means the
/// wire mangled the message — reported rather than rendered.
fn small_value(term: Eterm) -> Result<i64> {
    if is_small(term) {
        Ok(signed_value(term))
    } else {
        Err(BeamError::BadArg)
    }
}

/// The stable name of a [`DeliveryOutcome`] (for the `regsend_missing` report).
fn outcome_name(outcome: DeliveryOutcome) -> &'static str {
    match outcome {
        DeliveryOutcome::Delivered => "delivered",
        DeliveryOutcome::NoSuchProcess => "nosuchprocess",
        DeliveryOutcome::Unsupported => "unsupported",
        DeliveryOutcome::Dropped => "dropped",
    }
}

/// The sequence of 2-byte-framed handshake message tags in `bytes` — e.g. `sN` for
/// a `Status` followed by a `Challenge`.
///
/// The framed byte stream is never writable by an attacker here, but a malformed
/// buffer is reported as [`BeamError::BadArg`] rather than panicking.
fn frame_tags(bytes: &[u8]) -> Result<String> {
    let mut tags = String::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let prefix_end = pos.checked_add(2).ok_or(BeamError::BadArg)?;
        let prefix = bytes.get(pos..prefix_end).ok_or(BeamError::BadArg)?;
        let len = usize::from(u16::from_be_bytes([prefix[0], prefix[1]]));
        let body_end = prefix_end.checked_add(len).ok_or(BeamError::BadArg)?;
        let body = bytes.get(prefix_end..body_end).ok_or(BeamError::BadArg)?;
        tags.push(char::from(*body.first().ok_or(BeamError::BadArg)?));
        pos = body_end;
    }
    Ok(tags)
}

/// A deterministic 32-bit FNV-1a checksum of `bytes`.
fn checksum(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Uppercase hex of `bytes`.
fn hex_upper(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{checksum, frame_tags, hex_upper, Spy};
    use beam_dist::transport::{MemoryTransport, Write};

    #[test]
    fn hex_upper_encodes_the_term_image_cost() {
        // `[VERSION_MAGIC] ++ {1,2,3}` in the external term format.
        assert_eq!(
            hex_upper(&[0x83, 0x68, 0x03, 0x61, 0x01, 0x61, 0x02, 0x61, 0x03]),
            "836803610161026103",
        );
        assert_eq!(hex_upper(&[]), "");
        assert_eq!(hex_upper(&[0x00, 0x0f, 0xff]), "000FFF");
    }

    #[test]
    fn frame_tags_reads_the_two_byte_framed_message_tags() {
        // `Status` (`s`, body `ok`) then `Challenge` (`N`, body `x`).
        assert_eq!(
            frame_tags(&[0, 3, b's', b'o', b'k', 0, 2, b'N', b'x']).expect("well-formed frames"),
            "sN",
        );
        assert_eq!(frame_tags(&[]).expect("no frames"), "");
        // A truncated frame is a typed error, never a panic.
        assert!(frame_tags(&[0, 3, b's']).is_err());
        assert!(frame_tags(&[0, 2]).is_err());
    }

    #[test]
    fn checksum_is_the_fnv1a_32_of_the_bytes() {
        // The FNV-1a 32 offset basis over the empty input is the basis itself.
        assert_eq!(checksum(&[]), 0x811c_9dc5);
        // A zero byte still passes through the multiply step.
        assert_eq!(checksum(&[0x00]), 0x050c_5d1f);
        assert_eq!(checksum(b"a"), 0xe40c_292c);
        assert_eq!(checksum(b"foobar"), 0xbf9c_f968);
    }

    #[test]
    fn spy_counts_only_fragment_frames() {
        let mut spy = Spy::new(MemoryTransport::new());
        // A normal established frame body (`VERSION_MAGIC`, `'D'`) is not a fragment.
        spy.write_all(&[0x83, b'D', 0x00]).expect("write");
        // An `'E'` header frame plus two `'F'` continuations are.
        spy.write_all(&[0x83, b'E', 0, 0, 0, 0, 0, 0, 0, 3]).expect("write");
        spy.write_all(&[0x83, b'F', 0, 0, 0, 0, 0, 0, 0, 2]).expect("write");
        spy.write_all(&[0x83, b'F', 0, 0, 0, 0, 0, 0, 0, 1]).expect("write");
        // A 4-byte length prefix (its first byte is not `VERSION_MAGIC`) is not one.
        spy.write_all(&[0x00, 0x01, 0x00, 0x12]).expect("write");
        assert_eq!(spy.fragment_frames(), 3);
    }

    #[test]
    fn spy_ignores_a_short_or_non_magic_buffer() {
        let mut spy = Spy::new(MemoryTransport::new());
        spy.write_all(&[]).expect("write");
        spy.write_all(&[0x83]).expect("write");
        // `VERSION_MAGIC` must be the *first* byte, and the tag the second.
        spy.write_all(&[b'E', 0x83]).expect("write");
        assert_eq!(spy.fragment_frames(), 0);
    }
}
