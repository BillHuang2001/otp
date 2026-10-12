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

//! The conservative **reachability walk** behind the unscanned-root valve.
//!
//! [`super::gc::root_is_unscanned`] classifies a *single* word: it answers `true` when that
//! word names a container in a region the process's collector does not scan. The valve that
//! guards every collection site must ask the stronger question a **nested** graph asks — is an
//! unscanned container reachable from a *scanned* one? The shape that defeats the per-word test
//! is exactly one level of indirection deep: a young-heap list whose element is a container
//! built on the runtime's pooled BIF heap (`Runtime::take_bif_heap`), whose element is a
//! pointer into the process's nursery. The root's own top-level address is scanned, so the
//! per-word test calls it safe — yet relocating the nursery leaves the BIF-heap container's
//! child pointing into the evacuated from-space, and the next dereference of it reads a dead
//! word.
//!
//! # The walk
//!
//! [`any_root_is_unscanned`] answers that question with an **iterative worklist** walk:
//!
//! * it seeds from the machine roots the caller is about to publish (the engine's x registers,
//!   stack and FR file) plus every container `Process::collect` gathers — the registers, the
//!   mailbox, the dictionary, the saved arguments and the pending-signal queue;
//! * it descends **only** into objects that live in the scanned region (a term the per-word
//!   test calls safe, plus the existing "the object is a container" test), so a word no copier
//!   would follow is never dereferenced as an object header;
//! * it reports a hit as soon as any reachable child word is a container in a region the
//!   collector does not scan, and otherwise runs to exhaustion.
//!
//! A `visited` set of object addresses makes a shared or cyclic graph terminate: each object
//! is expanded at most once, and the walk is therefore `O(distinct live objects)` — the same
//! order as the collection it is deciding, which is the only time it runs.
//!
//! # Why no boxed kind can be missed
//!
//! The per-object child enumeration is the canonical
//! [`beam_term::layout::for_each_child_slot`] — the same model the workspace's own copier uses
//! (`beam_message::copy`) — so the walk can never disagree with the collector about which
//! payload words of an object are references. Two shapes need no descent at all and are
//! deliberately skipped:
//!
//! * an **immediate** (a small integer, an atom, `NIL`, an internal pid/port id) has no
//!   children;
//! * a **literal-area pointer** ([`beam_term::literal::is_literal`]) is a frozen code constant
//!   whose words reference the literal area alone, never a runtime heap region, so it can
//!   neither dangle itself nor lead to a word that does. Skipping it is also what keeps a
//!   *nested* pointer inside a literal (which carries no literal bit of its own) from being
//!   mis-read as an unscanned heap container.
//!
//! # Why this module is `unsafe`
//!
//! The walk must read an object's header word and its payload words at raw addresses; that is
//! the one operation `beam_term` does not expose through a safe API (its own safe entry point,
//! [`beam_term::size::size_object`], reports only a word *count*, never the child words a
//! classifier needs). The crate is `#![deny(unsafe_code)]`, so this module carries the
//! documentedly narrow `#![allow(unsafe_code)]` the workspace's `unsafe` policy sanctions for
//! the term-heap/GC category — `src/nif.rs` and the `gc.rs` unit tests take the same targeted
//! exception — and every raw access is confined to the single `expand` routine below, under a
//! `// SAFETY:` rationale naming the invariant that makes it sound: a word reaches the
//! worklist only through `seed`, which admits exactly the boxed/cons words addressed inside
//! the region the process's collector scans, i.e. live objects of the running process.

#![allow(unsafe_code)]

use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};

use beam_process::Process;
use beam_term::layout::for_each_child_slot;
use beam_term::list::ptr_val;
use beam_term::literal::is_literal;
use beam_term::prelude::{is_boxed, is_list};
use beam_term::Eterm;

use super::gc::root_is_unscanned;

/// Whether a collection of `process` would be **unsound** — whether a container in a region
/// the collector does not scan is reachable from one it does.
///
/// `machine_roots` are the words the caller is about to publish (the engine's x registers, the
/// whole Erlang stack and the whole FR file); the process's **own** root-set containers are
/// always included, because [`Process::collect`] gathers them on every collection. Passing an
/// over-approximation of the roots is always allowed: a spurious hit only **declines** a
/// collection that might have been sound (the allocation then falls back to the pointer-stable
/// arena), never allowing one that is not.
pub(crate) fn any_root_is_unscanned<'a, I>(process: &Process, machine_roots: I) -> bool
where
    I: IntoIterator<Item = &'a Eterm>,
{
    let mut walk = Walk::default();
    for &word in machine_roots {
        walk.seed(process, word);
        if walk.hit {
            return true;
        }
    }
    walk.seed_process_roots(process);
    if walk.hit {
        return true;
    }
    walk.run(process)
}

