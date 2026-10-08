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

//! **Port queues** family of the driver host layer — part of the `driver_*` / `erl_drv_*` C ABI
//! (workspace contract **A.4**).
//!
//! These are the functions a driver uses to read and write a **port's data queue**: append at the
//! tail ([`driver_enq`]/[`driver_enq_bin`]/[`driver_enqv`]), prepend at the head
//! ([`driver_pushq`]/[`driver_pushq_bin`]/[`driver_pushqv`]), inspect it without removing
//! ([`driver_sizeq`]/[`driver_peekq`]/[`driver_peekqv`]), drop from the head ([`driver_deq`]) and
//! linearize a vector into a caller buffer ([`driver_vec_to_buf`]).
//!
//! Each function is a `#[no_mangle] pub unsafe extern "C" fn` matching one field of the frozen
//! [`ErlDrvApi`] table exactly, and each `ErlDrvPort` operand **is** the pointer
//! [`DriverEnv::as_port`](crate::driver::host::DriverEnv::as_port) hands out
//! ([`DriverEnv::from_port`](crate::driver::host::DriverEnv::from_port) recovers it through the
//! shared [`env_for`] helper, exactly as the sibling `port.rs` does). The queue itself lives behind
//! the [`DriverHost`](crate::driver::host::DriverHost) seam hooks `enq`/`pushq`/`deq`/`sizeq`/
//! `peekq`, which the runtime supplies over its real port registry.
//!
//! # Semantics reproduced (ground truth `erts/emulator/beam/io.c` + `erl_io_queue.c`)
//!
//! | function | C | reproduced |
//! |---|---|---|
//! | `driver_enq`/`driver_pushq` | append/prepend at tail/head; `0` = ok, `-1` = failure; `len == 0` returns `0` **before** touching the port | [`driver_enq`]/[`driver_pushq`] delegate to [`DriverHost::enq`](crate::driver::host::DriverHost::enq)/[`pushq`](crate::driver::host::DriverHost::pushq) with `is_binary = false` |
//! | `driver_enq_bin`/`driver_pushq_bin` | enqueue a `bin->orig_bytes[offs .. offs+len]` window | [`driver_enq_bin`]/[`driver_pushq_bin`] with `is_binary = true` |
//! | `driver_enqv`/`driver_pushqv` | enqueue the vector with the first `skip` bytes dropped; `size <= skip` returns `0` without enqueuing | [`driver_enqv`]/[`driver_pushqv`] (gather → `enq`/`pushq`) |
//! | `driver_deq` | removes exactly `size` bytes and returns the **remaining** count; `size > q->size` → `(ErlDrvSizeT)-1` and the queue is untouched | [`driver_deq`] |
//! | `driver_sizeq` | the queued byte count; `(ErlDrvSizeT)-1` for a null/invalid port | [`driver_sizeq`] |
//! | `driver_peekq` | a `SysIOVec*` view of the queue head, `NULL` + `*vlen = 0` when empty, `NULL` + `*vlen = -1` for a null port; **`vlen` counts iovec entries, one per enqueue** | [`driver_peekq`] |
//! | `driver_peekqv` | fills the caller's `ErlIOVec` (`vsize`/`size`/`iov`/`binv`) and returns the queued byte count; zeroes the vector on an empty queue | [`driver_peekqv`] |
//! | `driver_vec_to_buf` | copies `min(len, the vectors' bytes)` into `buf`; returns the bytes copied (`orig_len` as soon as `len` bytes were copied) | [`driver_vec_to_buf`] |
//!
//! **Measured on the oracle** (system OTP 29.1.1 / erts-17.1, a real C driver loaded with
//! `erl_ddll:load_driver/2` and driven through `open_port`/`port_control`) — these are the numbers
//! pinned by `tests/driver_queues.rs`:
//!
//! * `driver_enq` returns `0` on success (not the queue size); `driver_sizeq` = 10 after two 5-byte
//!   enqueues; `driver_peekq` reports `*vlen = 2` with `iov[0].iov_len = 5`; `driver_peekqv` returns
//!   `10` with `ev.vsize = 2`, `ev.size = 10`.
//! * `driver_deq(port, 3)` returns `7` (the remaining count); `driver_deq(port, 100)` on a 7-byte
//!   queue returns `18446744073709551615` (i.e. `(ErlDrvSizeT)-1`) and leaves the queue at 7;
//!   `driver_deq(port, 0)` returns the current count unchanged.
//! * `driver_pushq` ×2 → size 4; `driver_enq_bin(bin, 1, 2)` + `driver_pushq_bin(bin, 0, 2)` → size 8;
//!   `driver_enq(port, "x", 0)` → `0` (a no-op); `driver_enqv(ev(6), 2)` + `driver_pushqv(ev(6), 4)`
//!   → size 14 (each adds `declared size − skip`).
//! * `driver_vec_to_buf(ev(6), buf, 4)` → `4` with `"abcd"`; `(…, 100)` → `6` with `"abcdef"`;
//!   `(…, 0)` → `0`.
//! * On an empty queue `driver_peekq` → `NULL` with `*vlen = 0` and `driver_peekqv` → `0` with
//!   `ev.vsize = 0`.
//!
//! There is **no maximum queue length** in the oracle: the only `driver_enq*` failure is an
//! allocation failure inside the queue (`erl_io_queue.c`'s `expandq` → `-1`, propagated by
//! `io.c:6869`); no `ERTS_DRV_QUEUE_MAX_LEN`-style constant exists anywhere in `erts/`. A
//! host-reported failure (`-1`) is passed straight through, so that is the behaviour the seam
//! reproduces.
//!
//! # Documented deviations from the oracle (never a crash, never a wild pointer)
//!
//! * **Ownership: the seam copies.** The C's `driver_enq`/`driver_pushq` *allocate a driver binary*
//!   and queue that; `driver_enq_bin`/`driver_pushq_bin` *share the caller's* binary and bump its
//!   refcount (the bytes are not copied, and the binary must stay alive until it drains); the `*_v`
//!   forms reference each vector's binary. The seam hands the host a plain byte slice, so every form
//!   **copies** the bytes. Consequence: the caller may free its `ErlDrvBinary` (or reuse its buffer)
//!   immediately after the call — the queued bytes are independent — where the C requires it to keep
//!   the binary alive. The `is_binary` flag still tells the host how the driver presented the bytes.
//! * **`driver_peekq`/`driver_peekqv` publish a per-thread *snapshot*, the C publishes a live view.**
//!   The seam returns an owned `Vec<SysIOVec>`, but the C API returns a pointer the driver keeps
//!   using, so the vector is copied into a thread-local scratch (`QUEUE_VIEW`) whose pointer is
//!   returned. The pointer is valid until the next `driver_peekq*` call **on that thread** (and, like
//!   the C, callers must treat it as invalidated once the queue changes). Differences from the C: the
//!   C's pointer is a live view into the port's own array (so a driver that peeks, mutates the queue
//!   and then re-reads the *old* pointer sees the mutation) while this one is a snapshot; and two
//!   ports peeked on the same thread overwrite each other's view — the C gives each port its own
//!   array.
//! * **`driver_peekqv`'s `binv` array is all-NULL.** The seam's `peekq` supplies only the
//!   `SysIOVec` view, so a parallel thread-local array of `NULL` `ErlDrvBinary*` is published: that is
//!   the C's own encoding for "this vector entry is plain memory, not a binary-backed entry", which
//!   is exactly what the seam's linearized view is. A driver reading `binv[i]` sees `NULL` rather
//!   than the queue's binary; a driver reading only the iovecs (the usual case, e.g. for
//!   `driver_outputv`) is unaffected.
//! * **`driver_enqv`/`driver_pushqv` do not mutate the caller's vector.** The C's `skip()` walk
//!   advances `iov[0].iov_base`/`iov_len` (and, for fully-skipped leading entries, the vector
//!   pointer) **in place in the caller's `ErlIOVec`**, so a second call with the same vector observes
//!   the already-skipped vector. This implementation only reads the vector. A driver that reuses one
//!   `ErlIOVec` for several `*_v` calls therefore sees different bytes than it would on the oracle.
//! * **Byte accounting uses the iovecs, not the declared `size`.** The C adds `vec->size - skip` to
//!   its queue counter *and* queues zero-length entries' references; the seam queues the actual
//!   remaining bytes. For a well-formed vector (`size == Σ iov_len`) the two agree exactly; for a
//!   malformed one the queue count here stays truthful where the C's is inflated. A `skip` that runs
//!   past the declared `iov_len`s is refused (`-1`) rather than read past the vector.
//! * **Refusals where the C would fault.** A null `port`, `buf`, `bin`, `ErlIOVec`, `vlen`/`ev`
//!   out-parameter, an `offs + len` outside the binary's `orig_size`, a negative `vsize` and a
//!   `deq` length above `c_int::MAX` all yield the C's failure code (`-1`/`(ErlDrvSizeT)-1`/`NULL`)
//!   instead of dereferencing or over-reading. `driver_vec_to_buf`, which has no failure code, returns
//!   `0` (the "copied nothing" value) for those cases.
//! * **`driver_peekq` on a host without queues** reports `*vlen = -1` + `NULL`, which is exactly what
//!   the C returns for a null queue (`io.c:6821`); `driver_peekqv`/`driver_sizeq`/`driver_deq` report
//!   `(ErlDrvSizeT)-1`.
//!
//! # `unsafe`
//!
//! The whole file is FFI. Every `unsafe fn` carries a `# Safety` section (its caller contract) and
//! every `unsafe` block a preceding `// SAFETY:` rationale (workspace `CONTEXT.md` §3). All raw
//! operand reads go through the two documented helpers [`read_bytes`]/[`binary_window`] and all
//! pointer recovery through [`env_for`](super::env_for); no runtime data is `unwrap()`ed and no
//! failure path panics.

