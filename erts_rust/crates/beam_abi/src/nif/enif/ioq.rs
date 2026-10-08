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

//! `enif_*` — the I/O queue and I/O vector family (workspace contract **A.3**).
//!
//! Owned slots: `enif_ioq_create`, `enif_ioq_destroy`, `enif_ioq_enq_binary`, `enif_ioq_enqv`,
//! `enif_ioq_size`, `enif_ioq_deq`, `enif_ioq_peek`, `enif_ioq_peek_head`, `enif_inspect_iovec`,
//! `enif_free_iovec`.
//!
//! A **NIF I/O queue** is the buffer a NIF accumulates data in between `enif_select` wakeups: the
//! NIF enqueues what it could not write, `enif_ioq_peek` hands it a `writev`-ready `SysIOVec` view,
//! `enif_ioq_deq` drops what the OS accepted, and `enif_ioq_peek_head` turns the remaining head into
//! a binary term for `enif_send`. None of this touches the scheduler, so — like the rest of the
//! `beam_abi` host layer — it lives entirely behind the frozen C ABI and needs no runtime subsystem.
//!
//! # Reproduced semantics (ground truth `erts/emulator/beam/erl_nif.c` + `erl_io_queue.c`)
//!
//! | function | C | behaviour reproduced |
//! |---|---|---|
//! | [`enif_ioq_create`] | `erl_nif.c:4061` | a fresh empty queue; the only option value is `ERL_NIF_IOQ_NORMAL` (the enum has no other variant) |
//! | [`enif_ioq_destroy`] | `erl_nif.c:4075` | drains and frees the queue |
//! | [`enif_ioq_enq_binary`] | `erl_nif.c:4368` | appends `bin->data[skip .. bin->size]`, then **consumes** the caller's binary (`enif_release_binary`), as the C does whether or not the enqueue was accepted |
//! | [`enif_ioq_enqv`] | `erl_nif.c:4359` | appends the vector's bytes after dropping the first `skip`; `1` on success, `0` when `skip` runs past the vector |
//! | [`enif_ioq_size`] | `erl_nif.c:4384` | the total queued byte count |
//! | [`enif_ioq_deq`] | `erl_nif.c:4389` | removes `count` bytes from the head, reports the **remaining** byte count through `*size` (`erl_nif.c:4394`), `0` when `count` exceeds the queue |
//! | [`enif_ioq_peek`] | `erl_nif.c:4449` | the queue's own `SysIOVec` view (one entry per enqueued buffer), `NULL` + `*iovlen = -1` for a null queue, `NULL` + `*iovlen = 0` when empty |
//! | [`enif_ioq_peek_head`] | `erl_nif.c:4398` | the head buffer as a binary term plus its byte count; `0` (and no writes) on an empty queue |
//! | [`enif_inspect_iovec`] | `erl_nif.c:4342` | an iovec over the list's binaries, at most `max_elements` list **elements**, `*tail` = the remainder; `0` for a non-iovec |
//! | [`enif_free_iovec`] | `erl_nif.c:4085` | releases an iovec that owns its storage (only reachable through a `NULL` environment, per the manual) |
//!
//! `ERL_SMALL_IO_QUEUE`'s `5`-entry inline arrays, the `expandq` growth policy and the C's
//! "restart pointers when the queue empties" optimisation are *representation*: the Rust queue is a
//! `VecDeque` of buffers, which reproduces every observable the API exposes (byte count, vector
//! count, vector lengths, head bytes) without the C's manual array management. The vector *count*
//! rule is preserved exactly, because it is observable: **one `SysIOVec` per non-empty enqueued
//! buffer** (`erl_io_queue.c:259-280`), with zero-length entries dropped.
//!
//! # Documented deviations from the oracle
//!
//! * **The queue always copies.** The C queues a *reference* to a refcounted binary whenever the
//! vector entry carries one (`ref_bins[i] != NULL`) and only copies when it does not
//! (`erl_io_queue.c:261-268`, `alloc_binary`). This crate's `ErlNifBinary` never exposes an off-heap
//! refcounted binary — its `ref_bin` is always null (see `mem.rs`) — so the copy arm is the only one
//! reachable, and it is implemented faithfully: the queued bytes are independent of the caller's
//! buffer, which may therefore be freed or reused immediately after the call where the C requires it
//! to stay alive until the queue drains.
//! * **`enif_ioq_enqv` / `enq_binary` do not mutate the caller's vector.** The C's `skip()` walk
//!   advances `iov[0].iov_base` / `iov[0].iov_len` **in place** in the caller's `ErlNifIOVec`
//!   (`erl_io_queue.c:203-229`), so a second enqueue of the same vector sees the already-skipped
//!   vector. This implementation only reads the vector. (Same deviation, same wording, as the driver
//!   port-queue family in `crate::driver::drv`.)
//! * **Byte accounting uses the iovec entries, not the declared `size`.** The C adds
//!   `iov->size - skip` to its counter while queueing the entries' references
//!   (`erl_io_queue.c:237`, `281`). For a well-formed vector (`size == Σ iov_len`) the two agree
//!   exactly; for a malformed one this queue count stays truthful where the C's is inflated.
//! * **`enif_ioq_peek_head` always builds an on-heap binary.** Above `ERL_ONHEAP_BINARY_LIMIT` (64)
//!   bytes the C builds an *off-heap* refcounted binary instead (`erl_nif.c:4415-4444`); off-heap refc
//!   binaries are not exposed by this crate's seam, so the term here is an ordinary heap binary of
//!   the same bytes — indistinguishable through every `enif_*` accessor, but not (yet) sharing the
//!   payload with the queue's buffer.
//! * **`enif_inspect_iovec` refuses a non-byte-aligned base.** The C copies a binary whose payload
//!   starts mid-byte into a scratch buffer (`erl_nif.c:4192-4207`); without a refcounted scratch
//!   binary this seam cannot hand out a stable pointer, so such a list is refused (`0`) — the same
//!   honest refusal `enif_inspect_binary` makes for an unaligned base.
//! * **`ref_bins[i]` is always `NULL`.** No `ErlNifBinary` here holds a refcounted payload, so the
//!   iovec records "plain memory, not a binary-backed entry" — the C's own encoding for exactly that
//!   case, and the one that makes the enqueue copy. Consequently `enif_free_iovec` has no binary
//!   refcounts to drop.
//! * **An env-allocated iovec is marked `ERL_NIF_IOVEC_FLAGS_PREALLOC`.** With a non-null `env` the
//!   C attaches the vector to the environment as a temp object ("valid until the called nif function
//!   returns", manual for `enif_inspect_iovec`); this seam allocates it from the env's term heap and
//!   marks it preallocated, so a spurious `enif_free_iovec` on it cannot free host memory. The
//!   documented rule — freeing is needed **only** when a `NULL` environment was passed — is
//!   unchanged, and that path (flags `0`, one `malloc`ed block) frees exactly what it allocated.
//! * **A caller-supplied preallocated structure is used for fewer than 16 vectors**, exactly as the C
//!   does (`erl_nif.c:4296`), with its inline `small_iov`/`small_ref_bin` arrays.
//! * **Refusals where the C would fault.** A null queue, binary, iovec, out-parameter or vector base
//!   with a non-zero length yields the C's failure code (`0` / `NULL` / `-1` for `*iovlen`) instead of
//!   dereferencing. The C's `ASSERT`s on the queue/iterator invariants are not reproduced as panics.
//!
//! # `unsafe`
//!
//! The whole file is FFI. Every `unsafe fn` carries a `# Safety` section (its caller contract) and
//! every `unsafe` block a preceding `// SAFETY:` rationale (workspace `CONTEXT.md` §3). All queue
//! handle recovery goes through [`queue_mut`]; no runtime data is `unwrap()`ed and no failure path
//! panics.

