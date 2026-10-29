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

//! The **copy cost** of the binary/iolist reader BIFs, pinned — and the byte
//! semantics that must survive the cost change.
//!
//! The four paths this file measures used to read the whole subject term out of the
//! heap before looking at one byte of it:
//!
//! | call | before | after |
//! |---|---|---|
//! | `iolist_to_binary/1` (list form) | its own `BitBuf` walk: work-stack `Vec` + a capacity-less byte `Vec` grown 8→16→32… | the crate's one walk (`src/iolist.rs`): **one** exactly-sized buffer, no per-element temporary |
//! | `binary:at/2` | `bits::bytes(..).to_vec()` over the **whole** subject | the borrowing `bits::with_bytes`, so only the byte asked for is read (**0** allocations) |
//! | `binary:part/2,3`, `bin_to_list/2,3` | the whole subject copied into a `Vec`, then the slice | the borrowing accessor plus C's sub-binary rule: an 8-word `[BinRef][ErlSubBits]` **window** above 512 bits over an off-heap parent (**0** allocations, 8 arena words), the selected bits written in place at or below it |
//! | `iolist_size/1` | flattened the whole iolist only to read `.len()` | measured by walking it (**0** allocations) |
//!
//! The allocation counts are measured with a per-thread armed counting
//! [`GlobalAlloc`] (the same instrument as `tests/binconv_alloc.rs`), so the harness's
//! own formatting can never perturb them.
//!
//! The byte expectations are the OTP 29 / erts-17.1 oracle's:
//!
//! ```text
//! erl -noshell -eval 'io:format("~p~n",[iolist_to_binary([<<"ab">>,[99],$d])]), halt().'
//!   % <<"abcd">>
//! erl -noshell -eval 'io:format("~p~n",[iolist_size([1,[2,<<3>>|<<4>>]])]), halt().'   % 4
//! erl -noshell -eval 'io:format("~p~n",[binary:at(<<1,2,3,4>>,2)]), halt().'          % 3
//! erl -noshell -eval 'io:format("~p~n",[binary:referenced_byte_size(
//!     binary:part(binary:copy(<<1>>,100),10,5))]), halt().'                           % 5
//! ```
//!
//! `binary:referenced_byte_size/1` of a *part* turns on the **window's** bit size, the
//! arm C's `erts_build_sub_bitstring` picks, so the large-window figures must come from
//! a **compiled** module (from `-eval` the C answers the payload's *allocated* width,
//! `2050` for the subject below):
//!
//! ```text
//! erlc -o /tmp/fx refcwin.erl && erl -noshell -pa /tmp/fx -eval 'refcwin:run().'
//!   % P = binary:copy(<<255>>,1024), B = <<"z", P/binary>>          (1025 bytes)
//!   % referenced_byte_size(B)              = 1025
//!   % referenced_byte_size(part(B,0,63))   = 63        (<= 512 bits: the copy arm)
//!   % referenced_byte_size(part(B,0,64))   = 64         (512 bits exactly: a copy)
//!   % referenced_byte_size(part(B,0,65))   = 1025       (520 bits: the first WINDOW)
//!   % referenced_byte_size(part(B,1,510))  = 1025
//!   % referenced_byte_size(part(B,0,1024)) = 1025
//!   % referenced_byte_size(part(B,0,0))    = 0
//!   % byte_size(part(part(B,1,510),254,100)) = 100, its referenced size = 1025
//!   % md5(part(B,1,510)) = 332849ab09670313c986615b4202f4f5
//! ```
//!
//! The producer meets that rule: `src/binary.rs::part_slice` mints the real
//! `[BinRef][ErlSubBits]` window over an off-heap parent above 512 bits and writes the
//! selected bits in place at or below it. The rows below pin both the bytes and the
//! figure through the public BIFs, and the compiled-module cross-check at the bottom
//! compares them with the oracle row for row.
//!
//! Two shapes still are not the oracle's, and each carries its own pin instead of a
//! line of prose:
//!
//! * **This crate's own producers are on-heap.** `binary:copy/1,2`, `list_to_bin/1` and
//!   `iolist_to_binary/1` hand back an `ErlHeapBits` however large the binary is
//!   (`src/bits.rs`'s placement policy), so a part of one above 64 bytes keeps the copy
//!   arm and answers its own size where the oracle windows the payload and answers
//!   `100`/`1024`/`1025`. See `an_on_heap_parent_above_the_limit_still_copies_the_range`
//!   (and the oracle's own `q_part_*` rows, which are exactly that divergence).
//! * **The host's result copy-out flattens a window.** `beam_rt`'s
//!   `context/calls.rs::copy_result_fresh` hands `beam_message::copy_struct` an **empty**
//!   source off-heap table, so `copy_struct` re-materializes the window as a fresh
//!   on-heap bitstring of the window's own length; on the full runtime a *program*
//!   asking `binary:referenced_byte_size/1` of a part therefore still sees the part's
//!   size. That is a `beam_rt`/`beam_message` seam (documented on `part_slice`), not a
//!   producer defect, and nothing here asserts a program-level figure through
//!   `beam_rt` — every pin below drives the registry directly, where no copy-out runs.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use beam_alloc::{Heap, RefcBinary};
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term as codec_term;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{is_list, is_nil, is_small, make_small, signed_value, AtomTable, Eterm};
// ---------------------------------------------------------------------------
// The counting allocator (as in `tests/binconv_alloc.rs`)
// ---------------------------------------------------------------------------
/// Number of `alloc`/`realloc` calls made on an armed thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Number of bytes requested on an armed thread.
static BYTES: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    /// Set only on the thread that is currently measuring: the counters are
    /// process-global, but the arming is per-thread, so the test harness (which
    /// allocates while formatting output) can never perturb a measurement.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}
/// Records one allocation of `bytes` on the current thread while it is armed.
#[inline]
fn record(bytes: usize) {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(bytes, Ordering::Relaxed);
        }
    });
}
/// `System` plus per-thread opt-in counting.
struct CountingAllocator;
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Count only the growth (a shrink is not an allocation event).
        record(new_size.saturating_sub(layout.size()));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
