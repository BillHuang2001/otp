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

//! Atom indices and the atom interning table.
//!
//! Ground truth for the ordering rule: `erts/emulator/utils/make_tables`, which
//! generates `#define am_<alias> make_atom($i)` with `$i` the **0-based file
//! order** of the name in `erts/emulator/beam/atom.names`. Reproducing that order
//! is required for preloaded modules, which reference atoms positionally.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::error::{TermError, TermResult};

/// The table's two storage directions — [`store::NameChunks`] (index → name, chunked) and
/// [`store::IndexShards`] (name → index, hash-sharded), each copy-on-write behind one `Arc` so
/// that [`AtomTable::fork`] shares instead of copies. Split out because the copy granularity is a
/// design of its own; see the module docs.
pub mod store;

/// `_TAG_IMMED2_SIZE` — the number of low bits used by the atom tag.
pub const ATOM_INDEX_BITS: u32 = 6;

/// The representation ceiling for an atom index (`MAX_ATOM_INDEX` in
/// `erl_term.h`: `2^58 - 1`). The practical table is capped far below this
/// (default `+t` limit 1,048,576).
pub const MAX_ATOM_INDEX: usize = (1usize << 58) - 1;

/// An atom index.
///
/// `Atom` is a compact `u32` even though the *encoding* allows 58 bits: the
/// practical table is capped far below `2^32`, and atoms are only ever produced
/// through [`crate::make_atom`], so the encoding invariant always holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Atom(u32);

impl Atom {
    /// The largest representable atom index (`u32::MAX`), the practical ceiling.
    pub const MAX: Atom = Atom(u32::MAX);

    /// The raw index.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Wrap a raw index. The caller must have obtained it from an [`AtomTable`].
    #[inline]
    pub const fn from_raw(raw: u32) -> Atom {
        Atom(raw)
    }
}

/// Deterministic atom interning table.
///
/// Indices are assigned in insertion order starting at 0, so seeding with the
/// names of `atom.names` in file order reproduces the standard `am_*` constants.
///
/// An [`AtomTable`] is **append-only**: names are added by [`AtomTable::intern`]
/// and never removed, renamed or reordered. That makes an atom index (and any
/// prefix of the table) stable for the table's lifetime, which is what lets a
/// memoized *image* of the table be validated in O(1) against
/// [`AtomSnapshot`] / [`AtomTable::appended_since`].
///
/// The two storage directions live in [`store`], each copy-on-write behind one
/// `Arc` ([`store::NameChunks`] for index → name, [`store::IndexShards`] for
/// name → index). That is what makes [`AtomTable::fork`] free: it shares both
/// directions instead of copying them, and a later append detaches only the one
/// chunk and the one shard it touches.
#[derive(Debug)]
pub struct AtomTable {
    /// Names in index order: `names[i]` names `Atom(i)`.
    ///
    /// The name is stored **once** as a shared, heap-allocated `Arc<str>`; `index` holds another
    /// handle to the same allocation, so interning a novel name costs exactly one string
    /// allocation. `Arc` (not `Rc`) keeps the table `Send + Sync`, and putting the chunk list
    /// behind one `Arc` lets [`AtomTable::fork`] share the whole prefix without copying a handle.
    names: store::NameChunks,
    /// Name → index, hash-sharded so that a copy-on-write step copies one shard rather than the
    /// whole map. Keyed by `Arc<str>`, which `Borrow<str>`s and hashes as `str`, so
    /// `index.get(name: &str)` is allocation-free and semantically identical to the previous
    /// `HashMap<String, Atom>`.
    index: store::IndexShards,
    /// A process-global monotonic identity for *this* table. Only distinctness is
    /// meaningful — never the number itself, the ordering, or the spacing. It is not
    /// observable behaviour and is not part of any compatibility contract: the only
    /// thing that reads it is identity equality against another table's `instance`
    /// inside [`AtomTable::appended_since`].
    instance: u64,
    /// The number of names appended by [`AtomTable::intern`] since construction.
    generation: u64,
}

/// A process-global monotonic identity source for [`AtomTable`]s.
///
/// Only *distinctness* of the handed-out values is meaningful — never the value itself,
/// its ordering, or the spacing between successive values. Two live `AtomTable`s get
/// different `instance`s (so neither can validate against the other's snapshot); a
/// table never reuses the `instance` of a dropped table. This is bookkeeping internal
/// to the crate: it is not observable behaviour and is not part of any compatibility
/// contract.
static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(0);

/// Hands out the next distinct table identity (see [`NEXT_INSTANCE`]).
fn next_instance() -> u64 {
    NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
}

impl Default for AtomTable {
    fn default() -> AtomTable {
        AtomTable::new()
    }
}

impl Clone for AtomTable {
    /// **Deep-copies** the entries (a fresh name string per table entry) and the generation, but
    /// assigns a **fresh** identity.
    ///
    /// Two independent reasons must hold together:
    ///
    /// * A clone is a *new* identity, not a copy of the original's: the two tables start
    ///   identical but may later diverge by independent appends, so a snapshot of either must
    ///   never validate against the other (see [`AtomTable::appended_since`]). This is exactly
    ///   what makes the witness sound under copy-on-write (`Rc::make_mut`): a borrowed shared
    ///   table that gets cloned into a privately-mutated one can never make a stale image look
    ///   valid. The cost is a *conservative* false negative right after a clone — the caller
    ///   rebuilds its memo instead of reusing it — which is the correct trade for soundness.
    /// * The clone **owns** its name storage. Each entry is materialized as a fresh
    ///   [`Arc<str>`] rather than sharing the source's, so a clone is genuinely independent and
    ///   a clone of an `N`-atom table allocates `Θ(N)` (one string per entry, plus the chunk/shard
    ///   containers it grows into). Sharing the source's `Arc`s would make the clone `Θ(1)` and
    ///   silently break `beam_rt/tests/atom_table_commit_alloc.rs` (and
    ///   `beam_bifs/tests/decode_hot_path.rs`), which use `AtomTable::clone()` as a positive
    ///   control that *must* scale with the table.
    fn clone(&self) -> AtomTable {
        // One entry per name, in index order: `intern` assigns `Atom(names.len())` as it appends,
        // so `names[i]` always names `Atom(i)` and the index can be rebuilt positionally — no
        // re-hashing and no lookup. Each entry is a fresh string; the containers are this
        // table's own (never the source's `Arc`-shared ones).
        self.debug_check();
        let mut names = store::NameChunks::new();
        let mut index = store::IndexShards::new();
        for position in 0..self.names.len() {
            let name = self
                .names
                .get(position)
                .expect("every index below `len` names a name");
            let owned: Arc<str> = Arc::from(name);
            index.insert(Arc::clone(&owned), Atom(position as u32));
            names.push(owned);
        }
        AtomTable {
            names,
            index,
            instance: next_instance(),
            generation: self.generation,
        }
    }
}

