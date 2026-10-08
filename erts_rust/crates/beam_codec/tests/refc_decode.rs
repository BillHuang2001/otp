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

//! Integration tests for the **off-heap binary decode seam**.
//!
//! [`beam_codec::OffHeapBinaries`] lets a host own the payload of a bitstring larger
//! than `ERL_ONHEAP_BITS_LIMIT` (512 bits = 64 bytes), so a decoded large binary
//! becomes the off-heap `[BinRef][ErlSubBits]` pair ERTS builds
//! (`erts/emulator/beam/external.c:4893-4980`: `size_in_bits <= ERL_ONHEAP_BITS_LIMIT`
//! ⇒ on-heap `ErlHeapBits`, otherwise `erts_bin_nrml_alloc` + `erts_wrap_refc_bitstring`).
//!
//! The seam is **additive**: with no host installed the decoder materializes every
//! bitstring on-heap exactly as before, and the two representations are the *same
//! observable term* — `bitstring_bytes`/`bitstring_size_bits` read both, so the
//! contents, the bit size and the ETF image agree. These tests pin that: the
//! on-heap/off-heap threshold (in **bits**, including the `BIT_BINARY_EXT`
//! trailing-bit arithmetic), wire-image invariance in both representations, and the
//! host's ownership/refcount/release contract.

use beam_alloc::{AllocRef, AllocatorConfig, CarrierAllocator, Heap, RefcBinary};
use beam_codec::term::{bitstring_bytes, bitstring_size_bits, make_refc_binary};
use beam_codec::{Decoder, Encoder, OffHeapBinaries, BINARY_EXT, BIT_BINARY_EXT, VERSION_MAGIC};
use beam_common::{BeamError, Result};
use beam_term::prelude::{boxed_val, is_bitstring, AtomTable, Eterm};
use beam_term::refc::{
    bin_ref_parts, is_refc_binary, refc_offset, refc_size, ERL_ONHEAP_BINARY_LIMIT,
    ERL_ONHEAP_BITS_LIMIT,
};

// ---------------------------------------------------------------------------
// Hosts
// ---------------------------------------------------------------------------

/// A host that allocates each payload through an injected allocator and **retains**
/// the `RefcBinary` handle for the lifetime of the host — the ownership record the
/// seam's contract requires (see [`OffHeapBinaries::alloc_refc_payload`]).
struct RetainingHost {
    alloc: AllocRef,
    store: Vec<RefcBinary>,
}

impl RetainingHost {
    fn new(alloc: AllocRef) -> RetainingHost {
        RetainingHost { alloc, store: Vec::new() }
    }

    /// The payload handles handed out so far, in allocation order.
    fn payloads(&self) -> &[RefcBinary] {
        &self.store
    }
}

impl OffHeapBinaries for RetainingHost {
    fn alloc_refc_payload(&mut self, bytes: &[u8]) -> Result<*mut Eterm> {
        let payload = RefcBinary::with_allocator(self.alloc.clone(), bytes)?;
        let val = payload.data_ptr() as *mut Eterm;
        self.store.push(payload);
        Ok(val)
    }
}

/// A host that returns a fixed pointer, to exercise the writer's guard layer.
struct FixedHost {
    ptr: *mut Eterm,
    calls: usize,
}

impl OffHeapBinaries for FixedHost {
    fn alloc_refc_payload(&mut self, _bytes: &[u8]) -> Result<*mut Eterm> {
        self.calls += 1;
        Ok(self.ptr)
    }
}

/// A fresh, isolated carrier allocator, so the ownership test can observe the
/// payload's allocation and release without interference from the shared registry
/// allocator the `Heap` uses.
fn counting_allocator(name: &str) -> AllocRef {
    let config = AllocatorConfig::builder()
        .name(name)
        .carrier_size(64 * 1024)
        .build();
    CarrierAllocator::new(config)
        .expect("allocator construction maps nothing")
        .share()
}

/// A heap large enough for the 8-word refc pair (or the on-heap block).
fn new_heap() -> Heap {
    Heap::new(64).expect("the carrier allocator never fails for a tiny heap")
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// `[VERSION_MAGIC][BINARY_EXT][len:4 BE][payload]`.
fn binary_image(payload: &[u8]) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, BINARY_EXT];
    image.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    image.extend_from_slice(payload);
    image
}

