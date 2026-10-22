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

//! The **native-record shape** shared by the `~w`/`~W` and `~p`/`~P` writers.
//!
//! A safe submodule of `crate::print` (it registers no BIFs). A native record
//! (`beam_term`'s `RECORD_SUBTAG` term, made by `records:create/4`) is the one
//! printable class whose rendering is **not** a leaf: both writers walk its fields,
//! so the record's parts are extracted here exactly once per record and handed to
//! both. Keeping the extraction in one place is what stops the two depth
//! accountings from drifting.
//!
//! # The oracle's part extraction
//!
//! `io_lib:write/2`'s `write_record/4` and `io_lib_pretty`'s
//! `print_length_native_record/7` read the same five things
//! (`lib/stdlib/src/io_lib.erl:944-960`,
//! `lib/stdlib/src/io_lib_pretty.erl:1011-1043`):
//!
//! * `records:get_module/1` and `records:get_name/1` — rendered by
//!   [`format_atom`] (the same `write_atom_as_latin1/1` the leaf atoms take, so a
//!   module/name/field atom that needs quoting is quoted: `#'Weird Mod':'R R'{…}`);
//! * `records:get_field_names/1` — the field keys in **declaration** order
//!   (`record_declaration_keys`), *not* the sorted order the value words sit in;
//! * `records:get(Field, Rec)` — each field's value.
//!
//! The value words are stored in **sorted-key** order
//! (`beam_term::record_values`), so the declaration-order value is
//! `values[field_order[i]]` ([`crate::access::native_record_declaration_order`]) —
//! one linear pass, never an O(n²) key scan.
//!
//! `is_exported` never affects the rendered text: an exported record and a
//! non-exported one print identically.
//!
//! # The two depth accountings
//!
//! Both writers spend the record's *own* level plus one per field, but they start
//! one level apart, so a record costs **1** level under `~w`/`~W` and **2** under
//! `~p`/`~P`:
//!
//! * flat (`write_record_1/5`): field names at `D`, values at `D-1`, and each
//!   *later* field at `D-1`, so field `i` (0-based) names at `D-i` and its value at
//!   `D-i-1`; `D =:= 1` elides the whole field list as `"..."` (checked *before*
//!   every field, and after the empty-field clause, so a zero-field record is
//!   `#m:r{}` at any positive depth);
//! * pretty (`print_length_native_record_fields/8`): `D-1` at the entry, so names
//!   at `D-1` and values at `D-2`.
//!
//! # Nesting
//!
//! The oracle's nested-list structure is reproduced exactly, and the two writers
//! nest the `#Module:Name` prefix differently:
//!
//! * flat: `[$#, Mod, $:, RName, ${, Body, $}]` — the four prefix pieces are
//!   separate elements of one list ([`flat_record_chunk`]);
//! * pretty: `Name = [$#, Mod, $:, RName]` is **one** element of
//!   `[Name, ${, Fields, $}]`, which is why [`RecordParts::name`] is that list.

