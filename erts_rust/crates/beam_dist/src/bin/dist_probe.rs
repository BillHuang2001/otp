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
//! ```
//! dist_probe loopback <report>     # <report> ∈ { ping, nodes, term }
//! ```
//!
//! The helper stands up two in-process [`beam_dist`] endpoints over a real
//! loopback TCP socket, completes the OTP-23 `'N'` handshake both ways, sends a
//! compound `{1,2,3}` tuple from A to B as a name-addressed `DOP_REG_SEND`, has B
//! echo it back to A's pid as a `DOP_SEND(_SENDER)`, receives it on A, and derives
//! the report from that observed state:
//!
//! * `ping`  → `pong` iff both handshakes completed (else `pang`);
//! * `nodes` → the number of peers node A completed a handshake with (`1`);
//! * `term`  → the uppercase hex of the ETF image of the **received** term
//!   (`836803610161026103` for `{1,2,3}`).
//!
//! On success exactly one line is printed to stdout and the process exits `0`. An
//! unknown or missing subcommand prints a usage line on stderr and exits `2`. If
//! the loopback genuinely cannot be set up, `dist_probe: unsupported: <reason>` is
//! printed on stderr and the process exits `3` — the harness's capability-gap
//! convention. A fabricated `pong`/`1`/hex is never printed.

use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use beam_alloc::Heap;
use beam_codec::VERSION_MAGIC;
use beam_common::{BeamError, Result};
use beam_dist::{Connection, Delivery, TcpListener, TcpTransport};
use beam_term::prelude::{make_atom, make_small};
use beam_term::AtomTable;

/// The shared cookie both endpoints authenticate with.
const COOKIE: &[u8] = b"dist-probe-cookie";
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
/// A finite read timeout, so a broken link fails instead of hanging.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The per-endpoint heap size in words.
const HEAP_WORDS: usize = 1 << 16;
/// The accepted report kinds.
const REPORT_KINDS: [&str; 3] = ["ping", "nodes", "term"];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let report = match args.as_slice() {
        [sub, report] if sub == "loopback" && REPORT_KINDS.contains(&report.as_str()) => {
            report.as_str()
        }
        _ => {
            eprintln!("usage: dist_probe loopback <ping|nodes|term>");
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

/// Runs the full loopback (handshake + `{1,2,3}` round trip) and derives the
/// requested report from the observed state.
fn probe(report: &str) -> Result<String> {
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
        // `main` validated the report before calling in.
        _ => return Err(BeamError::BadArg),
    })
}

/// The two live endpoints of one loopback link.
struct Pair {
    /// Node A (`alpha@localhost`), the acceptor.
    alpha: Connection<TcpTransport>,
    /// Node B (`beta@localhost`), the connector.
    beta: Connection<TcpTransport>,
}

/// Binds a loopback listener, completes the `'N'` handshake on the acceptor
/// thread and returns both established endpoints, owned by this thread.
fn connect_pair() -> Result<Pair> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;

    let acceptor = thread::spawn(move || -> Result<Connection<TcpTransport>> {
        let mut transport = listener.accept()?;
        transport.set_read_timeout(Some(TIMEOUT))?;
        Connection::server(transport, ALPHA_NAME, COOKIE, ALPHA_CREATION)
    });

    let mut transport = TcpTransport::connect(&addr.to_string())?;
    transport.set_read_timeout(Some(TIMEOUT))?;
    let beta = Connection::client_with_creation(transport, BETA_NAME, COOKIE, BETA_CREATION)?;
    let alpha = acceptor
        .join()
        .map_err(|_| BeamError::Io(std::io::Error::other("the acceptor thread panicked")))??;

    Ok(Pair { alpha, beta })
}

/// A typed error for a delivery whose shape was not the one the probe expects.
fn unexpected(expected: &str, got: &Delivery) -> BeamError {
    BeamError::Io(std::io::Error::other(format!(
        "expected {expected}, decoded {got:?}"
    )))
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
    use super::hex_upper;

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
}
