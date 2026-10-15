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

//! Deterministic pin on the **atom-intern commit path's allocation behaviour**: interning a
//! novel name through a registered BIF must allocate `O(names)` — never `O(table)`.
//!
//! `CodeServer::atoms_mut` is `Rc::make_mut`, so a commit taken while the running call still
//! holds an `Rc<AtomTable>` handle (the call's own `atoms` binding, the `RuntimeMemory`
//! snapshot built for `erlang:memory/0,1`) deep-cloned the whole ~3000-atom table on **every**
//! interning call — `binary_to_term`/`list_to_atom`/`binary_to_atom`/`load_nif`. The commit now
//! goes through `CodeServer::intern_pending`, the append-only route, and `context::calls` drops
//! both transient handles first so that route's `Rc::get_mut` fast path really applies: the
//! names are appended in place, at the indices the `PendingInterner` predicted.
//!
//! ## The measurement
//!
//! A counting `#[global_allocator]` (the exact pattern of `beam_bifs/tests/decode_hot_path.rs`
//! and `beam_bifs/tests/registry_hot_path.rs`) wraps `std::alloc::System` with two
//! `AtomicUsize` counters armed **per-thread**, so the libtest harness and any sibling test can
//! never perturb a measurement. This is host-load independent: an allocation either happens or
//! it does not, so nothing here asserts on wall-clock time.
//!
//! The **same** canonical workload — `erlang:binary_to_term/1` of a bare ETF atom image whose
//! name is *novel* every time — is run against **two very different shared-table sizes** (the
//! booted runtime's own table, and that table plus [`LARGE_PAD`] synthetic atoms appended
//! through the ordinary public `Runtime::intern`). Each size is warmed up once (an unarmed
//! call, which also extends `beam_bifs`' per-thread decode memo to the new table length) and
//! then measured once, so the measured call is the steady state.
//!
//! ## What is asserted
//!
//! 1. The two interning-call counts must **not scale with the shared `AtomTable`**: the large
//!    measurement may differ from the small one by at most [`ALLOCATION_SLACK`] allocations —
//!    the `Vec`/`HashMap` growth an append may or may not trigger at that size, not a table
//!    clone. A re-introduced per-call clone would add thousands.
//! 2. The absolute count is bounded by a small, payload-intrinsic budget
//!    ([`HOT_ALLOCATION_BUDGET`]), far below even the smallest table clone here.
//! 3. The measurement is proven **sensitive** by a positive control in the same harness: an
//!    actual `AtomTable::clone()` (exactly the work `Rc::make_mut` performs on a shared table)
//!    at each size. It must scale with the table *and* sit an order of magnitude above the
//!    interning path — without it, an accidentally-too-narrow window would make (1) vacuous.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_engine::{terms, ExecutionContext};
use beam_rt::{Runtime, RuntimeConfig, RuntimeContext};

// ---------------------------------------------------------------------------
// The counting allocator (as in `beam_bifs/tests/decode_hot_path.rs`)
// ---------------------------------------------------------------------------

/// Number of `alloc`/`realloc` calls made on an armed thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Number of bytes requested on an armed thread.
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring.
    ///
    /// The counters are process-global, but the *arming* is per-thread, so the test harness
    /// (which allocates while formatting output) can never perturb a measurement. That is what
    /// makes the allocation evidence deterministic rather than merely usually-zero.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Records one allocation of `bytes` on the current thread while it is armed.
///
/// `try_with` tolerates a thread whose TLS is being torn down; with a `const`-init `Cell`
/// there is no lazy allocation and no destructor to run, so this is safe to call from inside
/// the allocator.
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

/// One armed measurement: the number of global allocations and the bytes they requested.
#[derive(Debug, Clone, Copy)]
struct Measured {
    allocations: usize,
    bytes: usize,
}

/// Runs `body` with the counters armed on this thread and returns what it measured.
fn allocations_in<T>(body: impl FnOnce() -> T) -> (Measured, T) {
    // Force the TLS to initialize *before* the window opens.
    ARMED.with(|armed| armed.set(false));
    let before_allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let before_bytes = BYTES.load(Ordering::Relaxed);
    ARMED.with(|armed| armed.set(true));
    let result = body();
    ARMED.with(|armed| armed.set(false));
    let measured = Measured {
        allocations: ALLOCATIONS.load(Ordering::Relaxed) - before_allocations,
        bytes: BYTES.load(Ordering::Relaxed) - before_bytes,
    };
    (measured, result)
}

// ---------------------------------------------------------------------------
// The scenario
// ---------------------------------------------------------------------------

/// The synthetic atoms appended to reach the **large** shared-table size. The booted runtime's
/// own table (a few thousand atoms, the whole preloaded set) is the small one.
const LARGE_PAD: usize = 6000;

/// How far the large measurement's allocation count may exceed the small one's. Both calls run
/// the identical workload, so the only legitimate difference is whether the single appended
/// name happened to make the table's `Vec`/`HashMap` grow at that size — at most two
/// allocation events. A per-call table clone would be thousands, so this bound is the pin.
const ALLOCATION_SLACK: usize = 8;