/// The canonical payload bytes for `size_bits`: the unused low bits of the last byte
/// are zeroed — the form the decoder's `make_bitstring` writes, so a round trip is
/// byte-stable.
fn canonical_bits_payload(payload: &[u8], size_bits: usize) -> Vec<u8> {
    let tail = size_bits % 8;
    // A byte-aligned bitstring has all 8 bits of its last byte significant, which the
    // wire encodes as the compatibility value `8`.
    let bits = if tail == 0 { 8 } else { tail };
    let mut bytes = payload.to_vec();
    if let Some(last) = bytes.last_mut() {
        *last &= 0xFFu8 << (8 - bits);
    }
    bytes
}

/// `[VERSION_MAGIC][BIT_BINARY_EXT][len:4 BE][bits:1][payload]` for `size_bits` bits.
///
/// `payload` must be `ceil(size_bits / 8)` bytes long; `bits` is the number of
/// significant bits of the last byte (`1..=8`, `8` being the byte-aligned
/// compatibility form) and is derived from `size_bits` exactly as the oracle's
/// `term_to_binary/1` does.
fn bit_binary_image(payload: &[u8], size_bits: usize) -> Vec<u8> {
    assert_eq!(payload.len(), size_bits.div_ceil(8), "the payload byte count");
    let tail = size_bits % 8;
    let bits = if tail == 0 { 8 } else { tail };
    let bytes = canonical_bits_payload(payload, size_bits);
    let mut image = vec![VERSION_MAGIC, BIT_BINARY_EXT];
    image.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    image.push(bits as u8);
    image.extend_from_slice(&bytes);
    image
}

// ---------------------------------------------------------------------------
// Decode helpers
// ---------------------------------------------------------------------------

fn decode_with(
    image: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
    host: &mut RetainingHost,
) -> Eterm {
    Decoder::new(image)
        .expect("the image starts with the version byte")
        .with_off_heap(host)
        .decode(heap, atoms)
        .expect("the image is well-formed")
}

fn decode_plain(image: &[u8], heap: &mut Heap, atoms: &mut AtomTable) -> Eterm {
    let mut decoder = Decoder::new(image).expect("the image starts with the version byte");
    decoder.decode(heap, atoms).expect("the image is well-formed")
}

/// Re-encodes `term` and returns the full `term_to_binary` image.
fn reencode(term: Eterm, atoms: &AtomTable) -> Vec<u8> {
    let mut encoder = Encoder::new(atoms);
    encoder.encode(term).expect("the term is encodable");
    encoder.finish()
}

/// Decodes `image` with a host (into locals that stay alive for the whole call) and
/// reports `(is_refc_binary, size_bits, payload bytes, re-encoded image)`.
fn off_heap_facts(image: &[u8], alloc: &AllocRef) -> (bool, usize, Vec<u8>, Vec<u8>) {
    let mut heap = new_heap();
    let mut atoms = AtomTable::new();
    let mut host = RetainingHost::new(alloc.clone());
    let term = decode_with(image, &mut heap, &mut atoms, &mut host);
    // SAFETY: `term` is the live term the decoder just materialized, and both its heap
    // and its off-heap payload (retained by `host`) outlive this call.
    let refc = unsafe { is_refc_binary(term) };
    let (bytes, _offset, size) = bitstring_bytes(term).expect("a decoded bitstring");
    let reencoded = reencode(term, &atoms);
    (refc, size, bytes.to_vec(), reencoded)
}

/// The `BinRef` term of a refc binary (`ErlSubBits::orig`).
///
/// # Safety
///
/// `root` must be a live refc binary term.
unsafe fn bin_ref_of(root: Eterm) -> Eterm {
    // SAFETY: a refc binary is an `ErlSubBits` (`[thing_word, base_flags, start, end,
    // orig]`), so word 4 is a live term.
    unsafe { *boxed_val(root).add(4) }
}

/// `(val, next)` of a refc binary's `BinRef`.
///
/// # Safety
///
/// `root` must be a live refc binary term.
unsafe fn bin_ref_val_next(root: Eterm) -> (*mut Eterm, *mut Eterm) {
    // SAFETY: `bin_ref_of` yields the live `BinRef` term.
    unsafe { bin_ref_parts(bin_ref_of(root)) }
}