impl AtomTable {
    /// A second, independent table over the **same** names: every index resolves exactly as it
    /// does in `self`, but nothing is copied at all — the fork is two `Arc` clones.
    ///
    /// This is the constructor for a consumer that must be *allowed to append* (typically one
    /// literal-only atom while lowering a module) without paying a whole-table copy. Both storage
    /// directions are copy-on-write behind one `Arc` each ([`store::NameChunks`] for index → name,
    /// [`store::IndexShards`] for name → index), so:
    ///
    /// * **a fork allocates nothing**, at any table size — not a name string, not a name handle,
    ///   not a bucket. An `N`-atom fork is `Θ(1)` time and `Θ(1)` space beyond the two shared
    ///   containers, where [`AtomTable::clone`] is `Θ(N)` string allocations.
    /// * **the whole prefix stays shared**, and the two sides stay independent anyway: every
    ///   mutating path ([`store::NameChunks::push`], [`store::IndexShards::insert`]) goes through
    ///   `Arc::make_mut`, which *copies* a container instead of mutating one the other side can
    ///   see. So the first append after a fork re-allocates at most **one chunk**
    ///   (≤ [`store::NAMES_PER_CHUNK`] handles) and **one shard** (about
    ///   `len / store::INDEX_SHARDS` entries) — O(1) in the number of newly interned names, never
    ///   O(table).
    ///
    /// What is preserved, and why each half matters:
    ///
    /// * **Every index.** The chunked name storage is shared verbatim and the sharded index maps
    ///   the same handles to the same indices, so `fork.name(a) == self.name(a)` (the very same
    ///   `str` allocation) and `fork.lookup(n) == self.lookup(n)` for every atom of `self`.
    ///   Indices are the only thing downstream code can observe (`Atom` is a bare, table-relative
    ///   index), so this makes a fork a drop-in replacement for a clone in any "lower with a
    ///   private table, then merge the appended tail back" protocol.
    /// * **The generation**, so the fork's `generation - len` arithmetic means the same thing as
    ///   the parent's. A producer that only needs a *prefix-stable* table can therefore hand a
    ///   fork to a decoder whose memo was validated against the parent's last snapshot.
    /// * **A fresh identity** ([`AtomTable::appended_since`]'s `instance`). A fork is not an
    ///   extension of `self`: both sides may append independently, in different orders, so neither
    ///   one's snapshot may ever validate against the other. That holds in **both** directions —
    ///   `fork.appended_since(self.snapshot())` and `self.appended_since(fork.snapshot())` are
    ///   both `false` — which is what keeps the O(1) witness sound.
    ///
    /// The two tables are otherwise fully independent: appending to one leaves the other's
    /// `len`/`generation`/contents untouched, because the copy-on-write step detaches the appending
    /// side's containers rather than writing through the shared one. A name interned into the fork
    /// after the fork is therefore invisible to the parent and vice versa. A name that *both* sides
    /// intern afterwards lands at the same index (both start at the same length) but is a separate
    /// `Arc<str>` allocation — the fork shares only the names it was born with.
    ///
    /// [`AtomTable::clone`] deliberately keeps its deep-copy meaning; use this only where the
    /// copy's *independence of name storage* is not part of the contract. See the `Clone` impl's
    /// rustdoc for the two suites that pin `clone`'s scaling.
    pub fn fork(&self) -> AtomTable {
        self.debug_check();
        AtomTable {
            names: self.names.fork(),
            index: self.index.fork(),
            instance: next_instance(),
            generation: self.generation,
        }
    }

    /// Assert the table's structural invariant: exactly one index entry per interned name.
    ///
    /// `O(store::INDEX_SHARDS)` (one entry count per shard), so it is called only from the two
    /// whole-table constructors — never from `intern`, whose hot path must stay O(1).
    #[inline]
    fn debug_check(&self) {
        debug_assert!(
            self.index.entries() == self.names.len(),
            "the index holds exactly one entry per interned name"
        );
    }
}

/// An O(1) witness of an [`AtomTable`]'s state, for validating a memoized image
/// of the table in constant time. Obtained from [`AtomTable::snapshot`].
///
/// It carries only three scalars — the table's identity, its append generation and its
/// length — so [`AtomTable::appended_since`] never touches the names. It is `Copy`, and
/// both obtaining it and using it are allocation-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AtomSnapshot {
    instance: u64,
    generation: u64,
    len: usize,
}

impl AtomSnapshot {
    /// The table's length when the snapshot was taken.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the table was empty when the snapshot was taken.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The table's append generation when the snapshot was taken (the number of names
    /// it had interned at that point; see [`AtomTable::generation`]).
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

impl AtomTable {
    /// An empty table.
    ///
    /// Allocates nothing: both storage directions start as `None` and are materialized by the
    /// first [`AtomTable::intern`].
    pub fn new() -> AtomTable {
        AtomTable {
            names: store::NameChunks::new(),
            index: store::IndexShards::new(),
            instance: next_instance(),
            generation: 0,
        }
    }

