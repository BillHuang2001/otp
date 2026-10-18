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

//! Deterministic, dependency-free pin on the **transient allocation cost** of the
//! `erlang:'+'/2`, `'-'/2` and `'*'/2` BIFs in `src/arith.rs`.
//!
//! ## Why
//!
//! The exact-integer operators used to widen **every** operand into [`Big`]
//! (`{negative: bool, mag: Vec<u64>}`) before doing anything: `Big::from_i64` is
//! `Big::new(value < 0, vec![value.unsigned_abs()])`, i.e. one heap-allocated
//! `Vec` per small-integer operand, plus the magnitude vector of the result —
//! even though a sum/difference/product of two 60-bit smalls is a 60-bit small
//! again in the overwhelming majority of cases and the term itself is an
//! immediate that lives in a machine word.
//!
//! So `2 + 3` allocated three `Vec`s (two operands, one result) to produce the
//! immediate `5`. The operators now take a machine-arithmetic fast path when
//! **both** operands are small integers, and route the result to the bignum path
//! only when it leaves `MIN_SMALL..=MAX_SMALL`.
//!
//! ## The measurement
//!
//! A counting `#[global_allocator]` — the exact pattern of
//! `tests/{registry_hot_path,decode_hot_path,binconv_alloc}.rs`, armed **per
//! thread** so the libtest harness can never perturb a window — reports the
//! number of `alloc`/`realloc` calls and the bytes they requested. Every figure
//! below is therefore host-load independent (an allocation either happens or it
//! does not) and nothing here asserts on wall-clock time.
//!
//! Note what the counters do *not* include: a term the BIF builds on the
//! [`BifContext`] heap (`ctx.int`) is a bump allocation inside a preallocated
//! arena, so it is invisible here by construction. That is the point — a small
//! integer result is an immediate and must cost nothing at all.
//!
//! Measured figures (`realloc` counts its *growth*, so the byte total is the sum
//! of what the allocator was asked for). This file's assertions pin the post-fix
//! column; the pre-fix column is what the same harness reported before the fast
//! path:
//!
//! | case | before (alloc/bytes) | after (alloc/bytes) |
//! |---|---|---|
//! | `2 + 3` (small)                    | 3 / 32 | **0 / 0** |
//! | `5 - 3` (small)                    | 4 / 32 | **0 / 0** |
//! | `2 * 3` (small)                    | 3 / 32 | **0 / 0** |
//! | `MAX_SMALL + 1` (promotes)         | 3 / 32 | 1 / 8     |
//! | `MAX_SMALL * 2` (promotes)         | 3 / 32 | 1 / 8     |
//! | `MIN_SMALL - 1` (promotes)         | 4 / 40 | 1 / 8     |
//! | `2 * 1.5` (float path)             | 1 / 8  | 1 / 8     |
//! | `2 + 0.5` (float path)             | 1 / 8  | 1 / 8     |
//!
//! The pre-fix shape is the widening, not an accident: `+`/`*` were two 8-byte
//! operand `Vec`s plus a 16-byte result magnitude, and `-` went through
//! `Big::sub`'s `self.add(&other.neg())`, which allocates the negated operand as
//! well (hence its fourth allocation and its 40 bytes on a promoted result).
//!
//! The promoted row keeps **one** allocation: the fast path materializes its
//! out-of-range result through `int_from_i64`, which widens the value into a
//! `Big` exactly as before, so a promoted result is bit-for-bit the pre-fix
//! one. Promotion is not the hot path and is reported honestly rather than
//! claimed as zero.
//!
//! The float row is **unchanged** on purpose: the mixed int/float route still
//! widens the integer operand for the correctly-rounded `big_to_double`
//! conversion, so the fast path deliberately does not touch it (see
//! `a_float_operand_still_takes_the_float_path`).
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
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{make_atom, make_small, signed_value, AtomTable, Eterm, MAX_SMALL, MIN_SMALL};

// ---------------------------------------------------------------------------
// The counting allocator (as in `tests/binconv_alloc.rs`)
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

