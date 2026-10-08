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

//! The native-record encoder of [`Encoder`]: `RECORD_EXT` (`67`).
//!
//! [`Encoder::encode`]'s classification chain (in the parent [`crate::encode`]
//! module) probes the native-record family with [`crate::term::record::is_record`]
//! **before** the other boxed families — a record header carries a distinct subtag
//! ([`beam_term::tag::RECORD_SUBTAG`]), so the classification cannot overlap — and
//! dispatches here. The encoder lives in this child module as a second `impl` block
//! on the encoder, so it reaches its private output buffer, atom writer and
//! integer-width helper directly.
//!
//! Wire format (`external.c:4006-4036`):
//!
//! ```text
//! [67][num_fields:4 BE][flags:1][module atom][name atom]
//! [num_fields × key atom]                 # DECLARATION order
//! [num_fields × value term]               # DECLARATION order
//! ```
//!
//! This module is safe Rust, like its parent.

use beam_common::Result;
use beam_term::prelude::{is_atom, is_small, make_atom, unsigned_value};
use beam_term::Eterm;

use crate::error;
use crate::tag::RECORD_EXT;
use crate::term::{
    record_declaration_keys, record_definition, record_definition_field_order,
    record_definition_is_exported, record_definition_module, record_definition_name,
    record_field_count, record_values, AM_FALSE,
};

use super::Encoder;

impl<'a> Encoder<'a> {
    /// Encodes a native record as `RECORD_EXT` (`67`) (`external.c:4006-4036`).
    ///
    /// The definition supplies the module, the name and the `is_exported` flag, and
    /// the wire emits the keys and then the values in **declaration** order: the
    /// declared `d`-th value is `values[unsigned_val(field_order[d])]` (the instance
    /// stores values in sorted-key order; `field_order` maps declaration → sorted
    /// rank, `erl_record.c:1072-1073`).
    ///
    /// A malformed record block, a non-atom module/name/key or an inconsistent field
    /// count yields `BeamError::BadArg`, never a mis-encoded term.
    pub(super) fn encode_record(&mut self, term: Eterm) -> Result<()> {
        let n = record_field_count(term)?;
        let def = record_definition(term)?;
        let module = record_definition_module(def)?;
        let name = record_definition_name(def)?;
        let is_exported = record_definition_is_exported(def)?;
        let declaration_keys = record_declaration_keys(def)?;
        let field_order = record_definition_field_order(def)?;
        let values = record_values(term)?;

        // A well-formed record's definition and instance agree on the field count.
        if declaration_keys.len() != n || field_order.len() != n || values.len() != n {
            return Err(error::bad_arg());
        }
        // The module and the name are atoms on the wire (`enc_atom`).
        if !is_atom(module) || !is_atom(name) {
            return Err(error::bad_arg());
        }
        let n32 = u32::try_from(n).map_err(|_| error::system_limit())?;

        self.buf.u8(RECORD_EXT);
        self.buf.u32be(n32);
        // The C writes `is_exported == am_false ? 0 : 1` (`external.c:4021`); the flag
        // is exactly the atom `true`/`false` the definition stored.
        let flags = if is_exported == make_atom(AM_FALSE) { 0 } else { 1 };
        self.buf.u8(flags);
        self.encode_atom(module)?;
        self.encode_atom(name)?;
        // Keys in declaration order (each an atom, `enc_atom`).
        for &key in &declaration_keys {
            if !is_atom(key) {
                return Err(error::bad_arg());
            }
            self.encode_atom(key)?;
        }
        // Values in declaration order, through the declaration → sorted-rank map.
        for &rank in field_order {
            if !is_small(rank) {
                return Err(error::bad_arg());
            }
            let index = unsigned_value(rank) as usize;
            let value = *values.get(index).ok_or_else(error::bad_arg)?;
            self.encode(value)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Byte-exact `RECORD_EXT` encoder tests: the exact wire bytes for a record whose
    //! declaration order differs from its atom-index order, plus the malformed cases.

    use beam_alloc::Heap;
    use beam_term::prelude::{make_small, AtomTable};

    use crate::encode::encode_term_body;
    use crate::term::{make_record, record_block_words};

    /// A table seeded like the standard `atom.names` (`false`, `true` first), with
    /// `m`, `r`, `a`, `b` interned in a fixed order (`a` before `b`).
    fn atoms() -> AtomTable {
        let mut table = AtomTable::new();
        table.seed(["false", "true"]);
        table.seed(["m", "r", "a", "b"]);
        table
    }

    #[test]
    fn record_encodes_in_declaration_order() {
        let t = atoms();
        let m = t.lookup("m").unwrap();
        let r = t.lookup("r").unwrap();
        let a = t.lookup("a").unwrap();
        let b = t.lookup("b").unwrap();
        assert!(a.raw() < b.raw(), "atom `a` must precede `b` by index");

        let mut heap = Heap::new(record_block_words(2)).expect("a sized heap");
        // Declared [(b, 22), (a, 11)] — the reverse of the atom-index order.
        let fields = [
            (beam_term::prelude::make_atom(b), make_small(22)),
            (beam_term::prelude::make_atom(a), make_small(11)),
        ];
        let rec = make_record(&mut heap, &t, m, r, false, &fields).unwrap();

        let bytes = encode_term_body(rec, &t).unwrap();
        let mut expected = vec![67u8];
        expected.extend_from_slice(&2u32.to_be_bytes()); // num_fields
        expected.push(0); // is_exported = false
        expected.extend_from_slice(&[119, 1, b'm']); // module
        expected.extend_from_slice(&[119, 1, b'r']); // name
        // Keys in declaration order: b, then a.
        expected.extend_from_slice(&[119, 1, b'b']);
        expected.extend_from_slice(&[119, 1, b'a']);
        // Values in declaration order: 22 (b), then 11 (a).
        expected.extend_from_slice(&[97, 22, 97, 11]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn exported_flag_is_one_and_zero_fields_is_allowed() {
        let t = atoms();
        let m = t.lookup("m").unwrap();
        let r = t.lookup("r").unwrap();

        // A zero-field exported record.
        let mut heap = Heap::new(record_block_words(0)).expect("a sized heap");
        let rec = make_record(&mut heap, &t, m, r, true, &[]).unwrap();
        let mut expected = vec![67u8];
        expected.extend_from_slice(&0u32.to_be_bytes());
        expected.push(1); // exported
        expected.extend_from_slice(&[119, 1, b'm']);
        expected.extend_from_slice(&[119, 1, b'r']);
        assert_eq!(encode_term_body(rec, &t).unwrap(), expected);
    }

    #[test]
    fn non_records_are_not_routed_here() {
        // `is_record` must be false for a plain tuple, so `encode` falls through to
        // the tuple family (this is the probe-order guarantee).
        let t = atoms();
        let mut heap = Heap::new(64).expect("a small heap");
        let tuple = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        assert!(!crate::term::is_record(tuple));
        assert_eq!(
            encode_term_body(tuple, &t).unwrap(),
            vec![104, 2, 97, 1, 97, 2]
        );
    }
}
