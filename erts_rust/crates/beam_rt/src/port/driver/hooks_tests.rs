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

//! The A.4 **driver-private queue / one-PDL / port-flags hook regression suite** for
//! [`DriverPort`]'s [`beam_abi::DriverHost`] implementation: the driver-private `prt->ioq` that
//! `driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/`driver_peekq[v]` reach (the
//! `state::DriverQueue` behind those hooks), the "a port admits exactly one PDL" rule behind
//! `driver_pdl_create`, and the port **state** flags `driver_get_port_flags` reports.
//!
//! # Which level this pins, and why
//!
//! The preference would be to drive the real `#[no_mangle]` C entry points themselves. They are
//! **not nameable** from `beam_rt`: `beam_abi::driver::drv` re-exports only its `mem` and `port`
//! families (`drv/mod.rs`'s two `pub use self::…::*` lines), while the queue, PDL and lifecycle
//! families live in **private** modules (`mod queues;`, `mod pdl;`, `mod lifecycle;`) — so
//! `driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/`driver_peekq[v]`/`driver_pdl_create`
//! and `get_port_flags` (the C has **no** `driver_` prefix on that one, `erl_driver.h:401`) are
//! out of reach. Even the families that *are* re-exported are `unsafe extern "C" fn`, which this
//! `#![deny(unsafe_code)]` crate may not call. What **is** reachable — and is exactly the seam every
//! one of those entry points delegates to — is the frozen [`beam_abi::DriverHost`] method set on
//! [`DriverPort`], so that is the level pinned here. `beam_abi`'s own suites pin each wrapper's
//! *translation onto* this seam (`queues.rs`'s `NOT_A_SIZE` for a refused `driver_deq`, `pdl.rs`'s
//! `NULL` for a second `driver_pdl_create`, `lifecycle.rs`'s `get_port_flags` /
//! `set_port_control_flags` round trip; `tests/driver_queues.rs` drives the whole C queue surface
//! through that translation), so this suite plus those cover the C surface without duplicating them.
//!
//! # The observable anchors
//!
//! Every case asserts something a host could get **wrong** and the oracle would show:
//!
//! * the queue's *order* — pinned with two **different-length** chunks, so the `driver_peekq` iovec
//!   lengths alone discriminate head from tail without reading a byte back through a raw pointer;
//! * the queue's *byte accounting* — `driver_sizeq` counts bytes, not chunks, and a **fresh** port
//!   answers `0` (the exact divergence: it used to answer the `NOT_A_SIZE` sentinel, which the
//!   wrapper turns into `-1`);
//! * `driver_deq`'s head semantics **and** its whole-queue refusal, which must not move a byte;
//! * the PDL's non-null / one-per-port contract, and its per-port identity across two ports;
//! * that `get_port_flags` reads the port **state** bits (`ERTS_PORT_SFLG_BINARY_IO`, what the
//!   `open_port/2` stream option set) and **not** `prt->control_flags` — the two cells are driven in
//!   opposite directions so a host that conflated them fails on at least one (the C's split is
//!   `io.c:5317-5344`).

use std::rc::Rc;

use beam_abi::driver::types::{PORT_CONTROL_FLAG_BINARY, PORT_FLAG_BINARY, PORT_FLAG_LINE};
use beam_abi::DriverHost;
use beam_alloc::Heap;
use beam_process::ProcessId;
use beam_term::AtomTable;

use super::DriverPort;
use crate::port::StreamType;

/// A host over a fresh atom table, running as pid 1, with a small arena — the same shape the
/// `../driver.rs` unit suite builds.
fn host() -> DriverPort {
    DriverPort::new(
        Heap::new(256).expect("a test heap"),
        Rc::new(AtomTable::new()),
        ProcessId::new(1, 0),
    )
}

/// The `driver_peekq` view's entry lengths, in queue order: the only part of a `SysIOVec` this
/// `#![deny(unsafe_code)]` crate may read without dereferencing `iov_base`.
fn iovec_lengths(view: &[beam_abi::SysIOVec]) -> Vec<usize> {
    view.iter().map(|iovec| iovec.iov_len).collect()
}