use core::cell::RefCell;
use libc::{c_char, c_int};

use crate::common::SysIOVec;
use crate::driver::api::ErlDrvApi;
use crate::driver::host::DriverEnv;
use crate::driver::types::{ErlDrvBinary, ErlDrvPort, ErlDrvSizeT, ErlIOVec};

use super::env_for;

/// The C's `(ErlDrvSizeT)-1` — the failure code of `driver_deq`/`driver_sizeq`/`driver_peekqv`
/// (`io.c:6807-6823`). Measured on the oracle as `18446744073709551615` (`SIZE_MAX`).
const NOT_A_SIZE: ErlDrvSizeT = ErlDrvSizeT::MAX;

// ---------------------------------------------------------------------------------------------
// Raw-operand helpers
// ---------------------------------------------------------------------------------------------

/// A borrowed view over the `len` bytes at `ptr` (`None` for a null `ptr` with a non-zero length).
///
/// `port.rs` has the identical private helper; it is not reachable from here (a module-private item
/// is visible only inside its own module) and that file is owned by the sibling family, so the
/// four-line read is repeated rather than shared.
///
/// # Safety
///
/// `ptr` must be null or point at `len` readable bytes.
unsafe fn read_bytes<'a>(ptr: *const u8, len: ErlDrvSizeT) -> Option<&'a [u8]> {
    if ptr.is_null() {
        return if len == 0 { Some(&[]) } else { None };
    }
    // SAFETY: the caller guarantees `len` readable bytes at `ptr`.
    Some(unsafe { core::slice::from_raw_parts(ptr, len) })
}

/// The `len` bytes at byte `offs` inside a driver binary (`None` when the request is malformed).
///
/// Same checked-add rule as `port.rs`'s `binary_payload_bytes`: the C trusts
/// `offs + len <= bin->orig_size` (`io.c:6849`); the **checked add** refuses an out-of-range request
/// instead of reading past the payload. Also repeated for the same privacy reason as [`read_bytes`].
///
/// # Safety
///
/// `bin` must be null or point at a live [`ErlDrvBinary`] whose `orig_bytes` flexible array holds at
/// least `orig_size` bytes.
unsafe fn binary_window<'a>(
    bin: *const ErlDrvBinary,
    offs: ErlDrvSizeT,
    len: ErlDrvSizeT,
) -> Option<&'a [u8]> {
    if bin.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a live `ErlDrvBinary` at `bin`.
    let orig_size = unsafe { (*bin).orig_size };
    if orig_size < 0 || offs.checked_add(len)? > orig_size as ErlDrvSizeT {
        return None;
    }
    // SAFETY: `orig_size` payload bytes are readable at `orig_bytes` (caller contract) and
    // `offs + len <= orig_size` (checked above), so `offs` heads `len` readable bytes (one past the
    // last payload byte is still a valid base for a zero-length view).
    let base = unsafe { core::ptr::addr_of!((*bin).orig_bytes) } as *const u8;
    Some(unsafe { core::slice::from_raw_parts(base.add(offs), len) })
}

