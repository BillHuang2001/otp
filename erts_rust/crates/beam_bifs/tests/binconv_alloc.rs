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

//! Deterministic, dependency-free pin on the **transient allocation cost** of
//! `src/binconv.rs`: the `erlang:term_to_binary/1` / `erlang:binary_to_term/1`
//! round trip and the `erlang:list_to_binary/1` iolist encode.
//!
//! ## Why
//!
//! Both directions used to hold a payload-proportional *second* copy of the image:
//!
//! * encode: `encode_body` returned the encoder's body `Vec` (moved out of its
//!   `beam_codec::Writer` by `Encoder::finish`), and `plain_image` then allocated a
//!   `body.len() + 1` `Vec`, pushed `VERSION_MAGIC` into it and copied the body —
//!   so one encode allocated the writer buffer **and** a copy of the whole image.
//! * decode: `bytes_arg` copies the input image into a fresh `Vec` before the codec
//!   ever sees it (`bits::bytes` → `access::binary_bytes`).
//!
//! The encode half is fixable inside this crate: the codec's `Encoder::new` writes
//! `VERSION_MAGIC` into the writer *first*, so `finish()` **is** the image and the
//! second buffer disappears. The decode half is not (see `bytes_arg`'s note) — its
//! copy is measured here so the remaining cost is a recorded number rather than a
//! guess.
//!
//! ## The measurement
//!
//! A counting `#[global_allocator]` (the exact pattern of
//! `tests/{registry_hot_path,decode_hot_path}.rs`, armed **per thread** so the
//! libtest harness can never perturb a window) reports the number of `alloc`/
//! `realloc` calls and the bytes they requested. Every figure below is therefore
//! host-load independent — an allocation either happens or it does not — and
//! nothing here asserts on wall-clock time.
//!
//! Two workloads, each measured on both directions:
//!
//! * `map` — the five-key map of `examples/etf_bench.rs` (a 10-element list, an
//!   `{ok, Binary, Float}` tuple, a 12-byte binary, a char-list string, `-42`);
//!   it re-encodes to 101 bytes, so it exercises the *small* end where the codec's
//!   fixed `DEFAULT_IMAGE_CAPACITY` (128 bytes) dominates.
//! * `big` — a 4 KiB binary; its image is 4102 bytes, so the buffer question
//!   dominates.
//!
//! Measured figures (`realloc` counts its *growth*, so the byte total is the sum of
//! what the allocator was asked for). This file's assertions pin the post-fix column;
//! the pre-fix column is what the same harness reported before the encode was
//! collapsed, and it differs by exactly the copy that was removed:
//!
//! | workload | direction | before (alloc/bytes) | after (alloc/bytes) |
//! |---|---|---|---|
//! | `map` (101 B image)  | `term_to_binary/1`  | 2 / 229  | **1 / 128** |
//! | `map` (101 B image)  | `binary_to_term/1`  | 4 / 285  | 4 / 285     |
//! | `big` (4102 B image) | `term_to_binary/1`  | 3 / 8203 | **2 / 4102** |
//! | `big` (4102 B image) | `binary_to_term/1`  | 1 / 4102 | 1 / 4102    |
//!
//! So the encode no longer copies the image (`plain_image`'s `body.len() + 1` vec is
//! gone: 128 + 101 = 229 became 128, and 4102 + 4101 = 8203 became 4102), while the
//! decode's single input copy stands and is *reported* rather than removed — see
//! `bytes_arg`'s note in `src/binconv.rs` for why that copy is kept.
//!
//! ## The iolist encode (`list_to_binary/1`)
//!
//! `flatten_iolist` used to pay three transient costs on every call: a `vec![root]`
//! work stack (plus one growth once it pushed a cons's tail and head), an `out: Vec<u8>`
//! grown byte by byte from zero capacity (8, 16, 32, …), and a **temporary `Vec` per
//! binary element** (`bits::bytes` copies the payload). It is now a validation
//! pre-pass plus one write pass over a single `WalkStack` — `INLINE_WALK_FRAMES`
//! (128) frames inline on the Rust stack, spilling to a `Vec` only for a deeply nested
//! iolist — into **one** buffer reserved to the iolist's exact byte count, with each
//! binary element copied straight into that buffer through the closure-scoped borrow
//! `access::with_binary_bytes`:
//!
//! | workload | bytes | before (alloc/bytes) | after (alloc/bytes) |
//! |---|---|---|---|
//! | `flat` (a 64-element int list)             | 64  | 6 / 96    | **1 / 64**   |
//! | `mixed` (ints + a 128 B binary + a sublist) | 133 | 6 / 420   | **1 / 133**  |
//! | `deep` (200 nested one-element lists)       | 1   | 9 / 2056  | **7 / 1025** |
//! | `empty` (`[]`)                              | 0   | 1 / 8     | **0 / 0**    |
//!
//! `deep` still pays the *spill* (its 200 levels exceed the 128 inline frames: the
//! spill `Vec` grows 4 → 128 `Eterm`s, 1024 bytes over 6 events, once — the same stack
//! serves both passes). `erlang:iolist_to_binary/1` is measured alongside as the
//! **unchanged** sibling walk in `src/binary.rs` (6/96, 5/292, 9/2056, 1/8), so a
//! regression there would be visible here.
//!
//! `std`-only: no new dependencies. The `beam_bifs` lib is `#![deny(unsafe_code)]`,
//! but this file is its own crate root (the `#[global_allocator]` needs
//! `unsafe impl`).
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, LocalNode};
use beam_process::ProcessId;
use beam_term::{make_atom, make_small, Atom, AtomTable, Eterm, NIL};