/// Serializes this file's tests: the counters are process-global, so a window open on
/// one thread would otherwise absorb another test's allocations.
static SERIAL: Mutex<()> = Mutex::new(());
/// Runs `body` with the counters armed on this thread and returns
/// `(allocations, bytes, result)`.
fn allocations_in<T>(body: impl FnOnce() -> T) -> (usize, usize, T) {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // Force the TLS to initialize *before* the window opens.
    ARMED.with(|armed| armed.set(false));
    let before_allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let before_bytes = BYTES.load(Ordering::Relaxed);
    ARMED.with(|armed| armed.set(true));
    let result = body();
    ARMED.with(|armed| armed.set(false));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before_allocations;
    let bytes = BYTES.load(Ordering::Relaxed) - before_bytes;
    (allocations, bytes, result)
}
// ---------------------------------------------------------------------------
// The fixture: a registry, a heap, an atom table and the off-heap payloads
// ---------------------------------------------------------------------------
/// A registry + heap + atom table the tests drive BIF calls through, plus the off-heap
/// payloads the refc-binary terms point into (kept alive here so the raw base pointer
/// stays valid for as long as the term is used).
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
    payloads: Vec<RefcBinary>,
}
impl Fixture {
    /// No optional seam is installed: none of these BIFs needs one.
    fn new() -> Fixture {
        Fixture {
            registry: default_registry(),
            heap: Heap::new(1 << 18).expect("test heap"),
            atoms: AtomTable::new(),
            payloads: Vec::new(),
        }
    }
    /// Calls `module:function(Args)` through the real registry.
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry.call(module, function, args.len() as u32, &mut ctx, args)
    }
    /// Calls `module:function(Args)` and unwraps the result.
    fn ok(&mut self, module: &str, function: &str, args: &[Eterm]) -> Eterm {
        self.call(module, function, args)
            .unwrap_or_else(|e| panic!("{module}:{function}/{} failed: {e:?}", args.len()))
    }
    /// Calls `module:function(Args)` and expects the oracle's `badarg`.
    fn badarg(&mut self, module: &str, function: &str, args: &[Eterm]) {
        assert!(
            matches!(self.call(module, function, args), Err(BeamError::BadArg)),
            "{module}:{function}/{} must be badarg",
            args.len()
        );
    }
    /// The small-int terms for `values`.
    fn ints(&mut self, values: &[i64]) -> Vec<Eterm> {
        let ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        values.iter().map(|&v| ctx.int(v).expect("a small int fits")).collect()
    }
    /// The proper list `[Items…]`.
    fn list(&mut self, items: &[Eterm]) -> Eterm {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        ctx.list(items).expect("the list fits")
    }
    /// The on-heap binary `<<Bytes>>`.
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        codec_term::make_binary(&mut self.heap, bytes).expect("the binary fits")
    }
    /// The **off-heap** refc binary over `bytes` whose bit window is
    /// `offset_bits..offset_bits + size_bits` — the `[BinRef][ErlSubBits]` pair the
    /// engine's `bs_create_bin` builds for a payload past `ERL_ONHEAP_BINARY_LIMIT`.
    fn refc(&mut self, bytes: &[u8], offset_bits: usize, size_bits: usize) -> Eterm {
        let payload = RefcBinary::new(bytes).expect("the off-heap payload allocates");
        let base = payload.data_ptr();
        // Moving the handle into `payloads` does not move the payload it points at
        // (that is off-heap), so `base` stays valid for the term's whole lifetime.
        self.payloads.push(payload);
        // SAFETY: `alloc_words` hands out `ERL_REFC_BITS_SIZE` (8) exclusively-owned,
        // writable words — exactly the pair `write_refc_binary` fully writes; `base`
        // is the first byte of the (now fixture-owned) payload's live, 4-byte-aligned
        // data area and covers bits `offset_bits..offset_bits + size_bits`.
        let hp = unsafe { self.heap.alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE) }
            .expect("the 8-word pair fits");
        unsafe {
            beam_term::refc::write_refc_binary(
                hp,
                base.cast::<Eterm>(),
                core::ptr::null_mut(),
                base,
                offset_bits,
                size_bits,
            )
        }
    }
}
/// The bytes of a byte-aligned binary result.
fn bytes_of(term: Eterm) -> Vec<u8> {
    let (bytes, offset, size_bits) = codec_term::bitstring_bytes(term).expect("a bitstring result");
    assert_eq!(offset % 8, 0, "the result must start on a byte boundary");
    assert_eq!(size_bits % 8, 0, "the result must be a whole number of bytes");
    bytes[offset / 8..offset / 8 + size_bits / 8].to_vec()
}
/// `binary:referenced_byte_size/1` through the real registry (the observable BIF, never a
/// private accessor — the arm a window takes must be visible to a program).
fn referenced(fx: &mut Fixture, term: Eterm) -> i64 {
    signed_value(fx.ok("binary", "referenced_byte_size", &[term]))
}
/// The integers of a proper-list result.
fn ints_of(term: Eterm) -> Vec<i64> {
    let mut values = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        let head = codec_term::cons_head(cur).expect("a cons head");
        assert!(is_small(head), "a byte-list element must be a small int");
        values.push(signed_value(head));
        cur = codec_term::cons_tail(cur).expect("a cons tail");
    }
    assert!(is_nil(cur), "the result must be a proper list");
    values
}
// ---------------------------------------------------------------------------
// The refc-parent helpers: the oracle's subject, a distinct-bytes subject, and
// the bit-level reader a mid-byte window needs
// ---------------------------------------------------------------------------
/// The oracle's own subject — `<<"z", (binary:copy(<<255>>, 1024))/binary>>`, **1025**
/// bytes — so the figures and the `md5` row below are the oracle transcript's verbatim.
fn oracle_payload() -> Vec<u8> {
    let mut data = vec![b'z'];
    data.extend(core::iter::repeat_n(255u8, 1024));
    data
}
/// A 1025-byte payload whose every byte differs, so a window at the wrong offset cannot
/// hide behind a uniform byte value.
fn pattern_payload() -> Vec<u8> {
    (0..1025u32).map(|i| (i % 251) as u8).collect()
}
/// The `byte_len` bytes starting at `offset_bits` of the **big-endian bit stream** of
/// `subject` — what a mid-byte window of `subject` reads as bytes, and the only honest
/// expected value for one (the window's start is not a byte boundary).
fn extract_bits(subject: &[u8], offset_bits: usize, byte_len: usize) -> Vec<u8> {
    let end_bit = offset_bits + byte_len * 8;
    assert!(
        end_bit <= subject.len() * 8,
        "the requested window must lie inside the subject"
    );
    (0..byte_len)
        .map(|i| {
            let mut byte = 0u8;
            for k in 0..8 {
                let bit = offset_bits + i * 8 + k;
                byte = (byte << 1) | ((subject[bit / 8] >> (7 - bit % 8)) & 1);
            }
            byte
        })
        .collect()
}
/// The bytes of a bitstring result that need not start on a byte boundary (`bytes_of`
/// asserts the aligned case; a window of a mid-byte parent does not have one).
fn bits_of(term: Eterm) -> Vec<u8> {
    let (bytes, offset, size_bits) = codec_term::bitstring_bytes(term).expect("a bitstring result");
    assert_eq!(size_bits % 8, 0, "the result must be a whole number of bytes");
    extract_bits(bytes, offset, size_bits / 8)
}
/// `bytes` as lowercase hex — the shape `binary:encode_hex/1` prints on the oracle.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
// ---------------------------------------------------------------------------
// `erlang:iolist_to_binary/1` — one buffer, one allocation
// ---------------------------------------------------------------------------
/// A **shallow mixed** iolist — a binary element, a nested byte list and a bare byte,
/// i.e. all three leaf shapes with no nesting deeper than one level — flattens to
/// exactly the oracle's bytes in exactly **one** allocation (the destination buffer).
#[test]
fn iolist_to_binary_over_a_shallow_mixed_iolist_is_byte_exact_in_one_allocation() {
    let mut fx = Fixture::new();
    let ab = fx.binary(b"ab");
    let cd = {
        let bytes = fx.ints(&[99, 100]);
        fx.list(&bytes)
    };
    let d = make_small(101);
    let iolist = fx.list(&[ab, cd, d]);
    let expected: Vec<u8> = b"abcde".to_vec();
    // The bytes first (outside the measurement window, so the assertion's own
    // allocations cannot leak into it).
    let built = fx.ok("erlang", "iolist_to_binary", &[iolist]);
    assert_eq!(bytes_of(built), expected, "iolist_to_binary/1 must build the exact bytes");
    // `iolist_size/1` agrees with the flattened length (and allocates nothing: it only
    // walks and counts — the measurement below is the pin for that).
    let size = fx.ok("erlang", "iolist_size", &[iolist]);
    assert_eq!(signed_value(size), expected.len() as i64);
    let (allocations, bytes, _) = allocations_in(|| fx.ok("erlang", "iolist_to_binary", &[iolist]));
    assert_eq!(
        (allocations, bytes),
        (1, expected.len()),
        "iolist_to_binary/1 must cost exactly one buffer of the iolist's byte count \
         (no work-stack Vec, no growth ladder, no per-element temporary)"
    );
    // A bare byte-aligned binary is the C's `is_binary` fast path: the argument itself,
    // so not even the destination buffer is allocated.
    let whole = fx.binary(&expected);
    let (allocations, bytes, _) = allocations_in(|| fx.ok("erlang", "iolist_to_binary", &[whole]));
    assert_eq!((allocations, bytes), (0, 0), "a bare binary must be returned as itself");
}
// ---------------------------------------------------------------------------
// `binary:at/2` — one byte read, no copy of the subject
// ---------------------------------------------------------------------------
/// `binary:at/2` over a **large off-heap refc binary** reads the one byte asked for and
/// allocates nothing: the whole payload is no longer copied per read.
#[test]
fn binary_at_reads_one_byte_of_a_large_refc_binary_without_allocating() {
    let mut fx = Fixture::new();
    // 4 KiB off the 64-byte on-heap limit: the `[BinRef][ErlSubBits]` representation,
    // i.e. the shape a real `bs_create_bin` produces.
    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let subject = fx.refc(&payload, 0, payload.len() * 8);
    for index in [0usize, 1, 4095] {
        let expected = i64::from(payload[index]);
        let index_term = make_small(index as i64);
        let byte = fx.ok("binary", "at", &[subject, index_term]);
        assert_eq!(signed_value(byte), expected, "binary:at/2 at {index}");
        let (allocations, bytes, _) =
            allocations_in(|| fx.ok("binary", "at", &[subject, index_term]));
        assert_eq!(
            (allocations, bytes),
            (0, 0),
            "binary:at/2 at {index} must not copy the subject"
        );
    }
    // Out of range is `badarg`, exactly as before (`binary:at(<<1,2,3>>, 3)`).
    fx.badarg("binary", "at", &[subject, make_small(4096)]);
    fx.badarg("binary", "at", &[subject, make_small(-1)]);
    // A **mid-byte** window (`offset_bits` not a multiple of 8) has no contiguous byte
    // image, so the accessor re-packs it: still the right byte, and the re-pack is the
    // one documented allocation on this path.
    let nibbles = fx.refc(&[0xF0, 0xA0], 4, 8);
    let byte = fx.ok("binary", "at", &[nibbles, make_small(0)]);
    assert_eq!(signed_value(byte), 0x0A, "bits 4..12 of <<F0,A0>> are <<16#0A>>");
}
// ---------------------------------------------------------------------------
// `binary:part/2,3` — the copy is proportional to the *result*, and above 64
// bytes over an off-heap parent there is no copy at all
// ---------------------------------------------------------------------------
/// `binary:part/3`'s **≤ 512-bit arm** writes the selected bits straight onto the
/// destination heap — a stack buffer inside `beam_term::refc::write_sub_binary`, then
/// one `write_heap_bits` — so the call allocates **nothing**, where it used to copy the
/// range into a `Vec` first. Above that arm the producer mints a real
/// `[BinRef][ErlSubBits]` window (the next test), and an **on-heap** parent above it
/// keeps the copying route (the residual test further down).
///
/// The `referenced_byte_size/1` figure of this 5-byte part is the part's own size, which
/// is the oracle's figure for the same shape too — a part of at most 64 bytes is a copy
/// on **both** sides (`binary:referenced_byte_size(binary:part(binary:copy(<<1>>,100),
/// 10,5))` ⇒ `5`, probed live at the bottom of this file).
#[test]
fn binary_part_of_an_on_heap_parent_copies_the_range_in_place() {
    let mut fx = Fixture::new();
    let one = fx.binary(&[1]);
    let parent = fx.ok("binary", "copy", &[one, make_small(100)]);
    let part = fx.ok("binary", "part", &[parent, make_small(10), make_small(5)]);
    assert_eq!(bytes_of(part), vec![1u8; 5], "the part's bytes");
    assert_eq!(
        signed_value(fx.ok("binary", "referenced_byte_size", &[parent])),
        100,
        "the parent reports its own 100 bytes"
    );
    assert_eq!(
        signed_value(fx.ok("binary", "referenced_byte_size", &[part])),
        5,
        "a 5-byte part reports its own size — the copy arm on both sides, so this row \
         agrees with the oracle"
    );
    // The cost: **no allocation at all**. A ≤ 512-bit window is written in place (a
    // 64-byte stack buffer inside `write_sub_binary`, then `write_heap_bits`), not
    // copied into a `Vec` of the range's length — the figure this pin used to assert
    // was `(1, 5)`.
    let (allocations, bytes, _) =
        allocations_in(|| fx.ok("binary", "part", &[parent, make_small(10), make_small(5)]));
    assert_eq!(
        (allocations, bytes),
        (0, 0),
        "a ≤ 512-bit binary:part/3 writes the window's bits in place (the arena block \
         is a `write_heap_bits` write, not a Rust allocation), so it must cost neither \
         a `Vec` allocation nor a byte of one"
    );
    // ... and the arena words are the block's own: `[thing_word, n_bits, data…]`.
    let before = fx.heap.used();
    let part = fx.ok("binary", "part", &[parent, make_small(10), make_small(5)]);
    assert_eq!(
        fx.heap.used() - before,
        1 + 1 + 1,
        "a 5-byte copy is the 3-word `ErlHeapBits` block, not the 8-word window pair"
    );
    assert_eq!(bytes_of(part), vec![1u8; 5]);
    // A negative `Len` is the reverse-part form (`part(<<1,2,3>>, 2, -2)` ⇒ `<<1,2>>`).
    let abc = fx.binary(&[1, 2, 3]);
    let reverse = fx.ok("binary", "part", &[abc, make_small(2), make_small(-2)]);
    assert_eq!(bytes_of(reverse), vec![1u8, 2]);
    // Out of range, a non-integer position and a non-binary subject are `badarg`.
    fx.badarg("binary", "part", &[abc, make_small(3), make_small(1)]);
    fx.badarg("binary", "part", &[abc, make_small(0), make_small(-4)]);
    fx.badarg("binary", "part", &[make_small(1), make_small(0), make_small(0)]);
    // A non-byte-aligned subject is `badarg` (`binary:part(<<1:3>>, 0, 1)`), on the
    // oracle too: the range rule is in *bytes* and `erts_binary_part` requires a binary.
    let three_bits = codec_term::make_bitstring(&mut fx.heap, &[0b1010_0000], 3).expect("fits");
    fx.badarg("binary", "part", &[three_bits, make_small(0), make_small(0)]);
    // `bin_to_list/3` shares the rule and the accessor: only the selected bytes are
    // copied out before the cons cells are built.
    let listed = fx.ok("binary", "bin_to_list", &[abc, make_small(1), make_small(2)]);
    assert_eq!(ints_of(listed), vec![2, 3]);
}
// ---------------------------------------------------------------------------
// `binary:part/2,3` over an **off-heap** parent — the real sub-binary window
// ---------------------------------------------------------------------------
/// The measured divergence, closed at the producer: a part of a **refc** parent above
/// 512 bits is the C's `[BinRef][ErlSubBits]` window, so
/// `binary:referenced_byte_size/1` answers the parent **payload's** width instead of the
/// window's own, and not a byte is copied.
///
/// The oracle's rows for these shapes, from the **compiled** fixture the cross-check at
/// the bottom of this file builds (`B = <<"z", (binary:copy(<<255>>,1024))/binary>>`,
/// 1025 bytes): `part(B,1,510)` ⇒ `1025`, `part(B,0,65)` ⇒ `1025`, `part(B,0,64)` ⇒
/// `64`, `part(B,0,63)` ⇒ `63`, `part(B,0,1024)` ⇒ `1025`, `part(B,0,0)` ⇒ `0`.
#[test]
fn a_part_of_a_refc_parent_is_a_window_over_the_parent_payload() {
    let mut fx = Fixture::new();
    let payload = pattern_payload();
    let parent = fx.refc(&payload, 0, payload.len() * 8);
    assert_eq!(referenced(&mut fx, parent), 1025, "the whole refc parent reports its payload");

    // The measured divergence: 510 bytes at offset 1 answers the **payload's** 1025, not
    // the window's 510.
    let part = fx.ok("binary", "part", &[parent, make_small(1), make_small(510)]);
    assert_eq!(bytes_of(part), payload[1..511].to_vec(), "the window reads the parent's bytes");
    assert_eq!(
        referenced(&mut fx, part),
        1025,
        "a 510-byte window over the 1025-byte payload answers 1025 (the C's \
         (br->val)->orig_size), not 510"
    );

    // Nothing is copied: the result is the 8-word pair on the arena, whatever its length,
    // and the call makes **no** Rust allocation.
    let before = fx.heap.used();
    let window = fx.ok("binary", "part", &[parent, make_small(1), make_small(510)]);
    assert_eq!(
        fx.heap.used() - before,
        8,
        "a window is the 8-word [BinRef][ErlSubBits] pair, not a block of the window's bytes"
    );
    assert_eq!(bytes_of(window), payload[1..511].to_vec());
    let (allocations, bytes, _) =
        allocations_in(|| fx.ok("binary", "part", &[parent, make_small(1), make_small(510)]));
    assert_eq!(
        (allocations, bytes),
        (0, 0),
        "a windowing binary:part/3 copies nothing into a Vec — the pair is written on the \
         arena and the payload stays where it is"
    );

    // `binary:part/2`'s `{Start, Len}` form reaches the same producer.
    let range = fx.heap.tuple(&[make_small(1), make_small(510)]).expect("the tuple fits");
    let via_two = fx.ok("binary", "part", &[parent, range]);
    assert_eq!(bytes_of(via_two), payload[1..511].to_vec());
    assert_eq!(referenced(&mut fx, via_two), 1025);

    // The arm switch, in bytes: at or below 64 bytes (512 bits) the result is the
    // in-place copy and answers its own size; 65 bytes (520 bits) is the first window.
    for (len, want) in [(63i64, 63i64), (64, 64), (65, 1025), (1024, 1025)] {
        let part = fx.ok("binary", "part", &[parent, make_small(0), make_small(len)]);
        assert_eq!(bytes_of(part), payload[..len as usize].to_vec(), "part(0,{len}) bytes");
        assert_eq!(referenced(&mut fx, part), want, "part(0,{len}) reports {want}");
    }

    // An empty window (at `byte_size`, the C's `<<>>` arm) is 0 on the oracle too.
    let empty = fx.ok("binary", "part", &[parent, make_small(1025), make_small(0)]);
    assert!(bytes_of(empty).is_empty(), "the empty part has no bytes");
    assert_eq!(referenced(&mut fx, empty), 0, "an empty part reports 0");

    // A negative `Len` (reverse part) over a refc parent takes the same window route.
    let reverse = fx.ok("binary", "part", &[parent, make_small(512), make_small(-511)]);
    assert_eq!(bytes_of(reverse), payload[1..512].to_vec());
    assert_eq!(referenced(&mut fx, reverse), 1025);
}

