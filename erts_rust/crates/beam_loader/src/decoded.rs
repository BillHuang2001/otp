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

//! The reusable holder for a `.beam` chunk whose payload is a single external term.
//!
//! Three small chunks carry their payload as `term_to_binary(Term)` and are decoded
//! into a real Erlang term at load time:
//!
//! * **`Attr`** — `term_to_binary(set_vsn_attribute(Attr, MD5))`
//!   (`lib/compiler/src/beam_asm.erl:307-310`), the value
//!   `Module:module_info(attributes)` serves (the C decodes it on demand:
//!   `erts/emulator/beam/erl_bif_info.c:5873-5894`, `attributes_for_module` →
//!   `erts_decode_ext`). Held by [`crate::attributes::AttributeTable`].
//! * **`CInf`** — `term_to_binary([{version,?COMPILER_VSN}|Compile])`
//!   (`lib/compiler/src/beam_asm.erl:236,309`), the value `Module:module_info(compile)`
//!   serves (the C decodes it the same way: `erl_bif_info.c:5896-5918`,
//!   `compilation_info_for_module` → `erts_decode_ext`).
//! * **`Meta`** — `term_to_binary(MetaData, [deterministic])`
//!   (`lib/compiler/src/compile.erl:2274-2295`, the `?META_CHUNK_NAME` extra chunk),
//!   the compiler's feature metadata (e.g. `[{enabled_features,[maybe_expr]}]`); the C
//!   only feeds the raw bytes to the module checksum (`beam_file.c:1811-1815`).
//!
//! All three are read by the *reader* as a bare `binary_to_term` of the chunk payload
//! — `lib/stdlib/src/beam_lib.erl:1063-1077` (`chunk_to_data/6`: `attributes` and
//! `compile_info` are `binary_to_term(Chunk)`, and `Meta` is reached as a raw chunk) —
//! so there is exactly one decode path and one holder for them.
//!
//! [`DecodedTerm`] is that holder: the payload decoded into a private immutable
//! [`beam_alloc::Heap`], write-once at load time and shared behind an [`Arc`]
//! ([`SharedDecodedTerm`]) so a [`Module`](crate::ir::Module) clone stays cheap. Like
//! [`crate::literal::LiteralArea`] it owns a raw-pointer heap, so it is **neither `Send`
//! nor `Sync`** and a loaded module is confined to its loading thread.
//!
//! Absence is not an error: a module without the chunk (or with an empty one) holds
//! [`DecodedTerm::empty`] and allocates nothing. A *malformed* payload is the
//! decoder's typed [`beam_common::BeamError`] — never a panic, never a substituted
//! value.
use std::sync::Arc;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::{AtomTable, Eterm};

use crate::literal::decode_terms;

/// One `.beam` chunk payload (`Attr` / `CInf` / `Meta`) decoded into a private
/// immutable heap.
///
/// A module without the chunk (or with an empty one) has `term() == None` and
/// allocates nothing.
pub struct DecodedTerm {
    /// The arena the decoded term lives in; `None` exactly when `term` is `None`.
    heap: Option<Heap>,
    /// The decoded term, if the chunk was present and non-empty.
    term: Option<Eterm>,
}

impl DecodedTerm {
    /// A holder with no term (`Attr`/`CInf`/`Meta` absent or empty).
    pub fn empty() -> DecodedTerm {
        DecodedTerm {
            heap: None,
            term: None,
        }
    }

