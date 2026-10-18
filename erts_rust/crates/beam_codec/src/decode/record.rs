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

//! The native-record tag arm of [`Decoder`]: `RECORD_EXT` (`67`).
//!
//! The tag is handled **inside the shared term walker** ([`Decoder::decode`]) — the
//! `match tag` dispatch stays in the parent [`crate::decode`] module — so a record
//! nested in a tuple/list/map decodes like any other sub-term. The helper lives here
//! as a second `impl` block on the decoder; being a child of the `decode` module it
//! reaches its private wire cursor and atom helper directly.
//!
//! Wire format (`external.c:5257-5381`):
//!
//! ```text
//! [67][num_fields:4 BE][flags:1][module atom][name atom]
//! [num_fields × key atom]                 # DECLARATION order
//! [num_fields × value term]               # DECLARATION order
//! ```
//!
//! `flags` is `0` for `is_exported == false` and `1` for `true`; any other byte is
//! `BeamError::BadArg`. Every key must be an atom. The frozen interior block (the
//! sort rule, `field_order`, the definition/instance word offsets) is owned by
//! [`beam_term::record`]; this module only reads the wire, validates it and forwards
//! to the [`crate::term`] forwarding layer, which allocates the block from the
//! [`beam_alloc::Heap`].
//!
//! This module is safe Rust, like its parent: all raw-heap writes stay inside the
//! [`crate::term`] subtree.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{atom_val, make_small, MAX_ARITYVAL};
use beam_term::{AtomTable, Eterm};

use crate::error::bad_arg;
use crate::term::{make_record, record_def_size_words};

use super::Decoder;

impl<'a> Decoder<'a> {
    /// Decodes `RECORD_EXT` (`67`) into a native-record instance
    /// (`external.c:5257-5381`).
    ///
    /// The `flags` byte is `0` (`is_exported == false`) or `1` (`true`); anything
    /// else is `BadArg`. Every key is required to be an atom. The `num_fields` keys
    /// and values are read in **declaration** order and the block is allocated
    /// **after** all values are decoded, so a malformed tail never leaves a
    /// half-built record. A field count whose definition arity would overflow
    /// `MAX_ARITYVAL` is a `BeamError::BadArg` (checked before any allocation, as
    /// the C routes every decode failure — bounds included — to `B2TBadArg`,
    /// `external.c:1989-1991`); a duplicate key is `BadArg` too.
    ///
    /// **Cached definition hash.** The definition's word 2 carries
    /// `make_hash2({module,name,is_exported,keys-in-declaration-order})` — exactly what
    /// C stores (not on the wire) — computed by [`crate::term::make_record`] from the
    /// decoded keys and the supplied `atoms` table. It is not wire-observable, but it
    /// *is* part of the record's `erlang:phash2/1`, so it must be the real value.
    pub(super) fn decode_record(&mut self, heap: &mut Heap, atoms: &mut AtomTable) -> Result<Eterm> {
        let num_fields = self.reader.u32be()? as usize;
        let is_exported = match self.reader.u8()? {
            0 => false,
            1 => true,
            // The C accepts only `am_false`/`am_true` (`external.c:5286-5296`).
            _ => return Err(bad_arg()),
        };
        // `module` and `name` are atoms; the layout needs the raw `Atom`, so keep the
        // term (which is `make_atom(atom)`) and unwrap its value below. C routes both
        // through `dec_atom` (`external.c:5301-5313`), which has no `NIL_EXT` case — so
        // the internal-node marker is `BadArg` here even under `LOCAL_EXT` mode.
        let module = self.decode_atom_strict(heap, atoms)?;
        let name = self.decode_atom_strict(heap, atoms)?;
        // A count whose definition arity would not fit the header's arity field is a
        // `BadArg` (the wire can declare more fields than a term can hold): the C
        // routes every decode failure, bounds included, to `B2TBadArg`
        // (`external.c:1989-1991`), so the oracle answers `badarg`, never
        // `system_limit`. This is the one `write_record` size guard, checked before the
        // block is allocated (and before the loop below), so the remaining failure from
        // `make_record` can only be a duplicate key (`BadArg`).
        if record_def_size_words(num_fields).saturating_sub(1) > MAX_ARITYVAL {
            return Err(bad_arg());
        }

        // Keys and values are both in declaration order; a key must be an atom
        // (`dec_atom` again, `external.c:5314-5321`).
        // Decoding with `Vec::new` (never `with_capacity`) keeps a lying `num_fields`
        // from driving an eager allocation: the reader runs out of bytes first.
        let mut fields: Vec<(Eterm, Eterm)> = Vec::new();
        for _ in 0..num_fields {
            let key = self.decode_atom_strict(heap, atoms)?;
            fields.push((key, make_small(0)));
        }        for field in fields.iter_mut() {
            field.1 = self.decode(heap, atoms)?;
        }

        // The block is allocated (and written) last, after every value is
        // materialized; `make_record` maps a duplicate key to `BadArg`.
        make_record(
            heap,
            atoms,
            atom_val(module),
            atom_val(name),
            is_exported,
            &fields,
        )
    }
}

