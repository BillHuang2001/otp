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

//! The `ExpT` / `LocT` / `ImpT` / `FunT` tables: exports, locals, imports and lambda
//! descriptors.
//!
//! * `ExpT` — `i32 count` × `{ i32 function_atom, i32 arity, i32 label }`; the
//!   label is the function **entry** label.
//! * `LocT` — byte-for-byte the `ExpT` layout (see [`parse_locals`]); the JIT-only
//!   local-function table.
//! * `ImpT` — `i32 count` × `{ i32 module_atom, i32 function_atom, i32 arity }`.
//! * `FunT` — `i32 count` × `{ i32 function_atom, i32 arity, i32 label, i32 index,
//!   i32 num_free, i32 old_uniq }` (24-byte entries).
//!
//! These are the *raw* wire forms (still atom **indices**, not interned atoms);
//! [`crate::lower`] maps them onto the IR using the atom table.
use beam_common::Result;

use crate::error::bad_file;

/// Reads a table's leading `i32` entry count, rejecting a negative value.
///
/// All three tables (`ExpT`/`ImpT`/`FunT`) open with a signed count; a negative
/// count is structurally invalid.
fn read_count(reader: &mut beam_codec::Reader<'_>) -> Result<usize> {
    let raw = reader.u32be()? as i32;
    if raw < 0 {
        return Err(bad_file("negative table count"));
    }
    Ok(raw as usize)
}

/// A raw `ExpT` entry (atom indices, not interned atoms).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawExport {
    /// The function-name atom index.
    pub function: u32,
    /// The exported arity.
    pub arity: u32,
    /// The function entry label.
    pub label: u32,
}

/// A raw `ImpT` entry (atom indices, not interned atoms).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawImport {
    /// The module-name atom index.
    pub module: u32,
    /// The function-name atom index.
    pub function: u32,
    /// The imported arity.
    pub arity: u32,
}

/// A raw `FunT` (lambda) descriptor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawFun {
    /// The function-name atom index.
    pub function: u32,
    /// The lambda arity.
    pub arity: u32,
    /// The lambda code label.
    pub label: u32,
    /// The index within the enclosing function's fun list.
    pub index: u32,
    /// The number of free variables captured.
    pub num_free: u32,
    /// The old unique-id field (`beam_file.c`).
    pub old_uniq: u32,
}

/// Parses an `ExpT` chunk payload into its raw entries.
pub fn parse_exports(data: &[u8]) -> Result<Vec<RawExport>> {
    let mut reader = beam_codec::Reader::new(data);
    let count = read_count(&mut reader)?;

    let mut exports = Vec::new();
    for _ in 0..count {
        // Each entry is `{ i32 function_atom, i32 arity, i32 label }`.
        let function = reader.u32be()?;
        let arity = reader.u32be()?;
        let label = reader.u32be()?;
        exports.push(RawExport {
            function,
            arity,
            label,
        });
    }
    Ok(exports)
}

/// Parses a `LocT` chunk payload into its raw entries.
///
/// `LocT` is the JIT-only **local-function** table. The C reader parses it with the
/// very same export-table reader (`parse_locals_chunk` calls `parse_export_table`,
/// `beam_file.c`) and the compiler writes it with the identical shape
/// (`chunk(<<"LocT">>, <<NumLocals:32>>, Loc)`, `beam_asm.erl:186`), so its payload
/// is byte-for-byte an `ExpT` payload — `i32 count` ×
/// `{ i32 function_atom, i32 arity, i32 label }`. This therefore *reuses*
/// [`parse_exports`] rather than duplicating the reader.
///
/// As in `ExpT`, the label is the local function's **entry** label. The table is
/// irrelevant to interpretation (the C keeps it only for `erts_jit_asm_dump`), but
/// parsing it makes the chunk a recognized one instead of a silently skipped payload.
pub fn parse_locals(data: &[u8]) -> Result<Vec<RawExport>> {
    parse_exports(data)
}

/// Parses an `ImpT` chunk payload into its raw entries.
pub fn parse_imports(data: &[u8]) -> Result<Vec<RawImport>> {
    let mut reader = beam_codec::Reader::new(data);
    let count = read_count(&mut reader)?;

    let mut imports = Vec::new();
    for _ in 0..count {
        // Each entry is `{ i32 module_atom, i32 function_atom, i32 arity }`.
        let module = reader.u32be()?;
        let function = reader.u32be()?;
        let arity = reader.u32be()?;
        imports.push(RawImport {
            module,
            function,
            arity,
        });
    }
    Ok(imports)
}