// ---------------------------------------------------------------------------
// (a) with a host: off-heap; without a host: on-heap; same observable term
// ---------------------------------------------------------------------------

#[test]
fn large_binary_is_off_heap_with_a_host_and_on_heap_without_one() {
    let payload: Vec<u8> = (0..100u8).map(|i| i.wrapping_mul(7).wrapping_add(3)).collect();
    let image = binary_image(&payload);
    assert!(payload.len() > ERL_ONHEAP_BINARY_LIMIT);

    let alloc = counting_allocator("refc-decode-a");

    let mut heap = new_heap();
    let mut atoms = AtomTable::new();
    let mut host = RetainingHost::new(alloc);
    let off_heap = decode_with(&image, &mut heap, &mut atoms, &mut host);

    // SAFETY: `off_heap` is the live term the decoder just materialized.
    assert!(
        unsafe { is_refc_binary(off_heap) },
        "a >64-byte binary must be off-heap when a host is installed"
    );
    assert!(unsafe { is_bitstring(off_heap) });

    // The `BinRef.val` is exactly the host's retained payload, and `next` — the MSO
    // chain link this workspace leaves null — is untouched.
    // SAFETY: `off_heap` is a live refc binary (just checked).
    let (val, next) = unsafe { bin_ref_val_next(off_heap) };
    assert_eq!(host.payloads().len(), 1, "exactly one payload was allocated");
    assert_eq!(
        val,
        host.payloads()[0].data_ptr() as *mut Eterm,
        "the on-heap `BinRef.val` is the host's payload"
    );
    assert!(next.is_null(), "the off-heap chain link is left null");
    // SAFETY: `off_heap` is a live refc binary with a full-binary window.
    unsafe {
        assert_eq!(refc_offset(off_heap), 0);
        assert_eq!(refc_size(off_heap), 800);
    }

    // The same image without a host: on-heap, but the very same observable term.
    let mut plain_heap = new_heap();
    let mut plain_atoms = AtomTable::new();
    let on_heap = decode_plain(&image, &mut plain_heap, &mut plain_atoms);

    // SAFETY: `on_heap` is a live term the decoder just materialized.
    assert!(
        !unsafe { is_refc_binary(on_heap) },
        "with no host every bitstring stays on-heap"
    );
    // SAFETY: `on_heap` is a live bitstring (the default path).
    assert!(unsafe { is_bitstring(on_heap) });

    let off = bitstring_bytes(off_heap).unwrap();
    let on = bitstring_bytes(on_heap).unwrap();
    assert_eq!(off, (payload.as_slice(), 0, 800));
    assert_eq!(on, off, "both representations read back identically");
    assert_eq!(bitstring_size_bits(off_heap).unwrap(), 800);
    assert_eq!(bitstring_size_bits(on_heap).unwrap(), 800);
    assert_eq!(reencode(off_heap, &atoms), image);
    assert_eq!(reencode(on_heap, &plain_atoms), image);
}

// ---------------------------------------------------------------------------
// (b) the bit-size threshold, incl. the `BIT_BINARY_EXT` trailing-bit arithmetic
// ---------------------------------------------------------------------------