use super::*;

use std::collections::VecDeque;

use libc::{c_int, c_void, size_t};

use crate::common::SysIOVec;
use crate::nif::types::{
    ErlNifBinary, ErlNifIOQueue, ErlNifIOQueueOpts, ErlNifIOVec, ERL_NIF_IOVEC_SIZE,
};

/// `ERL_NIF_IOVEC_FLAGS_PREALLOC` (`erl_nif.c:4083`): the vector lives in caller- or env-owned
/// storage, so [`enif_free_iovec`] must not free it — the C's own `enif_free_iovec` early exit
/// (`erl_nif.c:4097`).
const FLAGS_PREALLOC: c_int = 1 << 0;

/// `MAX_SYSIOVEC_IOVLEN` for a non-`DEBUG` build (`erl_io_queue.h:39-43`): `iov_len` is a `size_t`,
/// so a single `SysIOVec` may describe `1 << (8*8 - 1)` bytes. A larger payload is split across
/// vectors; for any realistic iolist this split never triggers, but the walk is faithful.
const MAX_SYSIOVEC_IOVLEN: usize = 1usize << 63;

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_ioq_create = Some(enif_ioq_create);
    api.enif_ioq_destroy = Some(enif_ioq_destroy);
    api.enif_ioq_enq_binary = Some(enif_ioq_enq_binary);
    api.enif_ioq_enqv = Some(enif_ioq_enqv);
    api.enif_ioq_size = Some(enif_ioq_size);
    api.enif_ioq_deq = Some(enif_ioq_deq);
    api.enif_ioq_peek = Some(enif_ioq_peek);
    api.enif_ioq_peek_head = Some(enif_ioq_peek_head);
    api.enif_inspect_iovec = Some(enif_inspect_iovec);
    api.enif_free_iovec = Some(enif_free_iovec);
    api
}