    /// Intern `name`, returning its index.
    ///
    /// The first time a name is seen it is assigned the next index; later calls
    /// return the same index. One index per distinct name.
    ///
    /// This is the only mutating path in this crate: [`AtomTable::seed`] and
    /// [`AtomTable::seed_from_atom_names`] both route through it, and there is no
    /// removal, renaming or reordering API. `intern` therefore bumps the generation
    /// (see [`AtomTable::generation`]) by exactly one for a newly interned name, and
    /// not at all for a name that is already present — no mutation can bypass the bump.
    ///
    /// Inlined because the already-present case *is* [`AtomTable::lookup`]: a warm decode
    /// interns every atom it sees and takes the early return below on all of them, so the
    /// probe must compile into the caller as one hash → shard → slot sequence.
    #[inline]
    pub fn intern(&mut self, name: &str) -> Atom {
        if let Some(atom) = self.index.get(name) {
            return atom;
        }
        debug_assert!(
            self.names.len() <= u32::MAX as usize,
            "atom table exhausted: {} exceeds the Atom index space",
            self.names.len()
        );
        // The name is stored **once**, in a heap-allocated shared string that both the ordered
        // `names` view and the `index` shards reference: one allocation (plus a refcount bump),
        // not one per container. A known name takes the `index.get` early return above and
        // allocates nothing; a novel name copies at most one name chunk and one index shard
        // (the copy-on-write step), never the whole table.
        let shared: Arc<str> = Arc::from(name);
        let atom = Atom(self.names.push(Arc::clone(&shared)));
        self.index.insert(shared, atom);
        // Exactly-one-per-append: this is the only place the table grows.
        self.generation = self.generation.wrapping_add(1);
        atom
    }

    /// The monotonic generation: incremented by exactly one for every newly interned name.
    ///
    /// For a table grown only by [`AtomTable::intern`] from empty (or by a clone of such
    /// a table) it equals the number of names appended since construction, so it equals
    /// [`AtomTable::len`] minus the length the table had when it was cloned. It wraps
    /// (it is a counter, not a clock): the only guarantee is that every append changes it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// An O(1) witness of the current table state.
    pub fn snapshot(&self) -> AtomSnapshot {
        AtomSnapshot {
            instance: self.instance,
            generation: self.generation,
            len: self.names.len(),
        }
    }

    /// Whether `self` is guaranteed to be an append-only extension of the table the
    /// snapshot was taken from.
    ///
    /// Returns `true` only if
    ///
    /// * `self` and the snapshotting table are the **same instance** (not a clone, not a
    ///   different table that happens to hold the same names), and
    /// * every mutation since the snapshot appended exactly one name — i.e. the length
    ///   delta equals the generation delta,
    ///
    /// which implies that every atom index `< snapshot.len()` still names the same atom,
    /// that any name added since occupies an index `>= snapshot.len()`, and that
    /// `self.len() >= snapshot.len()`. So a memo of the snapshotting table can be
    /// extended with `self`'s names at indices `snapshot.len()..` and used as an exact
    /// image of `self`.
    ///
    /// The generation delta `==` the length delta is what makes the check *exact* rather
    /// than merely plausible: each append adds one name and bumps the generation by one,
    /// so any mutation that is not a pure append breaks the equality (defence in depth —
    /// a future non-append mutating API cannot silently satisfy the witness).
    ///
    /// It is O(1), allocation-free, and never scans `names`. It returns `false` — never a
    /// wrong `true` — for a different or replaced table, a clone of the table (a clone is
    /// a new identity — see the `Clone` impl above), a table that shrank, or a generation
    /// that went backwards.
    ///
    /// This is *one half* of the standard memo protocol: the caller must additionally
    /// check `memo.len() <= shared.len()` before extending the memo (a memo that is
    /// *longer* than `self` cannot be extended into an image of `self`, however the
    /// scalars line up).
    pub fn appended_since(&self, snapshot: AtomSnapshot) -> bool {
        if self.instance != snapshot.instance {
            return false;
        }
        let Some(len_delta) = self.names.len().checked_sub(snapshot.len) else {
            return false;
        };
        let Some(gen_delta) = self.generation.checked_sub(snapshot.generation) else {
            return false;
        };
        gen_delta == len_delta as u64
    }

    /// Look up an already-interned name.
    ///
    /// The single hottest atom operation in the runtime — the ETF decode path calls it once per
    /// atom reference — so it is `#[inline]`: the whole body is one [`store::hash_of`], one shard
    /// mask and one probe, and a call frame around that costs more than the work.
    #[inline]
    pub fn lookup(&self, name: &str) -> Option<Atom> {
        self.index.get(name)
    }

    /// The name of an interned atom, if the index is in range.
    pub fn name(&self, atom: Atom) -> Option<&str> {
        self.names.get(atom.raw() as usize)
    }

    /// The number of interned atoms.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.names.len() == 0
    }

    /// Pre-intern a run of names in the given order.
    ///
    /// The name storage is reserved once from the iterator's size-hint lower bound (a lower bound
    /// on the number of names about to be seen), so seeding a large table does not repeatedly
    /// regrow the chunk list. The sharded index has no single bucket array to reserve — a seed run
    /// spreads over the shards in an order the caller cannot predict, so each shard grows itself.
    /// [`AtomTable::new`] still starts empty, so the single-`intern` path pays no eager
    /// reservation.
    pub fn seed<'a, I>(&mut self, ordered_names: I)
    where
        I: IntoIterator<Item = &'a str>,
    {
        let names = ordered_names.into_iter();
        let (lower, _) = names.size_hint();
        self.names.reserve_names(lower);
        for name in names {
            self.intern(name);
        }
    }