/// A **fresh** driver port's private queue is empty and answers the C's own `0`
/// (`io.c:6827-6834`), **not** the `NOT_A_SIZE` sentinel — the divergence this suite exists to keep
/// closed. `driver_peekq` has no view for an empty queue, and `driver_deq` has nothing to drop.
#[test]
fn a_fresh_driver_port_has_an_empty_private_queue() {
    let mut port = host();
    assert_eq!(
        port.sizeq(),
        0,
        "a fresh queue really is 0 bytes, not NOT_A_SIZE"
    );
    assert!(
        port.peekq().is_none(),
        "an empty queue has no iovec view (the C's NULL with *vlenp = 0)"
    );
    assert_eq!(port.deq(1), -1, "nothing to drop: the C's failure code");
    assert_eq!(port.sizeq(), 0, "a refused deq leaves the queue untouched");
    // The boundary the C also accepts: dropping zero bytes from a zero-byte queue is `0`
    // (`erts_ioq_size(&q) == 0 <= 0`), not a refusal.
    assert_eq!(port.deq(0), 0);
    assert_eq!(port.sizeq(), 0);
}

/// `driver_enq` appends at the **tail**, `driver_pushq` prepends at the **head**, both answer the
/// C's success code `0`, and `driver_sizeq` is the **byte** count rather than the chunk count.
///
/// The two chunks are deliberately different lengths (`H` vs `TALL`), so the `driver_peekq` iovec
/// lengths discriminate the order on their own.
#[test]
fn enq_appends_and_pushq_prepends_around_one_queue() {
    let mut port = host();
    assert_eq!(port.enq(b"TALL", false), 0, "the C's success code");
    assert_eq!(port.pushq(b"H", false), 0, "the C's success code");
    assert_eq!(port.sizeq(), 5, "bytes, not chunks");

    let view = port.peekq().expect("a non-empty queue has a view");
    assert_eq!(view.len(), 2, "one iovec entry per enqueue");
    assert_eq!(
        iovec_lengths(&view),
        vec![1, 4],
        "pushq's chunk is the head (`H`), enq's the tail (`TALL`)"
    );
}

/// The seam hands the host a byte slice either way, so the `is_binary` marker a
/// `driver_enq_bin`/`driver_pushq_bin` call carries must not change what the queue holds — only
/// *where* the bytes came from differs in the C (`ErlDrvBinary` window vs plain buffer).
#[test]
fn the_binary_marker_does_not_change_the_queue() {
    let mut port = host();
    assert_eq!(port.enq(b"bb", true), 0);
    assert_eq!(port.pushq(b"p", true), 0);
    assert_eq!(port.sizeq(), 3);
    assert_eq!(iovec_lengths(&port.peekq().expect("a view")), vec![1, 2]);

    // ...and the buffer forms interleave with the binary ones on the same queue.
    assert_eq!(port.enq(b"zzzz", false), 0);
    assert_eq!(port.sizeq(), 7);
    assert_eq!(
        iovec_lengths(&port.peekq().expect("a view")),
        vec![1, 2, 4],
        "binary and buffer enqueues share one queue, in call order"
    );
}

/// A zero-length enqueue is a no-op: the wrapper answers `0` before reaching the hook for an empty
/// input, and the host's own queue never gains a zero-length chunk, so `driver_peekq` keeps exactly
/// one entry per **real** enqueue (the C's `io.c:6827-6834` counts non-empty enqueues).
#[test]
fn an_empty_enqueue_changes_nothing() {
    let mut port = host();
    assert_eq!(port.enq(b"", false), 0);
    assert_eq!(port.pushq(b"", true), 0);
    assert_eq!(port.sizeq(), 0);
    assert!(port.peekq().is_none(), "no chunk was ever queued");

    port.enq(b"x", false);
    assert_eq!(port.enq(b"", false), 0);
    assert_eq!(port.pushq(b"", false), 0);
    assert_eq!(port.sizeq(), 1);
    let view = port.peekq().expect("the one real chunk");
    assert_eq!(
        iovec_lengths(&view),
        vec![1],
        "a zero-length enqueue adds no iovec entry"
    );
}