// ---------------------------------------------------------------------------------------------
// The queue behind the opaque `*mut ErlNifIOQueue`
// ---------------------------------------------------------------------------------------------

/// One queued buffer: the bytes copied out of what was enqueued.
struct Chunk {
    /// The enqueued bytes.
    data: Vec<u8>,
    /// How many leading bytes the head has already consumed — the C's partial-head dequeue
    /// (`erl_io_queue.c:364-367`).
    start: usize,
}

impl Chunk {
    /// The bytes still queued from this buffer.
    fn pending(&self) -> &[u8] {
        // `start` is only ever advanced while `start < data.len()` (a fully consumed buffer is
        // removed instead), so this range is always inside `data`.
        &self.data[self.start..]
    }

    /// The number of bytes still queued from this buffer.
    fn len(&self) -> usize {
        self.data.len() - self.start
    }
}

/// The concrete I/O queue reached through the opaque `*mut ErlNifIOQueue` (`ErlNifIOQueue` is an
/// incomplete C type — `struct erts_io_queue`).
struct IoQueue {
    /// The queued buffers, head first.
    chunks: VecDeque<Chunk>,
    /// Total queued bytes — the C's `q->size`.
    size: usize,
    /// The cached [`enif_ioq_peek`] view, rebuilt whenever the queue changes. The C publishes
    /// `q->v_head`, its own `SysIOVec` array, which the next enqueue reallocates: the same "valid
    /// until the queue is modified" contract (`erl_nif.c:4449-4452`).
    view: Vec<SysIOVec>,
}

impl IoQueue {
    /// A fresh, empty queue.
    fn new() -> IoQueue {
        IoQueue {
            chunks: VecDeque::new(),
            size: 0,
            view: Vec::new(),
        }
    }

    /// Rebuild [`IoQueue::view`] from the queued buffers: one entry per non-empty buffer, in queue
    /// order (`erl_io_queue.c:398-408`).
    fn refresh_view(&mut self) {
        self.view.clear();
        for chunk in &self.chunks {
            let bytes = chunk.pending();
            if !bytes.is_empty() {
                self.view.push(SysIOVec {
                    iov_base: bytes.as_ptr().cast_mut().cast::<c_void>(),
                    iov_len: bytes.len(),
                });
            }
        }
    }

