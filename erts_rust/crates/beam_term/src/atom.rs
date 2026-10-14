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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{TermError, TermResult};

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
#[derive(Debug)]
pub struct AtomTable {
    names: Vec<String>,
    index: HashMap<String, Atom>,
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
    /// Deep-copies the entries and the generation, but assigns a **fresh** identity.
    ///
    /// A clone is deliberately a *new* identity, not a copy of the original's: the two
    /// tables start identical but may later diverge by independent appends, so a
    /// snapshot of either must never validate against the other (see
    /// [`AtomTable::appended_since`]). This is exactly what makes the witness sound
    /// under copy-on-write (`Rc::make_mut`): a borrowed shared table that gets cloned
    /// into a privately-mutated one can never make a stale image look valid. The cost
    /// is a *conservative* false negative right after a clone — the caller rebuilds its
    /// memo instead of reusing it — which is the correct trade for soundness.
    fn clone(&self) -> AtomTable {
        AtomTable {
            names: self.names.clone(),
            index: self.index.clone(),
            instance: next_instance(),
            generation: self.generation,
        }
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
    pub fn new() -> AtomTable {
        AtomTable {
            names: Vec::new(),
            index: HashMap::new(),
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
    pub fn intern(&mut self, name: &str) -> Atom {
        if let Some(&atom) = self.index.get(name) {
            return atom;
        }
        let idx = self.names.len();
        debug_assert!(
            idx <= u32::MAX as usize,
            "atom table exhausted: {idx} exceeds the Atom index space"
        );
        let atom = Atom(idx as u32);
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), atom);
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
    pub fn lookup(&self, name: &str) -> Option<Atom> {
        self.index.get(name).copied()
    }

    /// The name of an interned atom, if the index is in range.
    pub fn name(&self, atom: Atom) -> Option<&str> {
        self.names.get(atom.raw() as usize).map(String::as_str)
    }

    /// The number of interned atoms.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Pre-intern a run of names in the given order.
    pub fn seed<'a, I>(&mut self, ordered_names: I)
    where
        I: IntoIterator<Item = &'a str>,
    {
        for name in ordered_names {
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
    pub fn seed_from_atom_names(&mut self, src: &str) -> TermResult<()> {
        for line in src.lines() {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let rest = match trimmed.strip_prefix("atom") {
                Some(rest)
                    if rest.is_empty()
                        || rest.as_bytes()[0].is_ascii_whitespace() =>
                {
                    rest
                }
                _ => return Err(TermError::InvalidAtomName),
            };
            let mut names = Vec::new();
            parse_atom_list(rest, &mut names)?;
            if names.is_empty() {
                return Err(TermError::InvalidAtomName);
            }
            for name in names {
                self.intern(&name);
            }
        }
        Ok(())
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

/// Tokenise the names on one `atom ...` line, appending them to `out`.
///
/// `rest` must be ASCII (`atom.names` guarantees 7-bit ASCII).
fn parse_atom_list(rest: &str, out: &mut Vec<String>) -> TermResult<()> {
    let bytes = rest.as_bytes();
    let len = bytes.len();
    let mut i = 0;

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
            out.push(rest[start..i].to_string());
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
            let bare = &rest[start..i];
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
                out.push(rest[s..i].to_string());
                i += 1; // closing quote
            } else {
                out.push(bare.to_string());
            }
        }
    }

    Ok(())
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
}