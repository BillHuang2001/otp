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

//! **Live** interoperability of the **non-blocking** connection driver with a real
//! Erlang/OTP node.
//!
//! The blocking [`beam_dist::Connection`] owns a transport and performs its own
//! I/O — the right shape for one link on one thread, and the shape
//! [`../live_interop.rs`] proves. A runtime that multiplexes many links on a single
//! event loop (e.g. `beam_rt` over `beam_io`'s poller) cannot use that: it owns the
//! socket itself and needs a driver it can *feed*. [`ConnectionDriver`] is that
//! shape, and this test is its live evidence.
//!
//! # What is proven
//!
//! The socket is a genuine `std::net::TcpStream` in **non-blocking** mode and the
//! test performs every read and write itself — exactly the contract the driver's
//! module documentation describes:
//!
//! 1. `step()` is called until it reports [`StepOutcome::WouldBlock`], i.e. no
//!    further progress is possible without more bytes;
//! 2. whatever the driver queued is drained with `take_output()` and written to the
//!    socket (handling `WouldBlock` with a small bounded sleep);
//! 3. everything the socket has is read and handed over with `feed()`;
//! 4. the handshake completes *incrementally* across those rounds — the driver
//!    reports [`DriverState::Established`], [`ConnectionDriver::peer_name`] names the
//!    real node and [`ConnectionDriver::negotiated`] carries the `DFLAG_*` set OTP
//!    advertised (including the mandatory flag the test pins).
//!
//! It then queues a `DOP_REG_SEND` (`{DOP_REG_SEND, From, Cookie, ToName} --
//! Message`, `dist.c:2468`) carrying `{hello, 7, OurPid}` with
//! [`ConnectionDriver::send_control`]; the real node decodes and prints it —
//! asserted on the node's stdout, so the driver's envelope, atom cache and control
//! tuple are proven against the actual implementation.
//!
//! # When it is skipped
//!
//! If no usable `erl` is on `PATH` the test **honestly skips** (a clear reason on
//! stderr) instead of failing — the driver's hermetic byte-shuttle tests
//! (`../driver.rs`) already cover the state machine. Every wait is bounded by a
//! deadline, so the test can neither hang nor leak the spawned node (reaped by the
//! shared harness's `Drop`).

mod common;

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_codec::term::make_external_pid;
use beam_dist::flags::{DistFlags, Dop, DFLAG_SEND_SENDER};
use beam_dist::{ConnectionDriver, DriverState, StepOutcome};
use beam_term::prelude::{make_atom, make_small};

use common::{
    erl_hostname, free_port, resolve_listen_port, run_id, ErlangNode, PortSource, EPMD_TIMEOUT,
    READY_TIMEOUT,
};

/// The name the spawned node registers its receiver loop under.
const REGISTERED_NAME: &str = "genesis_drv";

/// The Erlang script the node evaluates: register a receiver that prints every
/// message it gets, then park.
const NODE_EVAL: &str = r#"P = spawn(fun Loop() ->
    receive
        M ->
            io:format("GOT ~p~n", [M]),
            Loop()
    end
end),
register(genesis_drv, P),
io:format("READY~n", []),
timer:sleep(30000),
halt()."#;

/// How long the non-blocking handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the driven send may take to become observable on the node.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a single non-blocking write may take to drain.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to sleep between polls when a socket is not ready.
const POLL_SLEEP: Duration = Duration::from_millis(2);
/// The read buffer size for one non-blocking `read`.
const READ_BUF: usize = 64 * 1024;

/// Writes `bytes` in full to a non-blocking socket, retrying `WouldBlock` with a
/// bounded sleep (a test helper — the driver itself never touches the socket).
fn write_all_nonblocking(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    let deadline = Instant::now() + WRITE_TIMEOUT;
    let mut written = 0;
    while written < bytes.len() {
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(std::io::Error::new(ErrorKind::WriteZero, "the socket accepted 0 bytes"))
            }
            Ok(n) => written += n,
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        ErrorKind::TimedOut,
                        "the non-blocking write did not drain in time",
                    ));
                }
                std::thread::sleep(POLL_SLEEP);
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Reads everything the socket currently has (bounded by `READ_BUF` per call) and
/// hands it to the driver; `Ok(0)` is a clean EOF.
fn pump_input(stream: &mut TcpStream, driver: &mut ConnectionDriver) -> std::io::Result<()> {
    let mut buf = vec![0u8; READ_BUF];
    match stream.read(&mut buf) {
        Ok(0) => driver.feed_eof(),
        Ok(n) => driver.feed(&buf[..n]),
        Err(err) if err.kind() == ErrorKind::WouldBlock => {}
        Err(err) if err.kind() == ErrorKind::Interrupted => {}
        Err(err) => return Err(err),
    }
    Ok(())
}

/// Makes one round of progress: `step` to quiescence, drain `take_output` to the
/// socket, then read whatever arrived. Panics on a protocol error (a driver error
/// is fatal for the link by design).
fn round(stream: &mut TcpStream, driver: &mut ConnectionDriver) {
    loop {
        match driver.step() {
            Ok(StepOutcome::WouldBlock) | Ok(StepOutcome::Closed) | Ok(StepOutcome::Failed) => {
                break
            }
            Ok(_) => {}
            Err(err) => panic!("the driver reported a protocol error: {err:?}"),
        }
    }
    let pending = driver.take_output();
    if !pending.is_empty() {
        write_all_nonblocking(stream, &pending).expect("write the driver's output");
    }
    pump_input(stream, driver).expect("read from the socket");
}