    /// Decodes a chunk payload that is a single external term into a [`DecodedTerm`].
    ///
    /// `raw` is the chunk payload (`None` when the chunk is absent); `atoms` is the
    /// module's atom table (the term's atoms are interned into it — interning is
    /// idempotent, so passing the table the module itself uses keeps atom identity
    /// consistent). An empty payload yields [`DecodedTerm::empty`]; a malformed payload
    /// yields the decoder's typed [`beam_common::BeamError`].
    pub fn decode(raw: Option<&[u8]>, atoms: &mut AtomTable) -> Result<DecodedTerm> {
        let raw = match raw {
            Some(bytes) if !bytes.is_empty() => bytes,
            _ => return Ok(DecodedTerm::empty()),
        };
        let (heap, terms) = decode_terms(&[raw.to_vec()], atoms)?;
        // `decode_terms` returns exactly one term per payload; this call passes a single
        // payload, so this is always `Some`. Keep the `pop` defensive: a future
        // multi-payload caller must not panic here.
        let term = terms.into_iter().next();
        Ok(DecodedTerm {
            heap: term.map(|_| heap),
            term,
        })
    }

    /// The decoded term, or `None` when the chunk was absent/empty.
    pub fn term(&self) -> Option<Eterm> {
        self.term
    }

    /// Whether this holder carries no term.
    pub fn is_empty(&self) -> bool {
        self.term.is_none()
    }

    /// The heap the decoded term lives in (introspection and tests), if any.
    pub fn heap(&self) -> Option<&Heap> {
        self.heap.as_ref()
    }

    /// The value of the first `{Key, Value}` pair in the decoded property list whose key
    /// is the atom term `key`, or `None` when the list carries no such pair.
    ///
    /// `Attr`, `CInf` and `Meta` are all property lists (`[{Key, Value}]`), so this walk
    /// is shared. It is defensive: a non-list tail, a non-tuple element or a tuple that
    /// is not a 2-tuple simply ends the search (`None`) — a malformed property list is
    /// never a panic.
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

impl Default for DecodedTerm {
    /// An empty holder (`Module::default()` must not allocate).
    fn default() -> DecodedTerm {
        DecodedTerm::empty()
    }
}

impl core::fmt::Debug for DecodedTerm {
    /// A compact, meaningful rendering: neither [`Eterm`] nor [`Heap`] has a useful
    /// `Debug` (the heap is a raw pointer).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        fmt_holder(f, "DecodedTerm", self.term, self.heap.as_ref())
    }
}

/// Renders a decoded-term holder as `<name> { has_term: …, heap_words: …, .. }`.
///
/// Shared with the named holders that wrap a [`DecodedTerm`]
/// ([`crate::attributes::AttributeTable`]) so their `Debug` says what they are.
pub(crate) fn fmt_holder(
    f: &mut core::fmt::Formatter<'_>,
    name: &str,
    term: Option<Eterm>,
    heap: Option<&Heap>,
) -> core::fmt::Result {
    f.debug_struct(name)
        .field("has_term", &term.is_some())
        .field("heap_words", &heap.map_or(0, Heap::used))
        .finish_non_exhaustive()
}

/// A shared, immutable decoded term — the type of
/// [`crate::ir::Module::compile_info_table`] / [`crate::ir::Module::meta_table`].
///
/// The `Arc` is sound because the holder is write-once at load time and only read
/// afterwards. It is *not* `Send`/`Sync` (the heap holds a raw pointer), so a loaded
/// module is confined to its loading thread.
pub type SharedDecodedTerm = Arc<DecodedTerm>;

#[cfg(test)]
mod tests {
    //! Decoded-term decoding: the hand-built payloads, the lookup walk and its
    //! defensive edges, the absent/empty/malformed cases and the cheap `Arc` clone.
    //! Tests are allowed to `unwrap` (a panic there is a test failure).

    use super::*;
    use beam_common::BeamError;
    use beam_term::make_atom;

    /// `term_to_binary([{version,"1.2.3"},{source,"a.erl"}])` — the shape of a real
    /// `CInf` payload (`beam_asm.erl:309`: `[{version,?COMPILER_VSN}|Compile]`), with the
    /// strings written as `STRING_EXT` exactly as the oracle does.
    fn sample_compile_info() -> Vec<u8> {
        let mut bytes = vec![131, 108, 0, 0, 0, 2]; // VERSION_MAGIC, LIST_EXT, 2 elems
        // {version,"1.2.3"}
        bytes.extend_from_slice(&[104, 2, 119, 7]);
        bytes.extend_from_slice(b"version");
        bytes.extend_from_slice(&[107, 0, 5]);
        bytes.extend_from_slice(b"1.2.3");
        // {source,"a.erl"}
        bytes.extend_from_slice(&[104, 2, 119, 6]);
        bytes.extend_from_slice(b"source");
        bytes.extend_from_slice(&[107, 0, 5]);
        bytes.extend_from_slice(b"a.erl");
        bytes.push(106); // NIL tail
        bytes
    }

