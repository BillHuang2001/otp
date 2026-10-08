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

//! The **off-heap binary seam**: the host hook that lets the decoder place a large
//! binary's payload outside the process heap.
//!
//! ERTS does not copy a binary larger than [`ERL_ONHEAP_BITS_LIMIT`] bits (512, i.e.
//! 64 bytes) onto the process heap: C's `case BINARY_EXT/BIT_BINARY_EXT`
//! (`../../erts/emulator/beam/external.c:4893-4980`) allocates an off-heap payload
//! with `erts_bin_nrml_alloc(NBYTES(size_in_bits))` and wraps it in an on-heap
//! `[BinRef][ErlSubBits]` pair with `erts_wrap_refc_bitstring(..., data, 0,
//! size_in_bits)`. Only `size_in_bits <= ERL_ONHEAP_BITS_LIMIT` becomes an on-heap
//! `ErlHeapBits` block.
//!
//! The **on-heap** half of that arrangement is frozen and owned by
//! [`beam_term::refc`] (the `BinRef` + `ErlSubBits` pair written by
//! [`beam_term::refc::write_refc_binary`]). The **off-heap payload** — its
//! allocation, its reference count and its release — is [`beam_alloc::RefcBinary`]'s,
//! which is *runtime* state the codec must not own: a decoded term has to outlive the
//! decoder, and reclamation follows the owning process's heap, not the ETF buffer.
//!
//! [`OffHeapBinaries`] is therefore an **optional host seam**. A caller that
//! installs one with [`Decoder::with_off_heap`](crate::Decoder::with_off_heap) hands
//! the payload's lifetime to the host; a caller that does not keeps the previous
//! behaviour unchanged (every binary materialized on-heap), so the seam is purely
//! additive. It complements [`AtomCacheWriter`](crate::AtomCacheWriter) and
//! [`AtomCache`](crate::AtomCache): the same "the codec owns the wire, the host owns
//! the state" split.

use beam_common::Result;
use beam_term::Eterm;

/// A host that owns the off-heap payload of a large decoded binary.
///
/// Installed on a [`Decoder`](crate::Decoder) with
/// [`Decoder::with_off_heap`](crate::Decoder::with_off_heap). When a bitstring
/// decodes to more than [`ERL_ONHEAP_BITS_LIMIT`](beam_term::refc::ERL_ONHEAP_BITS_LIMIT)
/// bits and a host is installed, the decoder routes the payload through
/// [`alloc_refc_payload`](OffHeapBinaries::alloc_refc_payload) and materializes the
/// term as the off-heap `[BinRef][ErlSubBits]` pair — exactly the C's `refc_binary`
/// path. With no host the payload stays on-heap (`ErlHeapBits`), which is still the
/// *same observable term* (`is_binary/1`, `byte_size/1`, contents, and the ETF image
/// are identical); only the interior representation differs.
///
/// The runtime (`beam_rt`) registers the off-heap payload with its MSO/`beam_gc`
/// bookkeeping and supplies the concrete implementation; the codec never sees that
/// state.
pub trait OffHeapBinaries {
    /// Copies `bytes` into a fresh off-heap refcounted payload and returns the value
    /// the on-heap `BinRef.val` word receives: a raw pointer to the payload's **first
    /// data byte**.
    ///
    /// An implementation typically builds a [`beam_alloc::RefcBinary`] over `bytes`,
    /// retains the handle in a store of its own and returns
    /// [`RefcBinary::data_ptr`](beam_alloc::RefcBinary::data_ptr) cast to
    /// `*mut Eterm` (the frozen `BinRef.val` word type — see
    /// [`beam_term::refc::bin_ref_parts`], whose `val` this is).
    ///
    /// # Contract
    ///
    /// * **Ownership.** The host retains the payload; the returned pointer stays valid,
    ///   unmoved and readable for at least as long as any term the decoder built from
    ///   it is reachable. The decoder keeps no record of the payload — the host's store
    ///   *is* the ownership record — so releasing it too early would dangle. (The
    ///   on-heap `BinRef.next` word is left null: the ERTS MSO off-heap chain link,
    ///   which nothing in this workspace walks, is documented at
    ///   [`crate::term::make_refc_binary`].)
    /// * **Contents.** The payload must hold `bytes`, at least for the bits the caller
    ///   asks for. Decoding passes the exactly-`NBYTES(size_in_bits)`-byte payload of a
    ///   full binary (`offset_bits = 0`), byte for byte as the C copies the wire data
    ///   into the freshly allocated binary.
    /// * **Alignment.** The first data byte must be word-aligned: the low
    ///   [`BITSTRING_POINTER_FLAG_MASK`](crate::term::BITSTRING_POINTER_FLAG_MASK) bits
    ///   of the `ErlSubBits::base_flags` pointer are flag bits, so a misaligned payload
    ///   would corrupt the base pointer. [`beam_alloc::RefcBinary`] guarantees this.
    /// * **Immutability.** The payload is never mutated after this call.
    ///
    /// Failure (e.g. out of memory) is return-based: a `BeamError` aborts the decode
    /// with nothing materialized, as every other step of the codec does.
    fn alloc_refc_payload(&mut self, bytes: &[u8]) -> Result<*mut Eterm>;
}