/// The absolute allocation count one interning `binary_to_term/1` call may cost, whatever the
/// table size: the input byte copy, the codec's one-name `fresh` bookkeeping and the two
/// `String`s (plus possibly one `Vec`/`HashMap` growth) the append itself needs. Measured
/// below; the bound leaves room for a differently-sized payload without admitting a table
/// clone (a clone of the *smallest* table here is thousands of allocations).
const HOT_ALLOCATION_BUDGET: usize = 32;

/// Builds a runtime and runs its **boot sequence** (the preloaded set, resolved natively from
/// the filesystem). A missing installation is a **loud skip**, never a silent pass.
fn booted_runtime() -> Option<Runtime> {
    let mut rt = match Runtime::new(RuntimeConfig::default()) {
        Ok(rt) => rt,
        Err(error) => {
            skip_loudly(&format!("the runtime could not start: {error}"));
            return None;
        }
    };
    if let Err(error) = rt.boot() {
        skip_loudly(&format!("the boot sequence failed: {error}"));
        return None;
    }
    Some(rt)
}

/// A loud skip: never a silent pass.
fn skip_loudly(why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt atom-commit-allocation SKIPPED ===============");
    eprintln!("{why}: the shared AtomTable's commit behaviour cannot be measured.");
    eprintln!("Install Erlang/OTP (or fix the runtime) and re-run. NOT a pass.");
    eprintln!("=====================================================================");
    eprintln!();
}

/// The ETF image of a bare UTF-8 atom `name` (`131`, `SMALL_ATOM_UTF8_EXT`, length, bytes) —
/// short enough for the on-heap binary form (`erlang:binary_to_term/1`'s only M1 input shape).
fn atom_image(name: &str) -> Vec<u8> {
    assert!(
        name.len() <= 60,
        "keep the image inside the 64-byte on-heap limit"
    );
    let mut image = vec![131u8, 119u8, name.len() as u8];
    image.extend_from_slice(name.as_bytes());
    image
}

/// A **novel** atom name for `label` at `slot`. Both labels are printed at one length, so the
/// two measured images are the same size and their decodes cost the same transients.
fn novel_name(label: &str, slot: u32) -> String {
    format!("commit_pin_{label}_{slot:04}")
}

/// One table size's measurement: the interning call, the clone control, and the size itself.
#[derive(Debug, Clone, Copy)]
struct Case {
    /// The shared table's length when the case started — before this case's own two novel names
    /// (the warm-up's and the measured call's) were appended.
    table_atoms: usize,
    /// The interning `binary_to_term/1` call.
    hot: Measured,
    /// An actual `AtomTable::clone()` of the same table — the work the removed per-call
    /// `Rc::make_mut` commit performed, and the positive control this measurement needs.
    control: Measured,
}

impl Case {
    fn line(&self, label: &str) -> String {
        format!(
            "{label}: {} atoms | interning call {} allocations / {} bytes | \
             AtomTable::clone control {} allocations / {} bytes",
            self.table_atoms,
            self.hot.allocations,
            self.hot.bytes,
            self.control.allocations,
            self.control.bytes,
        )
    }
}

/// Measures one table size: append `pad` synthetic atoms, then warm up and measure the
/// interning call, then the clone control.
fn measure(rt: &mut Runtime, label: &str, pad: usize) -> Case {
    // The pad is appended through the ordinary public seeding route, so the novel `binary_to_term`
    // names that follow are the only interning the measured calls do.
    for n in 0..pad {
        rt.intern(&format!("commit_pad_{label}_{n}"));
    }
    let table_atoms = rt.atoms().len();

    let mut ctx = RuntimeContext::new(rt);
    // The warm-up: the same shape with a different novel name, so the memo `beam_bifs` keeps of
    // the caller's table is extended to this length *outside* the measured window, and the code
    // paths the measured call takes are all warm.
    let warm_image = atom_image(&novel_name(label, 0));
    let warm = terms::make_heap_binary(&mut ctx, &warm_image).expect("a heap binary");
    ctx.call_bif("erlang", "binary_to_term", 1, &[warm])
        .expect("the warm-up decode interns its novel atom");

    let measured_image = atom_image(&novel_name(label, 1));
    let image = terms::make_heap_binary(&mut ctx, &measured_image).expect("a heap binary");
    let (hot, decoded) = allocations_in(|| ctx.call_bif("erlang", "binary_to_term", 1, &[image]));
    let decoded = decoded.expect("the measured decode interns its novel atom");
    drop(ctx);
    // The interning call really did intern: the name is in the shared table now, and the atom
    // term names it. (Semantics are pinned in full by the test below; this keeps the measurement
    // honest — a *cached* decode would allocate nothing and prove nothing.)
    assert_eq!(
        rt.atom_name_of(decoded),
        Some(novel_name(label, 1).as_str()),
        "the measured call's novel name is interned in the shared table"
    );
    let control = allocations_in(|| black_box(rt.atoms().clone())).0;
    Case {
        table_atoms,
        hot,
        control,
    }
}

