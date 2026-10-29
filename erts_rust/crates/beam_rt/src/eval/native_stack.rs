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

//! **How much native stack this thread has left** — the physical measurement behind the
//! tree-walker's application budget ([`super::nesting`]).
//!
//! # Why a measurement instead of a constant
//!
//! A walker closure application costs an *unknown* amount of Rust stack: it depends on the shape
//! of the clause body (`G(N) -> G(N - 1)` is a handful of small frames, `G(N) -> [N | G(N - 1)]`
//! adds the whole cons/list operand-evaluation path), and the thread it runs on is not
//! necessarily the binary's 64 MiB `beam_rt-driver` thread — libtest gives every test a **2 MiB**
//! thread, and `main` has 8 MiB. So no per-level constant can answer "may one more level be
//! walked here?": the same `WALK_APPLY_LIMIT` is safe on one thread and an abort on another.
//! [`headroom_bytes`] answers it from the real stack bounds, so the budget's decision is
//! physical: walk while the remaining stack is above the reserve, take the compiled route
//! otherwise.
//!
//! # The two paths
//!
//! * **`linux`/`gnu` (exact).** `pthread_getattr_np` + `pthread_attr_getstack` give this thread's
//!   stack bounds; the probe runs **once per thread** and the bounds are cached, so the per-call
//!   work is one thread-local read, the address of a local and a subtraction. The main thread and
//!   every pthread (libtest's and `crate::main`'s driver thread alike) are covered, because glibc
//!   answers for the main thread too.
//! * **Every other target (an estimate).** The first probe records the stack pointer, and later
//!   probes answer `ASSUMED_THREAD_STACK_FLOOR` minus the depth grown since. The floor is 2 MiB —
//!   std's default thread stack, i.e. the smallest stack the entry points this crate supports
//!   hand out (libtest 2 MiB, `main` 8 MiB, the driver thread 64 MiB). This path is an
//!   *estimate*: it cannot see the stack top, so it measures from the first probe inward and is
//!   optimistic by whatever was already consumed above it. It is honest about that in the
//!   return value only insofar as `None` means "no answer" — see the cost note below.
//!
//! # Cost and behaviour neutrality
//!
//! One probe is a thread-local read plus an address computation: nothing is allocated, locked or
//! logged, and no term, heap or table is read — so no result, exception or exit code can change.
//! The caller ([`super::nesting::Slot::enter`]) is on the walker's per-application path, which is
//! why the bounds must be cached rather than re-derived per call (`pthread_getattr_np` reads
//! `/proc/self/maps` internally and costs microseconds).

// The crate is `#![deny(unsafe_code)]` (workspace `CONTEXT.md` §3): `unsafe` is confined to
// explicitly-marked modules, and this is one of them. The module's *entire* raw surface is the
// three glibc pthread calls in [`stack_low_bound`], which take a pthread attribute object by
// pointer, initialise or read it, and return a status word — no Rust memory is read through them
// and no borrow outlives the call.
#![allow(unsafe_code)]

use core::cell::Cell;

/// The stack size assumed when the platform gives no stack bounds: std's default thread stack
/// (`std::thread`'s 2 MiB), i.e. the smallest stack this crate's supported entry points use.
///
/// See the module docs: the portable path measures from its first probe *inward*, so this is a
/// floor for the estimate, not a claim about the thread's real size.
const ASSUMED_THREAD_STACK_FLOOR: usize = 2 * 1024 * 1024;

thread_local! {
    /// This thread's probe, taken on the first [`headroom_bytes`] call and reused afterwards.
    static PROBE: Cell<Option<Probe>> = const { Cell::new(None) };
}

/// One thread's stack measurement (see [`probe`]).
#[derive(Clone, Copy, Debug)]
struct Probe {
    /// The stack pointer at the first probe — the portable path's only reference point.
    base_sp: usize,
    /// The thread's stack low address, when the platform answered exactly.
    low: Option<usize>,
}

/// Bytes of native stack still available below the current stack pointer on this thread, or
/// `None` when the platform gives no answer at all (the caller then keeps its counter-only
/// behaviour; on the targets this crate supports the answer is always `Some`).
///
/// "Available" is what the OS will let this thread still touch: the distance from the current
/// stack pointer down to the bottom of the thread's stack mapping, i.e. without subtracting any
/// guard page — the caller's reserve is what keeps the guard untouched.
pub(crate) fn headroom_bytes() -> Option<usize> {
    let sp = current_sp();
    let probe = probe(sp);
    match probe.low {
        // Contracted: `sp` lives inside `[low, low + size)` (this very call runs on the stack),
        // so the difference is the live headroom and `saturating_sub` is only belt-and-braces.
        Some(low) => Some(sp.saturating_sub(low)),
        None => Some(
            ASSUMED_THREAD_STACK_FLOOR.saturating_sub(probe.base_sp.saturating_sub(sp)),
        ),
    }
}

/// This thread's [`Probe`], measured on the first call and cached for the thread's life.
///
/// `sp` is the caller's stack pointer, so the portable path's reference point is the caller's
/// frame rather than this function's.
fn probe(sp: usize) -> Probe {
    PROBE.with(|cell| match cell.get() {
        Some(probe) => probe,
        None => {
            let probe = Probe {
                base_sp: sp,
                low: stack_low_bound(),
            };
            cell.set(Some(probe));
            probe
        }
    })
}