/// The bytes of `iov[0 .. vsize]` with the first `skip` bytes dropped, gathered into one buffer.
///
/// Mirrors the C's `skip()` walk (`erl_io_queue.c:193-230`): entries whose `iov_len` is fully covered
/// by the skip are dropped, the first entry that straddles it contributes its tail, and every
/// following entry is taken whole. Returns `None` when the skip runs **past** the declared lengths or
/// an entry with a positive length has a null base — the C would read past the vector there.
///
/// # Safety
///
/// `iov` must be null or point at `vsize` readable [`SysIOVec`] entries, each heading `iov_len`
/// readable bytes.
unsafe fn gather_after_skip(
    iov: *const SysIOVec,
    vsize: c_int,
    skip: ErlDrvSizeT,
) -> Option<Vec<u8>> {
    if vsize < 0 {
        return None;
    }
    if iov.is_null() {
        return if vsize == 0 { Some(Vec::new()) } else { None };
    }
    // SAFETY: the caller guarantees `vsize` readable entries at `iov`.
    let entries = unsafe { core::slice::from_raw_parts(iov, vsize as usize) };
    let mut out = Vec::new();
    let mut left = skip;
    for entry in entries {
        let len = entry.iov_len;
        if left >= len {
            left -= len;
            continue;
        }
        if entry.iov_base.is_null() {
            return None;
        }
        // SAFETY: `iov_base` heads `iov_len` readable bytes (caller contract); the view starts
        // `left` bytes in and runs to the entry's end.
        let bytes = unsafe {
            core::slice::from_raw_parts((entry.iov_base as *const u8).add(left), len - left)
        };
        out.extend_from_slice(bytes);
        left = 0;
    }
    if left != 0 {
        return None; // `skip` ran past the declared lengths: malformed.
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Queue views (`driver_peekq`/`driver_peekqv`)
// ---------------------------------------------------------------------------------------------

/// The per-thread scratch the `driver_peekq*` family publishes its view through.
struct QueueView {
    /// The iovec array handed back to the driver.
    iov: Vec<SysIOVec>,
    /// The parallel `ErlDrvBinary*` array (all `NULL` — see the module header).
    binv: Vec<*mut ErlDrvBinary>,
}

impl QueueView {
    const fn new() -> QueueView {
        QueueView {
            iov: Vec::new(),
            binv: Vec::new(),
        }
    }
}

thread_local! {
    /// The scratch behind [`driver_peekq`]/[`driver_peekqv`].
    ///
    /// **Safety invariant:** the only code that ever mutates this vector is
    /// [`publish_queue_view`], so a pointer returned from a `driver_peekq*` call is valid (and points
    /// at the snapshot taken then) until the next such call on the same thread — at least as long as
    /// the C's "valid until the queue changes" contract. `clear` + `extend_from_slice` keeps the
    /// buffer when it is already large enough, so a repeated peek does not even invalidate it.
    static QUEUE_VIEW: RefCell<QueueView> = const { RefCell::new(QueueView::new()) };
}

/// Copy `iov` into this thread's scratch view, returning `(iov, binv, length)`.
fn publish_queue_view(iov: &[SysIOVec]) -> (*mut SysIOVec, *mut *mut ErlDrvBinary, usize) {
    QUEUE_VIEW.with(|cell| {
        let mut view = cell.borrow_mut();
        view.iov.clear();
        view.iov.extend_from_slice(iov);
        view.binv.clear();
        view.binv.resize(iov.len(), core::ptr::null_mut());
        (view.iov.as_mut_ptr(), view.binv.as_mut_ptr(), view.iov.len())
    })
}

/// Write the C's empty-queue result into `ev` (`erl_io_queue.c:386-388`).
///
/// # Safety
///
/// `ev` must point at a writable [`ErlIOVec`].
unsafe fn clear_iovec(ev: *mut ErlIOVec) {
    // SAFETY: the caller guarantees a writable `ErlIOVec` at `ev`.
    unsafe {
        (*ev).vsize = 0;
        (*ev).size = 0;
        (*ev).iov = core::ptr::null_mut();
        (*ev).binv = core::ptr::null_mut();
    }
}

// ---------------------------------------------------------------------------------------------
// Enqueue
// ---------------------------------------------------------------------------------------------

/// `int driver_enq(ErlDrvPort port, char *buf, ErlDrvSizeT len);` — append `len` bytes at the tail of
/// the port queue (`io.c:6858`).
///
/// Returns the C's `0` for success and `-1` for a null port / buffer or a host failure. `len == 0`
/// returns `0` **before** the port is touched, exactly as the C's early return does.
///
/// # Safety
///
/// `port` must be null or a live port handle; `buf` must be null or point at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn driver_enq(port: ErlDrvPort, buf: *mut c_char, len: ErlDrvSizeT) -> c_int {
    if len == 0 {
        return 0; // `io.c:6864`, before any operand is used.
    }
    // SAFETY: `buf`/`len` are the caller's buffer (contract above).
    let Some(bytes) = (unsafe { read_bytes(buf as *const u8, len) }) else {
        return -1;
    };
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().enq(bytes, false),
        None => -1,
    }
}

/// `int driver_pushq(ErlDrvPort port, char *buf, ErlDrvSizeT len);` — prepend `len` bytes at the head
/// of the port queue (`io.c:6892`). Returns `0`/`-1` like [`driver_enq`], with the same `len == 0`
/// early return.
///
/// # Safety
///
/// As [`driver_enq`].
#[no_mangle]
pub unsafe extern "C" fn driver_pushq(
    port: ErlDrvPort,
    buf: *mut c_char,
    len: ErlDrvSizeT,
) -> c_int {
    if len == 0 {
        return 0; // `io.c:6898`, before any operand is used.
    }
    // SAFETY: `buf`/`len` are the caller's buffer (contract above).
    let Some(bytes) = (unsafe { read_bytes(buf as *const u8, len) }) else {
        return -1;
    };
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().pushq(bytes, false),
        None => -1,
    }
}

/// `int driver_enq_bin(ErlDrvPort port, ErlDrvBinary *bin, ErlDrvSizeT offs, ErlDrvSizeT len);` —
/// append the `bin->orig_bytes[offs .. offs+len]` window at the tail (`io.c:6840`).
///
/// Returns `0`/`-1`; `len == 0` returns `0` without touching `bin` or `port` (the C's early return at
/// `io.c:6847`). The window is **copied** here where the C shares the binary (see the module header).
///
/// # Safety
///
/// `port` must be null or a live port handle; `bin` must be null or a live driver binary whose
/// payload covers `offs + len`.
#[no_mangle]
pub unsafe extern "C" fn driver_enq_bin(
    port: ErlDrvPort,
    bin: *mut ErlDrvBinary,
    offs: ErlDrvSizeT,
    len: ErlDrvSizeT,
) -> c_int {
    if len == 0 {
        return 0; // `io.c:6847`, before `bin` is read.
    }
    // SAFETY: `bin`/`offs`/`len` are the caller's driver binary and its sub-range (contract above).
    let Some(bytes) = (unsafe { binary_window(bin, offs, len) }) else {
        return -1;
    };
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().enq(bytes, true),
        None => -1,
    }
}

/// `int driver_pushq_bin(ErlDrvPort port, ErlDrvBinary *bin, ErlDrvSizeT offs, ErlDrvSizeT len);` —
/// prepend the binary window at the head (`io.c:6874`). Returns `0`/`-1` exactly as
/// [`driver_enq_bin`] does.
///
/// # Safety
///
/// As [`driver_enq_bin`].
#[no_mangle]
pub unsafe extern "C" fn driver_pushq_bin(
    port: ErlDrvPort,
    bin: *mut ErlDrvBinary,
    offs: ErlDrvSizeT,
    len: ErlDrvSizeT,
) -> c_int {
    if len == 0 {
        return 0; // `io.c:6881`, before `bin` is read.
    }
    // SAFETY: `bin`/`offs`/`len` are the caller's driver binary and its sub-range (contract above).
    let Some(bytes) = (unsafe { binary_window(bin, offs, len) }) else {
        return -1;
    };
    // SAFETY: `port` is null or a live handle (contract above).
    match unsafe { env_for(port) } {
        Some(env) => env.host().pushq(bytes, true),
        None => -1,
    }
}

