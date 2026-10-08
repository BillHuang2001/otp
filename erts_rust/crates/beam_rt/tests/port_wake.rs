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

//! The **port → mailbox** delivery path of the scheduler: a process blocked in a compiled
//! `receive` is woken by **port data**, not by an explicit `send` from another process.
//!
//! `tests/scheduler.rs` proves a *process* can wake a blocked receiver; this test proves the
//! other producer can. The receiver is a real `.beam` image
//! (`tests/fixtures/port_recv.beam`, built with the system `erlc` — recipe in
//! `tests/fixtures/README.md`) whose `recv/0` opens a spawn-driver port and then blocks on
//! its stream:
//!
//! ```erlang
//! recv() ->
//!     Port = open_port({spawn, "echo hi"}, [binary]),
//!     receive {Port, {data, Data}} -> Data end.
//! ```
//!
//! Nothing ever `send`s to this process, so the only way it can return is through the
//! runtime's own scheduler port half ([`Runtime::run_scheduler`] → the `beam_io` poller →
//! `Runtime::deliver_port_messages` → the owning process's mailbox → `Scheduler::resume_woken`
//! re-enqueues the receiver). The child's startup is asynchronous, so the scheduler is driven
//! in a bounded loop: each `run_scheduler()` call polls the port for its
//! `PORT_POLL_SLICE` budget, and the loop retries until the data has arrived.
//!
//! Oracle (system `erl`, OTP 29 / `erts-17.1`):
//!
//! ```none
//! $ erl -noshell -eval 'io:format("~p~n",[port_recv:recv()])' -s init stop
//! <<"hi\n">>
//! ```

use std::thread::sleep;
use std::time::{Duration, Instant};

use beam_engine::terms;
use beam_rt::Runtime;

/// The committed receiver fixture (`tests/fixtures/port_recv.beam`).
const PORT_RECV_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/port_recv.beam"
);

/// A receiver blocked on a **port**'s stream is woken by the data the runtime's poller
/// delivers into its mailbox — the port half of the scheduler pass, with no `send` involved.
///
/// The child (`echo hi`) starts asynchronously, so a single `run_scheduler()` call may run
/// before the port has produced anything. The bounded loop below (generous 10 s deadline,
/// short sleeps between calls) makes the test independent of child-startup latency: every
/// call drives the poller for its `PORT_POLL_SLICE`, and the receiver is resumed exactly
/// once the child's output has been read and delivered.
#[test]
fn port_data_wakes_a_blocked_receiver() {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(PORT_RECV_BEAM)
        .expect("the committed fixture loads");
    let pid = rt
        .spawn_process("port_recv", "recv", vec![])
        .expect("spawn port_recv:recv/0");

    // Drive the scheduler until the receiver returns. `run_scheduler()` runs the receiver
    // (which opens the port and blocks in `receive`), then polls the port for
    // `PORT_POLL_SLICE`, delivers `{Port, {data, Data}}` into the receiver's mailbox, wakes
    // it through `resume_woken`, and runs it to its `return` — all inside the one call, once
    // the child has written. Early rounds simply see nothing to deliver.
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "the port never delivered data to wake the receiver"
        );
        rt.run_scheduler().expect("the scheduler pass runs");
        sleep(Duration::from_millis(5));
    }

    // The receiver returned the port's stream as an Erlang binary: `"hi\n"`.
    let result = rt
        .process_result(pid)
        .expect("the receiver returned the port data");
    assert_eq!(
        terms::binary_bytes(result).expect("the port delivered a binary"),
        b"hi\n"
    );
    // It terminated normally (a returning process's exit reason is `normal`, not recorded
    // as an abnormal exit) and left the process table and the run queue.
    assert!(rt.process_exit(pid).is_none());
    assert!(rt.processes().get(pid).is_none());
    assert_eq!(rt.run_queue_len(), 0);
}