    /// Parse an `atom.names` source and intern every name in file order.
    ///
    /// Grammar (from the `atom.names` header comment):
    ///
    /// ```text
    /// <atom-decl> ::= "atom" <atom>+
    /// <atom>      ::= <atom-name> | "'" <printname> "'" | <C-name> "=" "'" <printname> "'"
    /// ```
    ///
    /// Lines starting with `#` are comments. For the two quoted forms the
    /// *printname* is the atom name; the `C-name` is only the C alias.
    ///
    /// A malformed source stops at the first bad line with [`TermError::InvalidAtomName`]; the
    /// names of every preceding, well-formed line remain interned (a bad line interns none of
    /// its own names — it is fully validated before its names are appended).
    pub fn seed_from_atom_names(&mut self, src: &str) -> TermResult<()> {
        // Pass 1 — count (allocates nothing, interns nothing): the number of names that precede
        // the first malformed line. Best effort, so the error it may carry is ignored here and
        // reported authoritatively by the interning pass below. Reserving the whole run once
        // keeps the chunk list from growing name by name (the sharded index has no single bucket
        // array to reserve, so each shard grows itself).
        let mut total = 0usize;
        let _ = for_each_atom_line(src, |rest| {
            let count = parse_atom_list(rest, |_| Ok(()))?;
            if count == 0 {
                return Err(TermError::InvalidAtomName);
            }
            total += count;
            Ok(())
        });
        self.names.reserve_names(total);

        // Pass 2 — intern. Each line is validate-parsed *before* its names are appended, so a
        // line that turns out malformed contributes nothing (the previous behaviour).
        for_each_atom_line(src, |rest| {
            if parse_atom_list(rest, |_| Ok(()))? == 0 {
                return Err(TermError::InvalidAtomName);
            }
            parse_atom_list(rest, |name| {
                self.intern(name);
                Ok(())
            })?;
            Ok(())
        })
    }

    /// Convenience: a table seeded from an `atom.names` source.
    pub fn from_atom_names(src: &str) -> TermResult<AtomTable> {
        let mut table = AtomTable::new();
        table.seed_from_atom_names(src)?;
        Ok(table)
    }

    /// The atom's cached hash value — the C's `atom_tab(a)->slot.bucket.hvalue`
    /// (`erts/emulator/beam/atom.c:82` `atom_hash`): the Dragon-Book `hashpjw`
    /// over the atom name's UTF-8 bytes, with the latin1 "clutch" that folds a
    /// `0xC2 0x80..0xBF` pair back into one byte first.
    ///
    /// This is the value `make_hash2`/`make_hash` mix in for an atom, so it is
    /// part of the `erlang:phash2/1,2` / `erlang:phash/2` compatibility contract.
    ///
    /// The C keeps the running accumulator in a `byte` (`byte v;` in `atom_hash`)
    /// while the shift register `h` is a `Uint32`; both wrap — reproduced here.
    ///
    /// An `a` whose index is not in this table has no name and therefore no
    /// `hvalue`; rather than panicking we fall back to the raw index (documented,
    /// deterministic). Use [`AtomTable::name`] to distinguish that case.
    pub fn hvalue(&self, a: Atom) -> u32 {
        let Some(name) = self.name(a) else {
            return a.raw();
        };
        let mut h: u32 = 0;
        let bytes = name.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            // `byte v;` — 8-bit accumulator, so the shift-out wraps.
            let mut v: u8 = bytes[i];
            i += 1;
            if (v & 0xFE) == 0xC2 && i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
                // latin1 clutch: 0xC2 0x80..0xBF is one latin1 code point.
                v = (v << 6) | (bytes[i] & 0x3F);
                i += 1;
            }
            h = h.wrapping_shl(4).wrapping_add(u32::from(v));
            let g = h & 0xf000_0000;
            if g != 0 {
                h ^= g >> 24;
                h ^= g;
            }
        }
        h
    }
}

/// Run `visit_line` on the body of each `atom ...` declaration line of `src`, in file order.
///
/// Blank and `#`-comment lines are skipped; the `atom` keyword prefix is validated (the keyword
/// must be followed by whitespace or end-of-line). Stops at the first malformed line with
/// [`TermError::InvalidAtomName`]. This is the one place the `atom.names` line structure lives,
/// shared by [`AtomTable::seed_from_atom_names`]'s counting and interning passes.
fn for_each_atom_line(
    src: &str,
    mut visit_line: impl FnMut(&str) -> TermResult<()>,
) -> TermResult<()> {
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let rest = match trimmed.strip_prefix("atom") {
            Some(rest) if rest.is_empty() || rest.as_bytes()[0].is_ascii_whitespace() => rest,
            _ => return Err(TermError::InvalidAtomName),
        };
        visit_line(rest)?;
    }
    Ok(())
}

