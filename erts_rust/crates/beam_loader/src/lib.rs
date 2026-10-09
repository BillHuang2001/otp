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

//! # `beam_loader` — `.beam` reader & typed-IR lowering front-end
//!
//! Owns the `.beam` **container reader**, the **BEAM instruction decoder** and the
//! **lowering into the typed, block-structured IR** that `beam_engine` interprets.
//! Implements compatibility contract **A.2** (the `.beam` binary/assembly surface,
//! `../../CONTEXT.md` §1) and the Stage-1 loader half of Part II §E/§H (the IR
//! seam, `../CONTEXT.md` §4). This crate is on the critical path of the vertical
//! slice (load + run a real `.beam`).
//!
//! The authoritative design — the frozen public API, the container/chunk wire
//! format, the operand tag/length table and the design decisions — lives in this
//! crate's `CONTEXT.md`; read it before changing anything here.
//!
//! Ground truth (read-only references, never modified):
//! `../../erts/emulator/beam/beam_file.c` (container + operand decoding + function
//! grouping), `../../lib/compiler/src/genop.tab` (the opcode roster), and
//! `../../lib/compiler/src/beam_asm.erl` (the compiler-side writer).
//!
//! ## Status
//!
//! The crate is **implemented**: [`load`] / [`load_file`] run the whole pipeline —
//! `container::parse` → `chunk::parse` → `decode::decode_stream` →
//! `lower::build_module` — over the frozen IR of [`ir`]. The container/chunk layer
//! ([`container`], [`chunk`]) validates the `FOR1` framing and decodes every known
//! chunk, [`decode`] turns the `Code` stream into typed [`decode::RawInstr`]s, and
//! [`lower`] groups them into functions and basic blocks. The `LitT` static literals
//! are materialized into terms ([`literal::LiteralArea`], reachable through
//! [`ir::Module::literal`]) and the `FunT` lambda descriptors are lowered onto
//! [`ir::Module::funs`]. Coverage is the opcode set the compiler emits for ordinary
//! Erlang code (typed [`instr::Instr`] variants, see [`instr`]); obsolete `genop.tab`
//! entries and the OTP-29 native-record family round-trip losslessly as
//! [`instr::Instr::Generic`]. The remaining intentional Milestone-1 gaps
//! (preloaded/positional atoms) are listed under "Known Issues" in the crate
//! `CONTEXT.md`, which is the authoritative description of this crate's behaviour and
//! frozen public API.
//!
//! The container cursor is `beam_codec::Reader` (the ETF codec's bounds-checked
//! big-endian reader) — there is no second cursor and no `unsafe` in this crate.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod attributes;
pub mod chunk;
pub mod container;
pub mod decoded;
pub mod decode;
pub mod error;
pub mod instr;
pub mod ir;
pub mod literal;
pub mod load;
pub mod lower;
pub mod opcode;
pub mod operand;

pub use crate::attributes::{AttributeTable, SharedAttributeTable};
pub use crate::decoded::{DecodedTerm, SharedDecodedTerm};
pub use crate::ir::*;
pub use crate::literal::{LiteralArea, SharedLiteralArea};
pub use crate::load::{load, load_file};
pub use crate::opcode::*;
pub use crate::operand::*;

/// The conventional glob-import surface for downstream crates.
///
/// `use beam_loader::prelude::*;` brings the frozen IR (`ir::*`, including
/// [`FunDescriptor`](crate::ir::FunDescriptor)), the decoded literal area
/// ([`literal::*`](crate::literal)), the decoded term-chunk holders
/// ([`AttributeTable`](crate::attributes::AttributeTable),
/// [`DecodedTerm`](crate::decoded::DecodedTerm)), the operand and register
/// identifiers (`operand::*`), the opcode roster (`opcode::*`) and the two entry
/// points ([`load`], [`load_file`]) into scope. Mirrors the crate-root re-exports.
pub mod prelude {
    pub use crate::attributes::{AttributeTable, SharedAttributeTable};
    pub use crate::decoded::{DecodedTerm, SharedDecodedTerm};
    pub use crate::ir::*;
    pub use crate::literal::{LiteralArea, SharedLiteralArea};
    pub use crate::load::{load, load_file};
    pub use crate::opcode::*;
    pub use crate::operand::*;
}