/// The shared body of [`driver_enqv`] and [`driver_pushqv`] (`head` = prepend).
///
/// # Safety
///
/// `port` must be null or a live port handle; `ev` must be null or point at a live [`ErlIOVec`]
/// whose `iov` array holds `vsize` readable entries, each heading `iov_len` readable bytes.
unsafe fn driver_enqv_like(
    port: ErlDrvPort,
    ev: *mut ErlIOVec,
    skip: ErlDrvSizeT,
    head: bool,
) -> c_int {
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return -1; // `erts_ioq_enqv`'s `q == NULL` (`erl_io_queue.c:242`).
    };
    if ev.is_null() {
        return -1; // The C would read `vec->size` through a null pointer.
    }
    // SAFETY: `ev` is non-null and the caller guarantees a live `ErlIOVec`.
    let (vsize, size, iov) = unsafe { ((*ev).vsize, (*ev).size, (*ev).iov) };
    if size <= skip {
        return 0; // The C's early return: nothing to enqueue (`erl_io_queue.c:246`/`300`).
    }
    // SAFETY: the caller guarantees `vsize` readable entries at `iov` (contract above).
    let Some(bytes) = (unsafe { gather_after_skip(iov, vsize, skip) }) else {
        return -1; // A malformed vector: the C would read past it.
    };
    if head {
        env.host().pushq(&bytes, false)
    } else {
        env.host().enq(&bytes, false)
    }
}

/// `int driver_enqv(ErlDrvPort port, ErlIOVec *ev, ErlDrvSizeT skip);` — append `ev`'s bytes with the
/// first `skip` dropped at the tail (`io.c:6790`).
///
/// Returns `0`/`-1`; `ev->size <= skip` returns `0` without enqueuing (the C's early return) — note
/// that this implementation, unlike the C, does **not** modify `ev` (see the module header).
///
/// # Safety
///
/// As [`driver_enqv_like`].
#[no_mangle]
pub unsafe extern "C" fn driver_enqv(
    port: ErlDrvPort,
    ev: *mut ErlIOVec,
    skip: ErlDrvSizeT,
) -> c_int {
    // SAFETY: the caller's contract is `driver_enqv_like`'s contract.
    unsafe { driver_enqv_like(port, ev, skip, false) }
}

/// `int driver_pushqv(ErlDrvPort port, ErlIOVec *ev, ErlDrvSizeT skip);` — prepend `ev`'s bytes with
/// the first `skip` dropped at the head (`io.c:6797`). Returns `0`/`-1` exactly as [`driver_enqv`].
///
/// # Safety
///
/// As [`driver_enqv_like`].
#[no_mangle]
pub unsafe extern "C" fn driver_pushqv(
    port: ErlDrvPort,
    ev: *mut ErlIOVec,
    skip: ErlDrvSizeT,
) -> c_int {
    // SAFETY: the caller's contract is `driver_enqv_like`'s contract.
    unsafe { driver_enqv_like(port, ev, skip, true) }
}

// ---------------------------------------------------------------------------------------------
// Dequeue / size
// ---------------------------------------------------------------------------------------------

/// `ErlDrvSizeT driver_deq(ErlDrvPort port, ErlDrvSizeT size);` — drop exactly `size` bytes from the
/// head and return the **new queue size** (`io.c:6807`).
///
/// `size == 0` is a no-op that returns the current size (measured on the oracle). A `size` larger
/// than the queue is the C's failure: `erts_ioq_deq` returns `-1` for `q->size < size` and the queue
/// is left untouched, which `driver_deq` reports as `(ErlDrvSizeT)-1` — **not** a clamp.
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn driver_deq(port: ErlDrvPort, size: ErlDrvSizeT) -> ErlDrvSizeT {
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return NOT_A_SIZE;
    };
    let before = env.host().sizeq();
    if before < 0 {
        return NOT_A_SIZE; // No queue (or a host without one), as `driver_sizeq` reports.
    }
    if size > before as ErlDrvSizeT {
        return NOT_A_SIZE; // `erl_io_queue.c:351` — the queue is untouched.
    }
    // `size <= before <= c_int::MAX`, so the narrowing below is lossless.
    if env.host().deq(size as c_int) < 0 {
        return NOT_A_SIZE;
    }
    let after = env.host().sizeq();
    if after < 0 {
        return NOT_A_SIZE;
    }
    after as ErlDrvSizeT
}

/// `ErlDrvSizeT driver_sizeq(ErlDrvPort port);` — the number of bytes queued on the port
/// (`io.c:6827`), or `(ErlDrvSizeT)-1` for a null port / a host without queues.
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn driver_sizeq(port: ErlDrvPort) -> ErlDrvSizeT {
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return NOT_A_SIZE;
    };
    match env.host().sizeq() {
        n if n >= 0 => n as ErlDrvSizeT,
        _ => NOT_A_SIZE,
    }
}

// ---------------------------------------------------------------------------------------------
// Peek
// ---------------------------------------------------------------------------------------------

/// The `(vlen, supported)` answer for a `peekq` call that reported no view: `peekq` reports both
/// "unsupported" and "empty" as `None`, so `sizeq` tells them apart (`-1` = unsupported, which is
/// the C's null-queue path).
fn peekq_len_when_empty(env: &mut DriverEnv) -> c_int {
    if env.host().sizeq() < 0 {
        -1
    } else {
        0
    }
}

/// `SysIOVec *driver_peekq(ErlDrvPort port, int *vlen);` — a view of the queue head without removing
/// anything (`io.c:6821`).
///
/// Returns `NULL` with `*vlen = 0` for an empty queue and `NULL` with `*vlen = -1` for a null port or
/// a host without queues (the C's null-queue path). `vlen` counts **iovec entries, one per enqueue**
/// (measured on the oracle: two `driver_enq` calls → `*vlen == 2`). A null `vlen` is refused with
/// `NULL` where the C would dereference it.
///
/// # Safety
///
/// `port` must be null or a live port handle; `vlen` must be null or point at a writable `int`.
#[no_mangle]
pub unsafe extern "C" fn driver_peekq(port: ErlDrvPort, vlen: *mut c_int) -> *mut SysIOVec {
    if vlen.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        // SAFETY: `vlen` is non-null (checked above) and writable (contract above).
        unsafe { *vlen = -1 };
        return core::ptr::null_mut();
    };
    if let Some(iov) = env.host().peekq() {
        if !iov.is_empty() {
            let (ptr, _binv, len) = publish_queue_view(&iov);
            // SAFETY: `vlen` is non-null (checked above) and writable (contract above).
            unsafe { *vlen = len.min(c_int::MAX as usize) as c_int };
            return ptr;
        }
    }
    let len = peekq_len_when_empty(env);
    // SAFETY: `vlen` is non-null (checked above) and writable (contract above).
    unsafe { *vlen = len };
    core::ptr::null_mut()
}