/// A window **of a window** accumulates its offset into the ROOT payload: the bytes are
/// the inner slice, the `byte_size` is the child's, and the referenced width is still the
/// root payload's.
///
/// The oracle's rows (compiled): `byte_size(part(part(B,1,510),254,100))` ⇒ `100` and its
/// referenced size ⇒ `1025` — the same pair this test pins.
#[test]
fn a_window_of_a_window_reads_the_root_payload() {
    let mut fx = Fixture::new();
    let payload = pattern_payload();
    let parent = fx.refc(&payload, 0, payload.len() * 8);
    let mid = fx.ok("binary", "part", &[parent, make_small(1), make_small(510)]);
    assert_eq!(bytes_of(mid), payload[1..511].to_vec());

    let inner = fx.ok("binary", "part", &[mid, make_small(254), make_small(100)]);
    assert_eq!(bytes_of(inner), payload[255..355].to_vec(), "the child's start accumulates");
    assert_eq!(
        codec_term::bitstring_bytes(inner).expect("a bitstring").2,
        100 * 8,
        "the child's own byte size is its length"
    );
    assert_eq!(
        referenced(&mut fx, inner),
        1025,
        "a window of a window answers the ROOT payload's width"
    );
    assert_eq!(referenced(&mut fx, inner), referenced(&mut fx, mid));

    // The same slice taken straight from the root, so `start` composition cannot
    // double-count or drop an offset.
    let direct = fx.ok("binary", "part", &[parent, make_small(255), make_small(100)]);
    assert_eq!(bytes_of(inner), bytes_of(direct));
    assert_eq!(referenced(&mut fx, direct), 1025);

    // Three levels deep — above 512 bits, since the arm is chosen per call by the
    // **window's** size — still the root's payload (oracle: `deep_ref` `1025`,
    // `deep_size` `90`, and the same bytes as `part(B,260,90)`).
    let deep = fx.ok("binary", "part", &[inner, make_small(5), make_small(90)]);
    assert_eq!(bytes_of(deep), payload[260..350].to_vec());
    assert_eq!(codec_term::bitstring_bytes(deep).expect("a bitstring").2, 90 * 8);
    assert_eq!(referenced(&mut fx, deep), 1025);

    // ... and the ≤ 512-bit arm still copies when the parent is itself a window: a
    // 64-byte part of the 510-byte window answers 64 (the oracle's `small_ref` row).
    let small = fx.ok("binary", "part", &[mid, make_small(0), make_small(64)]);
    assert_eq!(bytes_of(small), payload[1..65].to_vec());
    assert_eq!(referenced(&mut fx, small), 64, "the copy arm applies over a window's parent too");
}

