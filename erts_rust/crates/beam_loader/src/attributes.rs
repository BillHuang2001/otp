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

//! The decoded `Attr` chunk: the module's attributes as a real Erlang term.
//!
//! The `Attr` chunk payload is `term_to_binary(Attributes)` — the module's
//! non-standard attributes as an external term (`beam_asm.erl:307-310`:
//! `term_to_binary(set_vsn_attribute(Attr, MD5))`). The runtime decodes it into a
//! term on demand for `module_info(attributes)` (`erl_bif_info.c:5873-5894`,
//! `attributes_for_module` → `erts_decode_ext`), so a round-trip through
//! [`beam_codec::decode_term`] must reproduce exactly what the oracle's
//! `Module:module_info(attributes)` returns (a `[{Key, Value}]` property list).
//!
//! [`crate::chunk::parse`] already keeps the *raw* bytes on
//! [`crate::ir::Module::attributes`]; this module adds the **decoded** term
//! ([`crate::ir::Module::attributes_term`]) additively, sharing the arena-sizing and
//! heap-growth retry of [`crate::literal`] so there is one decode path.
//!
//! Like [`crate::literal::LiteralArea`] the table owns a [`beam_alloc::Heap`] (a raw
//! pointer), so it is **neither `Send` nor `Sync`** and is shared through an [`Arc`]
//! ([`SharedAttributeTable`]) so a [`Module`](crate::ir::Module) clone stays cheap.
use std::sync::Arc;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::{AtomTable, Eterm};

use crate::literal::decode_terms;

/// The module's attributes, decoded into a private immutable heap.
///
/// A module without an `Attr` chunk (or with an empty one) has `term() == None` and
/// allocates nothing.
pub struct AttributeTable {
    /// The arena the decoded term lives in; `None` exactly when `term` is `None`.
    heap: Option<Heap>,
    /// The decoded attribute term (the property list), if the chunk was present.
    term: Option<Eterm>,
}

impl AttributeTable {
    /// A table with no attributes (`Attr` absent or empty).
    pub fn empty() -> AttributeTable {
        AttributeTable {
            heap: None,
            term: None,
        }
    }

    /// Decodes the raw `Attr` payload into an attribute table.
    ///
    /// `raw` is the chunk payload (`None` when the chunk is absent); `atoms` is the
    /// module's atom table (the attribute keys are atoms in it — interning is
    /// idempotent, so passing the table the module itself uses keeps atom identity
    /// consistent). An empty payload yields [`AttributeTable::empty`]; a malformed
    /// payload yields the decoder's typed [`beam_common::BeamError`] — never a panic
    /// and never a substituted value.
    pub fn decode(raw: Option<&[u8]>, atoms: &mut AtomTable) -> Result<AttributeTable> {
        let raw = match raw {
            Some(bytes) if !bytes.is_empty() => bytes,
            _ => return Ok(AttributeTable::empty()),
        };
        let (heap, terms) = decode_terms(&[raw.to_vec()], atoms)?;
        // `decode_terms` returns exactly one term per payload; the `Attr` chunk is a
        // single payload, so this is always `Some`. Keep the `pop` defensive: a future
        // multi-payload caller must not panic here.
        let term = terms.into_iter().next();
        Ok(AttributeTable {
            heap: term.map(|_| heap),
            term,
        })
    }

    /// The decoded attribute list term, or `None` when the chunk was absent/empty.
    ///
    /// This is exactly the value the oracle's `Module:module_info(attributes)` returns.
    pub fn term(&self) -> Option<Eterm> {
        self.term
    }

    /// Whether this table holds no attributes.
    pub fn is_empty(&self) -> bool {
        self.term.is_none()
    }

    /// The heap the decoded term lives in (introspection and tests), if any.
    pub fn heap(&self) -> Option<&Heap> {
        self.heap.as_ref()
    }

    /// The value of the first `{Key, Value}` attribute whose key is the atom term
    /// `key`, or `None` when the list carries no such attribute.
    ///
    /// The list is walked defensively: a non-list tail, a non-tuple element or a tuple
    /// that is not a 2-tuple simply ends the search (`None`) — a malformed attribute
    /// list is never a panic.
    pub fn lookup(&self, key: Eterm) -> Option<Eterm> {
        let mut rest = self.term?;
        loop {
            if beam_term::is_nil(rest) {
                return None;
            }
            let head = match beam_codec::term::cons_head(rest) {
                Ok(head) => head,
                Err(_) => return None,
            };
            if let Some(value) = pair_value(head, key) {
                return Some(value);
            }
            rest = match beam_codec::term::cons_tail(rest) {
                Ok(tail) => tail,
                Err(_) => return None,
            };
        }
    }
}

/// The value of a `{key, Value}` 2-tuple `element`, or `None` when `element` is not a
/// 2-tuple keyed by `key`.
fn pair_value(element: Eterm, key: Eterm) -> Option<Eterm> {
    if beam_codec::term::tuple_arity(element).ok()? != 2 {
        return None;
    }
    let head = beam_codec::term::tuple_element(element, 0).ok()?;
    if head != key {
        return None;
    }
    beam_codec::term::tuple_element(element, 1).ok()
}

impl Default for AttributeTable {
    /// An empty table (`Module::default()` must not allocate).
    fn default() -> AttributeTable {
        AttributeTable::empty()
    }
}