#[test]
fn an_interning_bif_call_does_not_allocate_in_proportion_to_the_shared_atom_table() {
    let Some(mut rt) = booted_runtime() else {
        return; // loud skip already printed
    };
    let small = measure(&mut rt, "small", 0);
    let large = measure(&mut rt, "large", LARGE_PAD);
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    println!("{}", small.line("small"));
    println!("{}", large.line("large"));
    println!("host load: {}", load.trim());

    // The two sizes really are far apart, and the control really scales with them: without that
    // the "no scaling" assertion below would be vacuous.
    assert!(
        large.table_atoms >= small.table_atoms + LARGE_PAD,
        "the large measurement must run against the padded table: {} vs {}",
        small.table_atoms,
        large.table_atoms
    );
    assert!(
        large.table_atoms > small.table_atoms * 2,
        "the two tables must differ by more than a factor of two: {} vs {}",
        small.table_atoms,
        large.table_atoms
    );
    assert!(
        large.control.allocations > small.control.allocations * 2,
        "the clone control must scale with the shared AtomTable (a clone of {} atoms cost {} \
         allocations, of {} atoms {}): {}",
        small.table_atoms,
        small.control.allocations,
        large.table_atoms,
        large.control.allocations,
        large.line("large")
    );

    // (1) THE PIN: the interning call's allocation count is independent of the table's size.
    assert!(
        large.hot.allocations <= small.hot.allocations + ALLOCATION_SLACK,
        "an interning BIF call must not allocate in proportion to the shared AtomTable \
         ({} atoms cost {} allocations, {} atoms {}): {}",
        small.table_atoms,
        small.hot.allocations,
        large.table_atoms,
        large.hot.allocations,
        large.line("large")
    );
    assert!(
        large.hot.bytes <= small.hot.bytes + HOT_BYTE_SLACK,
        "an interning BIF call's allocated bytes must not scale with the shared AtomTable \
         ({} atoms requested {} bytes, {} atoms {}): {}",
        small.table_atoms,
        small.hot.bytes,
        large.table_atoms,
        large.hot.bytes,
        large.line("large")
    );

    // (2) The absolute count stays inside the payload-intrinsic budget...
    for case in [small, large] {
        assert!(
            case.hot.allocations <= HOT_ALLOCATION_BUDGET,
            "the interning call over {} atoms allocated {} times, past the payload-intrinsic \
             budget of {HOT_ALLOCATION_BUDGET}: {}",
            case.table_atoms,
            case.hot.allocations,
            case.line("case")
        );
    }

    // (3) ...and the removed per-call table clone really is an order of magnitude dearer.
    assert!(
        large.hot.allocations * 10 < large.control.allocations,
        "the interning path ({} allocations over {} atoms) is not an order of magnitude below \
         the per-call table clone ({}): {}",
        large.hot.allocations,
        large.table_atoms,
        large.control.allocations,
        large.line("large")
    );
}

/// The byte half of the "no scaling" pin. A `Vec`/`HashMap` growth triggered by the one appended
/// name is small next to the table clone the pin exists to catch (which is megabytes here), so a
/// generous fixed slack is enough to keep the assertion meaningful and load-independent.
const HOT_BYTE_SLACK: usize = 1 << 20;

/// **Semantics**: the append-only commit lands the novel name at exactly the index the BIF built
/// into its term, and appends exactly one name — the property `PendingInterner`'s predicted index
/// depends on.
#[test]
fn an_interning_bif_call_appends_one_name_at_the_predicted_index() {
    let Some(mut rt) = booted_runtime() else {
        return; // loud skip already printed
    };
    let name = "commit_pin_semantics";
    assert!(rt.atoms().lookup(name).is_none(), "the name starts unknown");
    let before = rt.atoms().len();

    let mut ctx = RuntimeContext::new(&mut rt);
    let image = atom_image(name);
    let binary = terms::make_heap_binary(&mut ctx, &image).expect("a heap binary");
    let atom = ctx
        .call_bif("erlang", "binary_to_term", 1, &[binary])
        .expect("binary_to_term/1 interns the novel name");
    // The call is over: the one shared table (not a copy) holds the name.
    drop(ctx);
    assert_eq!(rt.atoms().len(), before + 1, "exactly one name is appended");
    assert_eq!(
        rt.atom_name_of(atom),
        Some(name),
        "the atom the BIF built resolves through the shared table"
    );

    // A second, identical image is a *cached* decode (the name is known now): nothing is
    // appended and the same index comes back.
    let mut ctx = RuntimeContext::new(&mut rt);
    let again = terms::make_heap_binary(&mut ctx, &image).expect("a heap binary");
    let repeat = ctx
        .call_bif("erlang", "binary_to_term", 1, &[again])
        .expect("binary_to_term/1 of the known name");
    drop(ctx);
    assert_eq!(
        repeat, atom,
        "a known name keeps its index instead of appending a second one"
    );
    assert_eq!(rt.atoms().len(), before + 1, "and the table did not grow");
}