// ---------------------------------------------------------------------------
// The counting allocator (as in `tests/registry_hot_path.rs`)
// ---------------------------------------------------------------------------

/// Number of `alloc`/`realloc` calls made on an armed thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Number of bytes requested on an armed thread.
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring.
    ///
    /// The counters are process-global, but the *arming* is per-thread, so the test
    /// harness (which allocates while formatting output) can never perturb a
    /// measurement. That is what makes the allocation evidence deterministic rather
    /// than merely usually-zero.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Records one allocation of `bytes` on the current thread while it is armed.
///
/// `try_with` tolerates a thread whose TLS is being torn down; with a `const`-init
/// `Cell` there is no lazy allocation and no destructor to run, so this is safe to
/// call from inside the allocator.
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

/// Serializes this file's two tests: the counters are process-global, so a window open
/// on one thread would otherwise absorb the other test's allocations (the arming is
/// per-thread, but the *totals* are not). Same guard as `tests/registry_hot_path.rs`.
static SERIAL: Mutex<()> = Mutex::new(());

/// Runs `body` with the counters armed on this thread and returns
/// `(allocations, bytes, result)`.
///
/// The window holds [`SERIAL`], so two tests in this binary never measure at once.
fn allocations_in<T>(body: impl FnOnce() -> T) -> (usize, usize, T) {
    // A poisoned lock only means the other test panicked; the counters are still
    // meaningful for this window, so recover rather than fail on the poison.
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

/// One armed measurement: the number of global allocations and the bytes they
/// requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Measurement {
    allocations: usize,
    bytes: usize,
}

impl Measurement {
    /// Measures `body` under the armed counters.
    fn of(body: impl FnOnce()) -> Measurement {
        let (allocations, bytes, ()) = allocations_in(body);
        Measurement { allocations, bytes }
    }
}

// ---------------------------------------------------------------------------
// The ETF fixtures
// ---------------------------------------------------------------------------

/// `VERSION_MAGIC`, the leading byte of a `term_to_binary` image.
const VERSION_MAGIC: u8 = 131;
const SMALL_ATOM_UTF8: u8 = 119;
const SMALL_INTEGER: u8 = 97;
const INTEGER: u8 = 98;
const SMALL_TUPLE: u8 = 104;
const BINARY_EXT: u8 = 109;
const NEW_FLOAT: u8 = 70;
const STRING_EXT: u8 = 107;
const LIST_EXT: u8 = 108;
const NIL_EXT: u8 = 106;
const MAP_EXT: u8 = 116;

