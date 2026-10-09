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

//! The table-level `ets:update_counter/3,4` and `ets:update_element/3`
//! operations on [`Table`].
//!
//! [`Table`]'s inherent element-update methods live here: they validate the
//! operands, consult the interior index and reconcile table-owned storage across
//! the A.1 copy boundary. The shared term-level operand machinery (counter
//! arithmetic, operand decoding, tuple rebuilding) stays in the sibling `update`
//! module, and the drop-in `Interior` defaults it feeds remain in the parent
//! module.

use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;
use crate::key::key_of;
use crate::kind::TableKind;

use super::update::{
    build_list, decode_counter_ops, object_arity, read_counter, replace_elements, warp_fires,
};
use super::Table;

impl Table {
    /// `ets:update_counter/3`: add `incr` to the element following the key
    /// (`keypos + 1`) of the object stored under `key`, and return the new value.
    ///
    /// Mirrors the verified system `erl` (OTP 29 / erts-17.1):
    /// * only `set`/`ordered_set` hold counters — `bag`/`duplicate_bag` are
    ///   `badarg`;
    /// * an **absent** `key` is `badarg`: the `/3` form has no default object;
    /// * a non-integer element or a non-integer increment is `badarg`.
    ///
    /// Despite the name, `update_op` may be any of the operands the BIF accepts
    /// (`Incr`, `{Pos,Incr}`, `{Pos,Incr,Threshold,SetValue}` or a list of those);
    /// a **list** operand yields a list result — exactly as `ets:update_counter/3`
    /// is `ets:update_counter/4` without a default.
    pub fn update_counter(
        &mut self,
        key: Eterm,
        update_op: Eterm,
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<Eterm> {
        self.do_update_counter(key, update_op, None, atoms, heap)
    }

    /// `ets:update_counter/4`: like [`update_counter`](Table::update_counter) but
    /// with a `default` object used when `key` is absent.
    ///
    /// Mirrors the verified system `erl` (OTP 29 / erts-17.1):
    /// * `default` must be a tuple with an arity of at least `keypos`, and an
    ///   invalid one is `badarg` even when `key` exists;
    /// * `update_op` is an increment, a `{Pos,Incr}`, a
    ///   `{Pos,Incr,Threshold,SetValue}`, or a list of those — only the list form
    ///   returns a **list** of the new values, one per operation, in order;
    /// * an absent `key` behaves as if the object `default` (with its key element
    ///   replaced by `key`) were present, and it is deposited in the table — even
    ///   when the operation list is empty;
    /// * `pos == keypos`, an out-of-range `pos`, a non-integer element and a
    ///   non-integer `Incr`/`Threshold`/`SetValue` are `badarg`;
    /// * the threshold rule: an increment `>= 0` whose result would be `>` the
    ///   threshold, or one `< 0` whose result would be `<` the threshold, makes
    ///   the counter `SetValue`.
    pub fn update_counter_with(
        &mut self,
        key: Eterm,
        update_op: Eterm,
        default: Eterm,
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<Eterm> {
        self.do_update_counter(key, update_op, Some(default), atoms, heap)
    }

    /// `ets:update_element/3`: replace element `pos` (1-based) of the object
    /// stored under `key` with each `Value` of `updates` (later entries win for a
    /// repeated position), and return whether `key` was present.
    ///
    /// Mirrors the verified system `erl` (OTP 29 / erts-17.1):
    /// * an **absent** `key` is `Ok(false)`, never an error — and it is answered
    ///   **before** the spec is inspected, so a malformed spec on an absent key is
    ///   still `Ok(false)`;
    /// * `bag`/`duplicate_bag` are `badarg`;
    /// * `pos == keypos`, `pos == 0` and `pos` beyond the object's arity are
    ///   `badarg`, with nothing written (every position is validated first);
    /// * an **empty** `updates` on a present key is `Ok(true)` and changes
    ///   nothing.
    ///
    /// The `pos == keypos` rule is enforced here (the interiors do not carry
    /// `keypos`); the interior rebuilds the object.
    pub fn update_element(
        &mut self,
        key: Eterm,
        updates: &[(usize, Eterm)],
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<bool> {
        self.check_counter_kind()?;
        let keypos = self.opts.keypos();
        // The key is looked up *before* the spec is validated: `erl` answers
        // `false` for an absent key whatever the spec says.
        let replaced = match self.interior.lookup(key, atoms).into_iter().next() {
            Some(object) => object,
            None => return Ok(false),
        };
        let arity = object_arity(replaced)?;
        for &(pos, _) in updates {
            if pos == keypos || pos == 0 || pos > arity {
                return Err(badarg());
            }
        }
        let updated = self.interior.update_element(key, updates, atoms, heap)?;
        if updated {
            // The interior rebuilt the object on the caller's `heap`; make it
            // table-owned (the A.1 copy boundary).
            self.relocate_rebuilt_object(replaced, atoms)?;
        }
        Ok(updated)
    }

    /// The shared `set`/`ordered_set` guard of the `ets:update_*` element
    /// operations: `bag`/`duplicate_bag` have no counters (`erl` raises `badarg`).
    fn check_counter_kind(&self) -> Result<()> {
        match self.opts.kind() {
            TableKind::Set | TableKind::OrderedSet => Ok(()),
            TableKind::Bag | TableKind::DuplicateBag => Err(badarg()),
        }
    }

    /// The common body of `ets:update_counter/3` (`default = None`) and `/4`
    /// (`default = Some`).
    ///
    /// The `default` object, when given, is validated before the key is consulted
    /// — `erl` fails on an invalid default even when the key exists. Every
    /// operation is validated before the first write, so a bad operation leaves
    /// the table (including a not-yet-deposited default object) untouched.
    ///
    /// Verified against the system `erl` (OTP 29 / erts-17.1):
    /// * `update_op` is `Incr | {Pos,Incr} | {Pos,Incr,Threshold,SetValue} |` a
    ///   list of those; only the **list** operand yields a list result;
    /// * with `default`, an absent key behaves as if the object `default` (with
    ///   its key element replaced by `key`) were present — it is deposited in the
    ///   table, even when the operation list is empty;
    /// * the threshold rule: an increment `>= 0` whose result would be `>` the
    ///   threshold, or one `< 0` whose result would be `<` the threshold, makes
    ///   the counter `SetValue`.
    fn do_update_counter(
        &mut self,
        key: Eterm,
        update_op: Eterm,
        default: Option<Eterm>,
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<Eterm> {
        self.check_counter_kind()?;
        let keypos = self.opts.keypos();
        // Validate the default object before consulting the key.
        let default = match default {
            Some(default) => {
                let arity = object_arity(default)?;
                if arity < keypos {
                    return Err(badarg());
                }
                Some((default, arity))
            }
            None => None,
        };
        let (ops, list_result) = decode_counter_ops(update_op, keypos)?;

        // The object to update: the stored one, or (the `/4` form only) the
        // default with its key element replaced by `key`, built on `heap`.
        let stored = self.interior.lookup(key, atoms).into_iter().next();
        let (object, deposit) = match stored {
            Some(object) => (object, false),
            None => match default {
                Some((default, arity)) => (
                    replace_elements(default, arity, &[(keypos, key)], heap)?,
                    true,
                ),
                // `/3` has no default object: an absent key is `badarg`.
                None => return Err(badarg()),
            },
        };

        // Validate every operation before the first write so the call is
        // all-or-nothing on a bad operand.
        let arity = object_arity(object)?;
        for op in &ops {
            if op.pos == keypos || op.pos == 0 || op.pos > arity {
                return Err(badarg());
            }
            // The addressed element must be an integer.
            read_counter(object, op.pos)?;
        }

        // Past this point the operations are applied; deposit the default object
        // first when the key was absent.
        let mut replaced = object;
        if deposit {
            self.insert(object, atoms)?;
            replaced = self
                .interior
                .lookup(key, atoms)
                .into_iter()
                .next()
                .ok_or_else(badarg)?;
        }

        let mut results = Vec::with_capacity(ops.len());
        for op in &ops {
            let value = self
                .interior
                .update_counter(key, op.pos, op.incr, atoms, heap)?;
            let value = match op.warp {
                Some((threshold, set_value)) if warp_fires(op.incr, value, threshold, atoms) => {
                    // The threshold rule replaces the counter outright; the hash
                    // override has no "write this value" entry point, so route it
                    // through `update_element` (same observable result).
                    self.interior
                        .update_element(key, &[(op.pos, set_value)], atoms, heap)?;
                    set_value
                }
                _ => value,
            };
            results.push(value);
        }
        // The last interior operation may have rebuilt the object on the caller's
        // `heap`; reconcile the index with table-owned storage once.
        self.relocate_rebuilt_object(replaced, atoms)?;

        if list_result {
            build_list(heap, &results)
        } else {
            // The scalar operands always decode to exactly one operation.
            results.first().copied().ok_or_else(badarg)
        }
    }

    /// Reconcile table-owned storage after an interior element update rebuilt the
    /// object stored under `key` on the caller's `heap`.
    ///
    /// The A.1 copy boundary forbids the table from retaining a pointer into a
    /// caller heap, so a rebuilt object is deep-copied into the table store, the
    /// store copy replaces it in the index, and the table-owned object it replaced
    /// is reclaimed. An **in-place** update (the hash index's `update_counter`)
    /// leaves the stored root untouched, and then there is nothing to do.
    ///
    /// The index key is **re-derived from the table-owned object** rather than
    /// reused from the caller: the stored key term must alias a live table object,
    /// and only the table-owned copy (or the object being restored) qualifies. The
    /// caller's `key` may point into a caller heap, so storing it would let the
    /// index dangle once that heap is dropped.
    fn relocate_rebuilt_object(&mut self, replaced: Eterm, atoms: &AtomTable) -> Result<()> {
        let keypos = self.opts.keypos();
        // The key is the table-owned object's own key element, so it is only
        // reachable once we know which root anchors the entry. Locate the entry by
        // the replaced object's key (the object is still table-owned and live here).
        let located = key_of(replaced, keypos)?;
        let current = match self.interior.lookup(located, atoms).into_iter().next() {
            Some(current) => current,
            // The `set`/`ordered_set` kinds this path serves always keep exactly
            // one object per key.
            None => return Err(badarg()),
        };
        if current == replaced {
            return Ok(());
        }
        match self.store.copy(current) {
            Ok(stored) => {
                // `stored` is the table-owned copy: its own key element anchors the
                // entry it is about to replace.
                let stored_key = key_of(stored, keypos)?;
                self.interior.insert(stored_key, stored, atoms)?;
                // The object it replaced was in the table, so a fixed table defers
                // its storage like any other removal.
                self.free_object(replaced);
                Ok(())
            }
            Err(err) => {
                // The rebuilt object is not expressible in table storage (a fun,
                // an external pid, …): put the table-owned object back rather than
                // retain a caller-heap pointer, and report the failure.
                self.interior.insert(located, replaced, atoms)?;
                Err(err)
            }
        }
    }
}