/// Serializes this file's measuring tests: the counters are process-global, so a
/// window open on one thread would otherwise absorb another test's allocations (the
/// arming is per-thread, but the *totals* are not). Same guard as
/// `tests/binconv_alloc.rs`.
static SERIAL: Mutex<()> = Mutex::new(());

/// Runs `body` with the counters armed on this thread and returns
/// `(allocations, bytes, result)`.
///
/// The window holds [`SERIAL`], so two tests in this binary never measure at once.
fn allocations_in<T>(body: impl FnOnce() -> T) -> (usize, usize, T) {
    // A poisoned lock only means another test panicked; the counters are still
    // meaningful for this window, so recover rather than fail on the poison.
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
// The fixture
// ---------------------------------------------------------------------------

/// The registry, the shared atom table, the arguments' heap and the BIFs'
/// destination heap.
///
/// The two heaps are **different arenas**, exactly as in the runtime (a process
/// heap vs. the pooled BIF heap): the arguments live on [`Fixture::terms`], which
/// is never rewound, while [`Fixture::dest`] is where a result term is built and
/// is reset between calls. Resetting `dest` can therefore never disturb an
/// argument.
struct Fixture {
    registry: BifRegistry,
    atoms: AtomTable,
    terms: Heap,
    dest: Heap,
}

impl Fixture {
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        // The one atom the error case needs (`1 + a` ⇒ `badarith`).
        atoms.seed(["a"]);
        Fixture {
            registry: default_registry(),
            atoms,
            terms: Heap::new(1 << 16).expect("argument heap"),
            dest: Heap::new(1 << 16).expect("destination heap"),
        }
    }

    /// `erlang:function/args.len()` over `args`, reporting the BIF's own result.
    fn try_call(&mut self, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let atoms = &self.atoms;
        let pid = ProcessId::new(1, 0);
        self.dest.reset();
        let mut ctx = BifContext::new(&mut self.dest, atoms, pid);
        self.registry
            .call("erlang", function, args.len() as u32, &mut ctx, args)
    }

    /// [`Fixture::try_call`] with the success required.
    fn call(&mut self, function: &str, args: &[Eterm]) -> Eterm {
        let arity = args.len();
        self.try_call(function, args)
            .unwrap_or_else(|e| panic!("erlang:{function}/{arity} unexpectedly failed: {e:?}"))
    }

    /// The armed allocation cost of one [`Fixture::call`]: one warm-up call
    /// outside the window (so any first-call state settles), then one measured
    /// call inside it.
    fn measure(&mut self, function: &str, args: &[Eterm]) -> Measurement {
        let _ = self.call(function, args);
        let registry = &self.registry;
        let atoms = &self.atoms;
        let dest = &mut self.dest;
        let pid = ProcessId::new(1, 0);
        dest.reset();
        Measurement::of(|| {
            let mut ctx = BifContext::new(dest, atoms, pid);
            black_box(
                registry
                    .call("erlang", function, args.len() as u32, &mut ctx, args)
                    .expect("the measured call succeeds"),
            );
        })
    }

    /// A float term on the argument heap.
    fn float(&mut self, value: f64) -> Eterm {
        beam_codec::term::make_float(&mut self.terms, value).expect("a float fits")
    }
}

// ---------------------------------------------------------------------------
// Value readers (the fast path is asserted *by value*, never by cost alone)
// ---------------------------------------------------------------------------

/// The value of a small-integer term.
fn small(term: Eterm) -> i64 {
    assert!(
        beam_term::is_small(term),
        "expected a small int, got {term:?}"
    );
    signed_value(term)
}

/// The `(negative, limbs)` parts of a bignum term, copied out of its storage.
fn parts(term: Eterm) -> (bool, Vec<u64>) {
    let (negative, digits) = beam_codec::term::big_parts(term).expect("expected a bignum");
    (negative, digits.to_vec())
}

/// The `f64` value of a float term.
fn float(term: Eterm) -> f64 {
    beam_codec::term::float_value(term).expect("expected a float")
}

