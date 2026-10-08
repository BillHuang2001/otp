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

//! The mechanical chunk-classification table: how the loader treats every chunk id.
//!
//! A `.beam` image is an IFF container of independent chunks. For a *complete* reader
//! every id a real image can carry must be one of:
//!
//! * **[`ChunkClass::ParsedExposed`]** — decoded into a typed structure and exposed on
//!   the IR ([`crate::chunk::Chunks`] / [`crate::ir::Module`]).
//! * **[`ChunkClass::ParsedRaw`]** — captured verbatim as raw bytes (no interpretation
//!   at this layer, but the payload is materialized rather than dropped).
//! * **[`ChunkClass::ToolingOnly`]** — recognized but deliberately left unparsed
//!   because its payload is only meaningful to tooling (ExDoc, the debugger, the
//!   compiler itself), never to the runtime.
//!
//! This module is the single in-code enumeration of that decision so the committed
//! `tests/chunks.rs` checker can walk the whole installed OTP corpus and assert that
//! **every** chunk id it encounters is classified — an unparsed runtime-relevant chunk
//! cannot slip in unnoticed.
//!
//! Ground truth: the C reader's chunk directory `beam_file.c:1588-1605` (the
//! `chunk_iffs` table) plus the compiler's writer side — `beam_asm.erl` (the essential
//! chunks) and `compile.erl` (`?META_CHUNK_NAME` = `Meta`, `?META_DOC_CHUNK` = `Docs`).
use crate::container::ChunkId;

/// How the loader treats one chunk id (see the module docs for the three categories).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChunkClass {
    /// Decoded into a typed structure and exposed on the IR / raw chunk set.
    ParsedExposed,
    /// Captured verbatim as raw bytes; not interpreted at this layer.
    ParsedRaw,
    /// Recognized but deliberately left unparsed: its payload is tooling-only.
    ToolingOnly,
}

impl ChunkClass {
    /// A short human label for diagnostics and test failure messages.
    pub fn label(self) -> &'static str {
        match self {
            ChunkClass::ParsedExposed => "parsed+exposed",
            ChunkClass::ParsedRaw => "parsed-as-raw",
            ChunkClass::ToolingOnly => "tooling-only",
        }
    }
}

/// One row of the classification table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChunkKind {
    /// The chunk identifier.
    pub id: ChunkId,
    /// A short human name (used in test failure messages).
    pub name: &'static str,
    /// How the loader treats the chunk.
    pub class: ChunkClass,
    /// Where the parsed form lives, or why the chunk is left unparsed (one line).
    pub reason: &'static str,
}

