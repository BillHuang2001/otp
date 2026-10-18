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

//! Contract A.4 — `driver_output2`'s **header/body split**, through a real
//! [`PortRegistry`] over a real [`Poller`].
//!
//! # Which half is this crate's
//!
//! `beam_io` owns the **split**: one [`ReadMessage`] per driver `driver_output*` call, the
//! `hbuf`/`hlen` bytes apart from the `buf`/`len` ones, with the C's `buf == NULL` kept as
//! `body: None`. Building the Erlang term — `[H1,H2,H3 | <<Body>>]` for a `[binary]` port, the
//! flat int list for a `[list]` port — is the **runtime's** (`beam_rt`), because it is a term
//! and this crate owns no terms.
//!
//! # The pinned oracle table
//!
//! `deliver_read_message` (`../../../erts/emulator/beam/io.c:3363-3442`) turns one
//! `driver_output2(ix, hbuf, hlen, buf, len)` call into exactly one
//! `{Port, {data, Data}}` message. Probed with a real C driver under the system `erl` (OTP
//! 29.1.1 / erts-17.1; the call sites are `io.c:3335-3360`):
//!
//! | driver call | `[list]` port `Data` | `[binary]` port `Data` |
//! |---|---|---|
//! | `driver_output2("HDR",3,"abc",3)` | `[72,68,82,97,98,99]` | `[72,68,82\|<<97,98,99>>]` (improper) |
//! | `driver_output("abc",3)` | `[97,98,99]` | `<<97,98,99>>` (bare binary) |
//! | `driver_output2("HDR",3,"abc",0)` | `[72,68,82]` | `[72,68,82\|<<>>]` |
//! | `driver_output2("HDR",3,NULL,0)` | `[72,68,82]` | `[72,68,82]` (proper list) |
//! | `driver_output2(NULL,0,NULL,0)` | `[]` | `[]` (not `<<>>`) |
//!
//! Other pinned facts this file asserts: exactly **one** message per `driver_output*` call (two
//! calls in one callback ⇒ two messages; the header and body of one call are never two
//! messages and never merged), and `port_info(P, input)` counts `hlen + len` — the header
//! included, and only the bytes not yet delivered.
//!
//! # A **C** driver still arrives flattened — and that is a gap, not the contract
//!
//! A loaded `.so` driver's `driver_output2` reaches this layer through
//! `beam_abi`'s `DriverHost::output`, which has **no two-buffer hook yet**, so its bytes are
//! handed up through [`DriverContext::input`] and arrive as one body-only [`ReadMessage`].
//! These tests pin the **seam** (a Rust [`Driver`] that calls [`DriverContext::deliver_read`])
//! so the C-ABI bridge has somewhere correct to deliver to once that hook exists; they do not
//! pin the flattening as correct behaviour.

use beam_common::Result;
use beam_io::prelude::*;
use beam_process::PortId;

/// The header the scripted driver puts in front of its body (`"HDR"` in the pinned table).
const HEADER: &[u8] = b"HDR";

/// A driver that answers each `port_command` with a **scripted** reply, in the shape of the
/// table above. It registers no descriptor: the port is driven by `port_command`/`Port::output`
/// alone, so no `poll(2)` is ever needed.
struct SplitReplyDriver {
    /// The `driver_output*` calls the next command will make.
    plan: Vec<Reply>,
}