impl core::fmt::Debug for AttributeTable {
    /// A compact, meaningful rendering: neither [`Eterm`] nor [`Heap`] has a useful
    /// `Debug` (the heap is a raw pointer).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AttributeTable")
            .field("has_term", &self.term.is_some())
            .field("heap_words", &self.heap.as_ref().map_or(0, Heap::used))
            .finish_non_exhaustive()
    }
}

/// A shared, immutable attribute table — the type of
/// [`crate::ir::Module::attribute_table`] and the return type of
/// [`crate::ir::Module::attributes_term`]'s backing storage.
///
/// The `Arc` is sound because the table is write-once at load time and only read
/// afterwards. It is *not* `Send`/`Sync` (the heap holds a raw pointer), so a loaded
/// module is confined to its loading thread.
pub type SharedAttributeTable = Arc<AttributeTable>;

#[cfg(test)]
mod tests {
    //! Attribute decoding: the hand-built payload, the oracle-shaped `gen_server`
    //! attributes, the `lookup` walk and its defensive edges, and the malformed case.
    //! Tests are allowed to `unwrap` (a panic there is a test failure).

    use super::*;
    use beam_common::BeamError;
    use beam_term::prelude::make_atom;

    /// `term_to_binary([{vsn,[200]},{behaviour,[sys]}])` — the external form the
    /// compiler writes into `Attr` (a list of 2-tuples of `{atom, term}`).
    fn sample_attributes() -> Vec<u8> {
        let mut bytes = vec![131, 108, 0, 0, 0, 2]; // VERSION_MAGIC, LIST_EXT, 2 elems
        // {vsn,[200]}
        bytes.extend_from_slice(&[104, 2, 119, 3, b'v', b's', b'n']);
        bytes.extend_from_slice(&[108, 0, 0, 0, 1, 97, 200, 106]);
        // {behaviour,[sys]}
        bytes.extend_from_slice(&[104, 2, 119, 9]);
        bytes.extend_from_slice(b"behaviour");
        bytes.extend_from_slice(&[108, 0, 0, 0, 1, 119, 3, b's', b'y', b's', 106]);
        bytes.push(106); // NIL tail
        bytes
    }

    #[test]
    fn decodes_a_hand_built_attribute_list() {
        let mut atoms = AtomTable::new();
        let table = AttributeTable::decode(Some(&sample_attributes()), &mut atoms).unwrap();

        assert!(!table.is_empty());
        assert!(table.heap().is_some());
        let term = table.term().unwrap();

        // The whole list: `[{vsn,[1234]},{behaviour,[sys]}]`.
        let first = beam_codec::term::cons_head(term).unwrap();
        assert_eq!(beam_codec::term::tuple_arity(first).unwrap(), 2);
        assert_eq!(table.lookup(make_atom(atoms.intern("vsn"))).is_some(), true);
        assert_eq!(
            table.lookup(make_atom(atoms.intern("behaviour"))).unwrap(),
            beam_codec::term::tuple_element(
                beam_codec::term::cons_head(beam_codec::term::cons_tail(term).unwrap()).unwrap(),
                1
            )
            .unwrap()
        );

        // A key that is not present is `None`, never a panic.
        assert_eq!(table.lookup(make_atom(atoms.intern("absent"))), None);
    }

    #[test]
    fn absent_or_empty_payloads_yield_an_empty_table() {
        let mut atoms = AtomTable::new();
        for raw in [None, Some(&[][..])] {
            let table = AttributeTable::decode(raw, &mut atoms).unwrap();
            assert!(table.is_empty());
            assert_eq!(table.term(), None);
            assert_eq!(table.lookup(make_atom(atoms.intern("vsn"))), None);
            assert!(table.heap().is_none(), "no term means no heap");
        }

        let default = AttributeTable::default();
        assert!(default.is_empty());
        assert_eq!(
            format!("{default:?}"),
            "AttributeTable { has_term: false, heap_words: 0, .. }"
        );
    }

    #[test]
    fn a_malformed_payload_is_a_typed_error() {
        let mut atoms = AtomTable::new();
        // A truncated `[{vsn,[1]}]`: the decoder must reject it, not panic.
        let truncated = vec![131, 108, 0, 0, 0, 1, 104, 2, 119, 3, b'v', b's'];
        let err = AttributeTable::decode(Some(&truncated), &mut atoms).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");

        // A payload that is not an external term at all (no `VERSION_MAGIC`).
        let err = AttributeTable::decode(Some(&[0, 1, 2, 3]), &mut atoms).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    }

    #[test]
    fn lookup_tolerates_a_non_property_list() {
        let mut atoms = AtomTable::new();
        // `[1, {vsn,[7]}]` — the head is not a tuple, so the walk must skip it and
        // still find `vsn` (never panic on the malformed element).
        let mut bytes = vec![131, 108, 0, 0, 0, 2, 97, 1, 104, 2, 119, 3, b'v', b's', b'n'];
        bytes.extend_from_slice(&[108, 0, 0, 0, 1, 97, 7, 106]);
        bytes.push(106);
        let table = AttributeTable::decode(Some(&bytes), &mut atoms).unwrap();
        assert!(table.lookup(make_atom(atoms.intern("vsn"))).is_some());
    }

    #[test]
    fn a_shared_table_is_cheap_to_clone() {
        let mut atoms = AtomTable::new();
        let shared: SharedAttributeTable =
            Arc::new(AttributeTable::decode(Some(&sample_attributes()), &mut atoms).unwrap());
        let clone = Arc::clone(&shared);
        assert!(Arc::ptr_eq(&shared, &clone));
        assert!(clone.term().is_some());
    }
}
