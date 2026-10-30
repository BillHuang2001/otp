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

//! The `-eval` evaluator's **off-heap bitstring writer**: a grown-capacity payload
//! behind a frozen `[BinRef][ErlSubBits]` pair.
//!
//! The tree-walker builds a `<<…>>` result through a [`super::BitBuf`] accumulator and
//! then materializes it. A small result stays on-heap, but a result above
//! [`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`] must become a refc binary: the 8-word
//! on-heap pair whose `BinRef.val` names an off-heap payload ([`super::materialize_bits`]).
//!
//! # Why a grown capacity, not an exact-size payload
//!
//! The C `erl_eval` `create_binary` runs a `bs_create_bin` **writable accumulator** —
//! initial alloc `MAX(alloc_size, 256)` and growth `GROW_PROC_BIN_SIZE(used)` (2× below
//! 16 MiB, 1.2× above) — so the payload the term names is **wider** than the apparent
//! byte size once the accumulator grew. `binary:referenced_byte_size/1` answers that
//! payload's `orig_size` ([`beam_bifs`]'s C-faithful reader reads
//! [`beam_alloc::payload_capacity`]), so `<<"z",(binary:copy(<<255>>,1024))/binary>>`
//! reports `{byte_size, referenced_byte_size} == {1025, 2050}`, not `{1025, 1025}`.
//!
//! [`make_refc_binary`] reproduces the accumulator's **final** `orig_size` with a single
//! allocation: start at the 256-byte floor, append the bytes, and let
//! [`beam_alloc::RefcBinary::append`] raise the capacity through the exact same
//! `grow_span` rule (`2×` below 16 MiB, `1.2×` above). The apparent `byte_size` stays
//! `bits / 8` because the `ErlSubBits` window's size is the requested bit count, never
//! the capacity.

// The crate is `#![deny(unsafe_code)]` (workspace `CONTEXT.md` §3): `unsafe` is confined
// to explicitly-marked modules, and this is one of them. Its whole raw surface is the two
// writes [`make_refc_binary`] performs into words the caller's heap just handed out — the
// `beam_term` layout owners require `unsafe` to write raw term words, and both writes are
// bounded by the frozen sizes and kept alive by the returned handle.
#![allow(unsafe_code)]

use beam_alloc::{Heap, RefcBinary};
use beam_common::Result;
use beam_term::refc::{write_refc_binary, ERL_REFC_BITS_SIZE};
use beam_term::Eterm;

/// The C `erl_eval` writable accumulator's initial allocation floor
/// (`erl_bits.c`: `MAX(alloc_size, 256)`). A payload at or below this many bytes keeps
/// capacity 256; a larger one grows by [`beam_alloc::RefcBinary`]'s `grow_span` rule.
const INITIAL_ACCUMULATOR_CAPACITY: usize = 256;

/// Materializes a `<<…>>` result whose payload lives **off-heap**, mirroring the C
/// writable accumulator's grown capacity rather than an exact-size host payload.
///
/// Returns the tagged term and the [`RefcBinary`] handle that owns the payload. The
/// caller must retain the handle (see
/// [`crate::runtime::Runtime::adopt_escaped_binaries`]) for as long as the term is used:
/// the on-heap `BinRef.val` word names the payload's first data byte, and the payload is
/// owned by the handle, not by the heap.
///
/// `bytes` holds `ceil(bits / 8)` payload bytes, `bits` the window's size in bits (a
/// byte-aligned binary and a non-byte-aligned bitstring are handled uniformly: the
/// window starts at the payload's first byte and covers exactly `bits` bits).
pub(crate) fn make_refc_binary(
    heap: &mut Heap,
    bytes: &[u8],
    bits: usize,
) -> Result<(Eterm, RefcBinary)> {
    // The C accumulator starts with `MAX(alloc_size, 256)` writable bytes and appends;
    // `with_capacity` + `append` reproduces its final `orig_size` (the figure
    // `binary:referenced_byte_size/1` reports) in one allocation. `append` grows
    // `capacity()` to `grow_span(len)` only when the request exceeds the 256-byte
    // floor; otherwise the capacity stays 256, exactly the C's floor.
    let mut handle = RefcBinary::with_capacity(INITIAL_ACCUMULATOR_CAPACITY)?;
    handle.append(bytes)?;
    // `data_ptr` is the payload's first data byte, non-null and word-aligned (the
    // `beam_alloc` payload contract) and valid for as long as `handle` lives.
    let val = handle.data_ptr();

    // Only once the payload exists is the on-heap pair allocated, so a failed payload
    // allocation leaves the heap untouched.
    // SAFETY: `alloc_words` returned `ERL_REFC_BITS_SIZE` (8) exclusively-owned,
    // writable words.
    let hp = unsafe { heap.alloc_words(ERL_REFC_BITS_SIZE)? };
    // SAFETY: `hp` owns the 8 words the `[BinRef][ErlSubBits]` pair occupies — the
    // layout `write_refc_binary` documents. `val` is a non-null, word-aligned pointer
    // to `handle`'s payload (kept alive by the caller through the returned handle), and
    // `base = val` means the window starts at the payload's first data byte covering
    // the `bits` bits `bytes` holds (`bytes.len() >= ceil(bits / 8)`). `next = null` is
    // a valid chain link: nothing in this workspace walks the MSO chain.
    let term = unsafe {
        write_refc_binary(
            hp,
            val.cast::<Eterm>(),
            core::ptr::null_mut(),
            val,
            0,
            bits,
        )
    };
    Ok((term, handle))
}