/// Parses a `FunT` chunk payload into its raw lambda descriptors.
///
/// The descriptors are lowered onto [`crate::ir::Module::funs`] by
/// [`crate::lower::build_module`]; their position in the returned vector *is* the
/// fun-table index `make_fun3` carries as its first operand.
pub fn parse_funs(data: &[u8]) -> Result<Vec<RawFun>> {
    let mut reader = beam_codec::Reader::new(data);
    let count = read_count(&mut reader)?;

    let mut funs = Vec::new();
    for _ in 0..count {
        // Each entry is a 24-byte
        // `{ function, arity, label, index, num_free, old_uniq }` record.
        let function = reader.u32be()?;
        let arity = reader.u32be()?;
        let label = reader.u32be()?;
        let index = reader.u32be()?;
        let num_free = reader.u32be()?;
        let old_uniq = reader.u32be()?;
        funs.push(RawFun {
            function,
            arity,
            label,
            index,
            num_free,
            old_uniq,
        });
    }
    Ok(funs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;

    /// Appends `count` then `values.len()/3` big-endian `u32` triples.
    fn triples(count: u32, triples: &[[u32; 3]]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&count.to_be_bytes());
        for t in triples {
            for v in t {
                data.extend_from_slice(&v.to_be_bytes());
            }
        }
        data
    }

    #[test]
    fn parses_exports() {
        // {function, arity, label} triples.
        let data = triples(2, &[[2, 2, 2], [5, 1, 4]]);
        let exports = parse_exports(&data).unwrap();
        assert_eq!(
            exports,
            vec![
                RawExport {
                    function: 2,
                    arity: 2,
                    label: 2
                },
                RawExport {
                    function: 5,
                    arity: 1,
                    label: 4
                },
            ]
        );
    }

    #[test]
    fn parses_locals_like_exports() {
        // `LocT` is byte-for-byte an `ExpT` payload.
        let data = triples(1, &[[5, 3, 6]]);
        assert_eq!(
            parse_locals(&data).unwrap(),
            vec![RawExport {
                function: 5,
                arity: 3,
                label: 6
            }]
        );
    }

    #[test]
    fn parses_imports() {
        // {module, function, arity} triples.
        let data = triples(2, &[[3, 4, 2], [3, 10, 1]]);
        let imports = parse_imports(&data).unwrap();
        assert_eq!(
            imports,
            vec![
                RawImport {
                    module: 3,
                    function: 4,
                    arity: 2
                },
                RawImport {
                    module: 3,
                    function: 10,
                    arity: 1
                },
            ]
        );
    }

    #[test]
    fn parses_funs() {
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_be_bytes()); // count
        for v in [1u32, 2, 3, 0, 1, 0] {
            data.extend_from_slice(&v.to_be_bytes());
        }
        let funs = parse_funs(&data).unwrap();
        assert_eq!(
            funs,
            vec![RawFun {
                function: 1,
                arity: 2,
                label: 3,
                index: 0,
                num_free: 1,
                old_uniq: 0,
            }]
        );
    }

    #[test]
    fn zero_entries_is_allowed() {
        let zero = 0u32.to_be_bytes();
        assert!(parse_exports(&zero).unwrap().is_empty());
        assert!(parse_locals(&zero).unwrap().is_empty());
        assert!(parse_imports(&zero).unwrap().is_empty());
        assert!(parse_funs(&zero).unwrap().is_empty());
    }

    #[test]
    fn negative_counts_are_rejected() {
        let neg = (-1i32).to_be_bytes();
        assert!(matches!(
            parse_exports(&neg).unwrap_err(),
            BeamError::BadArg
        ));
        assert!(matches!(parse_locals(&neg).unwrap_err(), BeamError::BadArg));
        assert!(matches!(
            parse_imports(&neg).unwrap_err(),
            BeamError::BadArg
        ));
        assert!(matches!(parse_funs(&neg).unwrap_err(), BeamError::BadArg));
    }

    #[test]
    fn short_tables_are_rejected() {
        // count = 1 but no entry bytes follow.
        let data = 1u32.to_be_bytes();
        assert!(matches!(
            parse_exports(&data).unwrap_err(),
            BeamError::BadArg
        ));
        assert!(matches!(
            parse_locals(&data).unwrap_err(),
            BeamError::BadArg
        ));
        assert!(matches!(
            parse_imports(&data).unwrap_err(),
            BeamError::BadArg
        ));
        assert!(matches!(parse_funs(&data).unwrap_err(), BeamError::BadArg));
    }

    #[test]
    fn partial_fun_entry_is_rejected() {
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_be_bytes()); // count
        for v in [1u32, 2, 3] {
            // only 3 of the 6 fields
            data.extend_from_slice(&v.to_be_bytes());
        }
        assert!(matches!(parse_funs(&data).unwrap_err(), BeamError::BadArg));
    }

    #[test]
    fn missing_count_is_rejected() {
        assert!(matches!(
            parse_exports(&[0, 0]).unwrap_err(),
            BeamError::BadArg
        ));
    }
}
