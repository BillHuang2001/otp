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

//! Guard-scoped views over a [`Table`] — the *safe* way to consume table-owned
//! terms, and the fix for the aliasing hazard the crate's `CONTEXT.md` records
//! under *Known Issues*.
//!
//! # The hazard
//!
//! The A.1 copy boundary is on the **insert** side only: an inserted object is
//! deep-copied into the table's [`TermStore`](crate::store::TermStore) arena, but
//! a *read* hands back `Eterm`s that **alias those table-owned words**. The arena
//! is [never relocated](crate::store::TermStore) but its words are reused: a
//! `delete` returns an object's runs to the store's per-size free lists, and a
//! later `insert` of an object of the same word size gets handed *exactly those
//! words* back. So a term read under one guard, then used after the guard is
//! dropped, silently re-reads as a **different, well-formed object** — a wrong
//! result, never a loud error (workspace `CONTEXT.md` §3 forbids exactly that).
//!
//! # The fix: a lifetime-tagged term
//!
//! [`TableRead`] and [`TableWrite`] own the `RwLock` guard and are the *only*
//! source of [`TableTerm`], a term view whose lifetime `'g` is tied to the
//! **borrow of the view itself** — not to the guard's own `RwLockReadGuard<'a, _>`
//! lifetime. That distinction is load-bearing:
//!
//! * the guard's type lifetime `'a` is the `&self` borrow of the
//!   [`SharedTable`](crate::table::SharedTable), which **outlives** the
//!   `with_read` call — tagging to `'a` would let a closure return a term that
//!   outlives the guard (the original bug, just moved);
//! * tagging to the view borrow instead means a term can only be produced while
//!   the view is borrowed, the view owns the guard, and the view therefore cannot
//!   be dropped (nor can it end) while a term is still live. A term that escaped
//!   its closure or its `read()` scope is a **compile error** — see the
//!   `compile_fail` examples on [`TableTerm`] and
//!   [`SharedTable::with_read`](crate::table::SharedTable::with_read).
//!
//! The default read path allocates **nothing extra**: a [`TableTerm`] is one
//! `Eterm` plus a zero-sized lifetime marker.
//!
//! # Explicit copy-out
//!
//! A caller that genuinely needs an *owned* term — one that may outlive the guard
//! — asks for it: [`TableTerm::copy_into`] deep-copies the term into a
//! caller-owned [`TermStore`](crate::store::TermStore) through the very same
//! deep-copier an `insert` uses (so there is no capability regression: anything a
//! table can store, a copy-out can reproduce). The select/match family — whose
//! results may alias table storage — is exposed only in that copy-out form;
//! [`TableRead::select`] and friends build into a view-owned scratch heap and then
//! deep-copy every result into the caller's store.
//!
//! # What is *not* fixed here
//!
//! The raw [`Table`] and [`TableAccess`](crate::table::TableAccess) APIs still
//! hand out bare `Eterm`s; making *them* safe is a cross-crate API decision (their
//! consumer is `beam_bifs`). See the crate `CONTEXT.md`.

mod iter;
mod read;
mod term;
mod write;

pub use read::TableRead;
pub use term::TableTerm;
pub use write::TableWrite;

use beam_common::{AllocKind, BeamError};

/// The size (in words) the read view's scratch heap starts at. Sized so the
/// common `select`/`match` over a small table never triggers a regrow.
const SCRATCH_MIN_WORDS: usize = 1024;

/// The largest scratch heap the read view will grow to before it reports the
/// heap-exhaustion error instead of retrying. Bounds the retry loop.
const SCRATCH_MAX_WORDS: usize = 1 << 20;

/// Whether `err` is a **scratch-heap exhaustion** — the one failure the read
/// view's select family retries with a larger heap. Every other error is
/// reported untouched (a typed capability gap is never masked; workspace §3).
fn is_heap_exhaustion(err: &BeamError) -> bool {
    matches!(err, BeamError::Alloc(failure) if failure.kind == AllocKind::Heap)
}

#[cfg(test)]
mod tests;
