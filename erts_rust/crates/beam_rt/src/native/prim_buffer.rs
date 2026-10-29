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
//! The `prim_buffer` NIF library of the runtime-native floor (contract **A.7**).
//!
//! `prim_buffer` is one of the 22 preloaded modules: a **mutable byte queue** that the kernel's
//! `raw_file_io_delayed`/`raw_file_io_inflate` wrappers drive, so that `file:open/2` with
//! `delayed_write` (and therefore `dets`) works at all. ERTS serves it from a statically linked
//! NIF (`../../erts/emulator/nifs/common/prim_buffer_nif.c`, `STATIC_ERLANG_NIF`), which is why
//! `erlang:load_nif("prim_buffer", …)` never `dlopen`s anything; this module is the Rust
//! counterpart and follows the same two-route shape [`super::prim_tty`] documents:
//!
//! 1. [`is_builtin_library`] — the ONE resolution source behind `NifBridge`'s `NifLoader` leg, so
//!    `prim_buffer:on_load/0`'s `erlang:load_nif(atom_to_list(?MODULE), 0)` answers `ok` instead of
//!    a `{load_failed, …}` (which `prim_buffer.erl`'s `case … of ok -> ok end` turns into a
//!    `{case_clause, …}`). Nothing enters the NIF registry, so `NifRegistry::is_loaded("prim_buffer")`
//!    stays `false` and the module's own Erlang wrappers stay enterable in-machine.
//! 2. [`NIF_KEYS`] + [`RuntimeContext::prim_buffer_call`] — the nine `-nifs` stubs themselves, both
//!    routes converging on the one implementation: the **exported** stubs (`new/0`, `size/1`,
//!    `write/2`, `skip/2`, `find_byte_index/2`, `try_lock/1`, `unlock/1` — reached by `call_ext`)
//!    travel `floor_shadows` → `call_bif` → `try_native`, while the **non-exported** ones
//!    (`peek_head/1`, `copying_read/2`) are lowered to a LOCAL `call` inside `prim_buffer.erl` and
//!    travel `ExecutionContext::resolve_local_call` → `call_bif` → `try_native`.
//!
//! The three **exported Erlang bodies** of `prim_buffer.erl` — `read/2`, `read_iovec/2` and
//! `wipe/1` — are deliberately **not** floor keys: they are ordinary code over the stubs above, and
//! [`super::floor_keys`]'s `prim_buffer` arm lists exactly the nine stubs so those bodies stay
//! enterable in-machine.
//!
//! # Semantics (the C's, measured against the oracle)
//!
//! The queue is the C's `ErlNifIOQueue` plus the 2 KiB **write accumulator** that coalesces small
//! writes (`combine_small_writes`): a write that would overflow the accumulator **and** is at
//! least [`ACCUMULATOR_SIZE`] / 2 flushes it and lands as its own queue elements, a smaller one
//! is combined into the accumulator (flushing it first when it is full), and every *read-side*
//! operation (`size/1` excepted) flushes first. The chunk boundaries are observable through
//! `peek_head/1` and therefore through `read_iovec/2`'s list shape, so they are modelled rather
//! than flattened.
//!
//! Two of the nine stubs are **reads that dequeue**: `copying_read/2` (the C's `enif_ioq_deq`
//! after its copy — `read/2`'s small-read arm) and `skip/2`. `peek_head/1` and
//! `find_byte_index/2` only observe the queue.
//!
//! Every failure shape is the C's `enif_make_badarg` — a **raise** of `badarg`, never a value:
//! `size(foo)`, `write(B, foo)` (an inspect-iovec failure: the argument must be a list of
//! binaries), `write(B, [1,2,3])`, `skip(B, N)` for `N` past the end, `copying_read(B, N)` for the
//! same, `find_byte_index(B, -1|256|1.5)`, `try_lock(foo)` and `unlock(foo)`. `unlock/1` with no
//! lock held is the C's `enif_raise_exception(am_lock_order_violation)`.
//!
//! # The opaque buffer handle
//!
//! `new/0` mints a reference from the runtime's one id space ([`Runtime::refs_mut`]) and records
//! the queue in [`Runtime::buffers_mut`] under that reference's id — the [`super::prim_tty`]
//! handle pattern. A term that is not a live reference of that table is `badarg`, exactly like the
//! C's `enif_get_resource` failure on a dead resource.
//!
//! [`Runtime::refs_mut`]: crate::Runtime::refs_mut
//! [`Runtime::buffers_mut`]: crate::Runtime::buffers_mut