/// A window whose start is **not** a byte boundary is still a window: the pair's `start`
/// composes the parent's own offset, so `binary:referenced_byte_size/1` answers the
/// payload's width and the bytes are the shifted image.
///
/// The oracle facts this pins: a subject that merely *starts* mid-byte is accepted by
/// `erts_binary_part` (`<<_:3, R/binary>>`, then `binary:part(R,0,65)` succeeds), while a
/// subject with **tail** bits of its own is `badarg`.
#[test]
fn a_mid_byte_window_of_a_refc_parent_still_windows_the_payload() {
    let mut fx = Fixture::new();
    let payload = pattern_payload();
    // 1024 bytes starting at bit 3: byte-aligned as a whole (so it is a binary), but its
    // first byte straddles two payload bytes.
    let subject = fx.refc(&payload, 3, 1024 * 8);
    let (_, offset, size_bits) = codec_term::bitstring_bytes(subject).expect("a bitstring");
    assert_eq!((offset, size_bits), (3, 1024 * 8), "the subject starts mid-byte");
    assert_eq!(referenced(&mut fx, subject), 1025, "the subject is itself a window");

    let part = fx.ok("binary", "part", &[subject, make_small(0), make_small(65)]);
    assert_eq!(bits_of(part), extract_bits(&payload, 3, 65), "the shifted image");
    assert_eq!(referenced(&mut fx, part), 1025, "a mid-byte window answers the payload");

    // The copy arm re-packs its 64 bytes into an on-heap block.
    let small = fx.ok("binary", "part", &[subject, make_small(0), make_small(64)]);
    assert_eq!(bits_of(small), extract_bits(&payload, 3, 64));
    assert_eq!(referenced(&mut fx, small), 64);

    // A byte offset composes with the parent's bit offset: `part(subject, 1, 510)` starts
    // at bit 11 of the payload.
    let mid = fx.ok("binary", "part", &[subject, make_small(1), make_small(510)]);
    assert_eq!(bits_of(mid), extract_bits(&payload, 11, 510));
    assert_eq!(referenced(&mut fx, mid), 1025);
}