#[test]
fn only_sizes_strictly_above_the_bit_limit_go_off_heap() {
    let alloc = counting_allocator("refc-decode-b");
    assert_eq!(ERL_ONHEAP_BITS_LIMIT, ERL_ONHEAP_BINARY_LIMIT * 8);

    // `BINARY_EXT`: the threshold is 64 bytes; 64 is still on-heap, 65 goes off-heap.
    for (len, expected_refc) in [(63usize, false), (64, false), (65, true)] {
        let payload: Vec<u8> = (0..len).map(|i| 0x40u8.wrapping_add(i as u8)).collect();
        let (refc, size, bytes, reencoded) = off_heap_facts(&binary_image(&payload), &alloc);
        assert_eq!(refc, expected_refc, "{len}-byte BINARY_EXT");
        assert_eq!(size, len * 8, "{len}-byte BINARY_EXT size");
        assert_eq!(bytes, payload, "{len}-byte BINARY_EXT contents");
        assert_eq!(reencoded, binary_image(&payload), "{len}-byte BINARY_EXT image");
    }

    // `BIT_BINARY_EXT`: the threshold is on the **bit** size, narrowed by the trailing
    // bits: `size_bits = len * 8 - (8 - trailing_bits)`.
    // (byte_len, trailing_bits, expected_refc, expected_size_bits)
    let cases: [(usize, u8, bool, usize); 5] = [
        (64, 8, false, 512), // the byte-aligned compatibility form: exactly the limit
        (64, 7, false, 511), // one bit under
        (65, 3, true, 515),  // 520 - 5: over the limit
        (65, 7, true, 519),
        (66, 1, true, 521),
    ];
    for (byte_len, trailing, expected_refc, expected_size) in cases {
        let size_bits = byte_len * 8 - (8 - usize::from(trailing));
        assert_eq!(size_bits, expected_size, "the trailing-bit arithmetic");
        let payload: Vec<u8> = (0..byte_len).map(|i| 0x80u8.wrapping_add(i as u8)).collect();
        let image = bit_binary_image(&payload, size_bits);
        let expected = canonical_bits_payload(&payload, size_bits);

        let (refc, decoded_size, bytes, reencoded) = off_heap_facts(&image, &alloc);
        assert_eq!(
            refc, expected_refc,
            "{byte_len}-byte BIT_BINARY_EXT, {trailing} trailing bits"
        );
        assert_eq!(decoded_size, expected_size, "the decoded bit size");
        assert_eq!(bytes, expected, "the payload bytes");
        // The wire image is preserved — for the byte-aligned form the canonical
        // re-encoding is the (shorter) `BINARY_EXT` image, which is what the oracle
        // itself emits for that same bitstring.
        let expected_image = if size_bits % 8 == 0 {
            binary_image(&expected)
        } else {
            image
        };
        assert_eq!(reencoded, expected_image, "{byte_len}-byte BIT_BINARY_EXT image");
    }
}

// ---------------------------------------------------------------------------
// (c) wire-image invariance in both representations
// ---------------------------------------------------------------------------

#[test]
fn both_representations_reencode_to_the_original_image() {
    let alloc = counting_allocator("refc-decode-c");

    // A byte-aligned binary and a non-byte-aligned bitstring, both off-heap.
    let binary_payload: Vec<u8> = (0..130u8).collect();
    let binary_img = binary_image(&binary_payload);
    let bits_payload: Vec<u8> = (0..90u8).map(|i| 0xA5u8.wrapping_add(i)).collect();
    let bits_img = bit_binary_image(&bits_payload, 715); // 89*8 + 3 = 715 bits

    for image in [&binary_img, &bits_img] {
        let mut heap = new_heap();
        let mut atoms = AtomTable::new();
        let mut host = RetainingHost::new(alloc.clone());
        let off_heap = decode_with(image, &mut heap, &mut atoms, &mut host);
        // SAFETY: `off_heap` is the live term the decoder just materialized.
        assert!(unsafe { is_refc_binary(off_heap) });
        assert_eq!(
            reencode(off_heap, &atoms),
            *image,
            "the off-heap representation re-encodes byte-identically"
        );

        let mut plain_heap = new_heap();
        let mut plain_atoms = AtomTable::new();
        let on_heap = decode_plain(image, &mut plain_heap, &mut plain_atoms);
        assert_eq!(
            reencode(on_heap, &plain_atoms),
            *image,
            "the on-heap representation re-encodes byte-identically"
        );
        assert_eq!(reencode(off_heap, &atoms), reencode(on_heap, &plain_atoms));
    }
}

// ---------------------------------------------------------------------------
// (d) ownership: the host retains the payload, its refcount is the retained handle,
//     and releasing the host frees the allocation
// ---------------------------------------------------------------------------