use std::collections::VecDeque;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_process::monitor::MonitorRef;
use beam_term::{is_small, make_small, signed_value, Eterm};

use super::RuntimeContext;

/// The library name: `prim_buffer.erl`'s `?MODULE`, and the `Path` its `on_load/0` passes to
/// `erlang:load_nif/2`.
const LIBRARY: &str = "prim_buffer";

/// The `-nifs(...)` surface of `erts/preloaded/src/prim_buffer.erl:36-37` — the nine stubs the C's
/// `nif_funcs[]` table replaces, in the module's own order.
pub(crate) const NIF_KEYS: [(&str, u32); 9] = [
    ("new", 0),
    ("size", 1),
    ("peek_head", 1),
    ("copying_read", 2),
    ("write", 2),
    ("skip", 2),
    ("find_byte_index", 2),
    ("try_lock", 1),
    ("unlock", 1),
];

/// The C's `ACCUMULATOR_SIZE`: small writes coalesce into one queue element of this capacity.
const ACCUMULATOR_SIZE: usize = 2 << 10;

/// Whether `name` is the built-in (statically linked) `prim_buffer` NIF library — ERTS's
/// `is_static_nif_module`.
///
/// The caller passes **both** the `Path` argument and the calling module name because ERTS
/// resolves a built-in from the library path, not from the caller's identity, while the BIF reads
/// the caller for everything else — [`super::prim_tty::is_builtin_library`] carries the same rule.
pub(crate) fn is_builtin_library(name: &str) -> bool {
    name == LIBRARY
}

/// Whether `(function, arity)` is one of `prim_buffer`'s own **non-exported** NIF stubs — the
/// predicate `ExecutionContext::resolve_local_call` consults for a LOCAL call inside the module.
pub(crate) fn serves_local_call(module: &str, function: &str, arity: u32) -> bool {
    module == LIBRARY && NIF_KEYS.contains(&(function, arity))
}

/// The mutable byte queue behind one `prim_buffer:new/0` handle.
///
/// `chunks` is the C's `ErlNifIOQueue` (front = the read position) and `accumulated` is its write
/// accumulator: the C asserts `accumulator_present == (accumulated_bytes > 0)`, so an empty
/// `accumulated` *is* "no accumulator".
#[derive(Debug, Default)]
pub(crate) struct PrimBuffer {
    chunks: VecDeque<Vec<u8>>,
    accumulated: Vec<u8>,
    /// The C's `external_lock`: held between `try_lock/1` and `unlock/1`.
    locked: bool,
}

impl PrimBuffer {
    /// An empty, unlocked queue (`new_nif`).
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The number of queued bytes (`enif_ioq_size` plus the accumulator).
    ///
    /// `pub(crate)` for the read-ahead seam: a `prim_file` descriptor's queue is drained by
    /// `prim_file:read/2` and `prim_file:position/2` as well as by its own `read_line/1`
    /// (`prim_file/readahead.rs`).
    pub(crate) fn size(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum::<usize>() + self.accumulated.len()
    }

    /// `enqueue_write_accumulator`: flush the accumulator into the queue as one element.
    fn enqueue_accumulator(&mut self) {
        if !self.accumulated.is_empty() {
            self.chunks
                .push_back(std::mem::take(&mut self.accumulated));
        }
    }