/// The `payload/0` term of the `etf_roundtrip` bench case as an ETF image
/// (verbatim from `examples/etf_bench.rs`, as `tests/decode_hot_path.rs` carries
/// it): a map with five keys holding a 10-element list, a `{ok, <<"hello
/// world">>, 3.5}` tuple, a 12-byte binary, a char-list string and `-42`.
fn map_image() -> Vec<u8> {
    fn key(out: &mut Vec<u8>, name: &str) {
        out.push(SMALL_ATOM_UTF8);
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
    }

    let mut out = vec![VERSION_MAGIC, MAP_EXT];
    out.extend_from_slice(&5u32.to_be_bytes());
    // a => [1, ..., 10]
    key(&mut out, "a");
    out.push(LIST_EXT);
    out.extend_from_slice(&10u32.to_be_bytes());
    for n in 1..=10u8 {
        out.push(SMALL_INTEGER);
        out.push(n);
    }
    out.push(NIL_EXT);
    // b => {ok, <<"hello world">>, 3.5}
    key(&mut out, "b");
    out.push(SMALL_TUPLE);
    out.push(3);
    key(&mut out, "ok");
    out.push(BINARY_EXT);
    out.extend_from_slice(&11u32.to_be_bytes());
    out.extend_from_slice(b"hello world");
    out.push(NEW_FLOAT);
    out.extend_from_slice(&3.5f64.to_be_bytes());
    // c => <<1..12>>
    key(&mut out, "c");
    out.push(BINARY_EXT);
    out.extend_from_slice(&12u32.to_be_bytes());
    out.extend(1..=12u8);
    // d => "a char list"
    key(&mut out, "d");
    out.push(STRING_EXT);
    out.extend_from_slice(&11u16.to_be_bytes());
    out.extend_from_slice(b"a char list");
    // e => -42
    key(&mut out, "e");
    out.push(INTEGER);
    out.extend_from_slice(&(-42i32).to_be_bytes());
    out
}