#[cfg(test)]
mod tests {
    //! `RECORD_EXT` decode tests: the exact oracle-shaped wire image, the field
    //! count / flags / key rules and the negative cases.
    //!
    //! Tests are allowed to `unwrap` (a panic there is a test failure); the
    //! production body above never does.

    use super::*;

    use beam_common::BeamError;
    use beam_term::prelude::make_atom;

    use crate::decode::tests::{assert_bad_arg, decode};
    use crate::tag::RECORD_EXT;
    use crate::term::{
        is_record, record_declaration_keys, record_definition, record_definition_is_exported,
        record_definition_keys, record_definition_module, record_definition_name,
        record_field_count, record_values, AM_FALSE, AM_TRUE,
    };

    /// Builds the `[RECORD_EXT][n][flags][module][name][keys…][values…]` image from
    /// declaration-order atoms and values (each term encoded with `encode_term_body`,
    /// the atom names interned in `atoms`).
    fn image(
        atoms: &mut AtomTable,
        module: &str,
        name: &str,
        is_exported: bool,
        fields: &[(&str, Eterm)],
    ) -> Vec<u8> {
        let mut bytes = vec![131u8, RECORD_EXT];
        bytes.extend_from_slice(&(fields.len() as u32).to_be_bytes());
        bytes.push(u8::from(is_exported));
        for atom in [module, name] {
            let term = make_atom(atoms.intern(atom));
            bytes.extend_from_slice(&crate::encode::encode_term_body(term, atoms).unwrap());
        }
        for (key, _) in fields {
            let term = make_atom(atoms.intern(key));
            bytes.extend_from_slice(&crate::encode::encode_term_body(term, atoms).unwrap());
        }
        for (_, value) in fields {
            bytes.extend_from_slice(&crate::encode::encode_term_body(*value, atoms).unwrap());
        }
        bytes
    }

    #[test]
    fn record_ext_decodes_every_field() {
        let mut atoms = AtomTable::new();
        atoms.seed(["false", "true"]);
        atoms.seed(["m", "r", "a", "b"]);
        let image = image(
            &mut atoms,
            "m",
            "r",
            true,
            &[("b", make_small(22)), ("a", make_small(11))],
        );

        let mut heap = Heap::new(256).expect("a heap");
        let mut decoder = Decoder::new(&image).unwrap();
        let term = decoder.decode(&mut heap, &mut atoms).unwrap();
        decoder.finish().unwrap();

        assert!(is_record(term));
        assert_eq!(record_field_count(term).unwrap(), 2);
        let def = record_definition(term).unwrap();
        assert_eq!(atoms.name(atom_val(record_definition_module(def).unwrap())), Some("m"));
        assert_eq!(atoms.name(atom_val(record_definition_name(def).unwrap())), Some("r"));
        assert_eq!(record_definition_is_exported(def).unwrap(), make_atom(AM_TRUE));
        // Keys come back sorted by atom index (`a` before `b`), values in that order.
        let keys = record_definition_keys(def).unwrap();
        assert_eq!(atoms.name(atom_val(keys[0])), Some("a"));
        assert_eq!(atoms.name(atom_val(keys[1])), Some("b"));
        assert_eq!(record_values(term).unwrap(), &[make_small(11), make_small(22)]);
        // Declaration order is recovered through `field_order`: declared 0 is `b`.
        let decl = record_declaration_keys(def).unwrap();
        assert_eq!(atoms.name(atom_val(decl[0])), Some("b"));
        assert_eq!(atoms.name(atom_val(decl[1])), Some("a"));

        // A decoded record carries the **real** cached definition hash —
        // `make_hash2({module, name, is_exported, keys-in-declaration-order})` — and
        // not the old `0` placeholder. The hash is not on the wire, so it is computed
        // from what the wire carried; its observable consequence is that
        // `make_hash2` of the decoded record (which mixes definition word 2) is the C
        // value for this fixture. (`record_definition_hash` is not on this module's
        // safe surface; the term-layer test
        // `make_record_stores_the_real_definition_hash` reads the word itself.)
        assert_eq!(
            beam_term::hashing::make_hash2(term, &atoms).unwrap(),
            2_292_018_936,
            "the decoded record must carry its real cached definition hash"
        );
    }