/// The arm switch is a property of the **window's** size, and the BIF's range is in
/// **bytes** — so the reachable crossing is 64 bytes (512 bits, the last copying arm) to
/// 65 bytes (520 bits, the first windowing arm), where the two arms answer visibly
/// different figures (`64` vs the payload's `96`).
///
/// A 511- or 513-**bit** window cannot be requested through `binary:part/2,3` at all (a
/// non-byte-aligned subject is `badarg`, asserted below), which is why the bit-exact arm
/// boundary is pinned in `src/binary.rs`'s in-module arm test through
/// `access::make_sub_binary` directly, and here only the byte-granular crossing is.
#[test]
fn the_window_arm_switches_at_64_bytes_of_window() {
    let mut fx = Fixture::new();
    let payload = pattern_payload()[..96].to_vec();
    let parent = fx.refc(&payload, 0, 96 * 8);
    assert_eq!(referenced(&mut fx, parent), 96);
    for (len, want) in [(63i64, 63i64), (64, 64), (65, 96), (96, 96)] {
        let part = fx.ok("binary", "part", &[parent, make_small(0), make_small(len)]);
        assert_eq!(bytes_of(part), payload[..len as usize].to_vec(), "part(0,{len}) bytes");
        assert_eq!(referenced(&mut fx, part), want, "part(0,{len}) over a 96-byte payload");
    }
    // 65 bytes is 520 bits, so the 512-bit threshold is crossed *inside* this byte step;
    // no byte length yields a 512-then-513-bit pair. A subject with tail bits cannot even
    // be asked about, so a 513-bit part is unreachable from a program.
    let odd = fx.refc(&payload, 0, 96 * 8 - 3);
    fx.badarg("binary", "part", &[odd, make_small(0), make_small(1)]);
}