/// Every chunk id the loader knows about.
///
/// The order is the C reader's `chunk_iffs` directory order (`beam_file.c:1588-1605`),
/// followed by the compiler-only `Docs` extra chunk. The obsolete `Atom` chunk is the
/// only row that is neither parsed nor captured — it is recognized so a `.beam` that
/// still carries it is not misreported as "unknown".
pub const CHUNK_KINDS: &[ChunkKind] = &[
    ChunkKind {
        id: ChunkId::ATOM,
        name: "AtU8",
        class: ChunkClass::ParsedExposed,
        reason: "UTF-8 atom names -> Chunks::atoms / Module::atoms + Module::atom_table",
    },
    ChunkKind {
        id: ChunkId::CODE,
        name: "Code",
        class: ChunkClass::ParsedExposed,
        reason: "instruction stream -> Module::code_version + Module::functions",
    },
    ChunkKind {
        id: ChunkId::STRT,
        name: "StrT",
        class: ChunkClass::ParsedRaw,
        reason: "string-table bytes -> Module::strings (opaque at this layer)",
    },
    ChunkKind {
        id: ChunkId::IMPT,
        name: "ImpT",
        class: ChunkClass::ParsedExposed,
        reason: "import table -> Module::imports (the single source of BIF identity)",
    },
    ChunkKind {
        id: ChunkId::EXPT,
        name: "ExpT",
        class: ChunkClass::ParsedExposed,
        reason: "export table -> Module::exports",
    },
    ChunkKind {
        id: ChunkId::FUNT,
        name: "FunT",
        class: ChunkClass::ParsedExposed,
        reason: "lambda descriptors -> Module::funs",
    },
    ChunkKind {
        id: ChunkId::LITT,
        name: "LitT",
        class: ChunkClass::ParsedExposed,
        reason: "static literals -> Module::literals (decoded into terms at load)",
    },
    ChunkKind {
        id: ChunkId::ATTR,
        name: "Attr",
        class: ChunkClass::ParsedExposed,
        reason: "module attributes -> Module::attributes (raw) + Module::attributes_term (decoded)",
    },
    ChunkKind {
        id: ChunkId::CINF,
        name: "CInf",
        class: ChunkClass::ParsedRaw,
        reason: "compiler info term -> Module::compile_info (opaque at this layer)",
    },
    ChunkKind {
        id: ChunkId::LINE,
        name: "Line",
        class: ChunkClass::ParsedExposed,
        reason: "source-location table -> Module::line",
    },
    ChunkKind {
        id: ChunkId::LOCT,
        name: "LocT",
        class: ChunkClass::ParsedExposed,
        reason: "JIT-only local-function table (ExpT layout) -> Module::locals",
    },
    ChunkKind {
        id: ChunkId::OBSOLETE_ATOM,
        name: "Atom",
        class: ChunkClass::ToolingOnly,
        reason: "obsolete Latin-1 atom table (OTP 20 and earlier), superseded by AtU8; never an atom source",
    },
    ChunkKind {
        id: ChunkId::TYPE,
        name: "Type",
        class: ChunkClass::ParsedExposed,
        reason: "compile-time register types -> Module::types",
    },
    ChunkKind {
        id: ChunkId::META,
        name: "Meta",
        class: ChunkClass::ParsedRaw,
        reason: "compiler feature/CPU metadata (part of the module checksum) -> Module::meta (opaque at this layer)",
    },
    ChunkKind {
        id: ChunkId::DBGB,
        name: "DbgB",
        class: ChunkClass::ToolingOnly,
        reason: "legacy emulator debug-table chunk; tooling-only, never read by the runtime",
    },
    ChunkKind {
        id: ChunkId::RECS,
        name: "Recs",
        class: ChunkClass::ParsedExposed,
        reason: "native-record definitions -> Module::records",
    },
    ChunkKind {
        id: ChunkId::DOCS,
        name: "Docs",
        class: ChunkClass::ToolingOnly,
        reason: "compressed ExDoc documentation term; tooling-only, never read by the runtime",
    },
    ChunkKind {
        id: ChunkId::DBGI,
        name: "Dbgi",
        class: ChunkClass::ToolingOnly,
        reason: "compiler debug information (abstract code + vars); tooling-only, never read by the runtime",
    },
];

/// The classification of `id`, or `None` when the id is not recognized at all.
pub fn classify(id: ChunkId) -> Option<ChunkKind> {
    CHUNK_KINDS.iter().copied().find(|kind| kind.id == id)
}

/// Whether `id` appears in the classification table.
pub fn is_known(id: ChunkId) -> bool {
    classify(id).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_installed_corpus_id_is_classified() {
        // The exact chunk-id set the installed OTP corpus carries (34 apps, 1366
        // modules; see `tests/chunks.rs` for the live re-derivation). An unknown id
        // here is the regression the checker test guards against.
        let corpus: [ChunkId; 15] = [
            ChunkId::ATOM,
            ChunkId::ATTR,
            ChunkId::CINF,
            ChunkId::CODE,
            ChunkId::DBGI,
            ChunkId::DOCS,
            ChunkId::EXPT,
            ChunkId::FUNT,
            ChunkId::IMPT,
            ChunkId::LINE,
            ChunkId::LITT,
            ChunkId::LOCT,
            ChunkId::META,
            ChunkId::STRT,
            ChunkId::TYPE,
        ];
        for id in corpus {
            let kind = classify(id)
                .unwrap_or_else(|| panic!("corpus chunk {:?} is unclassified", String::from_utf8_lossy(&id.name())));
            assert_eq!(kind.id, id);
            assert!(!kind.reason.is_empty(), "{} has no reason", kind.name);
        }
    }

    #[test]
    fn an_unrecognized_id_is_not_classified() {
        assert!(classify(ChunkId(*b"XXXX")).is_none());
        assert!(!is_known(ChunkId(*b"Zzzz")));
    }

    #[test]
    fn the_table_has_no_duplicate_ids() {
        for (i, a) in CHUNK_KINDS.iter().enumerate() {
            for b in &CHUNK_KINDS[i + 1..] {
                assert_ne!(a.id, b.id, "duplicate row for {}", a.name);
            }
        }
    }

    #[test]
    fn classes_have_distinct_labels() {
        assert_eq!(ChunkClass::ParsedExposed.label(), "parsed+exposed");
        assert_eq!(ChunkClass::ParsedRaw.label(), "parsed-as-raw");
        assert_eq!(ChunkClass::ToolingOnly.label(), "tooling-only");
    }
}