    #[test]
    fn zero_fields_and_not_exported_round_trip() {
        let mut atoms = AtomTable::new();
        atoms.seed(["false", "true"]);
        atoms.seed(["m", "r"]);
        let image = image(&mut atoms, "m", "r", false, &[]);
        let (_heap, atoms, term) = decode(&image);
        assert_eq!(record_field_count(term).unwrap(), 0);
        let def = record_definition(term).unwrap();
        assert_eq!(atoms.name(atom_val(record_definition_name(def).unwrap())), Some("r"));
        assert_eq!(record_definition_is_exported(def).unwrap(), make_atom(AM_FALSE));
        assert!(record_values(term).unwrap().is_empty());
    }

    #[test]
    fn malformed_record_ext_is_rejected() {
        // The flags byte must be exactly 0 or 1.
        assert_bad_arg(&[131, RECORD_EXT, 0, 0, 0, 0, 2, 119, 1, b'm', 119, 1, b'r']);
        // A module field that is not an atom.
        let mut not_atom = vec![131u8, RECORD_EXT];
        not_atom.extend_from_slice(&0u32.to_be_bytes());
        not_atom.push(0);
        not_atom.push(97); // SMALL_INTEGER_EXT — not an atom
        not_atom.push(1);
        not_atom.extend_from_slice(&[119, 1, b'r']);
        assert_bad_arg(&not_atom);
        // A key that is not an atom.
        let mut bad_key = vec![131u8, RECORD_EXT];
        bad_key.extend_from_slice(&1u32.to_be_bytes());
        bad_key.push(0);
        bad_key.extend_from_slice(&[119, 1, b'm', 119, 1, b'r']);
        bad_key.push(97); // key — not an atom
        bad_key.push(1);
        bad_key.push(97); // value
        bad_key.push(2);
        assert_bad_arg(&bad_key);

        // Truncation at every offset never panics and is always a typed error.
        let mut seed = AtomTable::new();
        seed.seed(["false", "true"]);
        seed.seed(["m", "r", "a"]);
        let full = image(&mut seed, "m", "r", true, &[("a", make_small(1))]);
        for len in 0..full.len() {
            let mut heap = Heap::new(256).expect("a heap");
            let mut atoms = AtomTable::new();
            if let Ok(mut decoder) = Decoder::new(&full[..len]) {
                let _ = decoder.decode(&mut heap, &mut atoms);
            }
        }
    }

    #[test]
    fn duplicate_keys_are_bad_arg() {
        let mut atoms = AtomTable::new();
        atoms.seed(["false", "true"]);
        atoms.seed(["m", "r", "a"]);
        let image = image(&mut atoms, "m", "r", false, &[("a", make_small(1)), ("a", make_small(2))]);
        assert_bad_arg(&image);
    }

