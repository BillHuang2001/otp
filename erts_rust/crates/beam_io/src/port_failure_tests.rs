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

//! The failure channel's vocabulary (`port_failure.rs`) and its
//! [`DriverContext`] seam, in the sibling file so neither the vocabulary nor `driver.rs` grows a
//! second suite.
//!
//! The oracle shapes these tests encode were measured with a `cc`-built C driver under the system
//! `erl` (OTP 29.1.1 / erts-17.1): `driver_failure_posix(p, EINVAL)` ⇒ `{'EXIT',P,einval}`,
//! `driver_failure_atom(p,"enospc")` ⇒ `{'EXIT',P,enospc}`, `driver_failure(p,42)` ⇒ `{'EXIT',P,42}`,
//! `driver_failure_eof(p)` ⇒ `{'EXIT',P,normal}`, `driver_exit(p,0)`/`driver_exit(p,ENOSPC)` ⇒
//! **nothing** (the C unlinks the process first, `io.c:7244`). The end-to-end half over a real
//! `.so` is `../../../tests/abi_failure.rs`.

use std::collections::VecDeque;

use beam_common::BeamError;
use beam_process::PortId;

use super::{DriverFailure, FailureReason, PortFailure};
use crate::completion::{CompletionEvent, InetAsyncResult};
use crate::driver::{DriverContext, PortArms};
use crate::poller::Poller;
use crate::port_task::ReadMessage;
use crate::queue::ByteQueue;

// ---------------------------------------------------------------------------------------------
// The reason vocabulary
// ---------------------------------------------------------------------------------------------

/// Every `driver_failure*` entry point renders the reason the oracle prints — and `driver_exit`
/// differs from the family in the one way the C differs: it signals nobody.
#[test]
fn the_c_entry_points_map_onto_the_measured_reasons() {
    // driver_failure_posix(p, EINVAL) ⇒ {'EXIT',P,einval}
    let posix = DriverFailure::posix(libc::EINVAL);
    assert_eq!(posix.reason, FailureReason::Posix(libc::EINVAL));
    assert_eq!(posix.reason.atom_name().as_deref(), Some("einval"));
    assert_eq!(posix.reason.code(), None);
    assert!(posix.signals_owner, "the driver_failure* family signals the owner");

    // driver_failure_atom(p, "enospc") ⇒ {'EXIT',P,enospc}
    let atom = DriverFailure::atom("enospc");
    assert_eq!(atom.reason.atom_name().as_deref(), Some("enospc"));
    assert!(atom.signals_owner);

    // driver_failure(p, 42) ⇒ {'EXIT',P,42} — the integer, not an atom.
    let integer = DriverFailure::error(42);
    assert_eq!(integer.reason, FailureReason::Code(42));
    assert_eq!(integer.reason.code(), Some(42));
    assert_eq!(
        integer.reason.atom_name(),
        None,
        "the oracle sends the integer 42, so there is no reason atom"
    );
    assert!(integer.signals_owner);

    // driver_failure_eof(p) ⇒ {'EXIT',P,normal}, and it is the eof shape.
    let eof = DriverFailure::eof();
    assert_eq!(eof.reason.atom_name().as_deref(), Some("normal"));
    assert!(eof.is_eof());
    assert!(eof.signals_owner);

    // driver_exit(p, 0) ⇒ nothing at all: the C unlinked the process first (io.c:7244).
    let exit_zero = DriverFailure::exit(0);
    assert_eq!(exit_zero.reason, FailureReason::Normal);
    assert_eq!(exit_zero.reason.atom_name().as_deref(), Some("normal"));
    assert!(
        !exit_zero.is_eof(),
        "driver_exit is the flush-less normal close (io.c:7246), not the eof shape"
    );
    assert!(
        !exit_zero.signals_owner,
        "driver_exit unlinks the connected process, so no 'EXIT' reaches its owner"
    );

    // driver_exit(p, ENOSPC) ⇒ nothing, with the errno's name as the (unobserved) reason.
    let exit_errno = DriverFailure::exit(libc::ENOSPC);
    assert_eq!(exit_errno.reason.atom_name().as_deref(), Some("enospc"));
    assert!(!exit_errno.signals_owner);
}

/// `driver_failure(port, 0)` is the C's `eof` flag, not reason `0`
/// (`driver_failure_term(ix, make_small(code), code == 0)`, `io.c:7258`).
#[test]
fn a_zero_code_failure_is_the_eof_shape() {
    let zero = DriverFailure::error(0);
    assert_eq!(zero.reason, FailureReason::Eof);
    assert!(zero.is_eof(), "code 0 sets the C's eof flag");
    assert_eq!(zero.reason.atom_name().as_deref(), Some("normal"));
    assert!(zero.signals_owner);

    // A non-zero code is never the eof shape, even though both close the port.
    assert!(!DriverFailure::error(1).is_eof());
}