/// `ErlDrvSizeT driver_peekqv(ErlDrvPort port, ErlIOVec *ev);` — fill `ev` from the queue head and
/// return the queued byte count (`io.c:6816`).
///
/// On an empty queue the C zeroes `vsize`/`size`/`iov`/`binv` and returns `0`; for a null port (or a
/// host without queues) it returns `(ErlDrvSizeT)-1`. A null `ev` is refused with
/// `(ErlDrvSizeT)-1` where the C would dereference it.
///
/// # Safety
///
/// `port` must be null or a live port handle; `ev` must be null or point at a writable [`ErlIOVec`].
#[no_mangle]
pub unsafe extern "C" fn driver_peekqv(port: ErlDrvPort, ev: *mut ErlIOVec) -> ErlDrvSizeT {
    if ev.is_null() {
        return NOT_A_SIZE;
    }
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return NOT_A_SIZE;
    };
    if let Some(iov) = env.host().peekq() {
        if !iov.is_empty() {
            let (iov_ptr, binv_ptr, len) = publish_queue_view(&iov);
            let size: ErlDrvSizeT = iov.iter().map(|entry| entry.iov_len).sum();
            // SAFETY: `ev` is non-null (checked above) and the caller guarantees it is writable.
            unsafe {
                (*ev).vsize = len.min(c_int::MAX as usize) as c_int;
                (*ev).size = size;
                (*ev).iov = iov_ptr;
                (*ev).binv = binv_ptr;
            }
            return size;
        }
    }
    if env.host().sizeq() < 0 {
        return NOT_A_SIZE;
    }
    // SAFETY: `ev` is non-null (checked above) and writable (contract above).
    unsafe { clear_iovec(ev) };
    0
}

// ---------------------------------------------------------------------------------------------
// Vector linearization
// ---------------------------------------------------------------------------------------------

/// `ErlDrvSizeT driver_vec_to_buf(ErlIOVec *ev, char *buf, ErlDrvSizeT len);` — copy at most `len`
/// bytes of `ev` into `buf`, returning the number copied (`io.c:6510`).
///
/// Reproduces the C's loop exactly, including its return value: as soon as `len` bytes have been
/// copied it returns `orig_len`, and if the vectors run out first it returns `orig_len - len`
/// (measured on the oracle: a 6-byte vector with `len = 4` → `4`, with `len = 100` → `6`, with
/// `len = 0` → `0`). `driver_vec_to_buf` has no failure code, so a null `vec`/`buf`/`iov`, a negative
/// `vsize` or an entry with a positive length and a null base — all of which the C would dereference
/// — yield `0` ("copied nothing").
///
/// # Safety
///
/// `vec` must be null or point at a live [`ErlIOVec`] whose `iov` array holds `vsize` readable
/// entries, each heading `iov_len` readable bytes; `buf` must be null or point at `len` writable
/// bytes.
#[no_mangle]
pub unsafe extern "C" fn driver_vec_to_buf(
    vec: *mut ErlIOVec,
    buf: *mut c_char,
    len: ErlDrvSizeT,
) -> ErlDrvSizeT {
    if vec.is_null() || buf.is_null() {
        return 0;
    }
    // SAFETY: `vec` is non-null and the caller guarantees a live `ErlIOVec`.
    let (vsize, iov) = unsafe { ((*vec).vsize, (*vec).iov) };
    if vsize <= 0 || iov.is_null() {
        return 0;
    }
    let mut left = len;
    let mut dst = buf as *mut u8;
    for ix in 0..vsize as usize {
        // SAFETY: the caller guarantees `vsize` readable entries at `iov`.
        let entry = unsafe { *iov.add(ix) };
        let ilen = entry.iov_len;
        if ilen < left {
            if entry.iov_base.is_null() {
                return 0; // Malformed: the C would read from a null base.
            }
            // SAFETY: `iov_base` heads `iov_len` readable bytes (caller contract) and `dst` heads
            // `left` writable bytes, of which `ilen < left` are copied.
            unsafe { core::ptr::copy_nonoverlapping(entry.iov_base as *const u8, dst, ilen) };
            left -= ilen;
            // SAFETY: `ilen < left` before the subtraction, so `dst + ilen` is still inside the
            // caller's `len`-byte buffer.
            dst = unsafe { dst.add(ilen) };
        } else {
            if left > 0 && entry.iov_base.is_null() {
                return 0; // Malformed: the C would read from a null base.
            }
            // SAFETY: `iov_base` heads `iov_len >= left` readable bytes and `dst` heads `left`
            // writable bytes (caller contract).
            unsafe { core::ptr::copy_nonoverlapping(entry.iov_base as *const u8, dst, left) };
            return len; // The C's `return orig_len`.
        }
    }
    len - left
}

// ---------------------------------------------------------------------------------------------
// Table assembly (the port-queue family of the frozen `ErlDrvApi`)
// ---------------------------------------------------------------------------------------------

