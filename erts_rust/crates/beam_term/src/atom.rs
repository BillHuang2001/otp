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
#[derive(Debug, Default, Clone)]
pub struct AtomTable {
    names: Vec<String>,
    index: HashMap<String, Atom>,
}

impl AtomTable {
    /// An empty table.
    pub fn new() -> AtomTable {
        AtomTable::default()
    }

    /// Intern `name`, returning its index.
    ///
    /// The first time a name is seen it is assigned the next index; later calls
    /// return the same index. One index per distinct name.
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
        atom
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
}