/// An errno the crate's table does not name still renders through the C's `errno_<N>` fallback, so
/// a reason is never silently dropped (`ERTS_UNKNOWN_ERRNO_FORMAT`, `erl_errno_str.c`).
#[test]
fn an_unnamed_errno_renders_through_the_c_fallback() {
    assert_eq!(
        DriverFailure::posix(4711).reason.atom_name().as_deref(),
        Some("errno_4711")
    );
    assert_eq!(
        DriverFailure::exit(4711).reason.atom_name().as_deref(),
        Some("errno_4711"),
        "driver_exit names its errno through the same rule as driver_failure_posix"
    );
}

/// A [`PortFailure`] binds a port to the failure its driver reported (the shape the runtime
/// drains, mirroring [`PortCompletion`](crate::completion::PortCompletion)).
#[test]
fn a_port_failure_binds_the_port_to_the_reason() {
    let port = PortId::new(7);
    let failure = PortFailure::new(port, DriverFailure::posix(libc::EINVAL));
    assert_eq!(failure.port, port);
    assert_eq!(failure.failure.reason.atom_name().as_deref(), Some("einval"));
}

// ---------------------------------------------------------------------------------------------
// The `DriverContext` seam
// ---------------------------------------------------------------------------------------------

/// A context over throwaway queues, with every sink the caller asks for.
macro_rules! context {
    ($poller:ident, $input:ident, $output:ident, $arms:ident) => {
        DriverContext::new(
            PortId::new(0),
            &mut $poller,
            &mut $input,
            &mut $output,
            &mut $arms,
        )
    };
}

/// Without a sink a driver's failure is the typed `unsupported` — a bare context has no port to
/// close, and quietly dropping a port-closing request would strand the owner.
#[test]
fn signalling_a_failure_without_a_sink_is_the_typed_unsupported() {
    let mut poller = Poller::new().expect("a poll set");
    let mut input = ByteQueue::new();
    let mut output = ByteQueue::new();
    let mut arms = PortArms::default();
    let mut ctx = context!(poller, input, output, arms);

    let error = ctx
        .signal_failure(DriverFailure::posix(libc::EINVAL))
        .expect_err("no sink is installed");
    assert!(
        error.to_string().contains("no failure sink"),
        "the diagnostic names the missing sink: {error}"
    );
}

/// The seam latches **exactly one** failure: a driver that reports two (a `driver_failure_posix`
/// followed by a `driver_exit`, say) leaves the first in place, because the C's first call already
/// closed the port.
#[test]
fn the_first_failure_is_the_one_that_latches() {
    let mut poller = Poller::new().expect("a poll set");
    let mut input = ByteQueue::new();
    let mut output = ByteQueue::new();
    let mut arms = PortArms::default();
    let mut slot = None;
    let mut ctx = context!(poller, input, output, arms).with_failure(&mut slot);

    ctx.signal_failure(DriverFailure::posix(libc::EINVAL))
        .expect("the sink is installed");
    ctx.signal_failure(DriverFailure::exit(0))
        .expect("a repeated failure is accepted and ignored");
    assert_eq!(
        slot,
        Some(DriverFailure::posix(libc::EINVAL)),
        "the first reported failure is the one the port acts on"
    );
}

/// Once a failure is latched, the delivery seams answer the typed error instead of handing the
/// owner data from a port that is closing — the Rust analogue of the C's `-1` once the port is on
/// its way out.
#[test]
fn a_read_or_completion_after_the_failure_is_refused() {
    let mut poller = Poller::new().expect("a poll set");
    let mut input = ByteQueue::new();
    let mut output = ByteQueue::new();
    let mut arms = PortArms::default();
    let mut reads = VecDeque::new();
    let mut completions = Vec::new();
    let mut slot = None;
    {
        let mut ctx = context!(poller, input, output, arms)
            .with_reads(&mut reads)
            .with_completions(&mut completions)
            .with_failure(&mut slot);
        // Before the failure both seams work.
        ctx.deliver_read(b"HDR", Some(b"abc"))
            .expect("the read sink is installed");
        ctx.signal_failure(DriverFailure::eof())
            .expect("the failure sink is installed");
        // After it, neither does.
        for error in [
            ctx.deliver_read(b"", Some(b"late"))
                .expect_err("a read after the failure is refused"),
            ctx.deliver_completion(CompletionEvent::InetAsync {
                id: 1,
                result: InetAsyncResult::Ok,
            })
            .expect_err("a completion after the failure is refused"),
        ] {
            let BeamError::Raise(_) = error else {
                panic!("the refusal is the typed unsupported raise, got {error:?}");
            };
            assert!(
                error.to_string().contains("port failure"),
                "the diagnostic names the latched failure: {error}"
            );
        }
    }
    assert_eq!(
        reads,
        VecDeque::from(vec![ReadMessage {
            header: b"HDR".to_vec(),
            body: Some(b"abc".to_vec()),
        }]),
        "the pre-failure read is delivered exactly once and nothing follows it"
    );
    assert!(
        completions.is_empty(),
        "the refused completion is never staged"
    );
    assert_eq!(slot, Some(DriverFailure::eof()));
}