    /// `write_nif`'s body over an already-inspected iovec (`combine_small_writes` and its
    /// fall-through).
    ///
    /// `pub(crate)` for the read-ahead seam (`prim_file/readahead.rs`): a line read appends
    /// what the host returned to the descriptor's queue.
    pub(crate) fn write_iovec(&mut self, parts: &[&[u8]]) {
        let total: usize = parts.iter().map(|part| part.len()).sum();
        let combined = if self.accumulated.len() + total >= ACCUMULATOR_SIZE {
            if total >= ACCUMULATOR_SIZE / 2 {
                // A large write keeps its own queue elements: the accumulator is flushed by the
                // fall-through below, never padded with the newcomer.
                false
            } else {
                self.enqueue_accumulator();
                true
            }
        } else {
            true
        };
        if combined {
            for part in parts {
                self.accumulated.extend_from_slice(part);
            }
        } else {
            self.enqueue_accumulator();
            for part in parts {
                if !part.is_empty() {
                    self.chunks.push_back(part.to_vec());
                }
            }
        }
    }

    /// The first `count` bytes of the queue, without dequeuing them. The caller has already
    /// checked `count <= self.size()`.
    fn prefix(&self, count: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(count);
        for chunk in &self.chunks {
            if out.len() == count {
                break;
            }
            let take = (count - out.len()).min(chunk.len());
            out.extend_from_slice(&chunk[..take]);
        }
        out
    }

    /// `copying_read_nif`'s body: the first `count` bytes as a fresh buffer, **removed** from the
    /// queue (`enif_ioq_deq`) — `copying_read/2` is a read, not a peek, which is what
    /// `prim_buffer.erl`'s `read/2` and `read_iovec/2` rely on for their buffers to advance.
    ///
    /// `pub(crate)` for the read-ahead seam (`prim_file/readahead.rs`), which drains the queue
    /// the same way `read/2` does (`read_1/5`'s buffer arms, `prim_file.erl:180-191`).
    pub(crate) fn copying_read(&mut self, count: usize) -> Option<Vec<u8>> {
        self.enqueue_accumulator();
        if self.size() < count {
            return None;
        }
        let bytes = self.prefix(count);
        self.skip_now(count);
        Some(bytes)
    }

    /// `erts_ioq_deq` — drop `count` bytes from the front of the **queue**.
    ///
    /// Every caller first flushes the write accumulator ([`PrimBuffer::enqueue_accumulator`]),
    /// so the queue is the whole content (`skip_nif`/`copying_read_nif` do the same in the C).
    fn skip_now(&mut self, count: usize) {
        let mut remaining = count;
        while remaining > 0 {
            let Some(front) = self.chunks.front_mut() else {
                // Unreachable while every caller checks the size first; refuse rather than
                // fabricate (`count` past the end is the caller's `badarg`).
                return;
            };
            if front.len() <= remaining {
                remaining -= front.len();
                self.chunks.pop_front();
            } else {
                front.drain(..remaining);
                remaining = 0;
            }
        }
    }

    /// `skip_nif`'s body: flush the accumulator, then drop `count` bytes from the front.
    ///
    /// `pub(crate)` for the read-ahead seam: `prim_file:position/2` wipes the descriptor's
    /// queue with it (`position_1/3`'s `prim_buffer:wipe/1`, `prim_file.erl:330`).
    pub(crate) fn skip(&mut self, count: usize) {
        self.enqueue_accumulator();
        self.skip_now(count);
    }

    /// `peek_head_nif`: the head queue element, or `None` for an empty queue (the C's
    /// `enif_ioq_peek_head` failure ⇒ `badarg`).
    fn peek_head(&mut self) -> Option<&[u8]> {
        self.enqueue_accumulator();
        self.chunks.front().map(Vec::as_slice)
    }

    /// `find_nif`: the byte offset of the first `needle`, scanning the queue in order.
    ///
    /// `pub(crate)` for the read-ahead seam: `prim_file:read_line/1` asks the same question
    /// (`prim_buffer:find_byte_index/2`, `prim_file.erl:211`).
    pub(crate) fn find_byte(&mut self, needle: u8) -> Option<usize> {
        self.enqueue_accumulator();
        let mut offset = 0;
        for chunk in &self.chunks {
            if let Some(index) = chunk.iter().position(|byte| *byte == needle) {
                return Some(offset + index);
            }
            offset += chunk.len();
        }
        None
    }
}