#[test]
fn host_retains_the_payload_and_releases_it_on_drop() {
    let alloc = counting_allocator("refc-decode-d");
    let baseline = alloc.stats();

    let payload: Vec<u8> = (0..70u8).map(|i| i.wrapping_mul(3)).collect();
    let image = binary_image(&payload);

    let mut heap = new_heap();
    let mut atoms = AtomTable::new();
    let mut host = RetainingHost::new(alloc.clone());
    let term = decode_with(&image, &mut heap, &mut atoms, &mut host);

    // The host holds exactly one handle whose bytes are the payload, and the injected
    // allocator observed the allocation.
    assert_eq!(host.payloads().len(), 1);
    {
        let retained = &host.payloads()[0];
        assert_eq!(retained.as_bytes(), payload.as_slice());
        assert_eq!(retained.refcount(), 1, "the store holds the only handle");
    }
    let live = alloc.stats();
    assert_eq!(live.live_blocks, baseline.live_blocks + 1);
    assert!(
        live.live_bytes >= baseline.live_bytes + payload.len(),
        "the payload's bytes are attributed to the allocator"
    );

    // The refcount tracks the retained handles: a clone bumps it, dropping the clone
    // restores it.
    {
        let retained = &host.payloads()[0];
        let extra = retained.clone();
        assert_eq!(retained.refcount(), 2, "a clone shares the payload");
        assert_eq!(extra.as_bytes(), payload.as_slice(), "a clone reads the same bytes");
        drop(extra);
        assert_eq!(retained.refcount(), 1, "dropping the clone restores the count");
    }

    // The decoded term is still readable through that handle.
    assert_eq!(bitstring_bytes(term).unwrap(), (payload.as_slice(), 0, 560));

    // Releasing the host drops the last handle: the allocation returns to baseline.
    drop(host);
    let end = alloc.stats();
    assert_eq!(end.live_blocks, baseline.live_blocks, "no leaked block");
    assert_eq!(end.live_bytes, baseline.live_bytes, "no leaked bytes");
}

// ---------------------------------------------------------------------------
// The writer's guard layer
// ---------------------------------------------------------------------------

/// Asserts a `make_refc_binary` call failed with `BeamError::BadArg`.
fn assert_bad_arg(result: Result<Eterm>) {
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("expected BadArg, got {other:?}"),
    }
}

#[test]
fn make_refc_binary_rejects_a_broken_host_or_window() {
    let payload = [0x5Au8; 65];
    let mut heap = new_heap();

    // A null payload pointer is not a `BinRef.val` — rejected before the on-heap pair
    // is written, leaving the heap untouched.
    let mut null_host = FixedHost { ptr: core::ptr::null_mut(), calls: 0 };
    assert_bad_arg(make_refc_binary(&mut heap, &mut null_host, &payload, 0, 520));
    assert_eq!(null_host.calls, 1, "the host was consulted");
    assert_eq!(heap.used(), 0, "a rejected payload allocated nothing");

    // A misaligned payload pointer would set flag bits in `ErlSubBits::base_flags`.
    let mut misaligned_host = FixedHost { ptr: 0x1002_usize as *mut Eterm, calls: 0 };
    assert_bad_arg(make_refc_binary(&mut heap, &mut misaligned_host, &payload, 0, 520));
    assert_eq!(misaligned_host.calls, 1);
    assert_eq!(heap.used(), 0);

    // A `bytes` slice that cannot cover the requested window is rejected *before* the
    // host is asked for a payload.
    let mut unused_host = FixedHost { ptr: 0x1000_usize as *mut Eterm, calls: 0 };
    assert_bad_arg(make_refc_binary(&mut heap, &mut unused_host, &payload[..1], 0, 520));
    assert_eq!(unused_host.calls, 0, "the guard runs before the allocation");
    assert_eq!(heap.used(), 0);

    // An overflowing window cannot be a real bitstring.
    assert_bad_arg(make_refc_binary(&mut heap, &mut unused_host, &payload, usize::MAX, 1));
    assert_eq!(unused_host.calls, 0);
    assert_eq!(heap.used(), 0);

    // Control: an aligned, in-range request writes the 8-word pair, with the payload's
    // first byte as the `ErlSubBits` base.
    #[repr(align(8))]
    struct Aligned([u8; 65]);
    let storage = Aligned(payload);
    let mut host = FixedHost { ptr: storage.0.as_ptr() as *mut Eterm, calls: 0 };
    let term = make_refc_binary(&mut heap, &mut host, &storage.0, 0, 520).expect("a valid request");
    assert_eq!(host.calls, 1);
    // SAFETY: `term` is the term the call just materialized from a live payload.
    assert!(unsafe { is_refc_binary(term) });
    assert_eq!(heap.used(), 8, "the `[BinRef][ErlSubBits]` pair is 8 words");
    assert_eq!(bitstring_bytes(term).unwrap(), (&storage.0[..], 0, 520));
}
