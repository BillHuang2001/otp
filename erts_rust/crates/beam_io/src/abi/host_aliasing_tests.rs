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

//! The **release-profile aliasing guard** for [`AbiHost`] (contract A.4).
//!
//! A driver callback is C: it reaches the host through the [`DriverEnv`](beam_abi::driver::host::DriverEnv)
//! behind the `ErlDrvPort` it was handed, while the Rust frame that started the callback is still on
//! the stack. [`crate::abi::host`]'s module docs (*The aliasing contract*) state the rule this suite
//! enforces behaviourally: the callback-visible state lives behind interior mutability, so a live
//! `&AbiHost` held by that Rust frame cannot be `noalias readonly` and the callback's stores survive
//! the optimiser.
//!
//! # Why a `--release` run is the point
//!
//! The failure this guards is **silent and release-only**: with a `Freeze` host (every field plain)
//! and a caller-side `&H` live across the call, LLVM legally treats the write the callback makes
//! through the environment as unobservable and answers the caller's later read from the value it
//! already had. Measured with the exact shape of [`session`] below (a `#[inline(never)]` callback
//! frame writing through the environment's pointer, a `#[inline(never)]` caller frame reading through
//! its own `&Host`):
//!
//! | host shape | `--profile dev` | `--release` |
//! |---|---|---|
//! | `struct H { busy: bool }` (not interior-mutable) | `(false, true)` | `(false, false)` — store **lost** |
//! | `struct H { busy: RefCell<bool> }` (the shipped shape) | `(false, true)` | `(false, true)` |
//!
//! So `cargo test -p beam_io` in **both** profiles is the gate; a `--release` run is not optional
//! for this file. Nothing here drives a real C fixture: the hazard is a property of the Rust frames
//! around the callback, and reproducing it needs no `dlopen` (see `flush_tests.rs`/`tests/abi_busy.rs`
//! for the C-ABI suites that do).

use std::time::Instant;

use beam_abi::driver::host::DriverEnv;

use super::{AbiHost, LocalAtomPortHost};

/// A host with no driver identity and a private atom table — the shape the queue/timer/busy hooks
/// need (`AbiHost::new`), which is also what a `dlopen`ed port's host is, plus a driver identity.
fn new_host() -> AbiHost {
    AbiHost::new(Box::new(LocalAtomPortHost::new())).expect("the driver heap allocates")
}

/// What a C driver callback publishes through the `ErlDrvPort` it was handed.
///
/// Four `driver_*` entry points, each writing a different piece of the host's callback-visible
/// state through the environment's own pointer: `driver_set_busy_port` (the per-port busy flag),
/// `driver_set_timer` (the port deadline) and `driver_enq` (the driver queue). `#[inline(never)]` is
/// load-bearing: it keeps the stores in a frame the caller's optimiser analyses as an opaque call,
/// which is exactly the C-callback situation.
#[inline(never)]
fn callback_publishes(env: &DriverEnv) {
    // The `-1` a `DriverHost` hook answers for "unsupported" is irrelevant here: these four are
    // implemented by `AbiHost`; what the test reads is the state they wrote.
    let _ = env.host().set_busy_port(true);
    let _ = env.host().set_timer(core::ptr::null_mut(), 1_500);
    let _ = env.host().enq(b"published", false);
}

/// The caller frame: `shared` (a **live shared borrow of the host**) is held across the call into
/// the driver, and the callback's effect is read back through that same borrow.
#[inline(never)]
fn session(shared: &AbiHost, env: &DriverEnv) -> ((bool, bool, usize), (bool, bool, usize)) {
    let observe = || {
        (
            shared.is_busy(),
            shared.state().deadline().is_some(),
            shared.state().queue_len(),
        )
    };
    let before = observe();
    callback_publishes(env);
    let after = observe();
    (before, after)
}

/// **The trap guard.** A callback's stores through the environment must be visible to a caller that
/// held `&AbiHost` across the call, in `--release` as well as `debug`.
///
/// This is the assertion a sibling crate's release-only A.4 defect would have failed: with a
/// non-interior-mutable host the `after` tuple comes back equal to `before` once optimised (see the
/// table in this module's docs), and the runtime then behaves as though the driver never called
/// `set_busy_port`/`set_timer`/`enq` at all — a silent no-op, not a wrong value and not a crash.
#[test]
fn a_callback_store_through_the_env_survives_a_live_shared_borrow() {
    let mut host = new_host();
    let env = DriverEnv::new(&mut host);
    // The caller's own reference to the host, live across `session`'s call into the "driver". This
    // is the borrow whose `noalias`/`dereferenceable` attributes the optimiser derives, and the
    // reason the host's state must be interior-mutable.
    let shared: &AbiHost = &host;

    let (before, after) = session(shared, &env);

    assert_eq!(
        before,
        (false, false, 0),
        "a fresh host is idle, has no deadline and an empty driver queue"
    );
    assert_eq!(
        after,
        (true, true, 9),
        "the callback's stores through the environment were dropped: the host's callback-visible \
         state lost its interior mutability (see this module's docs, and `crate::abi::host`'s \
         *The aliasing contract*). This is an A.4 correctness regression, not a style change."
    );
}

/// **The structural guard.** Every piece of callback-visible state is reachable, and mutable,
/// through a **shared** borrow of the host.
///
/// A `Freeze` host cannot pass this test at all — the statements below would need `&mut AbiHost`,
/// which is exactly what the driver callback's caller must not have. So a "cleanup" that turns
/// [`AbiHost::state`]'s `RefCell` back into a plain field fails to compile here, before the
/// optimiser ever gets a chance to drop a store.
#[test]
fn the_callback_visible_state_is_reachable_through_a_shared_borrow() {
    let host = new_host();
    let shared: &AbiHost = &host;

    // Writes through the shared borrow — the shape the `driver_*` hooks use from inside a
    // callback, and the shape that keeps `AbiHost` non-`Freeze`.
    shared.state_mut().busy = true;
    shared.state_mut().deadline = Some(Instant::now());
    shared.state_mut().ioq.push(b"x");

    assert!(shared.is_busy(), "the busy latch is readable via `&self`");
    assert!(
        shared.state().deadline().is_some(),
        "the deadline is readable via `&self`"
    );
    assert_eq!(shared.state().queue_len(), 1);
    assert_eq!(shared.queue_head().as_deref(), Some(&b"x"[..]));

    // The same is true of the installed-context slot and the closing latch (both `Cell`s), which
    // are the other two pieces of state a callback writes.
    assert!(!shared.has_ctx(), "no callback is in flight");
    shared.closing.set(true);
    assert!(
        shared.closing.get(),
        "the closing latch is shared-mutable too"
    );
}
