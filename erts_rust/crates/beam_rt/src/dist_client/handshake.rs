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

//! [`ClientLink`]: one outbound link's connecting-side state machine and the non-blocking
//! read/step/write round [`DistClient::pump`](super::DistClient::pump) drives it with.
//!
//! Nothing here is protocol code: the bytes are the sibling `beam_dist` crate's
//! (`ConnectionDriver`), and this type only owns the socket a link's bytes travel over, its byte
//! counters and the "no data" rule that keeps a pump non-blocking.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;

use beam_common::{BeamError, Result};
use beam_dist::driver::{ConnectionDriver, DriverState};
use beam_dist::{Dop, StepOutcome};
use beam_term::prelude::Eterm;
use beam_term::AtomTable;

/// One outbound distribution link: the connecting state machine and its socket.
///
/// Crate-visible because the one node→link table ([`DistLinks`]) shares it with the
/// accepting half: it looks a link up by `node`, labels it by `name` and writes a control
/// message through [`ClientLink::send_control`]/[`ClientLink::send_message`].
///
/// The four `pub(super)` fields are built by `DistClient::connect_addr` and stepped by the
/// client's drain; inside a directory module that is exactly the scope a private field of the
/// former single-file module had.
pub(crate) struct ClientLink {
    /// The peer's node atom term — the key a routed signal addresses.
    pub(crate) node: Eterm,
    /// The peer's full `node@host` name (diagnostic and [`DistClient::connected_nodes`]).
    pub(crate) name: String,
    /// The connecting side's handshake/established state machine.
    pub(super) driver: ConnectionDriver,
    /// The socket the driver's bytes travel over.
    pub(super) stream: TcpStream,
    /// Whether this link **ever** completed its `'N'` handshake — the latch
    /// [`ClientLink::was_established`] reads, set by [`ClientLink::pump`].
    pub(super) was_established: bool,
    /// Bytes read from the peer so far.
    pub(super) bytes_in: u64,
    /// Bytes written to the peer so far.
    pub(super) bytes_out: u64,
}

impl ClientLink {
    /// Whether the handshake completed and the link carries `DOP_*`.
    pub(crate) fn is_established(&self) -> bool {
        matches!(self.driver.state(), DriverState::Established)
    }

    /// Whether the handshake **ever** completed — the question a retirement must ask.
    ///
    /// [`ClientLink::is_established`] is the *live* state, and the pump that discovers a dead
    /// link is what moves the driver out of `Established` (`Closed` on the peer's EOF,
    /// `Failed` on a protocol error) — so asking it at retirement time would call every lost
    /// peer "never handshook" and record no node down at all
    /// ([`crate::Runtime::service_lost_dist_peers`]). A link that really never handshook
    /// stays `false`, which is the distinction the recorder needs.
    pub(crate) fn was_established(&self) -> bool {
        self.was_established
    }

    /// The **peer's** validated capability flags, once the handshake completed
    /// ([`beam_dist::DistFlags::EMPTY`] before then).
    ///
    /// The same per-link gate the accepting half exposes
    /// ([`DistConnection::negotiated`](crate::dist::DistConnection::negotiated)): it
    /// decides whether an opcode may be used at all — e.g. `DFLAG_ALIAS` for a
    /// `DOP_ALIAS_SEND`.
    pub(crate) fn negotiated(&self) -> beam_dist::DistFlags {
        self.driver.negotiated()
    }

    /// Whether the link can still make progress (handshaking or established).
    ///
    /// `pub(super)` because the client's drain (`DistClientInner::step_links`) asks it after
    /// each pump to decide whether the link survives.
    pub(super) fn is_live(&self) -> bool {
        matches!(
            self.driver.state(),
            DriverState::Handshaking | DriverState::Established
        )
    }

    /// Encodes one `DOP_*` control on this link's driver and writes it.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] before the handshake completed (the driver's own contract),
    /// the encoder's failures and the socket's write failure.
    pub(crate) fn send_control(
        &mut self,
        dop: Dop,
        args: &[Eterm],
        atoms: &AtomTable,
    ) -> Result<()> {
        self.driver.send_control(dop, args, atoms)?;
        self.flush_output()
    }

    /// Encodes one message on this link; the driver picks `DOP_SEND_SENDER` when the peer
    /// negotiated `DFLAG_SEND_SENDER` and `DOP_SEND` otherwise (`dist.c:1686`).
    ///
    /// # Errors
    ///
    /// As [`ClientLink::send_control`].
    pub(crate) fn send_message(
        &mut self,
        from: Eterm,
        to: Eterm,
        msg: Eterm,
        atoms: &AtomTable,
    ) -> Result<()> {
        self.driver.send_message(from, to, msg, atoms)?;
        self.flush_output()
    }

    /// Writes whatever the driver has queued to the socket.
    fn flush_output(&mut self) -> Result<()> {
        if self.driver.has_output() {
            let bytes = self.driver.take_output();
            self.stream.write_all(&bytes).map_err(BeamError::Io)?;
            self.stream.flush().map_err(BeamError::Io)?;
            self.bytes_out += bytes.len() as u64;
        }
        Ok(())
    }

    /// Reads whatever has arrived, advances the handshake and writes the reply.
    ///
    /// A read timeout (or a clean `WouldBlock`) is "no data": not an event and not an
    /// error. Returns how many protocol events took effect.
    pub(super) fn pump(&mut self) -> Result<usize> {
        let mut events = 0;
        let mut buf = [0u8; 8192];
        match self.stream.read(&mut buf) {
            Ok(0) => {
                self.driver.feed_eof();
                events += 1;
            }
            Ok(n) => {
                self.bytes_in += n as u64;
                self.driver.feed(&buf[..n]);
                events += 1;
            }
            Err(err) if is_would_block(&err) => {}
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(BeamError::Io(err)),
        }
        loop {
            match self.driver.step() {
                // No progress is possible without more input.
                Ok(StepOutcome::WouldBlock) => break,
                // A dead link: the caller retires it.
                Ok(StepOutcome::Failed) | Ok(StepOutcome::Closed) => {
                    events += 1;
                    break;
                }
                Ok(outcome) => {
                    if matches!(outcome, StepOutcome::Established | StepOutcome::Progress) {
                        self.was_established = true;
                    }
                    events += 1;
                }
                Err(err) => {
                    // A refusing/misbehaving peer may have buffered a status frame
                    // before the failure — drain it (the driver's contract) and then
                    // report the failure so the caller retires the link.
                    self.flush_output()?;
                    return Err(err);
                }
            }
        }
        // The establishment latch, read again after the loop: a link that established in an
        // earlier step and dies now (a peer's EOF, a protocol error) stays `true`, which is
        // what the retirement site asks. See [`ClientLink::was_established`].
        self.was_established |= self.is_established();
        self.flush_output()?;
        Ok(events)
    }
}

/// A read that timed out (or would have blocked) is "no data", never an error.
///
/// `pub(super)` because the sibling suite builds its own loopback rounds with the same rule —
/// the scope a private item of the former single-file module had.
pub(super) fn is_would_block(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}
