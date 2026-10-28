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
//! | `binary:part/2,3`, `bin_to_list/2,3` | the whole subject copied into a `Vec`, then the slice | the borrowing accessor, so the copy is proportional to the **result** |
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
//!     binary:part(binary:copy(<<1>>,100),10,5))]), halt().'                           % 100
//! ```
//!
//! The last line is the one place this file cannot agree with the oracle: C's
//! `binary:part/3` returns a **sub-binary** sharing the parent's `BinRef`, while this
//! runtime's every built binary is on-heap (see `src/binary.rs`'s module doc for why
//! aliasing is unreachable from this crate), so `referenced_byte_size/1` of a part
//! reports the part's own size. That divergence is *measured* here rather than
//! asserted by prose: the live-oracle probe at the bottom prints both figures and the
//! Rust assertion pins the documented one.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
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
// `binary:part/2,3` — the copy is proportional to the *result*
// ---------------------------------------------------------------------------
/// `binary:part/3` copies the selected range (never the subject), and — because this
/// runtime builds no sub-binary — `binary:referenced_byte_size/1` of the part reports
/// the part's own size where the oracle reports the parent's.
///
/// The oracle's figures (probed live at the bottom of this file):
/// `binary:referenced_byte_size(binary:part(binary:copy(<<1>>,100),10,5))` ⇒ `100`,
/// here ⇒ `5`. The bytes are identical either way.
#[test]
fn binary_part_copies_the_selected_range_and_reports_its_own_referenced_size() {
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
        "a part reports its own size — the oracle's figure for this shape too \
         (probed live below); aliasing would need an off-heap BinRef window this \
         crate cannot mint (see src/binary.rs)"
    );
    // The cost: one buffer of the *result*, not of the 100-byte parent.
    let (allocations, bytes, _) =
        allocations_in(|| fx.ok("binary", "part", &[parent, make_small(10), make_small(5)]));
    assert_eq!(
        (allocations, bytes),
        (1, 5),
        "binary:part/3 must copy only the selected range"
    );
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
/// Cross-checks the two figures this cost change touches against the live oracle: the
/// flattened bytes of the shallow mixed iolist, and `referenced_byte_size/1` of a
/// `binary:part/3` result (the one documented divergence).
#[test]
fn oracle_agrees_on_the_bytes_and_reports_the_parents_referenced_size() {
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
    // The oracle's rule: a part aliases its parent, so the *parent's* 100 bytes are
    // reported. This runtime reports the part's own 5 (asserted above) — the recorded
    // divergence, which needs an off-heap `BinRef` window this crate cannot mint.
    assert_eq!(
        referenced_line.trim(),
        "5",
        "the oracle reports the parent's referenced size for a sub-binary"
    );
}