#[test]
fn live_nonblocking_driver_handshakes_and_sends_with_a_real_erl_node() {
    let Some(host) = erl_hostname() else {
        eprintln!(
            "SKIP live_driver: no usable `erl` on PATH — this test proves the non-blocking \
             `ConnectionDriver` interoperates with a REAL Erlang/OTP node; install Erlang/OTP \
             to run it (the hermetic byte-shuttle tests in tests/driver.rs already cover the \
             state machine itself)."
        );
        return;
    };

    let run = run_id();
    let pid = std::process::id();
    let erl_short = format!("drvnode{pid}x{run}");
    let client_short = format!("drvcli{pid}x{run}");
    let cookie = format!("drvcook{pid}x{run}");
    let our_node = format!("{client_short}@{host}");
    let pinned_port = free_port();

    let node = ErlangNode::start(&erl_short, &cookie, pinned_port, NODE_EVAL).expect("spawn erl");

    let ready = node.wait_for(|line| line.contains("READY"), READY_TIMEOUT);
    assert!(
        ready.is_some(),
        "the erl node never printed READY (stderr: {})",
        node.stderr_text()
    );

    // Resolve the listener through `epmd` (preferred); fall back to the pinned port.
    let (port, source, epmd_error) = resolve_listen_port(&erl_short, pinned_port, EPMD_TIMEOUT);
    match source {
        PortSource::Epmd => eprintln!(
            "live_driver: epmd resolved `{erl_short}` -> 127.0.0.1:{port}"
        ),
        PortSource::Fallback => eprintln!(
            "live_driver: epmd lookup failed ({}) — using the pinned port 127.0.0.1:{port}",
            epmd_error.as_deref().unwrap_or("(no error reported)")
        ),
    }

    // A genuinely non-blocking socket the test itself owns and drives.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the node");
    stream.set_nonblocking(true).expect("set the socket non-blocking");

    let mut driver = ConnectionDriver::client(our_node.as_bytes(), cookie.as_bytes(), 0);

    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while driver.state() == DriverState::Handshaking {
        round(&mut stream, &mut driver);
        match driver.state() {
            DriverState::Established => break,
            DriverState::Closed => {
                panic!(
                    "the peer closed the connection during the handshake (stderr: {})",
                    node.stderr_text()
                )
            }
            DriverState::Failed => panic!("the driver failed during the handshake"),
            DriverState::Handshaking => {
                assert!(
                    Instant::now() < deadline,
                    "the non-blocking handshake did not complete within {HANDSHAKE_TIMEOUT:?} \
                     (stderr: {})",
                    node.stderr_text()
                );
                std::thread::sleep(POLL_SLEEP);
            }
        }
    }

    let peer = driver
        .peer_name()
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .unwrap_or_default();
    eprintln!(
        "live_driver: handshake OK — peer={peer} creation={} negotiated_flags={:#x}",
        driver.peer_creation(),
        driver.negotiated().bits()
    );
    assert_eq!(peer, format!("{erl_short}@{host}"), "peer_name must be the started node");
    assert!(
        driver.negotiated().contains(DistFlags::from_bits(DFLAG_SEND_SENDER)),
        "an OTP node must negotiate DFLAG_SEND_SENDER (flags={:#x})",
        driver.negotiated().bits()
    );

    // Build `{DOP_REG_SEND, From, Cookie, ToName} -- {hello, 7, OurPid}`.
    let mut atoms = beam_term::AtomTable::new();
    let mut heap = Heap::new(1 << 20).expect("client heap");
    let node_atom = make_atom(atoms.intern(&our_node));
    let our_pid =
        make_external_pid(&mut heap, node_atom, 1, 0, 0).expect("materialize the client pid");
    let empty_atom = make_atom(atoms.intern(""));
    let to_name = make_atom(atoms.intern(REGISTERED_NAME));
    let hello = make_atom(atoms.intern("hello"));
    let message = heap
        .tuple(&[hello, make_small(7), our_pid])
        .expect("build the message tuple");

    driver
        .send_control(Dop::RegSend, &[our_pid, empty_atom, to_name, message], &atoms)
        .expect("queue DOP_REG_SEND");

    // Push it out through the driver's own output buffer.
    let pending = driver.take_output();
    assert!(
        !pending.is_empty(),
        "the queued control message must have been framed into the output buffer"
    );
    write_all_nonblocking(&mut stream, &pending).expect("write the queued message");

    // The real node must decode and deliver it (its stdout proves it), and the
    // driver must keep answering anything the node sends back meanwhile.
    let deadline = Instant::now() + SEND_TIMEOUT;
    let mut observed = None;
    while observed.is_none() && Instant::now() < deadline {
        observed = node.wait_for(
            |line| line.starts_with("GOT") && line.contains("hello") && line.contains('7'),
            Duration::from_millis(200),
        );
        if observed.is_none() {
            round(&mut stream, &mut driver);
        }
    }
    match &observed {
        Some(line) => eprintln!("live_driver: the erl node printed `{line}`"),
        None => eprintln!(
            "live_driver: the erl node never printed the driven message (stderr: {})",
            node.stderr_text()
        ),
    }
    assert!(
        observed.is_some(),
        "the erl node never observed the DOP_REG_SEND the ConnectionDriver queued (stderr: {})",
        node.stderr_text()
    );
    assert_eq!(
        driver.state(),
        DriverState::Established,
        "the link must still be established after the exchange"
    );

    // The node is killed and reaped by `Drop`.
}