    #[test]
    fn decodes_a_hand_built_payload() {
        let mut atoms = AtomTable::new();
        let decoded = DecodedTerm::decode(Some(&sample_compile_info()), &mut atoms).unwrap();

        assert!(!decoded.is_empty());
        assert!(decoded.heap().is_some());
        let term = decoded.term().unwrap();

        // The whole list: `[{version,"1.2.3"},{source,"a.erl"}]`.
        let first = beam_codec::term::cons_head(term).unwrap();
        assert_eq!(beam_codec::term::tuple_arity(first).unwrap(), 2);
        let version = decoded.lookup(make_atom(atoms.intern("version"))).unwrap();
        // The value is the byte list `"1.2.3"` (a `STRING_EXT` decodes to a list).
        let head = beam_codec::term::cons_head(version).unwrap();
        assert_eq!(beam_term::signed_value(head), i64::from(b'1'));

        // A key that is not present is `None`, never a panic.
        assert_eq!(decoded.lookup(make_atom(atoms.intern("absent"))), None);
    }

    #[test]
    fn absent_or_empty_payloads_yield_an_empty_holder() {
        let mut atoms = AtomTable::new();
        for raw in [None, Some(&[][..])] {
            let decoded = DecodedTerm::decode(raw, &mut atoms).unwrap();
            assert!(decoded.is_empty());
            assert_eq!(decoded.term(), None);
            assert_eq!(decoded.lookup(make_atom(atoms.intern("version"))), None);
            assert!(decoded.heap().is_none(), "no term means no heap");
        }

        let default = DecodedTerm::default();
        assert!(default.is_empty());
        assert_eq!(
            format!("{default:?}"),
            "DecodedTerm { has_term: false, heap_words: 0, .. }"
        );
    }

    #[test]
    fn a_malformed_payload_is_a_typed_error() {
        let mut atoms = AtomTable::new();
        // A truncated `[{version,"1.2.3"}]`: the decoder must reject it, not panic.
        let truncated = vec![131, 108, 0, 0, 0, 1, 104, 2, 119, 7, b'v', b'e'];
        let err = DecodedTerm::decode(Some(&truncated), &mut atoms).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");

        // A payload that is not an external term at all (no `VERSION_MAGIC`).
        let err = DecodedTerm::decode(Some(&[0, 1, 2, 3]), &mut atoms).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    }

    #[test]
    fn lookup_tolerates_a_non_property_list() {
        let mut atoms = AtomTable::new();
        // `[1, {version,[7]}]` — the head is not a tuple, so the walk must skip it and
        // still find `version` (never panic on the malformed element).
        let mut bytes = vec![131, 108, 0, 0, 0, 2, 97, 1, 104, 2, 119, 7];
        bytes.extend_from_slice(b"version");
        bytes.extend_from_slice(&[108, 0, 0, 0, 1, 97, 7, 106]);
        bytes.push(106);
        let decoded = DecodedTerm::decode(Some(&bytes), &mut atoms).unwrap();
        assert!(decoded.lookup(make_atom(atoms.intern("version"))).is_some());
    }

    #[test]
    fn a_shared_term_is_cheap_to_clone() {
        let mut atoms = AtomTable::new();
        let shared: SharedDecodedTerm =
            Arc::new(DecodedTerm::decode(Some(&sample_compile_info()), &mut atoms).unwrap());
        let clone = Arc::clone(&shared);
        assert!(Arc::ptr_eq(&shared, &clone));
        assert!(clone.term().is_some());
    }
}