/// The current stack pointer, as the address of a local.
///
/// Taking the address of a local is safe; nothing is read through the pointer, and the value is
/// used only as an address comparison against the thread's stack bounds. The compiler is entitled
/// to place `anchor` anywhere in the current frame, which is exactly what makes it a stack-pointer
/// sample: the answer carries at most one frame of slack, well inside the caller's reserve.
fn current_sp() -> usize {
    let anchor = 0u8;
    core::ptr::addr_of!(anchor) as usize
}

/// The low (smallest) address of this thread's stack, from glibc's own pthread attributes.
///
/// `pthread_getattr_np` answers for the calling thread, the main thread included; the attribute
/// object is destroyed before returning, whatever the outcome.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn stack_low_bound() -> Option<usize> {
    // SAFETY: all three calls are glibc's own pthread attribute API, reached through the standard
    // `libc` bindings. `pthread_getattr_np` initialises the attribute object it is handed (which
    // is why it is uninitialised storage here) and reports failure through its status word;
    // `pthread_attr_getstack` reads that initialised object and writes two out-parameters that are
    // valid locals; `pthread_attr_destroy` releases the object and takes nothing else. No Rust
    // reference is held across the calls (the raw pointers come from `MaybeUninit`/locals), the
    // object never escapes this function, and the calls are documented as async-signal-unsafe
    // only — this is ordinary code, never a signal handler.
    unsafe {
        let mut attr = core::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
        if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
            return None;
        }
        let mut attr = attr.assume_init();
        let mut addr: *mut libc::c_void = core::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let status = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        if status != 0 || addr.is_null() || size == 0 {
            return None;
        }
        Some(addr as usize)
    }
}

/// `None`: this target has no pthread stack-bounds API this module can call, so the portable
/// estimate ([`ASSUMED_THREAD_STACK_FLOOR`]) is the answer.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn stack_low_bound() -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe answers on every supported host, with a figure of the right order of magnitude:
    /// a thread always has *some* stack left, and never more than a generous ceiling. A `None`
    /// here would silently switch the caller back to counter-only behaviour, which is exactly the
    /// abort this module exists to prevent.
    #[test]
    fn the_probe_answers_with_a_plausible_figure() {
        let headroom = headroom_bytes().expect("a supported host answers its stack bounds");
        assert!(headroom > 0, "a running thread has stack left: {headroom}");
        assert!(
            headroom <= 1 << 30,
            "headroom {headroom} exceeds any stack this crate's entry points hand out"
        );
    }

    /// glibc really answers the stack bounds (not the portable estimate): this is the assertion
    /// that keeps the `cfg` gate and the target honest, the same way `malloc.rs` pins its lever.
    #[test]
    fn the_glibc_bounds_are_wired_up() {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            let low = stack_low_bound().expect("glibc must answer this thread's stack bounds");
            let sp = current_sp();
            assert!(
                low < sp,
                "the stack grows down, so its low bound {low:#x} must be below the pointer {sp:#x}"
            );
            assert!(
                sp - low <= 1 << 30,
                "the probed bounds place the stack pointer {} GiB above the stack",
                (sp - low) >> 30
            );
        }
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        assert_eq!(
            stack_low_bound(),
            None,
            "only glibc has a stack-bounds API here; the estimate is the answer"
        );
    }

    /// The measurement **tracks the real stack**: descending `LEVELS` frames that each keep their
    /// own live, black-boxed buffer must cost at least that many buffer bytes. If the probe were
    /// a constant (or measured the wrong direction, or the wrong thread) this test would fail —
    /// and an under-estimate here is precisely what turns a deep recursion into an abort.
    #[test]
    fn the_measurement_shrinks_as_the_stack_grows() {
        const LEVELS: usize = 64;
        const BUFFER: usize = 1024;

        /// Descends `levels` frames, each holding a live buffer, and answers the headroom at the
        /// bottom — `#[inline(never)]` so every level really is a frame.
        #[inline(never)]
        fn descend(levels: usize) -> usize {
            let mut buffer = [0u8; BUFFER];
            buffer[levels % BUFFER] = 1;
            // Forces the buffer to stay live and materialised in this frame.
            let keep = std::hint::black_box(&buffer);
            let headroom = if levels == 0 {
                headroom_bytes().expect("a supported host answers its stack bounds")
            } else {
                descend(levels - 1)
            };
            assert_eq!(keep[levels % BUFFER], 1);
            headroom
        }

        let top = headroom_bytes().expect("a supported host answers its stack bounds");
        let bottom = descend(LEVELS);
        let grown = top.checked_sub(bottom).unwrap_or_else(|| {
            panic!("the headroom grew from {top} to {bottom} while the stack grew")
        });
        assert!(
            grown >= LEVELS * BUFFER,
            "{LEVELS} frames of {BUFFER} bytes consumed only {grown} bytes of headroom"
        );
    }

    /// The probe is cached per thread: two calls from the same frame answer (nearly) the same
    /// figure, and the bounds are taken once rather than re-derived per call.
    #[test]
    fn the_probe_is_cached_per_thread() {
        let measured = probe(current_sp());
        let first = headroom_bytes().expect("a supported host answers its stack bounds");
        let second = headroom_bytes().expect("a supported host answers its stack bounds");
        let slack = first.abs_diff(second);
        assert!(
            slack <= 4096,
            "two back-to-back probes differ by {slack} bytes, so the bounds are re-derived"
        );
        assert_eq!(
            probe(current_sp()).low,
            measured.low,
            "the thread's bounds must not be re-measured"
        );
    }
}
