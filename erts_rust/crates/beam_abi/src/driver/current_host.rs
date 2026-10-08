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

//! The **current driver host** — the thread-local publication that lets the two frozen driver slots
//! with no port operand reach a [`DriverHost`].
//!
//! Every `driver_*` / `erl_drv_*` function whose operand is an `ErlDrvPort` recovers its host from the
//! port itself ([`DriverEnv::from_port`](crate::driver::host::DriverEnv::from_port)). Two slots carry
//! no port at all:
//!
//! * `void add_driver_entry(ErlDrvEntry *drv)` (`erts/emulator/beam/io.c:7640`),
//! * `int remove_driver_entry(ErlDrvEntry *drv)` (`io.c:7698`),
//!
//! because the C functions they back act on the emulator's **process-global** driver list rather
//! than on one port. They still need a host to delegate to, so the runtime publishes the host it is
//! currently running driver code for with [`with_current_host`], and the two slot bodies read it back
//! through the crate-internal `with_published_host`.
//!
//! # Mechanism
//!
//! A `thread_local!` cell holds an `Option<*mut dyn DriverHost>`: a **borrowed** host, published
//! only for the dynamic extent of a [`with_current_host`] call by a scope guard that always restores
//! the previous value — on return *and* on unwind. Nesting therefore restores the outer host, and it
//! is impossible to leave a dangling host published: the cell is reset before the borrow the pointer
//! was made from ends. All driver callbacks for one port run on the thread that drives that port, so
//! a thread-local is the correct scope (`beam_io` owns which thread that is).
//!
//! A slot body that runs **outside** any [`with_current_host`] scope sees "no current host" and then
//! answers the C's failure value without dereferencing anything (the `registry` family of
//! [`crate::driver::drv`] owns the two entry-list slots).
//!
//! # `unsafe`
//!
//! The two functions below are the only place a `&mut dyn DriverHost` is turned into a raw pointer
//! and back. Each `unsafe` block carries a `// SAFETY:` rationale (workspace `CONTEXT.md` §3); the
//! public API itself is safe — [`with_current_host`] is a plain scoped call.

use core::cell::Cell;

use crate::driver::host::DriverHost;

thread_local! {
    /// The host published for this thread, or `None`.
    ///
    /// A raw fat pointer keeps the cell `Copy` and lifetime-free; it is only ever written from a live
    /// `&mut dyn DriverHost` inside [`with_current_host`], and cleared before that borrow ends.
    static CURRENT: Cell<Option<*mut dyn DriverHost>> = const { Cell::new(None) };
}

/// Publish `host` as this thread's current driver host for the duration of `f`.
///
/// The runtime wraps each driver entry point that has no port operand (`add_driver_entry`,
/// `remove_driver_entry` and their `beam_io` callers) in this call, so the slot bodies can delegate
/// to a [`DriverHost`]. The previous host is **always** restored when `f` returns — including when it
/// unwinds — so a nested publication restores the outer host, and no dangling host can be left
/// behind.
///
/// Usage: `with_current_host(&mut host, || drv::add_driver_entry(entry))`.
pub fn with_current_host<R>(host: &mut dyn DriverHost, f: impl FnOnce() -> R) -> R {
    /// Restore the previous publication when the guarded scope ends (also on unwind).
    struct Restore(Option<*mut dyn DriverHost>);

    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|current| current.set(self.0));
        }
    }

    // SAFETY: the only thing reinterpreted here is the erased trait-object lifetime. `&mut dyn
    // DriverHost` and `*mut dyn DriverHost` are layout-identical (data word + vtable word), and the
    // borrow is reproduced exactly when the pointer is reborrowed in `with_published_host`. The
    // pointer is published only for the span of this call (the `Restore` guard clears it), and `host`
    // is borrowed `&mut` for that whole span, so no second `&mut` alias can exist while it is live.
    // (`host as *mut dyn DriverHost` alone is rejected: a raw pointer to a trait object is invariant
    // and defaults to `'static` — rust#141402. `DriverEnv::new` erases its borrow the same way.)
    let raw: *mut dyn DriverHost = unsafe { core::mem::transmute(host) };

    let previous = CURRENT.with(|current| current.replace(Some(raw)));
    let _restore = Restore(previous);

    f()
}

/// Run `f` with this thread's published host, or return `None` when no host is published.
///
/// Crate-internal: this is how the `add_driver_entry` / `remove_driver_entry` slot bodies reach
/// whatever [`with_current_host`] published. A consumer outside this crate can only ever be *inside*
/// a publication, so its correctness is a consequence of [`with_current_host`]'s scoping.
pub(crate) fn with_published_host<R>(f: impl FnOnce(&mut dyn DriverHost) -> R) -> Option<R> {
    let raw = CURRENT.with(|current| current.get())?;
    // SAFETY: `raw` was published by `with_current_host`, which keeps the host alive and exclusively
    // borrowed (`&mut`) for the whole of the `f` it wraps; this reborrow can only run inside that
    // scope, so the host is live and unaliased for the duration of the closure call.
    let host: &mut dyn DriverHost = unsafe { &mut *raw };
    Some(f(host))
}