/// `driver_deq` drops from the **head** and answers the number of bytes dropped; a request larger
/// than the queue is the C's failure code (the wrapper's `NOT_A_SIZE`) and must leave the queue
/// **untouched** — a partial-bite-then-refuse bug would show here.
#[test]
fn deq_drops_from_the_head_or_refuses() {
    let mut port = host();
    port.enq(b"abcd", false);
    port.enq(b"ef", false);
    assert_eq!(port.sizeq(), 6);

    // A partial deq bites into the first chunk only: the rest of `abcd` followed by `ef`.
    assert_eq!(port.deq(3), 3);
    assert_eq!(port.sizeq(), 3);
    assert_eq!(
        iovec_lengths(&port.peekq().expect("a view")),
        vec![1, 2],
        "the first chunk kept its remainder; the second is untouched"
    );

    // More than the queue holds: refused, and nothing moves.
    assert_eq!(port.deq(4), -1);
    assert_eq!(port.sizeq(), 3, "a refused deq leaves the queue untouched");
    assert_eq!(iovec_lengths(&port.peekq().expect("a view")), vec![1, 2]);

    // The boundary: exactly the whole queue empties it and retires the view.
    assert_eq!(port.deq(3), 3);
    assert_eq!(port.sizeq(), 0);
    assert!(port.peekq().is_none());

    // A negative request cannot come from the C (`driver_deq` takes the unsigned `ErlDrvSizeT`),
    // but the seam's signed `c_int` must not corrupt the queue either.
    port.enq(b"safe", false);
    assert_eq!(port.deq(-1), -1);
    assert_eq!(port.sizeq(), 4, "a negative request is refused, not applied");
}

/// `driver_peekq` is one `SysIOVec` per queued chunk, in queue order, each `iov_base` pointing at
/// that chunk's **own** bytes — so the published view really describes the queue — and `None` for
/// an empty queue (the C's `NULL` with `*vlenp = 0`, `io.c:6861-6880`).
#[test]
fn peekq_has_one_entry_per_chunk_and_none_when_empty() {
    let mut port = host();
    assert!(port.peekq().is_none());

    port.enq(b"aa", false);
    port.pushq(b"c", false);
    port.enq(b"bbb", false);

    let view = port.peekq().expect("a non-empty queue has a view");
    assert_eq!(view.len(), 3);
    assert_eq!(
        iovec_lengths(&view),
        vec![1, 2, 3],
        "the iovec order is the queue order (pushq's chunk is the head)"
    );
    // Distinct chunk allocations: one shared buffer would alias every `iov_base`, which the C's
    // per-enqueue chunking explicitly does not do (`io.c:6827-6834`).
    assert_ne!(view[0].iov_base, view[1].iov_base);
    assert_ne!(view[1].iov_base, view[2].iov_base);
    assert_ne!(view[0].iov_base, view[2].iov_base);
}

/// The teardown path drops the queue: `clear_driver_state` — what `DriverPortAdapter::stop` and its
/// `Drop` call — leaves the port's private queue empty, so a late `driver_sizeq`/`driver_peekq` sees
/// nothing rather than bytes of a port that has gone away. Clearing an already-empty queue is a
/// no-op, never a panic.
#[test]
fn clear_driver_state_empties_the_queue() {
    let mut port = host();
    port.enq(b"pending", false);
    assert_eq!(port.sizeq(), 7);

    port.clear_driver_state();
    assert_eq!(port.sizeq(), 0);
    assert!(port.peekq().is_none());

    port.clear_driver_state();
    assert_eq!(port.sizeq(), 0, "clearing an empty queue is a no-op");
}

/// `driver_pdl_create` hands out this port's **one** port data lock: a first call is a non-null
/// handle, and a second call on the **same** host is `None` (the wrapper's `NULL` — the C's "a port
/// admits exactly one PDL", `io.c:6716-6787`), which stays true on every further call.
#[test]
fn a_port_holds_exactly_one_pdl() {
    let mut port = host();
    let first = port.pdl_create().expect("the first call admits a PDL");
    assert!(
        !first.is_null(),
        "the C's handle is opaque, but it is never NULL on success"
    );
    assert!(
        port.pdl_create().is_none(),
        "a second call on the same port is the C's NULL"
    );
    assert!(port.pdl_create().is_none(), "and so is every call after it");
}

/// The PDL's identity is **per port**: two ports get distinct non-null handles, and each still
/// admits only its own one (a rebuilt host is a different port and answers its own fresh
/// `driver_pdl_create`, exactly as a restarting driver would see).
#[test]
fn two_ports_get_distinct_pdl_handles() {
    let mut first = host();
    let mut second = host();
    let a = first.pdl_create().expect("the first port admits a PDL");
    let b = second.pdl_create().expect("the second port admits a PDL");
    assert!(!a.is_null() && !b.is_null());
    assert_ne!(a, b, "the handle identity is the port, so two ports differ");
    assert!(first.pdl_create().is_none());
    assert!(second.pdl_create().is_none());
}