// ---------------------------------------------------------------------------
// The small-integer fast path
// ---------------------------------------------------------------------------

/// `+`, `-` and `*` of two small integers are pure machine arithmetic: the result
/// is an immediate, so the whole call touches the global allocator **zero** times.
///
/// Before the fast path each of these allocated three `Vec`s (two widened
/// operands plus the result magnitude, 8 bytes each), which the assertions below
/// would have failed on by exactly that margin.
#[test]
fn small_integer_operators_do_not_allocate() {
    let mut fx = Fixture::new();
    let two = make_small(2);
    let three = make_small(3);
    let five = make_small(5);

    let add = fx.measure("+", &[two, three]);
    let sub = fx.measure("-", &[five, three]);
    let mul = fx.measure("*", &[two, three]);
    eprintln!(
        "small: '+' {} alloc/{} B, '-' {} alloc/{} B, '*' {} alloc/{} B",
        add.allocations, add.bytes, sub.allocations, sub.bytes, mul.allocations, mul.bytes
    );

    // The values are the oracle's, and they are still *immediates*.
    assert_eq!(small(fx.call("+", &[two, three])), 5);
    assert_eq!(small(fx.call("-", &[five, three])), 2);
    assert_eq!(small(fx.call("*", &[two, three])), 6);

    for (what, case) in [("'+'/2", add), ("'-'/2", sub), ("'*'/2", mul)] {
        assert_eq!(
            case.allocations, 0,
            "erlang:{what} of two small integers allocated {} times / {} bytes: the \
             small-integer fast path is gone",
            case.allocations, case.bytes
        );
    }
}

/// A result that leaves the 60-bit small range still promotes to the **exact**
/// bignum, and still costs the widening it always did.
///
/// The fast path computes the operation in machine arithmetic and hands an
/// out-of-range result to `int_from_i64` — the pre-existing promotion seam — so
/// the value is bit-for-bit what the widened path produced. One 8-byte `Vec` (the
/// widened `i64`) is the honest remaining cost and is reported, not rounded down.
#[test]
fn a_result_past_the_small_range_still_promotes() {
    let mut fx = Fixture::new();
    let max = make_small(MAX_SMALL);
    let min = make_small(MIN_SMALL);
    let one = make_small(1);
    let two = make_small(2);

    let add = fx.measure("+", &[max, one]);
    let mul = fx.measure("*", &[max, two]);
    let sub = fx.measure("-", &[min, one]);
    eprintln!(
        "promotion: 'MAX_SMALL+1' {} alloc/{} B, 'MAX_SMALL*2' {} alloc/{} B, \
         'MIN_SMALL-1' {} alloc/{} B",
        add.allocations, add.bytes, mul.allocations, mul.bytes, sub.allocations, sub.bytes
    );

    // Oracle (`erl -noshell -eval`): 576460752303423488, 1152921504606846974,
    // -576460752303423489 — each a bignum, as limbs (least significant first).
    assert_eq!(
        parts(fx.call("+", &[max, one])),
        (false, vec![576_460_752_303_423_488]),
        "MAX_SMALL + 1"
    );
    assert_eq!(
        parts(fx.call("*", &[max, two])),
        (false, vec![1_152_921_504_606_846_974]),
        "MAX_SMALL * 2"
    );
    assert_eq!(
        parts(fx.call("-", &[min, one])),
        (true, vec![576_460_752_303_423_489]),
        "MIN_SMALL - 1"
    );

    // Reported, never pretended away: one allocation for the widened value. The
    // bound is a ceiling so a later improvement (materializing the bignum without
    // a `Big`) is allowed to lower it.
    for (what, case) in [
        ("MAX_SMALL + 1", add),
        ("MAX_SMALL * 2", mul),
        ("MIN_SMALL - 1", sub),
    ] {
        assert!(
            case.allocations <= 1 && case.bytes <= 8,
            "{what} allocated {} times / {} bytes: more than the single widened `Vec` \
             the promotion path needs",
            case.allocations,
            case.bytes
        );
    }
}