    /// Append the bytes `vecs` describes, after dropping the first `skip` bytes.
    ///
    /// Returns the C's `1` on success and `0` when `skip` runs past the end of the vector (its
    /// failure code — `enq_binary`/`enq_binary` never enqueue a partial buffer beyond the data).
    fn enqueue(&mut self, vecs: &[(*const u8, usize)], skip: usize) -> c_int {
        let total: usize = vecs.iter().map(|&(_, len)| len).sum();
        if skip > total {
            return 0;
        }
        let mut to_skip = skip;
        for &(ptr, len) in vecs {
            if len == 0 {
                // The C drops zero-length entries unconditionally (`erl_io_queue.c:260`).
                continue;
            }
            if ptr.is_null() {
                // A vector entry claiming bytes but holding no base would be read out of a null
                // pointer by the C; refuse instead.
                return 0;
            }
            if to_skip >= len {
                to_skip -= len;
                continue;
            }
            let start = to_skip;
            to_skip = 0;
            let n = len - start;
            let mut data = Vec::new();
            if data.try_reserve_exact(n).is_err() {
                return 0;
            }
            data.resize(n, 0);
            // SAFETY: the caller guarantees `ptr` addresses `len` readable bytes, and
            // `start + n == len`, so both ranges are inside that buffer; `data` is a distinct,
            // exclusively owned `n`-byte allocation.
            unsafe { core::ptr::copy_nonoverlapping(ptr.add(start), data.as_mut_ptr(), n) };
            self.chunks.push_back(Chunk { data, start: 0 });
            self.size += n;
        }
        self.refresh_view();
        1
    }
}

/// Recover the live [`IoQueue`] behind an opaque `*mut ErlNifIOQueue` (`None` when null).
///
/// # Safety
///
/// `q` must be null, or a pointer returned by [`enif_ioq_create`] that has not been destroyed.
unsafe fn queue_mut<'a>(q: *mut ErlNifIOQueue) -> Option<&'a mut IoQueue> {
    if q.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `q` came from `Box::into_raw` in `enif_ioq_create` and is still
    // live; `ErlNifIOQueue` is an incomplete C type, so this is the only interpretation of the
    // pointer, and `Box` guarantees the 8-byte alignment `IoQueue` needs.
    Some(unsafe { &mut *q.cast::<IoQueue>() })
}

// ---------------------------------------------------------------------------------------------
// enif_ioq_*
// ---------------------------------------------------------------------------------------------

/// `ErlNifIOQueue *enif_ioq_create(ErlNifIOQueueOpts opts);`
///
/// # Safety
///
/// The returned handle must be released with [`enif_ioq_destroy`].
#[no_mangle]
pub unsafe extern "C" fn enif_ioq_create(opts: ErlNifIOQueueOpts) -> *mut ErlNifIOQueue {
    // The C rejects anything but `ERL_NIF_IOQ_NORMAL` (`erl_nif.c:4065`); `ErlNifIOQueueOpts` has
    // exactly that one variant, so every value Rust can name is accepted.
    match opts {
        ErlNifIOQueueOpts::Normal => {}
    }
    Box::into_raw(Box::new(IoQueue::new())).cast::<ErlNifIOQueue>()
}

/// `void enif_ioq_destroy(ErlNifIOQueue *q);`
///
/// # Safety
///
/// `q` must be null or a handle from [`enif_ioq_create`] that has not already been destroyed.
#[no_mangle]
pub unsafe extern "C" fn enif_ioq_destroy(q: *mut ErlNifIOQueue) {
    if q.is_null() {
        return;
    }
    // SAFETY: the caller guarantees `q` is a live handle from `enif_ioq_create`; reclaiming the
    // `Box` frees the queue (and, with it, every queued buffer). A null handle is the C's crash —
    // here it is a no-op.
    drop(unsafe { Box::from_raw(q.cast::<IoQueue>()) });
}

/// `int enif_ioq_enq_binary(ErlNifIOQueue *q, ErlNifBinary *bin, size_t skip);`
///
/// # Safety
///
/// `q` must be a live queue handle; `bin` must be a live `ErlNifBinary`. The binary is consumed:
/// it must not be used afterwards (the C calls `enif_release_binary` on it, `erl_nif.c:4380`).
#[no_mangle]
pub unsafe extern "C" fn enif_ioq_enq_binary(
    q: *mut ErlNifIOQueue,
    bin: *mut ErlNifBinary,
    skip: size_t,
) -> c_int {
    let Some(queue) = (unsafe { queue_mut(q) }) else {
        return 0;
    };
    if bin.is_null() {
        return 0;
    }
    // SAFETY: `bin` is non-null (checked) and live (caller contract).
    let b = unsafe { &mut *bin };
    let vecs: [(*const u8, usize); 1] = [(b.data.cast_const(), b.size)];
    let result = queue.enqueue(&vecs, skip);
    // Consume the caller's binary whether or not the enqueue was accepted (`erl_nif.c:4379-4380`);
    // the bytes were copied by `enqueue`, so the release cannot invalidate the queue.
    // SAFETY: `bin` is live and owned/inspected per its own marker.
    unsafe { enif_release_binary(bin) };
    result
}