/// Populate this family's slots of the frozen [`ErlDrvApi`].
///
/// Called from `super::DRIVER_API`'s `populate` chain, so this file — not `mod.rs` — owns the
/// `Some(…)` list for the eleven functions it defines. Every field is `const`-assignable, so the
/// whole chain folds at compile time.
///
/// Implements: `driver_vec_to_buf`, the enqueue family (`driver_enq`/`driver_pushq`/
/// `driver_enq_bin`/`driver_pushq_bin`/`driver_enqv`/`driver_pushqv`), the dequeue/size pair
/// (`driver_deq`/`driver_sizeq`) and the peek pair (`driver_peekq`/`driver_peekqv`).
pub(super) const fn populate(mut api: ErlDrvApi) -> ErlDrvApi {
    api.driver_vec_to_buf = Some(driver_vec_to_buf);
    api.driver_enq = Some(driver_enq);
    api.driver_pushq = Some(driver_pushq);
    api.driver_deq = Some(driver_deq);
    api.driver_sizeq = Some(driver_sizeq);
    api.driver_enq_bin = Some(driver_enq_bin);
    api.driver_pushq_bin = Some(driver_pushq_bin);
    api.driver_peekqv = Some(driver_peekqv);
    api.driver_peekq = Some(driver_peekq);
    api.driver_enqv = Some(driver_enqv);
    api.driver_pushqv = Some(driver_pushqv);
    api
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::{make_atom, Eterm};

    use crate::driver::host::{DriverEnv, DriverHost};

    /// A [`DriverHost`] that really queues: one owned byte chunk per enqueue.
    ///
    /// That is the C's shape — one `SysIOVec`/binary per `driver_enq*` call — measured on the oracle
    /// (`driver_peekq` reports `*vlen == 2` after two `driver_enq`s).
    struct FifoHost {
        heap: Heap,
        atoms: AtomTable,
        /// The queue, head first.
        entries: Vec<Vec<u8>>,
        /// The `is_binary` flag of every `enq`/`pushq` call, in call order.
        flags: Vec<bool>,
        /// Every `deq` argument, in call order.
        deqs: Vec<c_int>,
        /// When set, the next `enq`/`pushq` reports the C's failure code (an allocation failure).
        fail_next: bool,
    }

    impl FifoHost {
        fn new() -> FifoHost {
            FifoHost {
                heap: Heap::new(1024).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
                entries: Vec::new(),
                flags: Vec::new(),
                deqs: Vec::new(),
                fail_next: false,
            }
        }

        /// The queue's bytes, head first.
        fn contents(&self) -> Vec<u8> {
            self.entries.concat()
        }

        /// The queued byte count.
        fn total(&self) -> usize {
            self.entries.iter().map(Vec::len).sum()
        }
    }

    impl DriverHost for FifoHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }

        fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
            0
        }

        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
            0
        }

        fn enq(&mut self, data: &[u8], is_binary: bool) -> c_int {
            self.flags.push(is_binary);
            if self.fail_next {
                self.fail_next = false;
                return -1;
            }
            self.entries.push(data.to_vec());
            0
        }

        fn pushq(&mut self, data: &[u8], is_binary: bool) -> c_int {
            self.flags.push(is_binary);
            if self.fail_next {
                self.fail_next = false;
                return -1;
            }
            self.entries.insert(0, data.to_vec());
            0
        }

        fn deq(&mut self, len: c_int) -> c_int {
            self.deqs.push(len);
            if len < 0 {
                return -1;
            }
            let mut left = len as usize;
            let mut dropped = 0usize;
            while left > 0 {
                let Some(head_len) = self.entries.first().map(Vec::len) else {
                    break;
                };
                if head_len <= left {
                    left -= head_len;
                    dropped += head_len;
                    self.entries.remove(0);
                } else {
                    self.entries[0].drain(0..left);
                    dropped += left;
                    left = 0;
                }
            }
            dropped as c_int
        }

        fn sizeq(&mut self) -> c_int {
            c_int::try_from(self.total()).unwrap_or(c_int::MAX)
        }

        fn peekq(&mut self) -> Option<Vec<SysIOVec>> {
            if self.entries.is_empty() {
                return None;
            }
            Some(
                self.entries
                    .iter()
                    .map(|entry| SysIOVec {
                        iov_base: entry.as_ptr() as *mut libc::c_void,
                        iov_len: entry.len(),
                    })
                    .collect(),
            )
        }
    }

    /// A host that implements **no** queue hook, so every queue call sees the seam's defaults.
    struct BareHost {
        heap: Heap,
        atoms: AtomTable,
    }

    impl BareHost {
        fn new() -> BareHost {
            BareHost {
                heap: Heap::new(1024).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
            }
        }
    }

    impl DriverHost for BareHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }

        fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
            0
        }

        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
            0
        }
    }

    /// Two `SysIOVec` entries over two static byte strings.
    ///
    /// The caller keeps the returned array alive (an `ErlIOVec`'s `iov` borrows it) and builds its
    /// own `ErlIOVec` over it — a helper that returned *both* would hand back a pointer into the
    /// callee's stack frame.
    fn stored_iov() -> [SysIOVec; 2] {
        static ABC: &[u8] = b"abc";
        static DEF: &[u8] = b"def";
        [
            SysIOVec {
                iov_base: ABC.as_ptr() as *mut libc::c_void,
                iov_len: 3,
            },
            SysIOVec {
                iov_base: DEF.as_ptr() as *mut libc::c_void,
                iov_len: 3,
            },
        ]
    }

    /// An `ErlIOVec` over `iov` declaring `size` total bytes.
    fn vector_over(iov: &mut [SysIOVec; 2], size: ErlDrvSizeT) -> ErlIOVec {
        ErlIOVec {
            vsize: 2,
            size,
            iov: iov.as_mut_ptr(),
            binv: core::ptr::null_mut(),
        }
    }

    /// An `ErlDrvBinary` holding `bytes` (released with `driver_free_binary`).
    fn test_binary(bytes: &[u8]) -> *mut ErlDrvBinary {
        // SAFETY: `driver_alloc_binary` needs only a size and the block below is `bytes.len()` long.
        let bin = unsafe { crate::driver::drv::mem::driver_alloc_binary(bytes.len()) };
        assert!(!bin.is_null(), "the test binary must be allocatable");
        // SAFETY: `bin` heads a block with `bytes.len()` (`orig_size`) payload bytes.
        unsafe {
            let base = core::ptr::addr_of!((*bin).orig_bytes) as *mut u8;
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), base, bytes.len());
        }
        bin
    }
    #[test]
    fn enq_appends_at_the_tail_and_pushq_prepends_at_the_head() {
        let mut host = FifoHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        let mut hello = *b"hello";
        // SAFETY: `port` is a live env handle and `hello` declares its own length.
        assert_eq!(
            unsafe { driver_enq(port, hello.as_mut_ptr() as *mut c_char, 5) },
            0
        );
        let mut world = *b"world";
        // SAFETY: as above.
        assert_eq!(
            unsafe { driver_enq(port, world.as_mut_ptr() as *mut c_char, 5) },
            0
        );
        assert_eq!(host.contents(), b"helloworld");

        let mut aa = *b"AA";
        // SAFETY: `port` is live and `aa` declares its length.
        assert_eq!(
            unsafe { driver_pushq(port, aa.as_mut_ptr() as *mut c_char, 2) },
            0
        );
        assert_eq!(
            host.contents(),
            b"AAhelloworld",
            "pushq must land at the head, enq at the tail"
        );
        assert_eq!(
            host.flags,
            vec![false, false, false],
            "the plain forms present plain buffers"
        );
    }

    #[test]
    fn the_zero_length_enqueue_is_a_no_op_success() {
        let mut host = FifoHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // The C returns 0 for `len == 0` *before* it touches the port, so even a null port succeeds.
        // SAFETY: a zero length is well formed with a null buffer and is never dereferenced.
        assert_eq!(
            unsafe { driver_enq(port, core::ptr::null_mut(), 0) },
            0
        );
        // SAFETY: as above, with a null port.
        assert_eq!(unsafe { driver_enq(core::ptr::null_mut(), core::ptr::null_mut(), 0) }, 0);
        // SAFETY: as above.
        assert_eq!(unsafe { driver_pushq(port, core::ptr::null_mut(), 0) }, 0);
        // SAFETY: as above, with a null binary and no port.
        assert_eq!(
            unsafe { driver_enq_bin(core::ptr::null_mut(), core::ptr::null_mut(), 7, 0) },
            0
        );
        assert_eq!(host.total(), 0, "a zero-length enqueue queues nothing");
        assert!(host.flags.is_empty(), "the host must not even be called");
    }

    #[test]
    fn enq_bin_and_pushq_bin_honour_the_offset_and_length() {
        let mut host = FifoHost::new();
        let bin = test_binary(b"0123");
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: `bin` is a live 4-byte driver binary and `1 + 2 <= orig_size`.
        assert_eq!(unsafe { driver_enq_bin(port, bin, 1, 2) }, 0);
        // SAFETY: as above, with a 0-byte offset window.
        assert_eq!(unsafe { driver_pushq_bin(port, bin, 0, 2) }, 0);
        assert_eq!(host.contents(), b"0112", "pushq_bin prepends, enq_bin appends");
        assert_eq!(host.total(), 4);
        assert_eq!(
            host.flags,
            vec![true, true],
            "the `_bin` forms must present the bytes as binary-backed"
        );

        // Out-of-range windows are refused instead of over-read.
        // SAFETY: the request is refused before any payload byte is read.
        assert_eq!(unsafe { driver_enq_bin(port, bin, 3, 2) }, -1);
        // SAFETY: as above.
        assert_eq!(unsafe { driver_pushq_bin(port, bin, 5, 0) }, 0, "len == 0 short-circuits first");
        // SAFETY: a null binary with a positive length is refused.
        assert_eq!(unsafe { driver_enq_bin(port, core::ptr::null_mut(), 0, 1) }, -1);
        assert_eq!(host.total(), 4, "no refused call may change the queue");

        // SAFETY: `bin` came from `driver_alloc_binary` and is released exactly once.
        unsafe { crate::driver::drv::mem::driver_free_binary(bin) };
    }

    #[test]
    fn enqv_and_pushqv_honour_the_skip() {
        let mut host = FifoHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let mut iov = stored_iov();
        let mut ev = vector_over(&mut iov, 6);

        // skip 2 over "abc","def" -> "cdef" at the tail.
        // SAFETY: `ev` is a live vector over the two static byte strings.
        assert_eq!(unsafe { driver_enqv(port, &mut ev, 2) }, 0);
        assert_eq!(host.contents(), b"cdef");
        // skip 4 over the same vector -> "ef" at the head.
        // SAFETY: as above.
        assert_eq!(unsafe { driver_pushqv(port, &mut ev, 4) }, 0);
        assert_eq!(host.contents(), b"efcdef");
        assert_eq!(host.total(), 6);
        assert_eq!(
            host.flags,
            vec![false, false],
            "the `_v` forms hand the host a plain gathered buffer"
        );

        // `skip >= size` is the C's early return: nothing enqueued, nothing changed.
        // SAFETY: as above — the call reads only `vsize`/`size`/`iov`.
        assert_eq!(unsafe { driver_enqv(port, &mut ev, 6) }, 0);
        // SAFETY: as above.
        assert_eq!(unsafe { driver_pushqv(port, &mut ev, 100) }, 0);
        assert_eq!(host.total(), 6);
        assert_eq!(host.entries.len(), 2, "no empty entry may be queued");

        // A `skip` past the vector's declared `iov_len`s (a malformed vector) is refused instead of
        // being read past: `size` claims 10 bytes but only 6 are there.
        ev.size = 10;
        // SAFETY: as above; the walk stops at the declared `vsize` and detects the overrun.
        assert_eq!(unsafe { driver_enqv(port, &mut ev, 8) }, -1);
        // SAFETY: a null vector is refused (the C would dereference it).
        assert_eq!(unsafe { driver_enqv(port, core::ptr::null_mut(), 0) }, -1);
        // SAFETY: a null port is refused.
        assert_eq!(unsafe { driver_enqv(core::ptr::null_mut(), &mut ev, 0) }, -1);
        assert_eq!(host.total(), 6, "no refused call may change the queue");
        // The caller's vector is untouched (the documented deviation from the C's in-place skip).
        assert_eq!(iov[0].iov_len, 3);
        assert_eq!(ev.iov, iov.as_mut_ptr());
    }

    #[test]
    fn sizeq_reports_the_queued_bytes() {
        let mut host = FifoHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: `port` is a live handle with an empty queue.
        assert_eq!(unsafe { driver_sizeq(port) }, 0);
        let mut abcd = *b"abcd";
        // SAFETY: `port` is live and `abcd` declares its length.
        assert_eq!(
            unsafe { driver_enq(port, abcd.as_mut_ptr() as *mut c_char, 4) },
            0
        );
        // SAFETY: as above.
        assert_eq!(unsafe { driver_sizeq(port) }, 4);
        // SAFETY: a null port reports the C's `(ErlDrvSizeT)-1`.
        assert_eq!(unsafe { driver_sizeq(core::ptr::null_mut()) }, NOT_A_SIZE);
    }

    #[test]
    fn deq_removes_from_the_head_and_reports_the_remaining_size() {
        let mut host = FifoHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        let mut hello = *b"hello";
        // SAFETY: `port` is live and `hello` declares its length.
        unsafe { driver_enq(port, hello.as_mut_ptr() as *mut c_char, 5) };
        let mut world = *b"world";
        // SAFETY: as above.
        unsafe { driver_enq(port, world.as_mut_ptr() as *mut c_char, 5) };

        // The oracle returns the remaining count: 10 - 3 = 7.
        // SAFETY: `port` is a live handle.
        assert_eq!(unsafe { driver_deq(port, 3) }, 7);
        assert_eq!(host.contents(), b"loworld");
        // SAFETY: as above.
        assert_eq!(unsafe { driver_sizeq(port) }, 7);

        // Over-large: the C reports `(ErlDrvSizeT)-1` and leaves the queue alone (measured on the
        // oracle as 18446744073709551615).
        // SAFETY: as above.
        assert_eq!(unsafe { driver_deq(port, 100) }, NOT_A_SIZE);
        // SAFETY: as above.
        assert_eq!(unsafe { driver_sizeq(port) }, 7);
        assert_eq!(host.contents(), b"loworld", "a refused deq must not drop anything");

        // `deq(0)` is a no-op that returns the current size (measured on the oracle).
        // SAFETY: as above.
        assert_eq!(unsafe { driver_deq(port, 0) }, 7);
        assert_eq!(host.contents(), b"loworld");

        // Exact: empties the queue and reports 0.
        // SAFETY: as above.
        assert_eq!(unsafe { driver_deq(port, 7) }, 0);
        assert_eq!(host.contents(), b"");
        // SAFETY: as above.
        assert_eq!(unsafe { driver_sizeq(port) }, 0);
        // SAFETY: a null port reports the failure code.
        assert_eq!(unsafe { driver_deq(core::ptr::null_mut(), 1) }, NOT_A_SIZE);
        assert_eq!(host.deqs, vec![3, 0, 7], "the refused call must not reach the host");
    }

    #[test]
    fn peekq_views_the_queue_and_reports_nothing_when_empty() {
        let mut host = FifoHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        let mut vlen = 99;
        // SAFETY: `port` is live and `vlen` is writable.
        assert!(unsafe { driver_peekq(port, &mut vlen) }.is_null());
        assert_eq!(vlen, 0, "an empty queue reports vlen 0");

        let mut hello = *b"hello";
        // SAFETY: `port` is live and `hello` declares its length.
        unsafe { driver_enq(port, hello.as_mut_ptr() as *mut c_char, 5) };
        let mut world = *b"world";
        // SAFETY: as above.
        unsafe { driver_enq(port, world.as_mut_ptr() as *mut c_char, 5) };

        // SAFETY: `port` is live and `vlen` is writable.
        let view = unsafe { driver_peekq(port, &mut vlen) };
        assert_eq!(vlen, 2, "one iovec entry per enqueue, as the oracle reports");
        assert!(!view.is_null());
        // SAFETY: `view` heads `vlen` published entries that no call has overwritten.
        assert_eq!(unsafe { (*view).iov_len }, 5);
        // SAFETY: as above, second entry.
        assert_eq!(unsafe { (*view.add(1)).iov_len }, 5);
        assert_eq!(
            host.total(),
            10,
            "peek must not remove anything"
        );

        // Drain, then peek again: the view goes back to empty.
        // SAFETY: `port` is live.
        assert_eq!(unsafe { driver_deq(port, 10) }, 0);
        // SAFETY: as above.
        assert!(unsafe { driver_peekq(port, &mut vlen) }.is_null());
        assert_eq!(vlen, 0);

        // A null port reports the C's null-queue answer: `NULL` with `*vlen == -1`.
        let mut vlen = 7;
        // SAFETY: a null port and a writable `vlen`.
        assert!(unsafe { driver_peekq(core::ptr::null_mut(), &mut vlen) }.is_null());
        assert_eq!(vlen, -1);
        // A null `vlen` is refused (`NULL`) instead of being dereferenced.
        // SAFETY: the call refuses the null out-parameter before writing through it.
        assert!(unsafe { driver_peekq(port, core::ptr::null_mut()) }.is_null());
    }

    #[test]
    fn peekqv_fills_the_vector_and_returns_the_queued_size() {
        let mut host = FifoHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        let mut ev = ErlIOVec::default();
        // SAFETY: `port` is live and `ev` is writable.
        assert_eq!(unsafe { driver_peekqv(port, &mut ev) }, 0);
        assert_eq!(ev.vsize, 0, "an empty queue zeroes the vector");
        assert_eq!(ev.size, 0);
        assert!(ev.iov.is_null());
        assert!(ev.binv.is_null());

        let mut hello = *b"hello";
        // SAFETY: `port` is live and `hello` declares its length.
        unsafe { driver_enq(port, hello.as_mut_ptr() as *mut c_char, 5) };
        let mut world = *b"world";
        // SAFETY: as above.
        unsafe { driver_enq(port, world.as_mut_ptr() as *mut c_char, 5) };

        // SAFETY: `port` is live and `ev` is writable.
        assert_eq!(unsafe { driver_peekqv(port, &mut ev) }, 10);
        assert_eq!(ev.vsize, 2);
        assert_eq!(ev.size, 10);
        assert!(!ev.iov.is_null());
        // SAFETY: `ev.iov` heads `ev.vsize` published entries.
        assert_eq!(unsafe { (*ev.iov).iov_len }, 5);
        // SAFETY: `ev.binv` heads `ev.vsize` NULL `ErlDrvBinary*` entries (documented deviation).
        assert!(unsafe { *ev.binv }.is_null());
        assert_eq!(host.total(), 10, "peekqv must not remove anything");

        // SAFETY: a null `ev` is refused (the C would dereference it).
        assert_eq!(
            unsafe { driver_peekqv(port, core::ptr::null_mut()) },
            NOT_A_SIZE
        );
        // SAFETY: a null port reports the failure code.
        assert_eq!(unsafe { driver_peekqv(core::ptr::null_mut(), &mut ev) }, NOT_A_SIZE);
    }

    #[test]
    fn vec_to_buf_truncates_to_len() {
        let mut iov = stored_iov();
        let mut ev = vector_over(&mut iov, 6);
        let mut buf = [0u8; 16];

        // `len` larger than the vector: the whole vector is copied and the vector's size returned.
        // SAFETY: `ev` is a live 6-byte vector and `buf` heads 16 writable bytes.
        assert_eq!(
            unsafe { driver_vec_to_buf(&mut ev, buf.as_mut_ptr() as *mut c_char, 100) },
            6
        );
        assert_eq!(&buf[..6], b"abcdef");

        // `len` inside the vector: exactly `len` bytes are copied and `len` is returned.
        buf = [0u8; 16];
        // SAFETY: as above, with `len = 4`.
        assert_eq!(
            unsafe { driver_vec_to_buf(&mut ev, buf.as_mut_ptr() as *mut c_char, 4) },
            4
        );
        assert_eq!(&buf[..4], b"abcd");

        // `len == 0`: the C copies nothing and returns 0.
        // SAFETY: as above, with `len = 0` (no byte is written).
        assert_eq!(
            unsafe { driver_vec_to_buf(&mut ev, buf.as_mut_ptr() as *mut c_char, 0) },
            0
        );

        // Refusals: no failure code exists, so "copied nothing" (0) is reported.
        // SAFETY: every operand below is null or a valid vector and no byte is read or written.
        unsafe {
            assert_eq!(
                driver_vec_to_buf(core::ptr::null_mut(), buf.as_mut_ptr() as *mut c_char, 4),
                0
            );
            assert_eq!(driver_vec_to_buf(&mut ev, core::ptr::null_mut(), 4), 0);
        }
        let mut empty = ErlIOVec::default();
        // SAFETY: an all-zero vector: `vsize == 0` and `iov` is null.
        assert_eq!(
            unsafe { driver_vec_to_buf(&mut empty, buf.as_mut_ptr() as *mut c_char, 4) },
            0
        );
    }

    #[test]
    fn a_host_without_queues_reports_the_c_failure_codes() {
        let mut host = BareHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let mut iov = stored_iov();
        let mut ev = vector_over(&mut iov, 6);
        let mut buf = [0u8; 8];
        let mut vlen = 5;
        let mut out = ErlIOVec::default();

        // SAFETY: every call below passes a live port handle (or a well-formed operand) and takes the
        // host's defaulted hook.
        unsafe {
            assert_eq!(driver_enq(port, buf.as_mut_ptr() as *mut c_char, 3), -1);
            assert_eq!(driver_pushq(port, buf.as_mut_ptr() as *mut c_char, 3), -1);
            assert_eq!(driver_enqv(port, &mut ev, 0), -1);
            assert_eq!(driver_pushqv(port, &mut ev, 0), -1);
            assert_eq!(driver_sizeq(port), NOT_A_SIZE);
            assert_eq!(driver_deq(port, 1), NOT_A_SIZE);
            assert_eq!(driver_peekqv(port, &mut out), NOT_A_SIZE);
            assert!(driver_peekq(port, &mut vlen).is_null());
            assert_eq!(vlen, -1, "an unsupported host is the C's null-queue path");
        }
    }

    #[test]
    fn a_host_failure_is_passed_through() {
        let mut host = FifoHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        host.fail_next = true;

        let mut data = *b"abc";
        // SAFETY: `port` is live and `data` declares its length.
        assert_eq!(
            unsafe { driver_enq(port, data.as_mut_ptr() as *mut c_char, 3) },
            -1,
            "the C's only enqueue failure (allocation) is reported as -1"
        );
        assert_eq!(host.total(), 0);
    }

    #[test]
    fn the_frozen_table_populates_every_slot_this_family_owns() {
        let table = crate::driver::drv::driver_api_table();
        let populated = [
            table.driver_vec_to_buf.is_some(),
            table.driver_enq.is_some(),
            table.driver_pushq.is_some(),
            table.driver_deq.is_some(),
            table.driver_sizeq.is_some(),
            table.driver_enq_bin.is_some(),
            table.driver_pushq_bin.is_some(),
            table.driver_peekqv.is_some(),
            table.driver_peekq.is_some(),
            table.driver_enqv.is_some(),
            table.driver_pushqv.is_some(),
        ];
        assert!(populated.iter().all(|&p| p), "{populated:?}");
    }
}