/// The [`Process`]-only form of [`any_root_is_unscanned`], for a collection site whose machine
/// roots are **already published** into the process's own root set — the runner's park arm,
/// where `publish_parked_roots` runs immediately before
/// [`Runtime::serve_gc_request`](crate::runtime::Runtime).
pub(crate) fn process_roots_are_unscanned(process: &Process) -> bool {
    any_root_is_unscanned(process, std::iter::empty::<&Eterm>())
}

/// The walk's mutable state: the object addresses already entered, the objects still to
/// expand, and whether an unscanned container has been reached.
#[derive(Default)]
struct Walk {
    visited: AddressSet,
    pending: Vec<Eterm>,
    hit: bool,
}

/// The `visited` set: a `HashSet` of object addresses keyed by [`AddrHasher`].
///
/// The key is a machine word that is already well distributed (a word-aligned term address), so
/// the walk needs no cryptographic mixing — and the walk runs on **every** collection, so the
/// per-key cost is the walk's dominant term. `AddrHasher` is a single multiply, which keeps the
/// walk's own overhead well under the copy it is deciding.
type AddressSet = HashSet<usize, BuildHasherDefault<AddrHasher>>;

/// The walk's key mixer: a multiplicative hash, no cryptographic mixing.
///
/// Nothing here is a security boundary — the keys are term addresses this runtime allocated
/// itself — and a collision only costs one extra probe, never a wrong answer.
#[derive(Default)]
struct AddrHasher(u64);

impl Hasher for AddrHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        // The generic path: `Hash for usize` may delegate here rather than to `write_usize`.
        // Feeding the bytes into the same word-wise accumulator keeps the mixer honest
        // whatever the standard library does, so a collision is the only possible cost.
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }

    fn write_usize(&mut self, value: usize) {
        self.0 = (value as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}
impl Walk {
    /// Classifies one root-set word: records a hit for a container in an unscanned region,
    /// enqueues a container in the scanned region, and ignores everything else.
    fn seed(&mut self, process: &Process, word: Eterm) {
        if self.hit {
            return;
        }
        // An immediate (a small integer, an atom, `NIL`, an internal pid/port id) has no
        // children: neither this walk nor the collector descends into it.
        if !is_boxed(word) && !is_list(word) {
            return;
        }
        // A literal-area pointer is a frozen code constant; see the module docs.
        if is_literal(word) {
            return;
        }
        // The per-word classifier: `true` for a container in a region the collector does not
        // scan, so the collection must be declined. Reused verbatim (never re-derived).
        if root_is_unscanned(process, word) {
            self.hit = true;
            return;
        }
        // A container the collector scans: descend into it (once — the visited set is what
        // makes a shared or cyclic graph terminate).
        if self.visited.insert(ptr_val(word) as usize) {
            self.pending.push(word);
        }
    }

    /// Seeds from every container [`Process::collect`] gathers: the registers (`x[0..live)`,
    /// the whole stack and the host-published extra roots), then each queued message's
    /// `payload`/`from`/`token`, the process dictionary, the saved arguments and the pending
    /// signals.
    fn seed_process_roots(&mut self, process: &Process) {
        let mut roots: Vec<Eterm> = Vec::new();
        process.registers().gather_roots(&mut roots);
        for message in process.mailbox().iter() {
            roots.push(message.payload());
            roots.push(message.from());
            roots.push(message.token());
        }
        for (key, value) in process.dict().entries() {
            roots.push(key);
            roots.push(value);
        }
        process.saved_args().gather_roots(&mut roots);
        process.signals().gather_roots(&mut roots);
        for word in roots {
            self.seed(process, word);
        }
    }

    /// Expands every enqueued object, answering whether a hit was reached.
    fn run(&mut self, process: &Process) -> bool {
        while let Some(word) = self.pending.pop() {
            self.expand(process, word);
            if self.hit {
                return true;
            }
        }
        false
    }

    /// Classifies every child of the object `word` names.
    fn expand(&mut self, process: &Process, word: Eterm) {
        let address: *const Eterm = ptr_val(word);
        // SAFETY: `word` reaches the worklist only through `seed`, which enqueues exactly the
        // boxed/cons words that are (a) genuine terms and (b) addressed inside a region the
        // process's collector scans — i.e. live objects of the running process. `address`
        // therefore heads a live object, so its header word and every payload word the shape
        // implies are live and readable (the same invariant `beam_term`'s own materialization
        // entry points document).
        unsafe {
            if is_list(word) {
                // A cons cell is header-less: both of its words are references.
                self.seed(process, *address);
                self.seed(process, *address.add(1));
                return;
            }
            let header = *address;
            // SAFETY: `address` heads the live object `header` names, so the canonical child
            // enumeration visits exactly that shape's term-reference slots (`for_each_child_slot`
            // yields offsets relative to the header word, and every one is in bounds and live).
            // This is the same enumeration `beam_message::copy` drives, so the walk cannot
            // diverge from the collector's own view of the object.
            for_each_child_slot(header, address, |offset| {
                self.seed(process, *address.add(offset));
            });
        }
    }
}