/// `int enif_ioq_enqv(ErlNifIOQueue *q, ErlNifIOVec *iov, size_t skip);`
///
/// # Safety
///
/// `q` must be a live queue handle; `iov` must be a live `ErlNifIOVec` whose `iov` array holds
/// `iovcnt` readable entries.
#[no_mangle]
pub unsafe extern "C" fn enif_ioq_enqv(
    q: *mut ErlNifIOQueue,
    iov: *mut ErlNifIOVec,
    skip: size_t,
) -> c_int {
    let Some(queue) = (unsafe { queue_mut(q) }) else {
        return 0;
    };
    if iov.is_null() {
        return 0;
    }
    // SAFETY: `iov` is non-null (checked) and live (caller contract).
    let v = unsafe { &*iov };
    let entries: &[SysIOVec] = if v.iovcnt <= 0 || v.iov.is_null() {
        // An empty vector: the C's `erts_ioq_enqv` reports success without enqueuing
        // (`erl_io_queue.c:246`).
        &[]
    } else {
        // SAFETY: the caller declares `iovcnt` readable entries at `iov`.
        unsafe { core::slice::from_raw_parts(v.iov, v.iovcnt as usize) }
    };
    let vecs: Vec<(*const u8, usize)> = entries
        .iter()
        .map(|e| (e.iov_base.cast::<u8>() as *const u8, e.iov_len))
        .collect();
    queue.enqueue(&vecs, skip)
}

/// `size_t enif_ioq_size(ErlNifIOQueue *q);`
///
/// # Safety
///
/// `q` must be a live queue handle (`NULL` reports `0` here; the C would fault).
#[no_mangle]
pub unsafe extern "C" fn enif_ioq_size(q: *mut ErlNifIOQueue) -> size_t {
    // SAFETY: `queue_mut` documents the handle contract; a null handle is `None`.
    match unsafe { queue_mut(q) } {
        Some(queue) => queue.size,
        None => 0,
    }
}

/// `int enif_ioq_deq(ErlNifIOQueue *q, size_t count, size_t *size);`
///
/// # Safety
///
/// `q` must be a live queue handle; `size` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_ioq_deq(
    q: *mut ErlNifIOQueue,
    count: size_t,
    size: *mut size_t,
) -> c_int {
    let Some(queue) = (unsafe { queue_mut(q) }) else {
        return 0;
    };
    if queue.size < count {
        // `erts_ioq_deq` returns -1 and leaves the queue untouched (`erl_io_queue.c:351`).
        return 0;
    }
    queue.size -= count;
    let mut left = count;
    while left > 0 {
        let Some(head) = queue.chunks.front_mut() else {
            // Unreachable while the `size` bookkeeping holds; keeps the loop total.
            break;
        };
        let head_len = head.len();
        if head_len <= left {
            left -= head_len;
            queue.chunks.pop_front();
        } else {
            head.start += left;
            left = 0;
        }
    }
    queue.refresh_view();
    // The C reports the size that *remains* after the dequeue (`erl_nif.c:4394`).
    if !size.is_null() {
        // SAFETY: `size` is non-null and writable (checked; caller contract).
        unsafe { *size = queue.size };
    }
    1
}