/// The **documented residual**: an on-**heap** parent above 64 bytes keeps the copy
/// route, because every binary this crate's own producers build is an `ErlHeapBits`
/// (`src/bits.rs`'s placement policy) and a block has no payload to share.
///
/// The oracle answers the payload's width for these shapes (`part(binary:copy(<<1>>,100),
/// 0,65)` ⇒ `100`, `part(...,0,64)` ⇒ `64`, `part(...,10,5)` ⇒ `5` — the `q_part_*` rows
/// of the compiled fixture), so this test is the pin that a later refc-producing seam
/// cannot change the figure here silently.
#[test]
fn an_on_heap_parent_above_the_limit_still_copies_the_range() {
    let mut fx = Fixture::new();
    let one = fx.binary(&[1]);
    // Every producer of a > 64-byte binary in this crate places it on-heap.
    let copied = fx.ok("binary", "copy", &[one, make_small(1025)]);
    let listed = fx.ok("binary", "bin_to_list", &[copied]);
    let relisted = fx.ok("binary", "list_to_bin", &[listed]);
    let iolist = fx.list(&[copied]);
    let flattened = fx.ok("erlang", "iolist_to_binary", &[iolist]);
    for (label, parent) in [
        ("binary:copy/2", copied),
        ("binary:list_to_bin/1", relisted),
        ("erlang:iolist_to_binary/1", flattened),
    ] {
        assert_eq!(bytes_of(parent), vec![1u8; 1025], "{label} bytes");
        assert_eq!(referenced(&mut fx, parent), 1025, "{label} is a whole binary");
        let part = fx.ok("binary", "part", &[parent, make_small(1), make_small(510)]);
        assert_eq!(bytes_of(part), vec![1u8; 510], "{label}: the part's bytes");
        assert_eq!(
            referenced(&mut fx, part),
            510,
            "{label}: an on-heap parent above the limit keeps the COPY arm, so the part \
             answers its own 510 bytes (the oracle answers the payload's 1025)"
        );
    }
    // That copy route still pays one buffer of the *result* — the figure a window no
    // longer pays, which is the residual's cost as well as its shape.
    let (allocations, bytes, _) =
        allocations_in(|| fx.ok("binary", "part", &[copied, make_small(1), make_small(510)]));
    assert_eq!(
        (allocations, bytes),
        (1, 510),
        "the on-heap parent's copy route copies the selected range into a buffer"
    );
    // ... while a ≤ 64-byte part of the same parent is the in-place arm (0 allocations).
    let (allocations, bytes, _) =
        allocations_in(|| fx.ok("binary", "part", &[copied, make_small(0), make_small(64)]));
    assert_eq!(
        (allocations, bytes),
        (0, 0),
        "the ≤ 512-bit arm writes in place whatever the parent is"
    );
}
// ---------------------------------------------------------------------------
// `erlang:iolist_size/1` — measured, not materialised
// ---------------------------------------------------------------------------
/// `iolist_size/1` on a nested iolist (a sublist, a binary leaf and an improper binary
/// tail) counts by walking: no buffer is built at all.
#[test]
fn iolist_size_measures_a_nested_iolist_without_building_it() {
    let mut fx = Fixture::new();
    // `[1, [2, <<3>> | <<4>>]]` ⇒ 4 bytes, the oracle's figure: a nested list whose
    // second element carries an **improper binary tail** — all three leaf shapes at once.
    let three = fx.binary(&[3]);
    let four = fx.binary(&[4]);
    let with_tail = fx.heap.cons(three, four).expect("fits");
    let two_rest = fx.heap.cons(make_small(2), with_tail).expect("fits");
    let inner = fx.list(&[two_rest]);
    let list = fx.heap.cons(make_small(1), inner).expect("fits");
    let size = fx.ok("erlang", "iolist_size", &[list]);
    assert_eq!(signed_value(size), 4, "iolist_size/1 must count every leaf");
    let (allocations, bytes, _) = allocations_in(|| fx.ok("erlang", "iolist_size", &[list]));
    assert_eq!(
        (allocations, bytes),
        (0, 0),
        "iolist_size/1 must measure by walking, never by flattening"
    );
    // The same term flattens to the same five bytes (one buffer).
    let flat = fx.ok("erlang", "iolist_to_binary", &[list]);
    assert_eq!(bytes_of(flat), vec![1u8, 2, 3, 4]);
    // The `badarg` set is `iolist_to_binary/1`'s: a bare integer is not an iolist, and
    // an element outside `0..=255` is rejected by the walk.
    fx.badarg("erlang", "iolist_size", &[make_small(7)]);
    let too_big = fx.ints(&[300]);
    let bad = fx.list(&too_big);
    fx.badarg("erlang", "iolist_size", &[bad]);
    // ... while a one-element list of a valid byte is 1.
    let ok_bytes = fx.ints(&[65]);
    let one = fx.list(&ok_bytes);
    assert_eq!(signed_value(fx.ok("erlang", "iolist_size", &[one])), 1);
}
// ---------------------------------------------------------------------------
// Live oracle cross-check (skips loudly when `erl` is absent)
// ---------------------------------------------------------------------------
/// Runs one `erl -noshell -eval` statement; `None` (with a loud message) when the oracle
/// is unavailable, so the cross-check skips rather than fails.
fn oracle(statement: &str) -> Option<String> {
    match std::process::Command::new("erl").args(["-noshell", "-eval", statement]).output() {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}
/// Cross-checks the byte figures this file pins against the live oracle: the flattened
/// bytes of the shallow mixed iolist, the `iolist_size/1` count, and
/// `referenced_byte_size/1` of a **5-byte** part (which is at most 64 bytes, so it is the
/// copy arm on both sides and the two agree — the large-window rows are the compiled
/// fixture's, below).
#[test]
fn oracle_agrees_on_the_flattened_bytes_and_the_short_part_figure() {
    let Some(bytes_line) = oracle(
        "io:format(\"~w~n\",[iolist_to_binary([<<\"ab\">>,[99],$d])]), halt().",
    ) else {
        eprintln!("===============================================================");
        eprintln!("[beam_bifs binary_copy_cost] No usable system `erl` oracle found.");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("===============================================================");
        return;
    };
    assert_eq!(bytes_line.trim(), "<<97,98,99,100>>", "the oracle's flat bytes");
    let Some(size_line) = oracle("io:format(\"~w~n\",[iolist_size([1,[2,<<3>>|<<4>>]])]), halt().")
    else {
        return;
    };
    assert_eq!(size_line.trim(), "4", "the oracle's iolist size");
    let Some(referenced_line) = oracle(
        "io:format(\"~w~n\",[binary:referenced_byte_size(\
         binary:part(binary:copy(<<1>>,100),10,5))]), halt().",
    ) else {
        return;
    };
    // A 5-byte part is at or below 512 bits, i.e. C's `erts_extracted_bitstring_size` copy
    // arm, so the oracle answers the part's own 5 — the same figure this runtime's copy
    // arm answers (asserted in `binary_part_of_an_on_heap_parent_copies_the_range_in_place`).
    assert_eq!(
        referenced_line.trim(),
        "5",
        "a <= 64-byte part is a copy on both sides, so the figure is the part's own size"
    );
}

// ---------------------------------------------------------------------------
// `binary:referenced_byte_size/1` — the two C arms (`erl_bif_binary.c:2691-2717`)
// ---------------------------------------------------------------------------
/// The oracle matrix these pins reproduce, probed on OTP 29.1.2 / erts-17.1.1
/// (`P = binary:copy(<<1>>,512)`, `Q = binary:copy(<<1>>,100)`):
///
/// ```text
/// erl -noshell -pa /tmp/fx -eval 'fx2:run(), halt().'
///   % tail(P)=502/rt=512 smalltail(P)=5/rt=5          (fx2:tail/1 = <<_:80,R/binary>>)
///   % part 63/64/65/66: 63 64 512 512
///   % part(P,511,1)=1 part(Q,0,65)=100 part(part(P,0,65),0,64)=64
///   % copy(Q)=100 copy(Q,5)=500 copy(P)=512 same(copy(Q),Q)=false
///   % append P,0 =1026 append 1,2,P=514 append P,Q=1224   (unpinned; see below)
/// ```
///
/// The rule the first six figures reveal: a range of at most `ERL_ONHEAP_BINARY_LIMIT`
/// (64) **bytes** is *copied* into an `ErlHeapBits` (so it reports its own size), and a
/// larger one becomes a real `ErlSubBits` window over the parent payload (so it reports
/// the **parent's** `orig_size`). `binary:copy/1,2` never shares
/// (`erts_debug:same(binary:copy(Q),Q)` ⇒ `false`), so its 100 is the new payload's own
/// width.
///
/// A live writable accumulator (`<<P/binary,0>>`) reports its grown allocation (1026)
/// only while it is *unpinned*; `erts_pin_writable_binary` shrinks the payload back to
/// its apparent size (`erl_bits.c:2050-2075`), so a value stored across a collection
/// reports 513 instead.
#[test]
fn referenced_byte_size_answers_the_parent_payload_for_a_real_refc_window() {
    let mut fx = Fixture::new();
    // The C's `br != NULL` arm: an off-heap `[BinRef][ErlSubBits]` window answers the
    // **parent payload's** width (`(br->val)->orig_size`), never the window's — this is
    // the shape the oracle builds for `binary:part(Q,0,65)` (⇒ 100) and for a
    // matched-out `/binary` tail (⇒ 512).
    let payload = vec![7u8; 100];
    let whole = fx.refc(&payload, 0, 100 * 8);
    assert_eq!(
        signed_value(fx.ok("binary", "referenced_byte_size", &[whole])),
        100,
        "a whole refc binary reports its payload's 100 bytes"
    );
    for window_bytes in [65usize, 66, 99, 40] {
        let window = fx.refc(&payload, 0, window_bytes * 8);
        assert_eq!(
            signed_value(fx.ok("binary", "referenced_byte_size", &[window])),
            100,
            "a {window_bytes}-byte window over the 100-byte payload must report 100 \
             (the C's orig_size, not the window)"
        );
        // ... and its bytes are still the window's own.
        let bytes = fx.ok("binary", "copy", &[window, make_small(1)]);
        assert_eq!(bytes_of(bytes), vec![7u8; window_bytes], "the window's bytes");
    }
    // A window that starts mid-byte answers the same payload width (the answer is a
    // property of the payload, not of the window's alignment).
    let nibbles = fx.refc(&payload, 4, 8);
    assert_eq!(signed_value(fx.ok("binary", "referenced_byte_size", &[nibbles])), 100);
    // The C's `br == NULL` arm: an on-heap `ErlHeapBits` answers its own byte size for
    // every size, above the 64-byte limit too — and this crate's own producers build
    // exactly that shape however large the binary is (the residual pin below).
    for n in [3usize, 63, 64, 65, 66, 100, 512] {
        let heap_binary = fx.binary(&vec![1u8; n]);
        assert_eq!(
            signed_value(fx.ok("binary", "referenced_byte_size", &[heap_binary])),
            n as i64,
            "an on-heap binary of {n} bytes reports its own size"
        );
    }
    // A **match context** (`ErlSubBits` whose `orig` is not a `BinRef`) keeps the
    // `br == NULL` answer: the discriminator is `beam_term::refc::is_refc_binary` (the
    // C's `*boxed_val(orig) == HEADER_BIN_REF`, `erl_bits.c:117`), not "is a sub bits".
    // Such a term is built by the engine's match family, never by this crate, so it is
    // not constructed here.
    // The badarg arm: not a bitstring.
    let list = fx.ints(&[1, 2]);
    let list = fx.list(&list);
    fx.badarg("binary", "referenced_byte_size", &[list]);
}
/// The `binary:copy` family's own figures, and the one place a *whole* binary past the
/// 64-byte limit could report a different width: the crate keeps every binary it builds
/// on the heap, so `copy/1`, `copy/2`, `list_to_bin/1` and `iolist_to_binary/1` all
/// report the flattened length.
///
/// Oracle rows (same probe): `copy(Q)=100 copy(Q,5)=500 copy(P)=512`, and
/// `binary:referenced_byte_size(binary:list_to_bin(binary_to_list(P)))` ⇒ `512`.
#[test]
fn whole_binaries_report_their_own_flattened_width() {
    let mut fx = Fixture::new();
    let one = fx.binary(&[1]);
    let q = fx.ok("binary", "copy", &[one, make_small(100)]);
    let p = fx.ok("binary", "copy", &[one, make_small(512)]);
    for (label, term, expected) in [("copy(Q)", q, 100i64), ("copy(Q,5)", fx.ok("binary", "copy", &[q, make_small(5)]), 500), ("copy(P)", p, 512)] {
        assert_eq!(bytes_of(term).len(), expected as usize, "{label} bytes");
        assert_eq!(
            signed_value(fx.ok("binary", "referenced_byte_size", &[term])),
            expected,
            "{label} reports its own flattened width"
        );
    }
    // `binary:list_to_bin(binary_to_list(P))` ⇒ 512 on both sides: a flattened 512-byte
    // on-heap binary is whole.
    let listed = fx.ok("binary", "bin_to_list", &[p]);
    let round_trip = fx.ok("binary", "list_to_bin", &[listed]);
    assert_eq!(bytes_of(round_trip), vec![1u8; 512]);
    assert_eq!(
        signed_value(fx.ok("binary", "referenced_byte_size", &[round_trip])),
        512,
        "a 512-byte flattened binary reports 512"
    );
    // `iolist_to_binary([P, 0])` is `<<P/binary,0>>`'s byte image: 513 bytes, and the
    // *pinned* oracle figure for that shape is 513 too (`erts_pin_writable_binary`
    // shrinks a writable payload back to its apparent size, `erl_bits.c:2050-2075`;
    // 1026 is only the unpinned allocation of the same expression).
    let zero = make_small(0);
    let iolist = fx.list(&[p, zero]);
    let appended = fx.ok("erlang", "iolist_to_binary", &[iolist]);
    assert_eq!(bytes_of(appended).len(), 513, "the appended byte image");
    assert_eq!(
        signed_value(fx.ok("binary", "referenced_byte_size", &[appended])),
        513,
        "a 513-byte flattened binary reports 513"
    );
}

// ---------------------------------------------------------------------------
// The live oracle's window matrix, driven through a **compiled** module
// ---------------------------------------------------------------------------
//
// `-eval` cannot answer these rows: there the C reports a payload's *allocated* width
// (`2050` for the subject below — `binary:copy/2`'s writable buffer), so the recorded
// figures have to come from a compiled module. That is the route the matrix on
// `src/binary.rs::part_slice` was measured on, and the one this cross-check repeats.
/// The fixture: exactly the subject and the shapes the oracle matrix was measured on.
const REFCWIN_ERL: &str = r#"
-module(refcwin).
-export([run/0]).

%% The subject: 1025 bytes, `<<"z", (binary:copy(<<255>>,1024))/binary>>`.
run() ->
    P = binary:copy(<<255>>, 1024),
    B = <<"z", P/binary>>,
    W = binary:part(B, 1, 510),
    W2 = binary:part(W, 254, 100),
    D = binary:part(B, 255, 100),
    io:format("parent_ref=~p~n", [binary:referenced_byte_size(B)]),
    io:format("ref_0_63=~p~n", [binary:referenced_byte_size(binary:part(B, 0, 63))]),
    io:format("ref_0_64=~p~n", [binary:referenced_byte_size(binary:part(B, 0, 64))]),
    io:format("ref_0_65=~p~n", [binary:referenced_byte_size(binary:part(B, 0, 65))]),
    io:format("ref_1_510=~p~n", [binary:referenced_byte_size(W)]),
    io:format("ref_0_1024=~p~n", [binary:referenced_byte_size(binary:part(B, 0, 1024))]),
    io:format("ref_0_0=~p~n", [binary:referenced_byte_size(binary:part(B, 0, 0))]),
    io:format("wow_ref=~p~n", [binary:referenced_byte_size(W2)]),
    io:format("wow_size=~p~n", [byte_size(W2)]),
    io:format("wow_same_root=~p~n", [binary_to_list(W2) =:= binary_to_list(D)]),
    io:format("part_bytes=~p~n", [binary_to_list(W) =:= binary_to_list(binary:part(B, {1, 510}))]),
    io:format("md5=~s~n", [string:lowercase(binary:encode_hex(erlang:md5(W)))]),
    io:format("bp3_same=~p~n", [erlang:binary_part(B, 1, 510) =:= W]),
    io:format("bp3_ref=~p~n", [binary:referenced_byte_size(erlang:binary_part(B, 1, 510))]),
    io:format("bp2_same=~p~n", [erlang:binary_part(B, {1, 510}) =:= W]),
    io:format("bp3_md5_same=~p~n", [erlang:md5(erlang:binary_part(B, 1, 510)) =:= erlang:md5(W)]),
    %% A 100-byte `binary:copy/2` parent: the oracle places it off-heap and windows it.
    Q = binary:copy(<<1>>, 100),
    io:format("q_ref=~p~n", [binary:referenced_byte_size(Q)]),
    io:format("q_part_0_65=~p~n", [binary:referenced_byte_size(binary:part(Q, 0, 65))]),
    io:format("q_part_0_64=~p~n", [binary:referenced_byte_size(binary:part(Q, 0, 64))]),
    io:format("q_part_10_5=~p~n", [binary:referenced_byte_size(binary:part(Q, 10, 5))]),
    halt().
"#;
/// Whether `path` is an executable file.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.is_file()
            && std::fs::metadata(path)
                .is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}