/// The ETF image of the binary `<<payload/binary>>` — i.e. exactly the image
/// `term_to_binary(<<Payload/binary>>)` produces.
fn binary_image(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 6);
    out.push(VERSION_MAGIC);
    out.push(BINARY_EXT);
    out.extend_from_slice(&u32::try_from(payload.len()).expect("payload fits").to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Decodes `image` onto `heap`, interning its atoms into `atoms` (so an encode of
/// the resulting term can resolve every name).
fn term_of(image: &[u8], heap: &mut Heap, atoms: &mut AtomTable) -> Eterm {
    let (term, consumed) =
        beam_codec::decode_term_used(image, heap, atoms).expect("the fixture image decodes");
    // The image must be consumed whole, so the fixture is never silently truncated.
    assert_eq!(consumed, image.len(), "the fixture image is decoded whole");
    term
}

// ---------------------------------------------------------------------------
// The measurement driver
// ---------------------------------------------------------------------------

/// Runs one `registry` call once outside the armed window (so any first-call state
/// settles) and once inside it, returning the armed cost of the second call.
///
/// `arg` lives on the *machine* heap (`run_case`'s `terms`), `dest` is the
/// [`BifContext`]'s heap where a result term is built — **different** arenas, exactly as
/// in the runtime (process heap vs. the pooled BIF heap). Rewinding `dest` between the
/// two calls can therefore never disturb the argument.
fn measure(
    reg: &BifRegistry,
    function: &str,
    atoms: &AtomTable,
    dest: &mut Heap,
    arg: Eterm,
) -> Measurement {
    let pid = ProcessId::new(1, 0);
    // Warm-up (outside the window).
    dest.reset();
    {
        let mut ctx = BifContext::new(dest, atoms, pid);
        black_box(
            reg.call("erlang", function, 1, &mut ctx, &[arg])
                .expect("the warm-up call succeeds"),
        );
    }
    dest.reset();

    Measurement::of(|| {
        let mut ctx = BifContext::new(dest, atoms, pid);
        black_box(
            reg.call("erlang", function, 1, &mut ctx, &[arg])
                .expect("the measured call succeeds"),
        );
    })
}

/// One workload's evidence: its image size and the cost of each direction.
#[derive(Debug, Clone, Copy)]
struct Case {
    /// The size of the term's `term_to_binary/1` image, in bytes.
    image_len: usize,
    /// `erlang:term_to_binary/1` of the workload term.
    encode: Measurement,
    /// `erlang:binary_to_term/1` of that image, held in a binary term.
    decode: Measurement,
}

/// Builds one workload and measures both directions.
fn run_case(reg: &BifRegistry, fixture: &[u8]) -> Case {
    let mut atoms = AtomTable::new();
    // The runtime's own atoms, as `examples/etf_bench.rs` seeds them, so the map's
    // keys resolve and the encode is a name lookup rather than an interning event.
    atoms.seed(["true", "false", "ok", "nonode@nohost", "a", "b", "c", "d", "e"]);
    // The workload term and the decode input live on the *machine* heap; that heap
    // is never reset, so no term on it is ever overwritten.
    let mut terms = Heap::new(1 << 16).expect("machine heap");
    // The BIF's destination heap: rewound between calls, never reallocated.
    let mut dest = Heap::new(1 << 16).expect("destination heap");

    let workload = term_of(fixture, &mut terms, &mut atoms);
    // The image `erlang:term_to_binary/1` produces for that term. The codec's plain
    // `encode_term` is the very `Encoder::new` + `finish` route the BIF drives, so the
    // decode workload consumes the *same* image the encode produced (and `image_len`
    // is that image's true length, not the hand-written fixture's).
    let image = beam_codec::encode_term(workload, &atoms).expect("the workload encodes");
    // The decode input: a binary term holding exactly those image bytes.
    let decode_input = term_of(&binary_image(&image), &mut terms, &mut atoms);

    let encode = measure(reg, "term_to_binary", &atoms, &mut dest, workload);
    let decode = measure(reg, "binary_to_term", &atoms, &mut dest, decode_input);

    Case {
        image_len: image.len(),
        encode,
        decode,
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// The codec's `DEFAULT_IMAGE_CAPACITY` (`beam_codec::encode`): the first allocation an
/// `Encoder` makes, and therefore the floor under any `term_to_binary` image smaller than
/// it. Restated here (rather than imported) because it is the number this file's encode
/// bound is written against.
const INITIAL_WRITER_CAPACITY: usize = 128;

#[test]
fn term_to_binary_and_binary_to_term_hold_no_second_image_copy() {
    let reg = default_registry();
    let big_payload: Vec<u8> = (0..4096u32).map(|n| n as u8).collect();
    let map = run_case(&reg, &map_image());
    let big = run_case(&reg, &binary_image(&big_payload));
    assert_eq!(big.image_len, 4102, "the 4 KiB workload's image size");
    // The five-key map re-encodes to 101 bytes: the hand-written fixture spells the
    // 10-element list as `LIST_EXT`, while the codec writes the canonical (shorter)
    // `STRING_EXT` form. The *encode* workload is the same term either way.
    assert_eq!(map.image_len, 101, "the five-key map image size");

    eprintln!(
        "map: image {} B -> encode {} alloc/{} B, decode {} alloc/{} B\n\
         big: image {} B -> encode {} alloc/{} B, decode {} alloc/{} B",
        map.image_len,
        map.encode.allocations,
        map.encode.bytes,
        map.decode.allocations,
        map.decode.bytes,
        big.image_len,
        big.encode.allocations,
        big.encode.bytes,
        big.decode.allocations,
        big.decode.bytes,
    );

    // ---- encode: the image is the writer's buffer, never a copy of it ------
    //
    // `Encoder::new` writes `VERSION_MAGIC` into the writer before the body, so
    // `Encoder::finish` moves the *image* out: one allocation for the writer (grown at
    // most once — `Writer::bytes` reserves the payload in one step, not by doubling)
    // and nothing else. The bound is therefore "the writer's initial capacity, or the
    // image itself, whichever is larger".
    //
    // The pre-fix encode added `plain_image`'s `body.len() + 1` vec on top — measured
    // as 2 allocations / 229 bytes for the map and 3 / 8203 for the 4 KiB binary, so
    // each of these two assertions failed by exactly the copy it removed.
    for case in [map, big] {
        let ceiling = case.image_len.max(INITIAL_WRITER_CAPACITY);
        assert!(
            case.encode.bytes <= ceiling,
            "the {}-byte encode allocated {} bytes over {} allocations: past the \
             writer's own {ceiling} bytes, so a second image-sized buffer is back",
            case.image_len,
            case.encode.bytes,
            case.encode.allocations
        );
        assert!(
            case.encode.allocations <= 2,
            "the {}-byte encode allocated {} times: more than the writer's first \
             allocation plus at most one growth",
            case.image_len,
            case.encode.allocations
        );
    }

    // ---- decode: one input copy is the remaining payload-proportional cost --
    //
    // `bytes_arg` copies the whole input image (`bits::bytes` -> `access::binary_bytes`),
    // which is the one payload-proportional allocation left on this path: 4102 bytes of
    // copy for the 4 KiB image, plus the codec's own accumulators. It is *bounded* here
    // so it cannot silently become two copies.
    assert!(
        big.decode.bytes <= big.image_len + 1024,
        "the 4 KiB decode allocated {} bytes: more than one input-image copy ({}) plus \
         the codec's accumulators",
        big.decode.bytes,
        big.image_len
    );
    // The small map is pinned by `tests/decode_hot_path.rs` (9 allocations / 602 bytes
    // for its own image); repeating the budget here keeps **both** directions of one
    // workload in one place.
    assert!(
        map.decode.allocations <= 16 && map.decode.bytes <= 1024,
        "the small decode allocated {} times / {} bytes, past the payload-intrinsic \
         budget",
        map.decode.allocations,
        map.decode.bytes,
    );
}

// ---------------------------------------------------------------------------
// The `[local]` half: a buffer `beam_codec` still copies (a recorded handoff)
// ---------------------------------------------------------------------------

/// A [`LocalNode`] seam that always resolves. `[local]` encoding needs one (the 4-byte
/// node hash comes from the runtime, never from this crate), and with it installed the
/// option is exercised the way `beam_rt` exercises it.
struct StubLocalNode {
    node: Atom,
}

impl LocalNode for StubLocalNode {
    fn local_node(&mut self) -> Option<(Atom, u32)> {
        Some((self.node, 0))
    }

    fn local_node_hash(&mut self) -> Option<u32> {
        Some(0x1234_5678)
    }
}

/// The `[local]` image's allocation cost, for the `beam_codec` handoff this file's
/// header records: `Encoder::finish_local` returns a **new** buffer
/// (`[131][121][hash:4][body]`, `body.len() + 6` bytes) built by copying the encoder's
/// own, so one `[local]` encode allocates two image-sized buffers where the plain
/// encode now allocates one.
///
/// The measured figures pin the numbers in the handoff; they are **not** a contract this
/// crate can improve (the copy is in `beam_codec`), which is why the assertions are
/// upper bounds that will fail loudly only if the duplication gets *worse*.
#[test]
fn the_local_option_still_costs_a_second_image_buffer_in_beam_codec() {
    let reg = default_registry();
    let mut atoms = AtomTable::new();
    atoms.seed(["local", "nonode@nohost"]);
    let local_atom = Atom::from_raw(0);
    let mut terms = Heap::new(1 << 16).expect("machine heap");
    let mut dest = Heap::new(1 << 16).expect("destination heap");
    let mut node = StubLocalNode {
        node: Atom::from_raw(1),
    };
    let options = terms
        .cons(make_atom(local_atom), NIL)
        .expect("the [local] option list fits");

    let payload: Vec<u8> = (0..4096u32).map(|n| n as u8).collect();
    let workload = term_of(&binary_image(&payload), &mut terms, &mut atoms);
    let image_len = beam_codec::encode_term(workload, &atoms)
        .expect("the workload encodes")
        .len();

    let pid = ProcessId::new(1, 0);
    dest.reset();
    {
        let mut ctx = BifContext::new(&mut dest, &atoms, pid).with_local_node(&mut node);
        black_box(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[workload, options])
                .expect("the warm-up call succeeds"),
        );
    }
    dest.reset();
    let local = Measurement::of(|| {
        let mut ctx = BifContext::new(&mut dest, &atoms, pid).with_local_node(&mut node);
        black_box(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[workload, options])
                .expect("the measured call succeeds"),
        );
    });
    eprintln!(
        "local: image {image_len} B + 5-byte LOCAL_EXT prologue -> encode {} alloc/{} B",
        local.allocations, local.bytes
    );
    // Measured: 4 allocations / 8241 bytes — the writer (128 + one 3973-byte growth)
    // plus `finish_local`'s `body.len() + 6` = 4107-byte copy of the whole image. The
    // bound below allows two image-sized buffers and nothing more: a *third* would mean
    // another copy was introduced on this path.
    assert!(
        local.allocations <= 4 && local.bytes <= 2 * image_len + 128,
        "the [local] encode allocated {} times / {} bytes for a {image_len}-byte image: \
         more than the encoder's writer plus finish_local's copy",
        local.allocations,
        local.bytes,
    );
}

// ---------------------------------------------------------------------------
// The iolist encode: `erlang:list_to_binary/1` and `erlang:iolist_to_binary/1`
// ---------------------------------------------------------------------------

/// How many nested one-element lists the `deep` workload wraps around its single
/// byte. Chosen to exceed `INLINE_WALK_FRAMES` (the fixed-size walk stack in
/// `src/binconv.rs`) several times over, so the spilling fallback is exercised.
const DEEP_NESTING: usize = 200;

/// The payload of a `term_to_binary` image of a **binary** term — the inverse of
/// [`binary_image`], used to read a built binary's bytes back out exactly.
fn binary_payload(image: &[u8]) -> &[u8] {
    assert_eq!(
        &image[..2],
        &[VERSION_MAGIC, BINARY_EXT],
        "the workload result must encode as a plain binary"
    );
    let len = u32::from_be_bytes(image[2..6].try_into().expect("4 bytes")) as usize;
    assert_eq!(image.len(), 6 + len, "the image length matches its BINARY_EXT header");
    &image[6..]
}

/// The flat byte list `[1, 2, …, n]`, built on `heap`.
fn flat_int_list(heap: &mut Heap, n: u8) -> Eterm {
    let mut list = NIL;
    for byte in (1..=n).rev() {
        list = heap.cons(make_small(i64::from(byte)), list).expect("the flat list fits");
    }
    list
}

/// `depth` nested one-element lists around the byte `7`:
/// `[[[…[7]…]]]` — the nesting ladder the walk's spilling fallback must survive.
fn nested_singletons(heap: &mut Heap, depth: usize) -> Eterm {
    let mut term = make_small(7);
    for _ in 0..depth {
        term = heap.cons(term, NIL).expect("the nested list fits");
    }
    term
}

/// The mixed iolist `[1, 2, <<payload/binary>>, [3, 4], 5]`: two loose bytes, one
/// *binary* element (the per-element copy the accessor used to make), a nested list
/// and one trailing byte.
fn mixed_iolist(heap: &mut Heap, atoms: &mut AtomTable, payload: &[u8]) -> Eterm {
    let binary = term_of(&binary_image(payload), heap, atoms);
    let inner = heap.cons(make_small(4), NIL).expect("fits");
    let nested = heap.cons(make_small(3), inner).expect("fits");
    let mut list = heap.cons(make_small(5), NIL).expect("fits");
    list = heap.cons(nested, list).expect("fits");
    list = heap.cons(binary, list).expect("fits");
    list = heap.cons(make_small(2), list).expect("fits");
    heap.cons(make_small(1), list).expect("fits")
}

/// Calls `function/1` once and returns the **bytes** of the binary it built, read
/// back through `beam_codec` — so the allocation measurements below are pinned to a
/// call whose exact result is also checked.
fn built_bytes(
    reg: &BifRegistry,
    function: &str,
    atoms: &AtomTable,
    dest: &mut Heap,
    arg: Eterm,
) -> Vec<u8> {
    let pid = ProcessId::new(1, 0);
    dest.reset();
    let result = {
        let mut ctx = BifContext::new(dest, atoms, pid);
        reg.call("erlang", function, 1, &mut ctx, &[arg]).expect("the call succeeds")
    };
    let image = beam_codec::encode_term(result, atoms).expect("the result encodes");
    binary_payload(&image).to_vec()
}

/// One iolist workload's evidence: the cost of each of the two BIFs over it.
#[derive(Debug, Clone, Copy)]
struct IolistCase {
    /// The number of bytes the iolist denotes.
    byte_len: usize,
    /// `erlang:list_to_binary/1`.
    list_to_binary: Measurement,
    /// `erlang:iolist_to_binary/1` (the sibling walk in `src/binary.rs`).
    iolist_to_binary: Measurement,
}

/// Checks `root` builds `expected` under **both** BIFs, then measures both.
fn iolist_case(
    reg: &BifRegistry,
    atoms: &AtomTable,
    dest: &mut Heap,
    root: Eterm,
    expected: &[u8],
) -> IolistCase {
    for function in ["list_to_binary", "iolist_to_binary"] {
        assert_eq!(
            built_bytes(reg, function, atoms, dest, root),
            expected,
            "{function}/1 must build exactly the iolist's bytes"
        );
    }
    IolistCase {
        byte_len: expected.len(),
        list_to_binary: measure(reg, "list_to_binary", atoms, dest, root),
        iolist_to_binary: measure(reg, "iolist_to_binary", atoms, dest, root),
    }
}

/// The iolist encode's transient allocation cost: **one** buffer for the bytes,
/// whatever the iolist's shape.
#[test]
fn the_iolist_encode_allocates_one_byte_buffer() {
    let reg = default_registry();
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "nonode@nohost"]);
    // The workload terms live on the *machine* heap (never reset); the BIF's
    // destination term lives on `dest`, which is rewound between calls.
    let mut terms = Heap::new(1 << 16).expect("machine heap");
    let mut dest = Heap::new(1 << 16).expect("destination heap");

    let flat = flat_int_list(&mut terms, 64);
    let flat_expected: Vec<u8> = (1..=64u8).collect();

    let payload: Vec<u8> = (0..128u32).map(|n| n as u8).collect();
    let mixed = mixed_iolist(&mut terms, &mut atoms, &payload);
    let mut mixed_expected = vec![1u8, 2];
    mixed_expected.extend_from_slice(&payload);
    mixed_expected.extend([3u8, 4, 5]);

    let deep = nested_singletons(&mut terms, DEEP_NESTING);
    let deep_expected = vec![7u8];

    // `[]` — the degenerate iolist: no bytes, so not even a byte buffer.
    let empty = iolist_case(&reg, &atoms, &mut dest, NIL, &[]);

    let cases = [
        ("flat", iolist_case(&reg, &atoms, &mut dest, flat, &flat_expected)),
        ("mixed", iolist_case(&reg, &atoms, &mut dest, mixed, &mixed_expected)),
        ("deep", iolist_case(&reg, &atoms, &mut dest, deep, &deep_expected)),
        ("empty", empty),
    ];
    for (name, case) in cases {
        eprintln!(
            "{name} ({} B): list_to_binary {} alloc/{} B, iolist_to_binary {} alloc/{} B",
            case.byte_len,
            case.list_to_binary.allocations,
            case.list_to_binary.bytes,
            case.iolist_to_binary.allocations,
            case.iolist_to_binary.bytes,
        );
    }

    // ---- `list_to_binary/1`: exactly one allocation, the byte buffer -------
    //
    // Pre-fix `flatten_iolist` allocated the work stack (`vec![root]`, then one
    // growth as it pushed a cons's tail and head), grew `out` byte by byte from zero
    // capacity (8, 16, 32, …) and copied every *binary* element into a temporary
    // `Vec` (`bits::bytes`) before appending it. The measured before/after:
    //
    // | workload | before (alloc/bytes) | after (alloc/bytes) |
    // |---|---|---|
    // | `flat`  (64 B)   | 6 / 96    | **1 / 64**  |
    // | `mixed` (133 B)  | 6 / 420   | **1 / 133** |
    // | `deep`  (1 B)    | 9 / 2056  | **7 / 1025** |
    // | `empty` (0 B)    | 1 / 8     | **0 / 0**   |
    //
    // `flat`/`mixed` are the ideal: one `Vec::with_capacity(total)`, exactly the
    // iolist's byte count (`mixed`'s 128-byte binary element is copied straight into
    // it, so the pre-fix 420 bytes — the temporary copy plus `out`'s growth — are gone).
    // `empty` needs no buffer at all, and the walk itself no longer allocates.
    //
    // `deep` additionally pays for the **spill**: its 200 nesting levels exceed
    // `INLINE_WALK_FRAMES` (128), so the frames past that depth grow the spill `Vec`
    // (4, 8, 16, …, 128 `Eterm`s = 1024 bytes over 6 events) — once, because the same
    // `WalkStack` serves both passes. That is the spilling fallback's pinned cost, and
    // it is inherent to walking an iolist of *unbounded* nesting depth iteratively
    // (C's `erts_iolist_to_buf` recurses and would overflow the C stack).
    for (name, case) in cases {
        assert_eq!(
            case.list_to_binary.allocations,
            match name {
                // The byte buffer, plus the spill's growth for the deep nest.
                "deep" => 7,
                "empty" => 0,
                _ => 1,
            },
            "{name}: list_to_binary/1 must allocate {} times ({} bytes measured)",
            match name {
                "deep" => 7,
                "empty" => 0,
                _ => 1,
            },
            case.list_to_binary.bytes,
        );
    }
    // The buffer is the exact byte count: no doubling, no spare capacity.
    for (name, case) in cases {
        if name == "deep" {
            // The 1-byte buffer plus the spill's 1024 bytes.
            assert_eq!(case.list_to_binary.bytes, 1 + 1024, "the deep nest's measured bytes");
        } else {
            assert_eq!(
                case.list_to_binary.bytes, case.byte_len,
                "{name}: the one allocation must be exactly the iolist's byte count"
            );
        }
    }
    // A *deep* iolist still builds its bytes correctly (checked in `iolist_case`), so
    // the spilling fallback is not merely cheap but correct.

    // ---- `iolist_to_binary/1` is the *sibling* walk, unchanged here ----------
    //
    // Its yard (`src/binary.rs::collect_byte_unit` + `BitBuf`) is a separate
    // implementation, so this change does not touch it: measured identically before
    // and after (6/96, 5/292, 9/2056, 1/8 for `flat`, `mixed`, `deep`, `empty`). The
    // figures are pinned so that walk cannot silently *regress*, and the `mixed` row's
    // 292 bytes show it already copies each binary element only once (through
    // `beam_codec::term::bitstring_bytes`, a borrowed view). Adopting the same
    // one-buffer shape there is a recorded follow-up, not part of this change.
    for (name, case) in cases {
        let (allocations, bytes) = match name {
            "flat" => (6, 96),
            "mixed" => (5, 292),
            "deep" => (9, 2056),
            _ => (1, 8),
        };
        assert_eq!(
            (case.iolist_to_binary.allocations, case.iolist_to_binary.bytes),
            (allocations, bytes),
            "{name}: iolist_to_binary/1's measured cost (unchanged by this change)"
        );
    }
}