/// `SysIOVec *enif_ioq_peek(ErlNifIOQueue *q, int *iovlen);`
///
/// The returned pointer is the queue's own view, valid until the queue is modified (exactly the C's
/// contract for `q->v_head`).
///
/// # Safety
///
/// `q` must be null or a live queue handle; `iovlen` must be null or writable. The returned pointer
/// must not be used after the next call that modifies `q`.
#[no_mangle]
pub unsafe extern "C" fn enif_ioq_peek(q: *mut ErlNifIOQueue, iovlen: *mut c_int) -> *mut SysIOVec {
    // SAFETY: `queue_mut` documents the handle contract; a null handle is `None`.
    let Some(queue) = (unsafe { queue_mut(q) }) else {
        // A null queue: `*-1` and `NULL` (`erl_io_queue.c:401-403`).
        if !iovlen.is_null() {
            // SAFETY: `iovlen` is non-null and writable (checked).
            unsafe { *iovlen = -1 };
        }
        return core::ptr::null_mut();
    };
    if !iovlen.is_null() {
        // SAFETY: `iovlen` is non-null and writable (checked).
        unsafe { *iovlen = queue.view.len() as c_int };
    }
    if queue.view.is_empty() {
        // An empty queue has no vectors (`erl_io_queue.c:405-406`).
        return core::ptr::null_mut();
    }
    queue.view.as_mut_ptr()
}

/// `int enif_ioq_peek_head(ErlNifEnv *env, ErlNifIOQueue *q, size_t *size, ERL_NIF_TERM *head);`
///
/// # Safety
///
/// `q` must be a live queue handle; `env` must be a live env; `size` must be null or writable and
/// `head` must be writable.
#[no_mangle]
pub unsafe extern "C" fn enif_ioq_peek_head(
    env: *mut crate::nif::types::ErlNifEnv,
    q: *mut ErlNifIOQueue,
    size: *mut size_t,
    head: *mut ERL_NIF_TERM,
) -> c_int {
    // SAFETY: `queue_mut` documents the handle contract; a null handle is `None`.
    let Some(queue) = (unsafe { queue_mut(q) }) else {
        return 0;
    };
    if queue.size == 0 {
        // The C returns before touching either out-parameter (`erl_nif.c:4402`).
        return 0;
    }
    let Some(chunk) = queue.chunks.front() else {
        return 0;
    };
    let bytes = chunk.pending();
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    if !size.is_null() {
        // SAFETY: `size` is non-null and writable (checked; caller contract).
        unsafe { *size = bytes.len() };
    }
    if head.is_null() {
        return 0;
    }
    let n_bits = bytes.len() * 8;
    // SAFETY: `heap_bits_size_words` sizes the `[hdr][size][data…]` block for `n_bits`.
    let Some(hp) = (unsafe { alloc(e, prelude::heap_bits_size_words(n_bits)) }) else {
        return 0;
    };
    // SAFETY: `hp` heads the reserved block and `bytes` provides exactly the payload it needs.
    let Ok(term) = (unsafe { prelude::write_binary(hp, bytes) }) else {
        return 0;
    };
    // SAFETY: `head` is non-null and writable (checked).
    unsafe { head.write(n(term)) };
    1
}

// ---------------------------------------------------------------------------------------------
// enif_inspect_iovec / enif_free_iovec
// ---------------------------------------------------------------------------------------------

/// Round `n` up to a multiple of 8 — the allocation unit of the term heap and the alignment every
/// member of an [`ErlNifIOVec`] needs.
const fn align_up8(n: usize) -> usize {
    (n + 7) & !7
}

/// The offsets inside one allocation of `n` vectors: `(SysIOVec array, ref_bins array, total bytes)`
/// — the C's single-block layout with `ERTS_ALC_DATA_ALIGN_SIZE` replaced by 8
/// (`erl_nif.c:4304-4308`).
fn block_layout(n: usize) -> (usize, usize, usize) {
    let iov_offset = align_up8(core::mem::size_of::<ErlNifIOVec>());
    let binv_offset = iov_offset + align_up8(n * core::mem::size_of::<SysIOVec>());
    let total = binv_offset + n * core::mem::size_of::<*mut c_void>();
    (iov_offset, binv_offset, total)
}

/// Initialise the `ErlNifIOVec` header of a [`block_layout`] block.
///
/// # Safety
///
/// `base` must point at `block_layout(n).2` writable, 8-aligned bytes owned exclusively by this
/// vector.
unsafe fn init_iovec_block(base: *mut u8, n: usize, size: usize, flags: c_int) -> *mut ErlNifIOVec {
    let (iov_offset, binv_offset, _) = block_layout(n);
    let vec = base.cast::<ErlNifIOVec>();
    // SAFETY: `base` is the caller's exclusively-owned block and both offsets are inside it by
    // construction; the header and the two arrays cover disjoint ranges.
    unsafe {
        (*vec).iovcnt = n as c_int;
        (*vec).size = size;
        (*vec).iov = base.add(iov_offset).cast::<SysIOVec>();
        (*vec).ref_bins = base.add(binv_offset).cast::<*mut c_void>();
        (*vec).flags = flags;
    }
    vec
}

