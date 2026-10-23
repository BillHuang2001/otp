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
//! The `tcp_inet` **write-shutdown answer matrix**, pinned against a real loopback pair.
//!
//! Mirrors the oracle (OTP 29.1.1 / erts-17.1), measured with `gen_tcp` after a completed
//! `gen_tcp:send(Cl, <<"hi">>)` and `gen_tcp:shutdown(Cl, write)`:
//!
//! * any number of **zero-length** sends answer `ok` — forever, while no non-empty write has
//!   failed (`inet_drv.c:13736`'s `(h_len == 0) && (len == 0)` shortcut, which precedes every
//!   closed check because `tcp_inet_commandv`'s `!IS_CONNECTED` arm never queues);
//! * the **first non-empty** send answers `{error, closed}` (the send ladder's one-shot, armed by
//!   `tcp_send_or_shutdown_error` → `TCP_ADDF_DELAYED_CLOSE_SEND`, `:13613`);
//! * the **second non-empty** send and every later one answer `{error, enotconn}` (the descriptor
//!   is closed by `tcp_desc_close` → `erl_inet_close` → `desc_close`'s
//!   `desc->state &= ~INET_STATE_CONNECTED`, `:5079`, whose comment at `:5078` names exactly this
//!   reply);
//! * a zero-length send **after** that first failure also answers `{error, enotconn}` — the
//!   exemption only exists while the socket is still connected;
//! * a repeated `shutdown(write)` and a following `shutdown(read)` answer `{error, enotconn}`.
//!
//! Every row also proves the command is **answered**: a refused command must consume exactly its
//! own bytes and stage its completion, so no caller is left parked on the port (the defect this
//! matrix was written for).

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::IntoRawFd;
use std::time::Duration;

use super::opcode_tests::{op, Rig};
use super::wire::ctl_ok;
use super::{TcpInetDriver, INET_REP_OK};
use crate::completion::{CompletionEvent, InetReply};

/// `TCP_REQ_SHUTDOWN` — the `inet` control verb `gen_tcp:shutdown/2` issues.
const SHUTDOWN: u32 = 44;
/// `TCP_SHUT_RD`/`TCP_SHUT_WR` of `inet_drv.c`.
const SHUT_RD: u8 = 0;
const SHUT_WR: u8 = 1;
/// A `ref`-shaped caller tag, the one `prim_inet` puts in front of a payload.
const CALLER_REF_TAG: &[u8] = &[131, 100, 0, 8, b'r', b'e', b'f', b'0'];

/// A driver adopted from one end of a real loopback pair, plus the peer end.
struct Wire {
    port: Rig,
    peer: TcpStream,
}

impl Wire {
    fn new() -> Wire {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let peer = TcpStream::connect(listener.local_addr().unwrap()).expect("connect");
        let (server, _accepted) = listener.accept().expect("accept");
        let fd = server.try_clone().unwrap().into_raw_fd();
        let driver = TcpInetDriver::from_socket(fd).expect("adopt");
        Wire {
            port: Rig::new(Box::new(driver)),
            peer,
        }
    }

    fn shutdown(&mut self, how: u8) -> Vec<u8> {
        let reply = self.port.try_control(&op(SHUTDOWN), &[how]).expect("reply");
        self.poll_round();
        reply
    }

    /// Queues one `port_command` — `[len:be16][tag][payload]`, the shape `prim_inet:send/3`
    /// builds — and returns the caller's answer as `ok` / `{error,Atom}`.
    ///
    /// `None` means **no completion was staged at all**: the port accepted the command and
    /// nobody was ever answered, which is the failure class this matrix pins.
    fn cmd(&mut self, payload: &[u8]) -> Option<String> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(CALLER_REF_TAG.len() as u16).to_be_bytes());
        bytes.extend_from_slice(CALLER_REF_TAG);
        bytes.extend_from_slice(payload);
        self.port.command(&bytes).expect("accepted");
        self.poll_round();
        let completions = self.port.completions();
        match completions.first().map(|c| &c.event) {
            Some(CompletionEvent::InetReply { reply, .. }) => Some(match reply {
                InetReply::Ok => "ok".to_owned(),
                InetReply::Error(reason) => format!("{{error,{}}}", reason.atom_name()),
            }),
            other => {
                assert!(
                    other.is_none(),
                    "unexpected completion while sending {} bytes: {other:?}",
                    payload.len()
                );
                None
            }
        }
    }

    fn poll_round(&mut self) {
        let events = self
            .port
            .poller
            .poll(Some(Duration::from_millis(20)))
            .expect("poll");
        self.port
            .registry
            .handle_events(&mut self.port.poller, &events)
            .expect("events");
    }
}

/// The whole measured matrix, in the oracle's order.
#[test]
fn write_shutdown_answer_matrix_matches_the_oracle() {
    let _ = (INET_REP_OK, SHUT_RD);
    let mut w = Wire::new();
    assert_eq!(w.cmd(b"hi"), Some("ok".to_owned()), "the pre-shutdown send");

    let shutdown = w.shutdown(SHUT_WR);
    assert_eq!(shutdown, ctl_ok(), "shutdown(write)");

    // Zero-length writes are exempt from the half-close's closed check — indefinitely.
    let empty = [
        w.cmd(b""),
        w.cmd(b""),
        w.cmd(b""),
        w.cmd(b""),
    ];
    assert_eq!(
        empty,
        [0; 4].map(|_| Some("ok".to_owned())),
        "every zero-length write after shutdown(write) answers ok"
    );

    // The first **non-empty** write crosses the half-close and arms the send one-shot.
    assert_eq!(
        w.cmd(b"x"),
        Some("{error,closed}".to_owned()),
        "the first non-empty write after shutdown(write)"
    );
    // ... and every later one — empty or not — is the closed descriptor's `enotconn`.
    assert_eq!(
        w.cmd(b"y"),
        Some("{error,enotconn}".to_owned()),
        "the second non-empty write after shutdown(write)"
    );
    assert_eq!(
        w.cmd(b"z"),
        Some("{error,enotconn}".to_owned()),
        "the third non-empty write after shutdown(write)"
    );
    assert_eq!(
        w.cmd(b""),
        Some("{error,enotconn}".to_owned()),
        "a zero-length write once a non-empty one has failed"
    );

    // The control verbs follow the same ladder.
    assert_eq!(w.shutdown(SHUT_WR), reply_error("enotconn"));
    assert_eq!(w.shutdown(SHUT_RD), reply_error("enotconn"));

    // The peer saw the pre-shutdown bytes and nothing else, then its end of file.
    let mut seen = Vec::new();
    w.peer
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let _ = w.peer.read_to_end(&mut seen);
    assert_eq!(seen, b"hi", "the peer receives only the pre-shutdown bytes");
}

fn reply_error(atom: &str) -> Vec<u8> {
    super::wire::ctl_xerror(atom)
}