/// A float operand still takes the float path — the same `f64` result with the same
/// `finite()` overflow rejection.
///
/// The mixed int/float route is deliberately **not** fast-pathed: the integer
/// operand is widened to `Big` for the correctly-rounded `big_to_double`
/// conversion (`(1 bsl 3000) + 1.0` ⇒ `badarith` via that same conversion), so its
/// cost is unchanged and reported here rather than claimed as zero.
#[test]
fn a_float_operand_still_takes_the_float_path() {
    let mut fx = Fixture::new();
    let two = make_small(2);
    let one_point_five = fx.float(1.5);
    let half = fx.float(0.5);

    let mul = fx.measure("*", &[two, one_point_five]);
    let add = fx.measure("+", &[two, half]);
    eprintln!(
        "float: '2*1.5' {} alloc/{} B, '2+0.5' {} alloc/{} B",
        mul.allocations, mul.bytes, add.allocations, add.bytes
    );

    assert_eq!(float(fx.call("*", &[two, one_point_five])), 3.0);
    assert_eq!(float(fx.call("+", &[two, half])), 2.5);
    // Unchanged by the fast path: one widened operand `Vec` per call.
    for (what, case) in [("2 * 1.5", mul), ("2 + 0.5", add)] {
        assert!(
            case.allocations <= 2,
            "{what} allocated {} times: more than the float path's one widened operand",
            case.allocations
        );
    }

    // The float overflow rejection is untouched (`1.0e308 * 2.0` ⇒ `badarith`).
    let huge = fx.float(1.0e308);
    assert!(matches!(
        fx.try_call("*", &[huge, two]),
        Err(BeamError::BadArith)
    ));
}

/// A table of results pinned to the OTP 29 oracle, asserted **by value** so the
/// fast path is proven not to change arithmetic — at the small range's edges, on
/// the signs, and across the promotion boundary in both directions.
#[test]
fn the_fast_path_does_not_change_any_result() {
    let mut fx = Fixture::new();
    let max = make_small(MAX_SMALL);
    let min = make_small(MIN_SMALL);
    let zero = make_small(0);
    let minus_two = make_small(-2);
    let minus_three = make_small(-3);

    // Small results, both signs.
    assert_eq!(small(fx.call("+", &[minus_two, minus_three])), -5);
    assert_eq!(small(fx.call("-", &[minus_two, minus_three])), 1);
    assert_eq!(small(fx.call("*", &[minus_two, minus_three])), 6);
    assert_eq!(small(fx.call("*", &[minus_two, zero])), 0);
    assert_eq!(small(fx.call("-", &[zero, zero])), 0);
    // The range's edges, where both operands are still smalls.
    assert_eq!(small(fx.call("+", &[max, min])), -1);
    assert_eq!(small(fx.call("-", &[max, max])), 0);
    assert_eq!(small(fx.call("-", &[min, min])), 0);

    // Oracle: `0 - -576460752303423488` ⇒ 576460752303423488 (a bignum).
    assert_eq!(
        parts(fx.call("-", &[zero, min])),
        (false, vec![576_460_752_303_423_488]),
        "0 - MIN_SMALL"
    );
    // Oracle: `MIN_SMALL + MIN_SMALL` ⇒ -1152921504606846976.
    assert_eq!(
        parts(fx.call("+", &[min, min])),
        (true, vec![1_152_921_504_606_846_976]),
        "MIN_SMALL + MIN_SMALL"
    );
    // Oracle: `MIN_SMALL - MAX_SMALL` ⇒ -1152921504606846975.
    assert_eq!(
        parts(fx.call("-", &[min, max])),
        (true, vec![1_152_921_504_606_846_975]),
        "MIN_SMALL - MAX_SMALL"
    );
    // Oracle: `MAX_SMALL * MIN_SMALL` ⇒ -332306998946228967649491012766662656
    // (= -(2^118 - 2^59)): a product that does not fit an `i64` either, so this is
    // the `checked_mul` overflow fallback as well as a promotion.
    assert_eq!(
        parts(fx.call("*", &[max, min])),
        (
            true,
            vec![17_870_283_321_406_128_128, 18_014_398_509_481_983]
        ),
        "MAX_SMALL * MIN_SMALL"
    );
    // Oracle: `MAX_SMALL * MAX_SMALL` ⇒ 332306998946228967073030260463239169
    // (= 2^118 - 2^60 + 1) — both operands small, product past `i64`.
    assert_eq!(
        parts(fx.call("*", &[max, max])),
        (
            false,
            vec![17_293_822_569_102_704_641, 18_014_398_509_481_983]
        ),
        "MAX_SMALL * MAX_SMALL"
    );

    // A non-number operand is still `badarith`, whichever side it sits on: the fast
    // path is guarded on **both** operands being small integers, so an atom falls
    // through to the classification the widened route uses (`1 + a` in the oracle).
    let name = make_atom(fx.atoms.lookup("a").expect("the seeded atom"));
    let one = make_small(1);
    assert!(matches!(
        fx.try_call("+", &[one, name]),
        Err(BeamError::BadArith)
    ));
    assert!(matches!(
        fx.try_call("+", &[name, one]),
        Err(BeamError::BadArith)
    ));
    assert!(matches!(
        fx.try_call("-", &[name, one]),
        Err(BeamError::BadArith)
    ));
    assert!(matches!(
        fx.try_call("*", &[one, name]),
        Err(BeamError::BadArith)
    ));
}