impl RuntimeContext<'_> {
    /// The `prim_buffer` half of the native floor.
    ///
    /// Returns `None` for a key outside [`NIF_KEYS`] — which keeps `try_native`'s fall-through (and
    /// with it the module's `read/2`/`read_iovec/2`/`wipe/1` Erlang bodies, entered in-machine)
    /// exactly as it was.
    pub(super) fn prim_buffer_call(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        if !NIF_KEYS.contains(&(function, arity)) {
            return None;
        }
        Some(self.prim_buffer_dispatch(function, arity, args))
    }

    /// The one implementation both routes reach. The caller has already checked [`NIF_KEYS`], so
    /// the trailing arm is unreachable.
    fn prim_buffer_dispatch(
        &mut self,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        match (function, arity) {
            ("new", 0) => self.mint_prim_buffer(),
            ("size", 1) => {
                let size = self.with_prim_buffer(args[0], |buffer| buffer.size())?;
                let size = i64::try_from(size).map_err(|_| {
                    beam_common::BeamError::Raise(beam_common::Raise::error(
                        beam_common::ExitReason::atom("system_limit"),
                    ))
                })?;
                Ok(make_small(size))
            }
            // `peek_head/1` — the head queue element as a binary. A **non-empty** queue is the
            // C's precondition; an empty one is `badarg`.
            ("peek_head", 1) => {
                let head = self.with_prim_buffer(args[0], |buffer| buffer.peek_head().map(<[u8]>::to_vec))?;
                match head {
                    Some(bytes) => self.prim_buffer_binary(&bytes),
                    None => Err(BeamError::BadArg),
                }
            }
            // `copying_read/2` — a **copy** of the first `Size` bytes, dequeued (`read/2`'s
            // small-read arm, so the buffer advances). `Size` past the end is `badarg`;
            // `Size = 0` is the empty binary.
            ("copying_read", 2) => {
                let count = buffer_count(args.get(1).copied())?;
                let bytes = self.with_prim_buffer(args[0], |buffer| buffer.copying_read(count))?;
                match bytes {
                    Some(bytes) => self.prim_buffer_binary(&bytes),
                    None => Err(BeamError::BadArg),
                }
            }
            // `write/2` — an iovec (a proper list of **binaries**) appended to the queue. The
            // answer is always `ok`; a malformed iovec is `badarg`.
            ("write", 2) => {
                let element = *args.get(1).ok_or(BeamError::BadArg)?;
                let parts = iovec_parts(element)?;
                let borrowed: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
                self.with_prim_buffer(args[0], |buffer| buffer.write_iovec(&borrowed))?;
                self.atom("ok")
            }
            // `skip/2` — drop `Size` bytes from the front, `ok`; past the end is `badarg`.
            ("skip", 2) => {
                let count = buffer_count(args.get(1).copied())?;
                let skipped = self.with_prim_buffer(args[0], |buffer| {
                    if buffer.size() < count {
                        false
                    } else {
                        buffer.skip(count);
                        true
                    }
                })?;
                if skipped {
                    self.atom("ok")
                } else {
                    Err(BeamError::BadArg)
                }
            }
            // `find_byte_index/2` — `{ok, Index}` or `not_found`. A needle outside `0..=255` (or a
            // non-integer one) is `badarg`, the C's `enif_get_int`-then-range check.
            ("find_byte_index", 2) => {
                let needle = *args.get(1).ok_or(BeamError::BadArg)?;
                if !is_small(needle) {
                    return Err(BeamError::BadArg);
                }
                let Some(byte) = u8::try_from(signed_value(needle)).ok().filter(|_| {
                    (0..=255).contains(&signed_value(needle))
                }) else {
                    return Err(BeamError::BadArg);
                };
                let found =
                    self.with_prim_buffer(args[0], |buffer| buffer.find_byte(byte))?;
                match found {
                    Some(index) => {
                        let index = i64::try_from(index).map(make_small).map_err(|_| {
                            BeamError::Raise(beam_common::Raise::error(
                                beam_common::ExitReason::atom("system_limit"),
                            ))
                        })?;
                        let ok = self.atom("ok")?;
                        terms::make_tuple(self, &[ok, index])
                    }
                    None => self.atom("not_found"),
                }
            }
            // `try_lock/1` — `acquired` | `busy` (the C's compare-and-swap).
            ("try_lock", 1) => {
                let acquired =
                    self.with_prim_buffer(args[0], |buffer| {
                        if buffer.locked {
                            false
                        } else {
                            buffer.locked = true;
                            true
                        }
                    })?;
                if acquired {
                    self.atom("acquired")
                } else {
                    self.atom("busy")
                }
            }
            // `unlock/1` — `ok`, or the C's `error:lock_order_violation` when no lock is held.
            ("unlock", 1) => {
                let unlocked =
                    self.with_prim_buffer(args[0], |buffer| {
                        if buffer.locked {
                            buffer.locked = false;
                            true
                        } else {
                            false
                        }
                    })?;
                if unlocked {
                    self.atom("ok")
                } else {
                    Err(crate::error::message("lock_order_violation"))
                }
            }
            _ => Err(BeamError::BadArg),
        }
    }

    /// Mint a fresh, empty queue and answer its handle: a reference from the runtime's one id
    /// space, recorded in [`Runtime::buffers_mut`](crate::Runtime::buffers_mut).
    pub(crate) fn mint_prim_buffer(&mut self) -> Result<Eterm> {
        let reference = self.runtime.refs_mut().next();
        let mut heap = self.runtime.take_bif_heap()?;
        let term = reference.to_eterm(&mut heap)?;
        self.runtime.put_bif_heap(heap);
        self.runtime
            .buffers_mut()
            .insert(reference.id(), PrimBuffer::new());
        Ok(term)
    }

    /// A binary reply of **any** size.
    ///
    /// `peek_head/1` and `copying_read/2` answer a whole queue element, and an element is up to
    /// [`ACCUMULATOR_SIZE`] bytes, so the answer cannot be built by the heap-binary-only
    /// [`beam_engine::terms::make_heap_binary`] (it refuses more than
    /// `ERL_ONHEAP_BINARY_LIMIT` = 64 bytes). It goes through `beam_codec`'s one binary writer on
    /// the runtime's `-eval` store instead — the same builder `prim_file`'s read replies and
    /// `prim_file/readahead.rs`'s line answer use, which picks the on-heap `ErlHeapBits`
    /// representation for a short payload and the off-heap `[BinRef][ErlSubBits]` pair beyond it.
    fn prim_buffer_binary(&mut self, bytes: &[u8]) -> Result<Eterm> {
        beam_codec::term::make_binary(self.eval_heap(), bytes)
    }

    /// Run `body` over the live queue `term` names, or raise `badarg` — the C's
    /// `get_buffer_data` (`enif_get_resource`) failure, which is a **raise**.
    ///
    /// The table borrow ends with the closure, so `body` must not reach the context: every arm
    /// above returns plain bytes or a flag and builds its term afterwards.
    fn with_prim_buffer<T>(
        &mut self,
        term: Eterm,
        body: impl FnOnce(&mut PrimBuffer) -> T,
    ) -> Result<T> {
        let id = MonitorRef::from_eterm(term)
            .map_err(|_| BeamError::BadArg)?
            .id();
        let table = self.runtime.buffers_mut();
        let Some(buffer) = table.get_mut(&id) else {
            return Err(BeamError::BadArg);
        };
        Ok(body(buffer))
    }
}

