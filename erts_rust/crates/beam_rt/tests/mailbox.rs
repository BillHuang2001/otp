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

//! The mailbox/receive **vertical slice**: a real `.beam` sends and receives end to end.
//!
//! `tests/slice.rs` proves the runtime can *run* a module; this test proves the piece
//! that made a `receive` impossible until now — the receive/message seam
//! ([`beam_engine::ExecutionContext`]'s nine mailbox methods) is wired to the running
//! process's real [`beam_message::Queue`] and to the per-process scan state that
//! survives a suspend/resume.
//!
//! The image is the committed oracle fixture
//! (`tests/fixtures/mailbox.beam`, reused by relative path so there is a single source
//! of truth for it — never copied; regeneration recipe in
//! `tests/fixtures/README.md`). Every expectation is the value the system `erl` returns
//! for the same call:
//!
//! | export | oracle `erl` expression | value |
//! |---|---|---|
//! | `selfsend/0` | `mailbox:selfsend()` | `hello` |
//! | `scan/0` | `mailbox:scan()` | `7` |
//! | `wait/0` | `mailbox:wait()` | blocks on an empty mailbox |
use beam_engine::blocked;
use beam_rt::Runtime;
use beam_term::{is_atom, signed_value};

/// The committed mailbox fixture (`erlc +deterministic mailbox.erl`).
const MAILBOX_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/mailbox.beam"
);

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// `self() ! hello, receive X -> X end` → `hello` (oracle-confirmed).
///
/// This is the whole point of the change: the `send` instruction reaches
/// `RuntimeContext::send_message`, which copies `hello` into the runtime process's
/// mailbox, and the following `loop_rec`/`remove_message` scan it back out through
/// `receive_next`/`receive_remove`.
#[test]
fn selfsend_delivers_and_receives_hello() {
    let mut rt = runtime();
    let result = rt
        .run_function(MAILBOX_BEAM, "selfsend", 0, &[])
        .expect("mailbox:selfsend/0");

    assert!(is_atom(result), "`hello` is an atom, got {result:?}");
    // The atom resolves through the runtime's one shared table, so the printer names it.
    assert_eq!(rt.atom_name_of(result), Some("hello"));
    assert_eq!(rt.format_term(result, None).expect("printable"), "hello\n");
}

/// `self() ! 42, self() ! {bar, 7}, receive {bar, X} -> X end` → `7`
/// (oracle-confirmed).
///
/// The first message is scanned **past** (`loop_rec_end`) because `42` fails the
/// `{bar, X}` test; only the second is removed.
#[test]
fn scan_skips_a_non_matching_head_and_takes_the_match() {
    let mut rt = runtime();
    let result = rt
        .run_function(MAILBOX_BEAM, "scan", 0, &[])
        .expect("mailbox:scan/0");

    assert_eq!(signed_value(result), 7, "the second message matched");
}

/// `wait/0` on a fresh (empty) mailbox **blocks**.
///
/// [`Runtime::run_function`] is the non-blocking entry, so the suspension surfaces as
/// the typed [`blocked`] capability gap — the engine's single definition of that reason
/// — never a wrong result and never a panic.
#[test]
fn wait_on_an_empty_mailbox_is_the_typed_blocked_error() {
    let mut rt = runtime();
    let error = rt
        .run_function(MAILBOX_BEAM, "wait", 0, &[])
        .expect_err("an empty mailbox blocks the receive");

    assert_eq!(error.to_string(), blocked().to_string());
}