/// The exact value of an integer term (small or bignum) as an `i128`.
///
/// An `i128` holds every value the matrix below can produce exactly — the widest
/// is a product of two 60-bit operands at 118 bits — so this is an *independent*
/// computation of the same number, not a re-run of the BIF's own arithmetic.
fn value(term: Eterm) -> i128 {
    if beam_term::is_small(term) {
        return i128::from(signed_value(term));
    }
    let (negative, digits) = parts(term);
    assert!(
        digits.len() <= 2,
        "the matrix's results need at most two limbs"
    );
    let magnitude = digits.iter().enumerate().fold(0i128, |acc, (i, limb)| {
        acc | (i128::from(*limb) << (64 * i))
    });
    if negative {
        -magnitude
    } else {
        magnitude
    }
}

/// The small range's edges, crossed in every direction, for all three operators —
/// asserted against the independent `i128` value **and** against the promotion
/// rule: the result is an immediate exactly when it fits `MIN_SMALL..=MAX_SMALL`.
///
/// This is the wrap/truncation guard. If the fast path ever produced a wrapped or
/// truncated value, or promoted one case too early / too late, some cell of this
/// matrix would disagree — `MAX_SMALL * MAX_SMALL` (118 bits) and `MIN_SMALL *
/// MIN_SMALL` are in it for the `checked_mul` fallback, `MAX_SMALL + 1` and
/// `MIN_SMALL - 1` for the promotion boundary, and `MAX_SMALL - MIN_SMALL` for the
/// widest difference.
#[test]
fn the_whole_small_range_boundary_matrix_is_exact() {
    let mut fx = Fixture::new();
    let samples = [
        MIN_SMALL,
        MIN_SMALL + 1,
        -(1 << 30),
        -2,
        -1,
        0,
        1,
        2,
        1 << 30,
        MAX_SMALL - 1,
        MAX_SMALL,
    ];
    let lowest = i128::from(MIN_SMALL);
    let highest = i128::from(MAX_SMALL);
    let mut checked = 0usize;
    for &a in &samples {
        for &b in &samples {
            let (x, y) = (i128::from(a), i128::from(b));
            for (op, expected) in [("+", x + y), ("-", x - y), ("*", x * y)] {
                let term = fx.call(op, &[make_small(a), make_small(b)]);
                assert_eq!(value(term), expected, "{a} {op} {b}: wrong value");
                let fits = (lowest..=highest).contains(&expected);
                assert_eq!(
                    beam_term::is_small(term),
                    fits,
                    "{a} {op} {b}: the result {expected} must be {} immediate",
                    if fits { "an" } else { "a bignum, not an" }
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, samples.len() * samples.len() * 3, "every cell ran");
}