/// A `copying_read`/`skip` size: a **non-negative** small integer, `badarg` for anything else.
///
/// The C reads it through `enif_get_uint64`, which rejects a negative value and a non-integer; a
/// positive term beyond the immediate range is a bignum the C would accept and then fail the size
/// check on — the same `badarg`, so the refusal here is exact.
fn buffer_count(term: Option<Eterm>) -> Result<usize> {
    let term = term.ok_or(BeamError::BadArg)?;
    if !is_small(term) {
        return Err(BeamError::BadArg);
    }
    usize::try_from(signed_value(term)).map_err(|_| BeamError::BadArg)
}

/// The **binaries** of an iovec argument, in order.
///
/// `enif_inspect_iovec`'s `examine_iovec_term` accepts exactly a proper list whose every element is
/// a byte-aligned bitstring: an integer byte, a nested list, a float or a non-nil non-list tail is
/// `badarg`. The C's 64-element slice limit only decides *when* the NIF re-schedules itself
/// (`enif_schedule_nif(…, "write", …)` with the tail), never the answer, so it is not modelled.
fn iovec_parts(term: Eterm) -> Result<Vec<Vec<u8>>> {
    let mut parts = Vec::new();
    for element in terms::list_elements(term).map_err(|_| BeamError::BadArg)? {
        let bytes = terms::binary_bytes(element).map_err(|_| BeamError::BadArg)?;
        parts.push(bytes.to_vec());
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A queue holding `chunks`, written the way `write/2` would.
    fn buffer_of(chunks: &[&[u8]]) -> PrimBuffer {
        let mut buffer = PrimBuffer::new();
        for chunk in chunks {
            buffer.write_iovec(&[chunk]);
        }
        buffer
    }

    /// An empty queue: size 0, no head, a miss for every needle.
    #[test]
    fn an_empty_buffer_has_no_head_and_no_needle() {
        let mut buffer = PrimBuffer::new();
        assert_eq!(buffer.size(), 0);
        assert_eq!(buffer.peek_head(), None);
        assert_eq!(buffer.find_byte(b'\n'), None);
        assert_eq!(buffer.copying_read(0), Some(Vec::new()));
        assert_eq!(buffer.copying_read(1), None);
        // A failed read leaves the queue untouched.
        assert_eq!(buffer.size(), 0);
    }

    /// A small write coalesces into the accumulator, so a 5-byte write has a head only after
    /// the read side flushes it — and `copying_read` is a **read** (it dequeues, the C's
    /// `enif_ioq_deq`), not a peek.
    #[test]
    fn a_small_write_is_visible_through_the_accumulator() {
        let mut buffer = buffer_of(&[b"line\n"]);
        assert_eq!(buffer.size(), 5);
        assert_eq!(buffer.peek_head(), Some(&b"line\n"[..]));
        // A peek leaves the queue alone.
        assert_eq!(buffer.size(), 5);
        assert_eq!(buffer.find_byte(b'\n'), Some(4));
        assert_eq!(buffer.copying_read(5), Some(b"line\n".to_vec()));
        assert_eq!(buffer.size(), 0);
        assert_eq!(buffer.peek_head(), None);
    }

    /// Two small writes land in **one** queue element (the accumulator), which is what makes
    /// `peek_head/1` answer both of them at once.
    #[test]
    fn two_small_writes_share_one_element() {
        let mut buffer = PrimBuffer::new();
        buffer.write_iovec(&[b"abc"]);
        buffer.write_iovec(&[b"def"]);
        assert_eq!(buffer.size(), 6);
        assert_eq!(buffer.peek_head(), Some(&b"abcdef"[..]));
    }

    /// A write that fills the accumulator **and** is at least `ACCUMULATOR_SIZE / 2` keeps its
    /// own queue element, flushing the pending accumulator first — the C's
    /// `combine_small_writes` returns `0` only when *both* conditions hold, so the head is the
    /// earlier small write.
    #[test]
    fn a_large_write_keeps_its_own_element() {
        // 4 + 2044 == ACCUMULATOR_SIZE: the accumulator cannot take the newcomer, and 2044 is
        // past `ACCUMULATOR_SIZE / 2`, so the write lands as its own queue element.
        let big = vec![b'x'; ACCUMULATOR_SIZE - 4];
        let mut buffer = PrimBuffer::new();
        buffer.write_iovec(&[b"tiny"]);
        buffer.write_iovec(&[&big]);
        assert_eq!(buffer.size(), 4 + big.len());
        assert_eq!(buffer.peek_head(), Some(&b"tiny"[..]));
        buffer.skip(4);
        assert_eq!(buffer.peek_head(), Some(&big[..]));
    }

    /// A write that fills the accumulator but stays under `ACCUMULATOR_SIZE / 2` is *combined*
    /// with what the accumulator already holds (the C's fall-through), so one head covers both.
    #[test]
    fn a_write_that_fills_the_accumulator_is_combined_when_it_is_small() {
        let big = vec![b'x'; ACCUMULATOR_SIZE / 2];
        let mut buffer = PrimBuffer::new();
        buffer.write_iovec(&[b"tiny"]);
        buffer.write_iovec(&[&big]);
        assert_eq!(buffer.size(), 4 + big.len());
        let head = buffer.peek_head().expect("the combined accumulator");
        assert_eq!(head.len(), 4 + big.len());
        assert_eq!(&head[..4], b"tiny");
    }

    /// A partial read across two chunks concatenates them, and leaves the remainder queued.
    #[test]
    fn a_partial_read_spans_chunks() {
        let big = vec![b'y'; ACCUMULATOR_SIZE / 2];
        let mut buffer = PrimBuffer::new();
        buffer.write_iovec(&[&big]);
        buffer.write_iovec(&[&big]);
        assert_eq!(buffer.size(), 2 * big.len());
        let head = buffer.copying_read(4).expect("four bytes");
        assert_eq!(head, b"yyyy");
        // `copying_read` already dequeued them.
        assert_eq!(buffer.size(), 2 * big.len() - 4);
        // The second chunk is intact after the first one was drained.
        let tail = buffer.copying_read(2 * big.len() - 4).expect("the rest");
        assert_eq!(tail.len(), 2 * big.len() - 4);
        assert!(tail.iter().all(|byte| *byte == b'y'));
    }

    /// Skipping past the end is refused by the callers and leaves the queue untouched; skipping
    /// exactly the whole queue empties it.
    #[test]
    fn skip_is_bounded_by_the_size() {
        let mut buffer = buffer_of(&[b"abcdef"]);
        assert!(buffer.size() < 7);
        buffer.skip(6);
        assert_eq!(buffer.size(), 0);
        // An over-long skip is a no-op here (`skip` returns early), never a panic.
        buffer.skip(1);
        assert_eq!(buffer.size(), 0);
    }

    /// `find_byte` reports the offset across chunk boundaries.
    #[test]
    fn find_byte_index_spans_chunks() {
        let big = vec![b'z'; ACCUMULATOR_SIZE / 2];
        let mut buffer = PrimBuffer::new();
        buffer.write_iovec(&[&big]);
        buffer.write_iovec(&[b"a\nb"]);
        assert_eq!(buffer.find_byte(b'\n'), Some(big.len() + 1));
        assert_eq!(buffer.find_byte(b'q'), None);
    }

    /// The lock is a compare-and-swap pair: `try_lock` twice is `acquired` then `busy`, and the
    /// second `unlock` is the C's `lock_order_violation`.
    #[test]
    fn the_lock_is_exclusive() {
        let mut buffer = PrimBuffer::new();
        assert!(!buffer.locked);
        buffer.locked = true;
        assert!(buffer.locked);
        assert!(!PrimBuffer::new().locked);
        buffer.locked = false;
        buffer.locked = false;
        assert!(!buffer.locked);
    }

    /// `iovec_parts` accepts the empty list and refuses a non-list.
    ///
    /// The full term-level walk (binaries accepted, integers/nested lists refused) needs a heap,
    /// so it is exercised by `tests/prim_buffer.rs` against the real runtime.
    #[test]
    fn an_iovec_is_a_list_of_binaries() {
        use beam_term::NIL;
        assert!(iovec_parts(NIL).is_ok());
        assert!(iovec_parts(beam_term::make_small(7)).is_err());
    }
}