/// `driver_get_port_flags` (`get_port_flags`, `erl_driver.h:401`) reports the port's **state** stream
/// flags, derived from the `open_port/2` stream option — `PORT_FLAG_BINARY` (1) for a `binary` port,
/// `0` for a `list` one (`io.c:5325-5339`) — and **not** the `port_control` reply flags
/// `set_port_control_flags` records into `prt->control_flags` (`io.c:5318`). The two cells are driven
/// in **opposite** directions so a host that conflated them fails whichever way it collapsed.
///
/// Oracle ground truth (OTP 29 / erts-17.1), observed through `port_control/3` **after** the port is
/// open: `open_port({spawn_driver, Name}, [binary])` → `"1"`, `[line]` → `"2"`, no option → `"0"`.
/// (A C driver that calls `get_port_flags` from **inside its own `start`** gets `0` in every case:
/// the C ORs the bits into `port->state` only *after* `open_driver` returns,
/// `erl_bif_port.c:1139-1143`. Visible through a later callback only — see `Known Issues`.)
#[test]
fn get_port_flags_reports_the_stream_not_the_control_flags() {
    // The provider computes the flags from the resolved stream option; the same bits the oracle
    // reports (`erl_driver.h`'s `PORT_FLAG_BINARY`/`PORT_FLAG_LINE`).
    assert_eq!(StreamType::Binary.port_flags(), PORT_FLAG_BINARY);
    assert_eq!(StreamType::List.port_flags(), 0);

    // A host nobody wired keeps the C's default `list` stream (`io.c:5338`).
    let mut bare = host();
    assert_eq!(bare.get_port_flags(), Some(0));

    // A `[binary]` port answers the oracle-observed `1` (every A.4 corpus fixture opens
    // `open_port({spawn_driver, …}, [binary])`).
    let mut binary = host().with_port_flags(StreamType::Binary.port_flags());
    assert_eq!(binary.get_port_flags(), Some(PORT_FLAG_BINARY));

    // ...and the port state bits are independent of the control flags: clearing the control flags
    // (what a `control` callback may do) must not clear the binary state bit.
    assert_eq!(binary.set_control_flags(0, 0), 0);
    assert_eq!(binary.control_flags().get(), 0);
    assert_eq!(
        binary.get_port_flags(),
        Some(PORT_FLAG_BINARY),
        "driver_get_port_flags reads the port state bits, not `prt->control_flags`"
    );

    // The other direction: a `list` port whose driver asked for a binary `port_control` reply is
    // still `0` from `driver_get_port_flags`, and its control cell really holds the request.
    let mut list = host().with_port_flags(StreamType::List.port_flags());
    assert_eq!(list.set_control_flags(0, PORT_CONTROL_FLAG_BINARY), 0);
    assert_eq!(list.control_flags().get(), PORT_CONTROL_FLAG_BINARY);
    assert_eq!(
        list.get_port_flags(),
        Some(0),
        "a binary control reply does not make the port a binary port"
    );

    // `PORT_FLAG_LINE` is unreachable here — `[line]`/`{line,N}` is whitelisted-but-inert in
    // `open_port/2`, so no stream resolves to it, while the oracle answers `2` for a `[line]` port —
    // pinned so a future `[line]` wiring changes this deliberately rather than by accident.
    assert_ne!(list.get_port_flags(), Some(PORT_FLAG_LINE));
    assert_ne!(binary.get_port_flags(), Some(PORT_FLAG_LINE));
}

/// Two hosts are two ports, so the flags, the queue and the PDL of one never leak into the other —
/// the private state this module exists for is genuinely per port.
#[test]
fn the_private_state_is_per_port() {
    let mut binary = host().with_port_flags(StreamType::Binary.port_flags());
    let mut list = host().with_port_flags(StreamType::List.port_flags());

    binary.enq(b"only-binary", false);
    let _ = binary.pdl_create().expect("the binary port's PDL");

    assert_eq!(list.sizeq(), 0, "the other port's queue is untouched");
    assert!(list.peekq().is_none());
    assert_eq!(list.get_port_flags(), Some(0));
    // The other port still admits **its own** PDL, independently.
    assert!(!list.pdl_create().expect("its own PDL").is_null());
}