/// One `driver_output*` call the driver makes.
enum Reply {
    /// A `driver_output2`-shaped read: `hbuf`/`hlen` apart from `buf`/`len` (`None` = `NULL`).
    Split {
        header: &'static [u8],
        body: Option<&'static [u8]>,
    },
    /// A plain `driver_output(buf, len)`: bytes banked on [`DriverContext::input`].
    Bank(&'static [u8]),
}

impl SplitReplyDriver {
    /// The scripted reply per command word, so a test names a shape and reads its answer.
    fn for_command(command: &[u8]) -> Vec<Reply> {
        match command {
            b"split" => vec![Reply::Split {
                header: HEADER,
                body: Some(b"abc"),
            }],
            b"empty" => vec![Reply::Split {
                header: HEADER,
                body: Some(b""),
            }],
            b"null" => vec![Reply::Split {
                header: HEADER,
                body: None,
            }],
            b"body" => vec![Reply::Split {
                header: b"",
                body: Some(b"abc"),
            }],
            b"nothing" => vec![Reply::Split {
                header: b"",
                body: None,
            }],
            b"bank" => vec![Reply::Bank(b"abc")],
            // Two calls in one callback: two messages, in call order — and the banked bytes
            // are closed before the split read that follows them.
            b"two" => vec![Reply::Bank(b"bank"), Reply::Split {
                header: HEADER,
                body: Some(b"abc"),
            }],
            other => panic!("the test script has no reply for {other:?}"),
        }
    }
}

impl Driver for SplitReplyDriver {
    fn name(&self) -> &str {
        "split-reply-test"
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        // Drain the command exactly as a driver must (the port checks the accounting), then
        // answer with the scripted read calls.
        let written = ctx.output().len();
        ctx.output().consume(written);
        for reply in std::mem::take(&mut self.plan) {
            match reply {
                Reply::Split { header, body } => ctx.deliver_read(header, body)?,
                Reply::Bank(bytes) => ctx.input().push(bytes),
            }
        }
        Ok(written)
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// Opens a port whose driver will answer `command` with that command's scripted reply, and
/// queues `command` for it.
///
/// The plan is installed at construction because the port layer has no seam for poking a
/// driver: it is the *test's* state, and one port per command keeps it simple.
fn answer(poller: &mut Poller, command: &[u8]) -> (PortRegistry, PortId) {
    let mut registry = PortRegistry::new();
    let id = registry
        .open(
            poller,
            Box::new(SplitReplyDriver {
                plan: SplitReplyDriver::for_command(command),
            }),
        )
        .expect("the scripted port opens");
    registry
        .port_command(poller, id, command, PortCommandOptions::new())
        .expect("the command is queued");
    (registry, id)
}

/// One port-task round's `Data` notifications, as `(header, body)` pairs.
fn round_reads(
    registry: &mut PortRegistry,
    poller: &mut Poller,
) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let outcome = registry
        .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the port-task round runs");
    outcome
        .notifications
        .into_iter()
        .map(|notification| match notification {
            PortNotification::Data { read, .. } => (read.header, read.body),
            PortNotification::Closed { port } => panic!("port {port:?} closed mid-test"),
        })
        .collect()
}

/// Every row of the pinned table, one call per command: **one** notification each, with the
/// exact header/body split (and `None` distinguishable from `Some(vec![])`).
#[test]
fn one_driver_output_call_is_one_notification_with_the_header_apart() {
    let cases: [(&[u8], Vec<u8>, Option<Vec<u8>>); 6] = [
        // driver_output2("HDR",3,"abc",3)
        (b"split", HEADER.to_vec(), Some(b"abc".to_vec())),
        // driver_output2("HDR",3,"abc",0) — present, empty body ⇒ `[H|<<>>]`
        (b"empty", HEADER.to_vec(), Some(Vec::new())),
        // driver_output2("HDR",3,NULL,0) — NULL body ⇒ the proper list `[H]`
        (b"null", HEADER.to_vec(), None),
        // driver_output("abc",3) — all body ⇒ the bare binary
        (b"body", Vec::new(), Some(b"abc".to_vec())),
        // driver_output2(NULL,0,NULL,0) ⇒ `[]`
        (b"nothing", Vec::new(), None),
        // …and the accumulator path, which is the same body-only read
        (b"bank", Vec::new(), Some(b"abc".to_vec())),
    ];
    for (command, header, body) in cases {
        let mut poller = Poller::new().expect("a poll set");
        let (mut ports, id) = answer(&mut poller, command);
        // The command was queued, not written: the port owes nothing yet.
        assert_eq!(ports.get(id).expect("live").input_len(), 0);

        let reads = round_reads(&mut ports, &mut poller);
        assert_eq!(
            reads,
            vec![(header.clone(), body.clone())],
            "one message for {command:?}, header apart from body"
        );

        // `has_body` is what decides the binary tail, and it is exactly what the driver said:
        // a `NULL` body is not an empty body.
        assert_eq!(
            body.is_some(),
            !matches!(command, b"null" | b"nothing"),
            "a NULL body is not an empty body ({command:?})"
        );
        ports.close(&mut poller, id).expect("closes");
    }
}

/// `driver_output2("HDR",3,"abc",0)` and `driver_output2("HDR",3,NULL,0)` differ in exactly one
/// bit — the presence of the binary tail — and the seam keeps them distinguishable.
#[test]
fn an_empty_body_and_a_null_body_are_different_messages() {
    let mut poller = Poller::new().expect("a poll set");
    let (mut ports, empty_id) = answer(&mut poller, b"empty");
    // A second port on the same registry, with the `NULL`-body plan.
    let null_id = ports
        .open(
            &mut poller,
            Box::new(SplitReplyDriver {
                plan: SplitReplyDriver::for_command(b"null"),
            }),
        )
        .expect("the second port opens");
    ports
        .port_command(&mut poller, null_id, b"null", PortCommandOptions::new())
        .expect("queued");

    let outcome = ports
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    let mut with_empty_body: Option<ReadMessage> = None;
    let mut with_null_body: Option<ReadMessage> = None;
    for notification in outcome.notifications {
        let PortNotification::Data { port, read } = notification else {
            panic!("only data is owed here");
        };
        if port == empty_id {
            with_empty_body = Some(read);
        } else if port == null_id {
            with_null_body = Some(read);
        }
    }
    let empty = with_empty_body.expect("the empty-body port answered");
    let null = with_null_body.expect("the NULL-body port answered");
    assert_eq!(empty.header, HEADER.to_vec());
    assert_eq!(null.header, HEADER.to_vec());
    assert!(empty.has_body() && !null.has_body());
    assert_eq!(empty.body, Some(Vec::new()));
    assert_eq!(null.body, None);
    assert_ne!(empty, null, "the two calls are different messages");
    assert_eq!(
        (empty.len(), null.len()),
        (3, 3),
        "…even though both carry the same three header bytes"
    );
    ports.close(&mut poller, empty_id).expect("closes");
    ports.close(&mut poller, null_id).expect("closes");
}

/// Two `driver_output*` calls in one callback are **two** notifications, in call order, and the
/// banked frame is closed before the split read that follows it.
#[test]
fn two_calls_in_one_callback_are_two_messages_in_arrival_order() {
    let mut poller = Poller::new().expect("a poll set");
    let (mut ports, id) = answer(&mut poller, b"two");
    assert_eq!(
        round_reads(&mut ports, &mut poller),
        vec![
            (Vec::new(), Some(b"bank".to_vec())),
            (HEADER.to_vec(), Some(b"abc".to_vec())),
        ],
        "one message per call, in the order the driver made them"
    );
    ports.close(&mut poller, id).expect("closes");
}

/// The port's input accounting is the C's `bytes_in`: `hlen + len`, header included, and only
/// what has not been delivered yet. A second round re-reports nothing.
#[test]
fn input_len_counts_the_header_and_a_second_round_reports_nothing() {
    let mut poller = Poller::new().expect("a poll set");
    let (mut ports, id) = answer(&mut poller, b"split");

    // Drive the queued write inline (`Port::output`) instead of through the round, so the
    // driver has answered but nothing has been harvested yet.
    let written = ports.output(&mut poller, id, b"").expect("the write runs");
    assert_eq!(written, b"split".len(), "the queued command is written once");
    assert_eq!(
        ports.get(id).expect("live").input_len(),
        3 + 3,
        "port_info(P, input) counts hlen + len of the undelivered read"
    );

    // Harvest: exactly one notification, and the accounting drops to zero.
    assert_eq!(
        round_reads(&mut ports, &mut poller),
        vec![(HEADER.to_vec(), Some(b"abc".to_vec()))]
    );
    assert_eq!(ports.get(id).expect("live").input_len(), 0);

    // Nothing is re-reported by a later round, and nothing is re-driven.
    assert!(round_reads(&mut ports, &mut poller).is_empty());
    assert_eq!(ports.get(id).expect("live").input_len(), 0);
    ports.close(&mut poller, id).expect("closes");
}

/// Line mode frames a split read's **body** and stamps every frame with that read's header —
/// the oracle's `driver_output2("HDR",3,"a\nb\n",4)` ⇒ two `{eol, [72,68,82 | <<"a">>]}` /
/// `{eol, [72,68,82 | <<"b">>]}` messages — and a `NULL` body stages no frame at all.
#[test]
fn line_mode_stamps_each_frame_with_its_reads_header() {
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    let id = registry
        .open(
            &mut poller,
            Box::new(SplitReplyDriver {
                plan: vec![
                    Reply::Split {
                        header: HEADER,
                        body: Some(b"a\nb\n"),
                    },
                    // A NULL body: no frame, and it does not disturb the held line.
                    Reply::Split {
                        header: HEADER,
                        body: None,
                    },
                    Reply::Split {
                        header: HEADER,
                        body: Some(b"tail"),
                    },
                ],
            }),
        )
        .expect("the port opens");
    registry.set_line_mode(id, Some(0)).expect("line mode is set");
    registry
        .port_command(&mut poller, id, b"lines", PortCommandOptions::new())
        .expect("queued");
    let outcome = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert!(
        outcome.notifications.is_empty(),
        "a line-mode port reports through the line channel, never as raw Data"
    );
    let frames: Vec<(Vec<u8>, Vec<u8>, bool)> = registry
        .take_line_notifications()
        .into_iter()
        .map(|frame| (frame.header, frame.bytes, frame.eol))
        .collect();
    assert_eq!(
        frames,
        vec![
            (HEADER.to_vec(), b"a".to_vec(), true),
            (HEADER.to_vec(), b"b".to_vec(), true),
        ],
        "each completed line carries the header of the read that produced it; the NULL body \
         produced no frame and the partial line stays held"
    );
    // The end-of-file flush releases the held partial line with no header of its own.
    registry.flush_linebuf(id).expect("the port is live");
    let flushed: Vec<(Vec<u8>, Vec<u8>, bool)> = registry
        .take_line_notifications()
        .into_iter()
        .map(|frame| (frame.header, frame.bytes, frame.eol))
        .collect();
    assert_eq!(
        flushed,
        vec![(Vec::new(), b"tail".to_vec(), false)],
        "the flushed partial line is the oracle's {{noeol, \"tail\"}}, with an empty header"
    );
    registry.close(&mut poller, id).expect("closes");
}

/// The flat byte view keeps every message's bytes in arrival order, header bytes included.
#[test]
fn the_flat_input_view_is_the_concatenation_in_arrival_order() {
    let mut poller = Poller::new().expect("a poll set");
    let (mut ports, id) = answer(&mut poller, b"two");
    // Drive the write inline so the driver has answered without a harvest.
    ports.output(&mut poller, id, b"").expect("the write runs");
    let port = ports.get_mut(id).expect("live");
    assert_eq!(
        port.drain_input(),
        b"bankHDRabc".to_vec(),
        "the banked frame, then the split read's header and body"
    );
    assert_eq!(port.input_len(), 0, "`has_input` is false once nothing is owed");
    ports.close(&mut poller, id).expect("closes");
}