    #[test]
    fn a_field_count_past_max_arityval_is_bad_arg() {
        // fields than the record header's arity field can hold. A wire bound, so the        // oracle's `binary_to_term` answers `badarg` (never `system_limit`).
        let mut image = vec![131u8, RECORD_EXT];
        image.extend_from_slice(&u32::MAX.to_be_bytes());
        image.push(0);
        image.extend_from_slice(&[119, 1, b'm', 119, 1, b'r']);
        let mut heap = Heap::new(256).expect("a heap");
        let mut atoms = AtomTable::new();
        let mut decoder = Decoder::new(&image).unwrap();
        assert!(matches!(
            decoder.decode(&mut heap, &mut atoms),
            Err(BeamError::BadArg)
        ));
    }

    /// `RECORD_EXT`'s module, name and key atoms go through C `dec_atom`
    /// (`external.c:5301-5321`), which has no `NIL_EXT` case — so the internal-node
    /// marker is `badarg` in those positions even under `LOCAL_EXT` mode, exactly as in
    /// `EXPORT_EXT`. Without the marker the record still decodes in internal-node mode.
    #[test]
    fn record_ext_rejects_the_internal_node_marker_as_an_atom() {
        const HASH: u32 = 0x0BAD_F00D;
        // `[LOCAL_EXT][hash:4][RECORD_EXT][0 fields][flags 0][<module>][<name>]`.
        let with_module = |module: &[u8]| {
            let mut image = vec![131u8, 121];
            image.extend_from_slice(&HASH.to_be_bytes());
            image.push(RECORD_EXT);
            image.extend_from_slice(&0u32.to_be_bytes());
            image.push(0);
            image.extend_from_slice(module);
            image.extend_from_slice(&[119, 1, b'r']);
            image
        };
        for (what, module) in [
            ("module", &[106u8][..]),   // NIL_EXT
            ("name", &[119, 1, b'm'][..]),
        ] {
            let mut image = with_module(module);
            if what == "name" {
                // Replace the trailing name with the marker instead.
                image.truncate(image.len() - 3); // drop `[119,1,'r']`
                image.push(106);
            }
            let mut heap = Heap::new(256).expect("a heap");
            let mut atoms = AtomTable::new();
            let mut decoder = Decoder::new(&image)
                .expect("well-formed image")
                .with_local_node_hash(HASH);
            assert!(
                matches!(decoder.decode(&mut heap, &mut atoms), Err(BeamError::BadArg)),
                "a NIL {what} must be badarg"
            );
        }

        // A key in the marker position is `badarg` too:
        // `[LOCAL_EXT][hash][RECORD_EXT][1 field][0][module][name][NIL key][value]`.
        let mut bad_key = vec![131u8, 121];
        bad_key.extend_from_slice(&HASH.to_be_bytes());
        bad_key.push(RECORD_EXT);
        bad_key.extend_from_slice(&1u32.to_be_bytes());
        bad_key.push(0);
        bad_key.extend_from_slice(&[119, 1, b'm', 119, 1, b'r']);
        bad_key.push(106); // NIL_EXT as the key
        bad_key.extend_from_slice(&[97, 1]); // the value
        let mut heap = Heap::new(256).expect("a heap");
        let mut atoms = AtomTable::new();
        let mut decoder = Decoder::new(&bad_key)
            .expect("well-formed image")
            .with_local_node_hash(HASH);
        assert!(matches!(
            decoder.decode(&mut heap, &mut atoms),
            Err(BeamError::BadArg)
        ));

        // A real record behind a `LOCAL_EXT` wrapper still decodes.
        let mut good = vec![131u8, 121];
        good.extend_from_slice(&HASH.to_be_bytes());
        good.push(RECORD_EXT);
        good.extend_from_slice(&0u32.to_be_bytes());
        good.push(0);
        good.extend_from_slice(&[119, 1, b'm', 119, 1, b'r']);
        let mut heap = Heap::new(256).expect("a heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["false", "true"]);
        let mut decoder = Decoder::new(&good)
            .expect("well-formed image")
            .with_local_node_hash(HASH);
        let term = decoder.decode(&mut heap, &mut atoms).expect("decodes");
        decoder.finish().expect("no trailing bytes");
        assert!(is_record(term));
        assert_eq!(record_field_count(term).unwrap(), 0);
    }
}