/// Write `elems` into `vec`'s vectors, recording every entry as "plain memory, no binary held"
/// (`ref_bins[i] = NULL` — the C's own encoding for exactly that, `erl_io_queue.c:261`).
///
/// # Safety
///
/// `vec` must be a live `ErlNifIOVec` whose `iov` and `ref_bins` each point at at least
/// `elems.len()` writable entries.
unsafe fn fill_iovec(vec: *mut ErlNifIOVec, elems: &[(*const u8, usize)]) {
    // SAFETY: the caller guarantees `iov`/`ref_bins` have room for `elems.len()` entries.
    unsafe {
        for (i, &(ptr, len)) in elems.iter().enumerate() {
            (*(*vec).iov.add(i)).iov_base = ptr.cast_mut().cast::<c_void>();
            (*(*vec).iov.add(i)).iov_len = len;
            *(*vec).ref_bins.add(i) = core::ptr::null_mut();
        }
    }
}

/// `int enif_inspect_iovec(ErlNifEnv *env, size_t max_length, ERL_NIF_TERM iovec_term,
/// ERL_NIF_TERM *tail, ErlNifIOVec **iovec);`
///
/// # Safety
///
/// `iovec_term` is any term; `tail` must be writable and `iovec` must hold a readable pointer
/// (`NULL`, or a live preallocated `ErlNifIOVec`). `env` may be null — that is the documented way
/// to obtain a vector that outlives the NIF call, and it then owns its storage.
#[no_mangle]
pub unsafe extern "C" fn enif_inspect_iovec(
    env: *mut crate::nif::types::ErlNifEnv,
    max_length: size_t,
    iovec_term: ERL_NIF_TERM,
    tail: *mut ERL_NIF_TERM,
    iovec: *mut *mut ErlNifIOVec,
) -> c_int {
    if iovec.is_null() || tail.is_null() {
        return 0;
    }
    // The in-value: `NULL`, or a caller-preallocated structure to use "if possible" (manual for
    // `enif_inspect_iovec`).
    // SAFETY: `iovec` is non-null (checked) and the caller guarantees it holds a readable pointer.
    let provided = unsafe { *iovec };

    // --- examine_iovec_term (`erl_nif.c:4113-4173`) ------------------------------------------
    let mut elems: Vec<(*const u8, usize)> = Vec::new();
    let mut elements: usize = 0;
    let mut rest = t(iovec_term);
    while prelude::is_list(rest) {
        // SAFETY: `rest` is a live cons cell.
        let (head_term, next) = unsafe { (prelude::car(rest), prelude::cdr(rest)) };
        if !is_term_word(head_term) {
            return 0;
        }
        // SAFETY: `is_bitstring` only dereferences the header of a boxed term.
        if !unsafe { prelude::is_bitstring(head_term) } {
            return 0;
        }
        // SAFETY: `head_term` is a live bitstring.
        let bits = unsafe { prelude::bitstring_size(head_term) };
        if bits & 7 != 0 {
            // `TAIL_BITS(size) != 0` — not a whole number of bytes (`erl_nif.c:4141`).
            return 0;
        }
        if bits > 0 {
            // SAFETY: as above; `bitstring_parts` reports `(base, bit_offset, bit_size)`.
            let (base, offset, _) = unsafe { prelude::bitstring_parts(head_term) };
            if offset & 7 != 0 || base.is_null() {
                // An unaligned base needs the C's scratch copy; without a refcounted scratch
                // binary this seam refuses (see the module docs).
                return 0;
            }
            let mut p = unsafe { base.add(offset / 8) };
            let mut left = bits / 8;
            // Split by `MAX_SYSIOVEC_IOVLEN`, like the C (`erl_nif.c:4154-4155`).
            while left > 0 {
                let chunk_len = core::cmp::min(left, MAX_SYSIOVEC_IOVLEN);
                elems.push((p, chunk_len));
                // SAFETY: `chunk_len` bytes are inside the live term's payload.
                p = unsafe { p.add(chunk_len) };
                left -= chunk_len;
            }
        }
        rest = next;
        elements += 1;
        // `max_elements` limits list *elements*, not vectors (`erl_nif.c:4161`).
        if elements >= max_length {
            break;
        }
    }
    if !prelude::is_nil(rest) && !prelude::is_list(rest) {
        // An improper-list tail (`erl_nif.c:4166`).
        return 0;
    }

    // --- create_iovec_from_slice (`erl_nif.c:4291-4340`) --------------------------------------
    let size: usize = elems.iter().map(|&(_, len)| len).sum();
    // `nvec`, not `n`: the sibling helper `super::n` (internal term → NIF term) is in scope here.
    let nvec = elems.len();
    let vec = if !provided.is_null() && nvec < ERL_NIF_IOVEC_SIZE {
        // Use the caller's structure and its inline arrays (`erl_nif.c:4296-4299`).
        // SAFETY: the caller guarantees `provided` is a live, unaliased `ErlNifIOVec`.
        unsafe {
            (*provided).iovcnt = nvec as c_int;
            (*provided).size = size;
            (*provided).iov = (*provided).small_iov.as_mut_ptr();
            (*provided).ref_bins = (*provided).small_ref_bin.as_mut_ptr();
            (*provided).flags = FLAGS_PREALLOC;
        }
        provided
    } else {
        let (_, _, total) = block_layout(nvec);
        let base = if env.is_null() {
            // The caller owns (and must `enif_free_iovec`) an allocation made without an env.
            // SAFETY: `malloc` yields a `total`-byte block or null; `total > 0`.
            unsafe { libc::malloc(total) }.cast::<u8>()
        } else {
            let Some(e) = (unsafe { env_mut(env) }) else {
                return 0;
            };
            // SAFETY: `align_up8(total) / 8` fresh words from the env's term heap (8-aligned, and
            // never reallocated).
            match unsafe { alloc(e, align_up8(total) / 8) } {
                Some(hp) => hp.cast::<u8>(),
                None => return 0,
            }
        };
        if base.is_null() {
            return 0;
        }
        // Without an env the vector owns its storage (`flags = 0`) and `enif_free_iovec` frees it;
        // with one it lives in the env's heap, so it is marked preallocated and never freed.
        // SAFETY: `base` addresses exactly `block_layout(nvec).2` exclusively-owned writable bytes.
        unsafe {
            init_iovec_block(base, nvec, size, if env.is_null() { 0 } else { FLAGS_PREALLOC })
        }
    };
    // SAFETY: `vec` is a live `ErlNifIOVec` whose `iov`/`ref_bins` have room for `nvec` entries.
    unsafe { fill_iovec(vec, &elems) };
    // SAFETY: `tail` is writable and `iovec` is a writable out-parameter (both checked above).
    unsafe {
        tail.write(n(rest));
        iovec.write(vec);
    }
    1
}

/// `void enif_free_iovec(ErlNifIOVec *iov);`
///
/// # Safety
///
/// `iov` must be null, or a vector returned by [`enif_inspect_iovec`] that has not been freed.
#[no_mangle]
pub unsafe extern "C" fn enif_free_iovec(iov: *mut ErlNifIOVec) {
    if iov.is_null() {
        return;
    }
    // SAFETY: `iov` is non-null (checked) and live (caller contract).
    let flags = unsafe { (*iov).flags };
    if flags & FLAGS_PREALLOC != 0 {
        // The structure and its arrays belong to the caller or to the env's term heap; the C's own
        // `enif_free_iovec` has the same early exit (`erl_nif.c:4097`).
        return;
    }
    // The C drops a binary reference per vector entry (`erl_nif.c:4088-4095`); every `ref_bins[i]`
    // here is `NULL` (see the module docs), so there is nothing to release.
    // SAFETY: an unflagged vector was allocated by `enif_inspect_iovec` without an env, as one
    // `malloc`ed block holding the header and both arrays, so freeing the base releases all of it.
    unsafe { libc::free(iov.cast::<c_void>()) };
}