/// Tokenise the names on one `atom ...` line, calling `visit` with each name's printname slice
/// in file order and returning how many names it visited.
///
/// `rest` must be ASCII (`atom.names` guarantees 7-bit ASCII). Handing each printname to a
/// callback (rather than pushing an owned `String` into a `Vec`) lets the caller intern it
/// directly, so seeding allocates no throw-away buffer.
fn parse_atom_list(
    rest: &str,
    mut visit: impl FnMut(&str) -> TermResult<()>,
) -> TermResult<usize> {
    let bytes = rest.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut count = 0usize;

    while i < len {
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= len {
            break;
        }

        if bytes[i] == b'\'' {
            // "'" <printname> "'"
            i += 1;
            let start = i;
            while i < len && bytes[i] != b'\'' {
                i += 1;
            }
            if i >= len {
                return Err(TermError::InvalidAtomName);
            }
            visit(&rest[start..i])?;
            count += 1;
            i += 1; // closing quote
        } else {
            // <atom-name> | <C-name> "=" "'" <printname> "'"
            let start = i;
            while i < len && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
                i += 1;
            }
            if start == i {
                return Err(TermError::InvalidAtomName);
            }
            if i < len && bytes[i] == b'=' {
                i += 1; // '='
                if i >= len || bytes[i] != b'\'' {
                    return Err(TermError::InvalidAtomName);
                }
                i += 1; // opening quote
                let s = i;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i >= len {
                    return Err(TermError::InvalidAtomName);
                }
                visit(&rest[s..i])?;
                count += 1;
                i += 1; // closing quote
            } else {
                visit(&rest[start..i])?;
                count += 1;
            }
        }
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::immediate::{atom_val, make_atom};

    #[test]
    fn intern_is_stable_and_ordered() {
        let mut t = AtomTable::new();
        assert!(t.is_empty());
        let a = t.intern("alpha");
        let b = t.intern("beta");
        let a2 = t.intern("alpha");
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(a.raw(), 0);
        assert_eq!(b.raw(), 1);
        assert_eq!(t.len(), 2);
        assert!(!t.is_empty());
    }

    #[test]
    fn lookup_and_name_round_trip() {
        let mut t = AtomTable::new();
        let a = t.intern("hello");
        assert_eq!(t.lookup("hello"), Some(a));
        assert_eq!(t.lookup("nope"), None);
        assert_eq!(t.name(a), Some("hello"));
        assert_eq!(t.name(Atom::from_raw(42)), None);
    }

    #[test]
    fn make_atom_atom_val_round_trip() {
        let mut t = AtomTable::new();
        let a = t.intern("ok");
        let term = make_atom(a);
        assert_eq!(atom_val(term), a);
        assert_eq!(t.name(atom_val(term)), Some("ok"));
    }

    #[test]
    fn seed_preserves_order() {
        let mut t = AtomTable::new();
        t.seed(["one", "two", "three"]);
        assert_eq!(t.lookup("one").unwrap().raw(), 0);
        assert_eq!(t.lookup("two").unwrap().raw(), 1);
        assert_eq!(t.lookup("three").unwrap().raw(), 2);
    }

    #[test]
    fn seed_from_atom_names_preserves_file_order() {
        // A snippet exercising: bare names, CName='printname', 'quoted', empty
        // printname, comments and blank lines.
        let src = "\
# comment line
atom false true
atom Underscore='_'
atom Noname='nonode@nohost'
atom EOT='$end_of_table'
atom Empty=''
atom infinity timeout normal

atom nil no none
";
        let t = AtomTable::from_atom_names(src).unwrap();
        // file-order indices (0-based), skipping comments/blank lines
        assert_eq!(t.name(Atom::from_raw(0)), Some("false"));
        assert_eq!(t.name(Atom::from_raw(1)), Some("true"));
        // `CName='printname'` -> the atom name is the printname
        assert_eq!(t.name(Atom::from_raw(2)), Some("_"));
        assert_eq!(t.name(Atom::from_raw(3)), Some("nonode@nohost"));
        assert_eq!(t.name(Atom::from_raw(4)), Some("$end_of_table"));
        assert_eq!(t.name(Atom::from_raw(5)), Some(""));
        assert_eq!(t.name(Atom::from_raw(6)), Some("infinity"));
        assert_eq!(t.name(Atom::from_raw(7)), Some("timeout"));
        assert_eq!(t.name(Atom::from_raw(8)), Some("normal"));
        assert_eq!(t.name(Atom::from_raw(9)), Some("nil"));
        assert_eq!(t.name(Atom::from_raw(10)), Some("no"));
        assert_eq!(t.name(Atom::from_raw(11)), Some("none"));
        assert_eq!(t.len(), 12);
        // the C alias is NOT interned
        assert_eq!(t.lookup("Underscore"), None);
    }

    #[test]
    fn seed_from_atom_names_quoted_with_spaces() {
        let t = AtomTable::from_atom_names("atom 'has space' plain\n").unwrap();
        assert_eq!(t.name(Atom::from_raw(0)), Some("has space"));
        assert_eq!(t.name(Atom::from_raw(1)), Some("plain"));
    }

    #[test]
    fn seed_from_atom_names_seeding_is_idempotent() {
        let mut t = AtomTable::new();
        t.seed_from_atom_names("atom a b c\n").unwrap();
        t.seed_from_atom_names("atom c d\n").unwrap();
        assert_eq!(t.len(), 4);
        assert_eq!(t.name(Atom::from_raw(0)), Some("a"));
        assert_eq!(t.name(Atom::from_raw(3)), Some("d"));
    }

    #[test]
    fn seed_from_atom_names_rejects_malformed() {
        assert_eq!(
            AtomTable::from_atom_names("atom \n").err(),
            Some(TermError::InvalidAtomName)
        );
        assert_eq!(
            AtomTable::from_atom_names("notatom a\n").err(),
            Some(TermError::InvalidAtomName)
        );
        assert_eq!(
            AtomTable::from_atom_names("atom 'unterminated\n").err(),
            Some(TermError::InvalidAtomName)
        );
        assert_eq!(
            AtomTable::from_atom_names("atom Aliased=unquoted\n").err(),
            Some(TermError::InvalidAtomName)
        );
        assert_eq!(
            AtomTable::from_atom_names("atom Aliased=\n").err(),
            Some(TermError::InvalidAtomName)
        );
    }

    #[test]
    fn atom_max_is_u32_max() {
        assert_eq!(Atom::MAX, Atom::from_raw(u32::MAX));
        assert_eq!(ATOM_INDEX_BITS, 6);
        assert_eq!(MAX_ATOM_INDEX, (1usize << 58) - 1);
    }

    #[test]
    fn snapshot_captures_len_and_generation() {
        let mut t = AtomTable::new();
        let empty = t.snapshot();
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
        assert_eq!(empty.generation(), 0);

        t.seed(["a", "b", "c"]);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 3);
        assert!(!snap.is_empty());
        assert_eq!(snap.generation(), 3);
        assert_eq!(snap, t.snapshot());
        assert_ne!(snap, empty);
    }

    #[test]
    fn intern_bumps_generation_only_for_new_names() {
        let mut t = AtomTable::new();
        assert_eq!(t.generation(), 0);
        t.intern("a");
        assert_eq!(t.generation(), 1);
        // re-interning an existing name must not bump
        t.intern("a");
        t.intern("a");
        assert_eq!(t.generation(), 1);
        // a new name must bump by exactly one
        t.intern("b");
        assert_eq!(t.generation(), 2);
        t.intern("c");
        assert_eq!(t.generation(), 3);
        // for a table grown from empty, generation == len
        assert_eq!(t.generation(), t.len() as u64);
    }

    #[test]
    fn appended_since_is_true_for_an_append_only_extension() {
        let mut a = AtomTable::new();
        a.seed(["one", "two"]);
        let snap = a.snapshot();
        // a table validates against its own snapshot, even with no append in between
        assert!(a.appended_since(snap));
        a.intern("three");
        assert!(a.appended_since(snap));
        a.seed(["four", "five"]);
        assert!(a.appended_since(snap));
        assert_eq!(a.len(), 5);
        // an older table must not validate against a newer table's snapshot
        let mut b = a.clone();
        b.intern("six");
        let snap_b = b.snapshot();
        assert!(!a.appended_since(snap_b));
        assert!(b.appended_since(snap_b));
        // ... and a clone, being a fresh identity, does not inherit `a`'s witness
        assert!(!b.appended_since(snap));
    }

    #[test]
    fn appended_since_is_false_for_a_clone_that_diverges() {
        let mut a = AtomTable::new();
        a.seed(["x", "y", "z"]);
        let snap = a.snapshot();

        // a clone has a fresh identity, so it never validates — not even unwritten
        let untouched = a.clone();
        assert!(!untouched.appended_since(snap));

        // clone, then mutate the two in *different* directions: divergence is caught,
        // not merely a content difference
        let mut b = a.clone();
        b.intern("only-in-b");
        a.intern("only-in-a");
        // `a` is still the snapshotting instance and appended exactly once, so its own
        // snapshot still validates; the *clone* never does
        assert!(a.appended_since(snap));
        assert!(!b.appended_since(snap));
        let snap_a = a.snapshot();
        let snap_b = b.snapshot();
        assert!(!b.appended_since(snap_a));
        assert!(!a.appended_since(snap_b));
        // both clones are still append-valid against a snapshot of themselves
        assert!(a.appended_since(snap_a));
        assert!(b.appended_since(snap_b));
        // same length, different content: still no mutual validation
        assert_eq!(a.len(), b.len());
        assert_ne!(a.snapshot(), b.snapshot());
    }

    #[test]
    fn appended_since_is_false_when_the_table_is_replaced() {
        // Two independently constructed tables seeded with identical names: same len,
        // same names in the same order, different identity.
        let mut a = AtomTable::new();
        let mut b = AtomTable::new();
        a.seed(["alpha", "beta", "gamma"]);
        b.seed(["alpha", "beta", "gamma"]);
        assert_eq!(a.len(), b.len());
        assert_eq!(a.generation(), b.generation());
        assert!(!b.appended_since(a.snapshot()));
        assert!(!a.appended_since(b.snapshot()));
        // and appending to one must not make it validate against the other
        b.intern("delta");
        assert!(!b.appended_since(a.snapshot()));
        assert!(!a.appended_since(b.snapshot()));
    }

    #[test]
    fn appended_since_is_false_when_the_table_shrank() {
        let mut long = AtomTable::new();
        long.seed(["a", "b", "c", "d", "e"]);
        let snap_long = long.snapshot();

        // a snapshot of a longer table checked against a shorter one is `false`, with no
        // underflow: the identity check rejects a foreign table before any subtraction.
        // (A same-identity snapshot longer than the table cannot arise through the public
        // API — `AtomTable` only grows — so the `checked_sub` guards in `appended_since`
        // are defence in depth.)
        let mut short = AtomTable::new();
        short.seed(["a", "b"]);
        assert!(short.len() < snap_long.len());
        assert!(!short.appended_since(snap_long));

        // ... and the same holds for a clone of the longer table, which shares its
        // content but not its identity
        let clone = long.clone();
        assert_eq!(clone.len(), snap_long.len());
        assert!(!clone.appended_since(snap_long));
    }

    #[test]
    fn seed_and_from_atom_names_are_observed() {
        let mut t = AtomTable::new();
        t.seed(["p", "q"]);
        let snap = t.snapshot();
        t.seed(["r", "s"]);
        assert!(t.appended_since(snap), "seed routes through intern");
        assert_eq!(t.generation(), 4);

        // seed_from_atom_names is observed too
        let snap2 = t.snapshot();
        t.seed_from_atom_names("atom t u\n").unwrap();
        assert!(t.appended_since(snap2));
        assert_eq!(t.len(), 6);
        assert_eq!(t.generation(), 6);

        // from_atom_names produces a table whose generation equals its length
        let parsed = AtomTable::from_atom_names("atom false true nil\n").unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed.generation(), 3);
        assert_eq!(parsed.snapshot().len(), parsed.len());
    }

    #[test]
    fn atom_snapshot_is_a_cheap_copy() {
        let mut t = AtomTable::new();
        t.seed(["a", "b", "c", "d"]);
        let snap = t.snapshot();
        let copied = snap; // Copy, not move
        fn takes_copy(_: AtomSnapshot) {}
        takes_copy(snap); // still usable after a move-shaped use
        assert_eq!(snap, copied);
        assert_eq!(snap.len(), t.len());
        assert_eq!(snap.generation(), t.generation());
    }

    // -----------------------------------------------------------------------
    // `fork()` — the handle-sharing whole-table copy
    // -----------------------------------------------------------------------

    /// A table of `n` names, interned in index order.
    fn table_of(n: usize) -> AtomTable {
        let names: Vec<String> = (0..n).map(|i| format!("fork_test_atom_{i:05}")).collect();
        let mut t = AtomTable::new();
        t.seed(names.iter().map(String::as_str));
        t
    }

    #[test]
    fn fork_preserves_every_index_and_the_generation() {
        let parent = table_of(300);
        let fork = parent.fork();
        assert_eq!(fork.len(), parent.len());
        assert_eq!(fork.generation(), parent.generation());
        assert!(!fork.is_empty());
        for index in 0..parent.len() {
            let atom = Atom::from_raw(index as u32);
            assert_eq!(
                fork.name(atom),
                parent.name(atom),
                "index {index} must resolve identically in a fork"
            );
            // `lookup` sees the same atom for the same name
            let name = parent.name(atom).expect("in range");
            assert_eq!(fork.lookup(name), Some(atom));
        }
        // a name the parent never interned is in neither table
        assert_eq!(fork.lookup("fork_test_atom_99999"), None);
        assert_eq!(fork.name(Atom::from_raw(300)), None);
    }

    /// The `Arc::strong_count` of every name handle in `table`, in index order.
    ///
    /// A table on its own holds exactly **two** handles per name: one in the ordered name store
    /// (`NameChunks`) and one as an index key (`IndexShards`).
    fn handle_counts(table: &AtomTable) -> Vec<usize> {
        (0..table.len())
            .map(|index| {
                Arc::strong_count(
                    table
                        .names
                        .name_arc(index)
                        .expect("every index in range names a name"),
                )
            })
            .collect()
    }

    #[test]
    fn fork_shares_the_arc_handles_and_copies_no_string() {
        let parent = table_of(200);
        let alone = handle_counts(&parent);
        assert!(alone.iter().all(|&count| count == 2), "{alone:?}");

        let fork = parent.fork();
        for (index, &alone_count) in alone.iter().enumerate() {
            let atom = Atom::from_raw(index as u32);
            let parent_name = parent.names.name_arc(index).expect("in range");
            let fork_name = fork.names.name_arc(index).expect("in range");
            // same bytes, same allocation: `name()` hands out the shared `str`'s contents
            assert_eq!(fork.name(atom), parent.name(atom));
            assert!(
                std::ptr::eq(
                    fork.name(atom).expect("in range"),
                    parent.name(atom).expect("in range")
                ),
                "index {index}: a fork shares the name allocation, it does not copy the string"
            );
            assert!(
                Arc::ptr_eq(parent_name, fork_name),
                "index {index}: the name handle itself is shared"
            );
            // the fork's second direction maps the shared name straight back to the same index
            assert_eq!(
                fork.lookup(parent.name(atom).expect("in range")),
                Some(atom)
            );
            assert_eq!(
                Arc::strong_count(parent_name),
                alone_count,
                "index {index}: a fork adds **zero** `Arc<str>` handles — the whole prefix is \
                 shared through the two containers, not re-listed"
            );
        }
        // ... and it is the containers that are shared, in both directions.
        assert_eq!(
            parent.names.container_ptr(),
            fork.names.container_ptr(),
            "a fork shares the name-storage container"
        );
        assert_eq!(
            parent.index.container_ptr(),
            fork.index.container_ptr(),
            "a fork shares the index container"
        );
        assert!(parent.names.container_ptr().is_some());
        assert!(parent.index.container_ptr().is_some());
    }

    #[test]
    fn fork_copy_on_write_detaches_only_the_appending_side() {
        // The isolation half of the sharing: `Arc::make_mut` copies the container a write lands
        // in, so the *appending* side moves to a private container and the other side keeps
        // reading the shared one, unchanged.
        let mut parent = table_of(300);
        let mut fork = parent.fork();
        let shared_names = parent.names.container_ptr();
        let shared_index = parent.index.container_ptr();
        let fork_len = fork.len();
        let fork_generation = fork.generation();
        let fork_names: Vec<String> = (0..fork_len)
            .map(|index| {
                fork.name(Atom::from_raw(index as u32))
                    .expect("in range")
                    .to_string()
            })
            .collect();

        // --- an append into the parent detaches the parent, not the fork --------------------
        let appended = parent.intern("only_in_the_parent_after_the_fork");
        assert_ne!(
            parent.names.container_ptr(),
            shared_names,
            "the appending side gets its own name-storage container"
        );
        assert_ne!(
            parent.index.container_ptr(),
            shared_index,
            "the appending side gets its own index container"
        );
        assert_eq!(
            fork.names.container_ptr(),
            shared_names,
            "the fork still points at the container it shared before the append"
        );
        assert_eq!(fork.index.container_ptr(), shared_index);
        // ... and nothing the fork can observe moved
        assert_eq!(fork.len(), fork_len);
        assert_eq!(fork.generation(), fork_generation);
        for (index, name) in fork_names.iter().enumerate() {
            assert_eq!(
                fork.name(Atom::from_raw(index as u32)),
                Some(name.as_str()),
                "index {index} still names the same name in the untouched fork"
            );
        }
        assert_eq!(fork.lookup("only_in_the_parent_after_the_fork"), None);
        assert_eq!(
            parent.name(appended),
            Some("only_in_the_parent_after_the_fork")
        );
        // the shared prefix is still the *same allocation*, on both sides
        for index in 0..fork_len {
            assert!(std::ptr::eq(
                fork.name(Atom::from_raw(index as u32)).expect("in range"),
                parent.name(Atom::from_raw(index as u32)).expect("in range"),
            ));
        }

        // --- and symmetrically: an append into the fork detaches only the fork --------------
        // The parent already moved to its own container above, which left the fork the sole owner
        // of the one it shared — so the fork mutates *that* container in place (nothing to copy)
        // and the two sides still end up with containers of their own.
        let parent_names = parent.names.container_ptr();
        let parent_index = parent.index.container_ptr();
        let parent_len = parent.len();
        let forked = fork.intern("only_in_the_fork_after_the_fork");
        assert_ne!(
            fork.names.container_ptr(),
            parent_names,
            "after both sides append, each owns a private name-storage container"
        );
        assert_ne!(fork.index.container_ptr(), parent_index);
        assert_eq!(
            parent.names.container_ptr(),
            parent_names,
            "the parent keeps its own container across the fork's append"
        );
        assert_eq!(parent.index.container_ptr(), parent_index);
        assert_eq!(parent.len(), parent_len);
        assert_eq!(parent.lookup("only_in_the_fork_after_the_fork"), None);
        assert_eq!(fork.name(forked), Some("only_in_the_fork_after_the_fork"));
        // both appends landed at the same index (each side counted from the same length), and
        // they are different names: separate tables, separate containers.
        assert_eq!(appended.raw(), forked.raw());
        assert_ne!(parent.name(appended), fork.name(forked));
        // the shared prefix is still one allocation on both sides, though
        for index in 0..fork_len {
            assert!(std::ptr::eq(
                fork.name(Atom::from_raw(index as u32)).expect("in range"),
                parent.name(Atom::from_raw(index as u32)).expect("in range"),
            ));
        }
    }

    #[test]
    fn fork_of_a_fork_is_the_same_shape() {
        let parent = table_of(64);
        let alone = handle_counts(&parent);
        let fork = parent.fork();
        let grand = fork.fork();
        assert_eq!(grand.len(), parent.len());
        assert_eq!(grand.generation(), parent.generation());
        for (index, &alone_count) in alone.iter().enumerate() {
            let atom = Atom::from_raw(index as u32);
            assert_eq!(grand.name(atom), parent.name(atom));
            assert!(
                Arc::ptr_eq(
                    parent.names.name_arc(index).expect("in range"),
                    grand.names.name_arc(index).expect("in range")
                ),
                "the name handles stay shared across generations of forks"
            );
            assert_eq!(
                Arc::strong_count(parent.names.name_arc(index).expect("in range")),
                alone_count,
                "no fork generation adds an `Arc<str>` handle"
            );
        }
        // one container pair, shared by all three generations
        assert_eq!(parent.names.container_ptr(), fork.names.container_ptr());
        assert_eq!(parent.names.container_ptr(), grand.names.container_ptr());
        assert_eq!(parent.index.container_ptr(), fork.index.container_ptr());
        assert_eq!(parent.index.container_ptr(), grand.index.container_ptr());
        // three live tables, three distinct identities
        assert_ne!(parent.snapshot(), fork.snapshot());
        assert_ne!(fork.snapshot(), grand.snapshot());
        assert_ne!(parent.snapshot(), grand.snapshot());
        assert!(!grand.appended_since(parent.snapshot()));
        assert!(!grand.appended_since(fork.snapshot()));
    }

    #[test]
    fn a_fork_has_a_fresh_identity_in_both_directions() {
        let mut parent = table_of(4);
        let parent_snap = parent.snapshot();
        let fork = parent.fork();
        let fork_snap = fork.snapshot();
        // identical content, identical generation — and still no mutual validation
        assert_eq!(parent_snap.len(), fork_snap.len());
        assert_eq!(parent_snap.generation(), fork_snap.generation());
        assert_ne!(parent_snap, fork_snap, "the identity half differs");
        assert!(!fork.appended_since(parent_snap));
        assert!(!parent.appended_since(fork_snap));
        // each side is append-valid against its own snapshot
        assert!(parent.appended_since(parent_snap));
        assert!(fork.appended_since(fork_snap));
        // ... including after both have appended independently
        parent.intern("after_fork_parent");
        let mut fork = fork;
        fork.intern("after_fork_fork");
        assert!(parent.appended_since(parent_snap));
        assert!(fork.appended_since(fork_snap));
        assert!(!fork.appended_since(parent_snap));
        assert!(!parent.appended_since(fork_snap));
    }

    #[test]
    fn interning_into_either_side_leaves_the_other_untouched() {
        let mut parent = table_of(5);
        let fork = parent.fork();
        let parent_len = parent.len();
        let parent_generation = parent.generation();
        let parent_snap = parent.snapshot();

        // append to the fork: the parent's observable state is untouched
        let mut fork = fork;
        let forked_atom = fork.intern("only_in_the_fork");
        assert_eq!(forked_atom.raw() as usize, parent_len);
        assert_eq!(fork.len(), parent_len + 1);
        assert_eq!(parent.len(), parent_len);
        assert_eq!(parent.generation(), parent_generation);
        assert_eq!(parent.snapshot(), parent_snap);
        assert_eq!(parent.lookup("only_in_the_fork"), None);
        assert_eq!(parent.name(forked_atom), None);
        // `intern` appends in index order, so the fork is still a prefix extension of the parent
        for index in 0..parent_len {
            assert_eq!(
                fork.name(Atom::from_raw(index as u32)),
                parent.name(Atom::from_raw(index as u32))
            );
        }

        // ... and the mirror direction: a name interned into the parent after the fork is
        // invisible to the fork. Same index, different name — they are separate tables.
        let parent_atom = parent.intern("only_in_the_parent");
        assert_eq!(parent_atom.raw() as usize, parent_len);
        assert_eq!(parent.name(parent_atom), Some("only_in_the_parent"));
        assert_eq!(fork.name(parent_atom), Some("only_in_the_fork"));
        assert_eq!(parent.lookup("only_in_the_fork"), None);
        assert_eq!(fork.lookup("only_in_the_parent"), None);
        assert_eq!(parent.len(), fork.len());
    }

    #[test]
    fn fork_of_an_empty_table() {
        let parent = AtomTable::new();
        let fork = parent.fork();
        assert!(fork.is_empty());
        assert_eq!(fork.len(), 0);
        assert_eq!(fork.generation(), 0);
        assert_eq!(fork.lookup("anything"), None);
        // the fork of an empty table still has its own identity
        assert!(!fork.appended_since(parent.snapshot()));
        assert!(!parent.appended_since(fork.snapshot()));
        // and both can grow independently from index 0
        let mut parent = parent;
        let mut fork = fork;
        assert_eq!(parent.intern("a").raw(), 0);
        assert_eq!(fork.intern("b").raw(), 0);
        assert_eq!(parent.name(Atom::from_raw(0)), Some("a"));
        assert_eq!(fork.name(Atom::from_raw(0)), Some("b"));
    }

    #[test]
    fn fork_is_a_drop_in_for_the_lower_then_merge_protocol() {
        // The `beam_rt` shape: lower with a private table that may append literal-only atoms at
        // exactly `len..`, then merge that tail back into the shared table by index.
        let mut shared = table_of(16);
        let base = shared.len();
        let mut lowered = shared.fork();
        let literal_only = lowered.intern("only_in_a_literal");
        assert_eq!(literal_only.raw() as usize, base);
        assert_eq!(lowered.len(), base + 1);
        for index in base..lowered.len() {
            let name = lowered.name(Atom::from_raw(index as u32)).expect("in range");
            let merged = shared.intern(name);
            assert_eq!(merged.raw() as usize, index, "the tail merges back by index");
        }
        // the shared table now resolves that index to the same name, at the same index, as the
        // private table the module was lowered against
        assert_eq!(shared.name(literal_only), Some("only_in_a_literal"));
        assert_eq!(shared.len(), lowered.len());
    }
}