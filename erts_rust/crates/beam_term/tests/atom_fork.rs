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
//! `AtomTable::fork` — the handle-sharing whole-table copy, through the **public** API only.
//!
//! The runtime's per-module load takes a private copy of the whole live atom table (so the
//! lowering step may append a literal-only atom without touching the shared one) and then merges
//! the appended tail back by index. [`AtomTable::clone`] serves that need but is deliberately a
//! **deep** copy — one fresh string allocation per atom — which made it the largest single
//! removable cost of a `beam_rt` startup. [`AtomTable::fork`] answers the same need from shared
//! name handles, so nothing about indices or appends changes; only the copying stops.
//!
//! What this file pins, all through the general API:
//!
//! 1. **Index preservation** — every index of a fork names exactly the name the parent has, and
//!    `lookup` maps it back to the same `Atom` (so a fork is a drop-in for a clone).
//! 2. **Shared storage, not copied storage** — the address of each shared name's `str` data is
//!    identical in the fork and the parent (an `Arc<str>` deref hands out the same heap buffer),
//!    while a clone's names are distinct allocations. Allocation counts are pinned in
//!    `tests/intern_alloc.rs`; this is the identity half.
//! 3. **Independent append streams** — interning into either side leaves the other's `len`,
//!    `generation`, `snapshot` and contents untouched, and a name interned after the fork lands
//!    in exactly one table.
//! 4. **The witness stays sound** — `appended_since` is `false` in both directions between a fork
//!    and its parent (they may diverge), and stays `true` against each one's own snapshot.
//! 5. **Degenerate shapes** — an empty table, a table whose names were all re-looked-up, and a
//!    fork of a fork.
use std::ptr;
use beam_term::{Atom, AtomTable};
/// A realistically shaped table: an `atom.names`-style prefix (a quoted printname, a `CName=`
/// alias, an empty printname) plus synthetic names with varying lengths.
fn table_with(n: usize) -> AtomTable {
    let mut t = AtomTable::new();
    t.seed_from_atom_names(
        "# fork fixture\n\natom false true\natom Underscore='_'\natom Empty=''\natom nonode@nohost\n",
    )
    .expect("valid atom.names source");
    // The fixture source interns five names; every caller asks for at least that many.
    assert!(n >= 5, "the `atom.names` fixture alone interns five names");
    let rest: Vec<String> = (t.len()..n).map(|i| format!("fork_atom_{i:05}")).collect();
    t.seed(rest.iter().map(String::as_str));
    assert_eq!(t.len(), n);
    t
}
/// Every index of `fork` resolves to the same name as `parent`'s — by content **and** by address,
/// which is what proves the name storage is shared rather than copied.
fn assert_every_index_shared(parent: &AtomTable, fork: &AtomTable) {
    assert_eq!(fork.len(), parent.len());
    assert_eq!(fork.generation(), parent.generation());
    for index in 0..parent.len() {
        let atom = Atom::from_raw(index as u32);
        let parent_name = parent.name(atom).expect("in range");
        let fork_name = fork.name(atom).expect("in range");
        assert_eq!(
            fork_name, parent_name,
            "index {index} must resolve to the same name in a fork"
        );
        assert!(
            ptr::eq(parent_name, fork_name),
            "index {index}: a fork shares the name allocation, it does not copy the string"
        );
        assert_eq!(
            fork.lookup(parent_name),
            Some(atom),
            "a fork must look a shared name up at the same index"
        );
    }
}
#[test]
fn a_fork_preserves_every_index_and_shares_every_name_allocation() {
    let parent = table_with(3_000);
    let fork = parent.fork();
    assert_every_index_shared(&parent, &fork);
    // the alias in the fixture is not interned, only its printname — in both tables
    assert_eq!(parent.lookup("Underscore"), None);
    assert_eq!(fork.lookup("Underscore"), None);
    assert_eq!(fork.lookup("_"), Some(Atom::from_raw(2)));
    // a name nobody interned is in neither table, at no index
    assert_eq!(fork.lookup("fork_atom_99999"), None);
    assert_eq!(fork.name(Atom::from_raw(3_000)), None);
    // out-of-range reads agree too (the shared `names` is the same length)
    for raw in [3_000u32, 4_096, u32::MAX] {
        assert_eq!(
            fork.name(Atom::from_raw(raw)),
            parent.name(Atom::from_raw(raw))
        );
    }
}
#[test]
fn a_clone_does_copy_the_name_allocations() {
    // The sensitivity control for the test above: `clone` is deliberately a deep copy, so its
    // name addresses differ. If this ever stopped holding, `clone` would have lost the property
    // `beam_rt/tests/atom_table_commit_alloc.rs` and `beam_bifs/tests/decode_hot_path.rs` use it
    // as a positive control for.
    let parent = table_with(64);
    let clone = parent.clone();
    for index in 0..parent.len() {
        let atom = Atom::from_raw(index as u32);
        assert_eq!(clone.name(atom), parent.name(atom));
        assert!(
            !ptr::eq(
                clone.name(atom).expect("in range"),
                parent.name(atom).expect("in range")
            ),
            "index {index}: a clone must own a fresh name string"
        );
    }
}
#[test]
fn interning_into_either_side_leaves_the_other_untouched() {
    let mut parent = table_with(8);
    let parent_len = parent.len();
    let parent_generation = parent.generation();
    let parent_snapshot = parent.snapshot();
    let mut fork = parent.fork();

    // --- append to the fork: nothing the parent can observe moves -------------------------------
    let forked = fork.intern("only_in_the_fork");
    assert_eq!(
        forked.raw() as usize,
        parent_len,
        "an append lands at the same index a clone's append would have taken"
    );
    assert_eq!(fork.len(), parent_len + 1);
    assert_eq!(fork.generation(), parent_generation + 1);
    assert_eq!(parent.len(), parent_len);
    assert_eq!(parent.generation(), parent_generation);
    assert_eq!(parent.snapshot(), parent_snapshot);
    assert_eq!(parent.lookup("only_in_the_fork"), None);
    assert_eq!(parent.name(forked), None);
    // the fork is still a prefix-of-the-parent extension: every shared index is unchanged
    for index in 0..parent_len {
        let atom = Atom::from_raw(index as u32);
        assert_eq!(fork.name(atom), parent.name(atom));
        assert!(ptr::eq(
            fork.name(atom).expect("in range"),
            parent.name(atom).expect("in range")
        ));
    }

    // --- and the mirror direction: a name interned into the parent after the fork is invisible
    // to the fork. Both land at the same index; they are different names, different tables. ---
    let parent_atom = parent.intern("only_in_the_parent");
    assert_eq!(parent_atom.raw() as usize, parent_len);
    assert_eq!(parent.name(parent_atom), Some("only_in_the_parent"));
    assert_eq!(
        fork.name(parent_atom),
        Some("only_in_the_fork"),
        "same index, different name: a fork separates by independent appends"
    );
    assert_eq!(fork.lookup("only_in_the_parent"), None);
    assert_eq!(parent.lookup("only_in_the_fork"), None);
    assert_eq!(fork.len(), parent.len());
}
#[test]
fn the_witness_is_false_in_both_directions_between_a_fork_and_its_parent() {
    let mut parent = table_with(8);
    let parent_snapshot = parent.snapshot();
    let mut fork = parent.fork();
    let fork_snapshot = fork.snapshot();
    // identical content and generation, different identity
    assert_eq!(parent_snapshot.len(), fork_snapshot.len());
    assert_eq!(parent_snapshot.generation(), fork_snapshot.generation());
    assert_ne!(parent_snapshot, fork_snapshot);
    assert!(!fork.appended_since(parent_snapshot));
    assert!(!parent.appended_since(fork_snapshot));
    // each validates against its own snapshot, before and after independent appends
    assert!(parent.appended_since(parent_snapshot));
    assert!(fork.appended_since(fork_snapshot));
    parent.intern("post_fork_parent");
    fork.intern("post_fork_fork");
    assert!(parent.appended_since(parent_snapshot));
    assert!(fork.appended_since(fork_snapshot));
    assert!(!fork.appended_since(parent_snapshot));
    assert!(!parent.appended_since(fork_snapshot));
}
#[test]
fn a_fork_of_an_empty_table_and_of_a_fork() {
    // --- empty ---------------------------------------------------------------------------------
    let parent = AtomTable::new();
    let fork = parent.fork();
    assert!(fork.is_empty());
    assert_eq!(fork.len(), 0);
    assert_eq!(fork.generation(), 0);
    assert_eq!(fork.lookup("anything"), None);
    assert!(!fork.appended_since(parent.snapshot()));
    assert!(!parent.appended_since(fork.snapshot()));
    let mut parent = parent;
    let mut fork = fork;
    assert_eq!(parent.intern("a").raw(), 0);
    assert_eq!(fork.intern("b").raw(), 0);
    assert_eq!(parent.name(Atom::from_raw(0)), Some("a"));
    assert_eq!(fork.name(Atom::from_raw(0)), Some("b"));

    // --- a table whose names were all looked up again (no re-intern), then forked -------------
    let mut reused = AtomTable::new();
    reused.seed(["one", "two", "three"]);
    let known: Vec<String> = (0..3u32)
        .map(|i| reused.name(Atom::from_raw(i)).expect("in range").to_string())
        .collect();
    for name in &known {
        reused.intern(name);
    }
    assert_eq!(reused.len(), 3, "a re-intern is a no-op");
    let fork = reused.fork();
    assert_every_index_shared(&reused, &fork);

    // --- a fork of a fork ---------------------------------------------------------------------
    let parent = table_with(128);
    let fork = parent.fork();
    let grand = fork.fork();
    assert_every_index_shared(&parent, &grand);
    assert_every_index_shared(&fork, &grand);
    // three live tables, three distinct identities
    assert_ne!(parent.snapshot(), fork.snapshot());
    assert_ne!(fork.snapshot(), grand.snapshot());
    assert_ne!(parent.snapshot(), grand.snapshot());
    assert!(!grand.appended_since(parent.snapshot()));
    assert!(!grand.appended_since(fork.snapshot()));
    // appending to the grandfork is invisible to both ancestors
    let mut grand = grand;
    grand.intern("only_in_the_grandfork");
    assert_eq!(parent.len(), 128);
    assert_eq!(fork.len(), 128);
    assert_eq!(parent.lookup("only_in_the_grandfork"), None);
    assert_eq!(fork.lookup("only_in_the_grandfork"), None);
}
#[test]
fn a_fork_serves_the_lower_then_merge_back_protocol() {
    // The runtime's per-module shape, end to end: lower against a private table that appends
    // literal-only atoms at exactly `shared.len()..`, then merge that tail back into the shared
    // table in index order. A fork must be a drop-in for the clone the protocol was built on.
    let mut shared = table_with(32);
    let base = shared.len();
    let mut lowered = shared.fork();
    // a literal-only atom, and a re-intern of one the prefix already has (a no-op)
    let literal_only = lowered.intern("fork_only_in_a_literal");
    let known = lowered.intern("fork_atom_00031");
    assert_eq!(literal_only.raw() as usize, base);
    assert_eq!(known.raw() as usize, base - 1);
    assert_eq!(lowered.len(), base + 1);
    assert_eq!(shared.len(), base, "lowering did not touch the shared table");
    for index in base..lowered.len() {
        let name = lowered
            .name(Atom::from_raw(index as u32))
            .expect("in range")
            .to_string();
        let merged = shared.intern(&name);
        assert_eq!(
            merged.raw() as usize,
            index,
            "the appended tail merges back at the index the module was lowered against"
        );
    }
    assert_eq!(shared.name(literal_only), Some("fork_only_in_a_literal"));
    assert_eq!(shared.len(), lowered.len());
    // the two tables agree on every index of the merged prefix
    assert_eq!(shared.generation(), lowered.generation());
    for index in 0..shared.len() {
        let atom = Atom::from_raw(index as u32);
        assert_eq!(shared.name(atom), lowered.name(atom));
    }
}