/// `name` in the first `$PATH` directory that holds an executable of that name.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}
/// The system `erl`/`erlc` of that name: the sibling of `$ERTS_ORACLE` when that names an
/// executable (the crate's other oracle-driving suites' rule), else `$PATH`.
fn oracle_tool(name: &str) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let erl = PathBuf::from(value);
        if let Some(sibling) = erl.parent().map(|dir| dir.join(name)) {
            if is_executable(&sibling) {
                return Some(sibling);
            }
        }
    }
    on_path(name)
}
/// A `$TMPDIR` scratch directory holding the compiled `refcwin` fixture (the same shape
/// as `tests/term_to_binary_local_fun.rs`'s `Scratch`).
struct Scratch {
    dir: PathBuf,
}
impl Scratch {
    /// Creates the directory, writes [`REFCWIN_ERL`] and compiles it, panicking on any
    /// failure — a broken toolchain must never look like a pass.
    fn build(erlc: &Path) -> Scratch {
        let dir = std::env::temp_dir()
            .join(format!("beam_bifs_binary_copy_cost_refcwin_{}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
        let source = dir.join("refcwin.erl");
        std::fs::write(&source, REFCWIN_ERL)
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", source.display()));
        let output = Command::new(erlc)
            .arg("-o")
            .arg(&dir)
            .arg(&source)
            .output()
            .unwrap_or_else(|e| panic!("cannot run `{}`: {e}", erlc.display()));
        assert!(
            output.status.success(),
            "`{}` exited with {:?}\n--- stderr ---\n{}",
            erlc.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            dir.join("refcwin.beam").is_file(),
            "`erlc` produced no refcwin.beam in {}",
            dir.display()
        );
        Scratch { dir }
    }
    /// The scratch directory (the fixture's code path).
    fn path(&self) -> &Path {
        &self.dir
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }
}
/// Runs `refcwin:run()` in the oracle with the scratch directory on the code path and
/// returns its stdout, panicking on a spawn failure or a non-zero exit.
fn run_refcwin(erl: &Path, dir: &Path) -> String {
    let output = Command::new(erl)
        .arg("-noshell")
        .arg("-pa")
        .arg(dir)
        .args(["-eval", "refcwin:run()."])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle `{}`: {e}", erl.display()));
    assert!(
        output.status.success(),
        "the oracle `{}` exited with {:?}\n--- stderr ---\n{}",
        erl.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}
/// The value of the `key=value` line for `key` in an oracle transcript.
fn oracle_field<'a>(out: &'a str, key: &str) -> &'a str {
    let prefix = format!("{key}=");
    out.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("the oracle output has no `{key}=` line:\n{out}"))
}
/// Parses the `key=<integer>` line for `key`.
fn oracle_int(out: &str, key: &str) -> i64 {
    oracle_field(out, key)
        .parse()
        .unwrap_or_else(|e| panic!("the oracle's `{key}` is not an integer: {e}\n{out}"))
}
/// The compiled-module cross-check of the whole window matrix, row for row: the oracle's
/// own figures first (so a different oracle release shows up as itself, not as a
/// re-baselined Rust row), then the same shapes through this crate's registry, then the
/// one row that is **still** the oracle's and not ours.
///
/// Oracle rows (OTP 29.1.2 / erts-17.1.1, `B = <<"z", (binary:copy(<<255>>,1024))/binary>>`):
/// `parent_ref` `1025`, `ref_0_63` `63`, `ref_0_64` `64`, `ref_0_65` `1025`, `ref_1_510`
/// `1025`, `ref_0_1024` `1025`, `ref_0_0` `0`, `wow_ref` `1025`, `wow_size` `100`,
/// `md5` `332849ab09670313c986615b4202f4f5`, `bp3_ref` `1025`, and the `*_same` rows all
/// `true`.
///
/// Deferred, and **not** asserted through `beam_rt`: the runtime's result copy-out
/// (`context/calls.rs::copy_result_fresh`) passes `beam_message::copy_struct` an empty
/// source off-heap table, so a window is flattened back to an on-heap bitstring of the
/// window's length and a *program* asking `binary:referenced_byte_size/1` of a part still
/// sees the part's size. That is a `beam_rt`/`beam_message` seam, documented on
/// `part_slice`; this test drives the registry directly, where no copy-out runs.
#[test]
fn a_compiled_module_reports_the_payload_width_the_producer_now_matches() {
    let (Some(erl), Some(erlc)) = (oracle_tool("erl"), oracle_tool("erlc")) else {
        eprintln!("===============================================================");
        eprintln!("[beam_bifs binary_copy_cost] No usable `erl`/`erlc` oracle found.");
        eprintln!("The compiled-module window matrix is UNVERIFIED — NOT a pass.");
        eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
        eprintln!("===============================================================");
        return;
    };
    let scratch = Scratch::build(&erlc);
    let out = run_refcwin(&erl, scratch.path());

    // 1. The oracle's own matrix, against the recorded figures.
    for (key, want) in [
        ("parent_ref", 1025i64),
        ("ref_0_63", 63),
        ("ref_0_64", 64),
        ("ref_0_65", 1025),
        ("ref_1_510", 1025),
        ("ref_0_1024", 1025),
        ("ref_0_0", 0),
        ("wow_ref", 1025),
        ("wow_size", 100),
        ("bp3_ref", 1025),
    ] {
        assert_eq!(oracle_int(&out, key), want, "the oracle's `{key}` row");
    }
    for key in ["wow_same_root", "part_bytes", "bp3_same", "bp2_same", "bp3_md5_same"] {
        assert_eq!(oracle_field(&out, key), "true", "the oracle's `{key}` row");
    }
    assert_eq!(
        oracle_field(&out, "md5"),
        "332849ab09670313c986615b4202f4f5",
        "the oracle's md5 of part(B,1,510)"
    );

    // 2. The same shapes through this crate's registry, over the same 1025 bytes: every
    //    window row must be the oracle's figure, not merely a plausible one.
    let mut fx = Fixture::new();
    let payload = oracle_payload();
    assert_eq!(payload.len(), 1025);
    let parent = fx.refc(&payload, 0, payload.len() * 8);
    let mid = fx.ok("binary", "part", &[parent, make_small(1), make_small(510)]);
    let inner = fx.ok("binary", "part", &[mid, make_small(254), make_small(100)]);
    let direct = fx.ok("binary", "part", &[parent, make_small(255), make_small(100)]);
    let p63 = fx.ok("binary", "part", &[parent, make_small(0), make_small(63)]);
    let p64 = fx.ok("binary", "part", &[parent, make_small(0), make_small(64)]);
    let p65 = fx.ok("binary", "part", &[parent, make_small(0), make_small(65)]);
    let p1024 = fx.ok("binary", "part", &[parent, make_small(0), make_small(1024)]);
    let p0 = fx.ok("binary", "part", &[parent, make_small(0), make_small(0)]);
    let range = fx.heap.tuple(&[make_small(1), make_small(510)]).expect("the tuple fits");
    let via_two = fx.ok("binary", "part", &[parent, range]);
    let bp3 = fx.ok("erlang", "binary_part", &[parent, make_small(1), make_small(510)]);
    let bp_range = fx.heap.tuple(&[make_small(1), make_small(510)]).expect("the tuple fits");
    let bp2 = fx.ok("erlang", "binary_part", &[parent, bp_range]);
    let md5 = fx.ok("erlang", "md5", &[mid]);
    let bp3_md5 = fx.ok("erlang", "md5", &[bp3]);
    for (key, value) in [
        ("parent_ref", referenced(&mut fx, parent)),
        ("ref_0_63", referenced(&mut fx, p63)),
        ("ref_0_64", referenced(&mut fx, p64)),
        ("ref_0_65", referenced(&mut fx, p65)),
        ("ref_1_510", referenced(&mut fx, mid)),
        ("ref_0_1024", referenced(&mut fx, p1024)),
        ("ref_0_0", referenced(&mut fx, p0)),
        ("wow_ref", referenced(&mut fx, inner)),
        ("bp3_ref", referenced(&mut fx, bp3)),
    ] {
        assert_eq!(
            value,
            oracle_int(&out, key),
            "`{key}`: the registry's figure must be the oracle's"
        );
    }
    assert_eq!(
        codec_term::bitstring_bytes(inner).expect("a bitstring").2,
        usize::try_from(oracle_int(&out, "wow_size") * 8).expect("a bit count"),
        "the window-of-window child's own bit size"
    );
    assert_eq!(
        bytes_of(inner),
        bytes_of(direct),
        "the composed window must be the same bytes as the direct slice (the oracle's \
         `wow_same_root`)"
    );
    assert_eq!(oracle_field(&out, "wow_same_root"), "true");
    assert_eq!(
        bytes_of(via_two),
        bytes_of(mid),
        "`binary:part/2` and `binary:part/3` must agree (the oracle's `part_bytes`)"
    );
    assert_eq!(hex(&bytes_of(md5)), oracle_field(&out, "md5"), "the part's md5");
    assert_eq!(
        bytes_of(bp3),
        bytes_of(mid),
        "`erlang:binary_part/3` must be byte-identical to `binary:part/3`"
    );
    assert_eq!(bytes_of(bp2), bytes_of(mid), "`erlang:binary_part/2` too");
    assert_eq!(
        hex(&bytes_of(bp3_md5)),
        oracle_field(&out, "md5"),
        "the two spellings digest the same bytes"
    );

    // 3. The rows that are still the oracle's and not ours: a 100-byte `binary:copy/2`
    //    parent is placed **off-heap** by C (so a 65-byte part windows it and answers
    //    `100`), while this crate's producers keep it on-heap and the part answers its own
    //    `65` — the documented residual, asserted rather than glossed over. The ≤ 64-byte
    //    rows agree on both sides.
    assert_eq!(oracle_int(&out, "q_ref"), 100);
    assert_eq!(oracle_int(&out, "q_part_0_65"), 100);
    assert_eq!(oracle_int(&out, "q_part_0_64"), 64);
    assert_eq!(oracle_int(&out, "q_part_10_5"), 5);
    let one = fx.binary(&[1]);
    let q = fx.ok("binary", "copy", &[one, make_small(100)]);
    let q65 = fx.ok("binary", "part", &[q, make_small(0), make_small(65)]);
    let q64 = fx.ok("binary", "part", &[q, make_small(0), make_small(64)]);
    let q10_5 = fx.ok("binary", "part", &[q, make_small(10), make_small(5)]);
    assert_eq!(referenced(&mut fx, q), oracle_int(&out, "q_ref"), "the whole 100-byte copy");
    assert_eq!(
        referenced(&mut fx, q65),
        65,
        "the documented residual: an on-heap parent keeps the COPY arm, so this is 65 \
         where the oracle answers {} (q_part_0_65)",
        oracle_int(&out, "q_part_0_65")
    );
    assert_eq!(referenced(&mut fx, q64), oracle_int(&out, "q_part_0_64"));
    assert_eq!(referenced(&mut fx, q10_5), oracle_int(&out, "q_part_10_5"));
}