use beam_common::Result;
use beam_term::{atom_val, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error::badarg;

use super::{format_atom, render_chunk, Chunk, MapOrder};

/// One field of a native record, in **declaration** order.
pub(super) struct RecordField {
    /// The key atom's rendered text ([`format_atom`]), already quoted if needed.
    pub(super) key_text: String,
    /// `KeyL` — the key text's character count plus the three of `" = "`.
    pub(super) key_len: i64,
    /// The field's value term.
    pub(super) value: Eterm,
}

/// A native record's rendering-relevant parts.
pub(super) struct RecordParts {
    /// The `#Module:Name` element — `[$#, Mod, $:, RName]` (see the module docs).
    pub(super) name: Chunk,
    /// `NameL` — the module text's character count plus the name text's plus `2`
    /// (the `#` and the `:`).
    pub(super) name_len: i64,
    /// The fields in declaration order (`records:get_field_names/1`).
    pub(super) fields: Vec<RecordField>,
}

/// Extracts a native record's [`RecordParts`].
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the term is not a
/// well-formed native record (`TermError::BadRecord` reconciled upstream), when a
/// definition word is not an atom, when a key/module/name atom index is not
/// interned in `ctx`'s atom table, or when the definition's arity, `field_order`
/// and value words disagree — the same class the `records:*` BIFs answer for a
/// malformed record, never a fabricated rendering.
pub(super) fn record_parts(ctx: &mut BifContext<'_>, rec: Eterm) -> Result<RecordParts> {
    let def = access::native_record_definition(rec)?;
    let module = access::native_record_definition_module(def)?;
    let name = access::native_record_definition_name(def)?;
    let module_text = atom_text(ctx, module)?;
    let name_text = atom_text(ctx, name)?;
    let keys = access::native_record_declaration_keys(def)?;
    let order = access::native_record_declaration_order(def)?;
    let values = access::native_record_values(rec)?;
    if keys.len() != values.len() || keys.len() != order.len() {
        return Err(badarg());
    }
    let mut fields = Vec::with_capacity(keys.len());
    for (i, &key) in keys.iter().enumerate() {
        let key_text = atom_text(ctx, key)?;
        let value = match values.get(order[i]) {
            Some(&value) => value,
            None => return Err(badarg()),
        };
        let key_len = chars(&key_text) + 3;
        fields.push(RecordField {
            key_text,
            key_len,
            value,
        });
    }
    let name_len = chars(&module_text) + chars(&name_text) + 2;
    let name = Chunk::List(vec![
        Chunk::Char('#'),
        Chunk::text(&module_text),
        Chunk::Char(':'),
        Chunk::text(&name_text),
    ]);
    Ok(RecordParts {
        name,
        name_len,
        fields,
    })
}

/// Renders a native record as the flat `io_lib:write1/4` tree
/// (`write_record/4` → `write_record_1/5`, `lib/stdlib/src/io_lib.erl:944-960`).
///
/// `depth` must be non-zero (the caller renders `...` for depth `0`). The field
/// walk is **iterative**: one field per depth level means a per-field recursion
/// would grow the native stack once per field, the rule the parent module applies
/// to lists, tuples and maps.
///
/// # Errors
///
/// As [`record_parts`], plus whatever a field's value renderer reports.
pub(super) fn flat_record_chunk(
    ctx: &mut BifContext<'_>,
    parts: &RecordParts,
    printable: bool,
    depth: i64,
    order: MapOrder,
) -> Result<Chunk> {
    let n = parts.fields.len();
    let mut body: Vec<Chunk> = Vec::with_capacity(n.saturating_mul(3).saturating_add(1));
    let mut d = depth;
    let mut i = 0usize;
    while i < n {
        // The comma belongs to the *previous* field's clause
        // (`[.., value, "," | write_record_1(Fs, T, D-1, ..)]`), so it precedes the
        // depth-1 `"..."` the next level emits.
        if i > 0 {
            body.push(Chunk::Char(','));
        }
        if d == 1 {
            // `write_record_1(_Fs, _T, 1, _E, _O) -> "..."` — one nested element.
            body.push(Chunk::text("..."));
            break;
        }
        let field = &parts.fields[i];
        body.push(Chunk::text(&field.key_text));
        body.push(Chunk::text(" = "));
        body.push(render_chunk(ctx, field.value, printable, d - 1, order)?);
        i += 1;
        d -= 1;
    }
    let mut items = parts.name.clone().into_items();
    items.push(Chunk::Char('{'));
    items.push(Chunk::List(body));
    items.push(Chunk::Char('}'));
    Ok(Chunk::List(items))
}

/// One atom definition word's rendered text.
fn atom_text(ctx: &BifContext<'_>, term: Eterm) -> Result<String> {
    if !access::is_atom(term) {
        return Err(badarg());
    }
    let name = ctx.atoms().name(atom_val(term)).ok_or_else(badarg)?;
    Ok(format_atom(name))
}

/// `io_lib:chars_length/1` of a rendered atom text.
fn chars(s: &str) -> i64 {
    s.chars().count() as i64
}
