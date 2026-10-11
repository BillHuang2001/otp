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

//! # `beam_codec` — External Term Format (ETF) codec
//!
//! Owns the **External Term Format**: encoding and decoding
//! `term_to_binary`/`binary_to_term`, the distribution payload/control-term
//! format and the ETS wire format — the in-memory half of compatibility contract
//! **A.5** (workspace `../CONTEXT.md` §1, Part I §A.5; Stage 0 of Part II §H).
//!
//! Decoding is **allocation-aware**: decoded terms are written into a
//! caller-supplied [`beam_alloc::Heap`] and every atom name is resolved through a
//! caller-supplied [`beam_term::AtomTable`], so *one* code path serves
//! distribution (`beam_dist`), ETS (`beam_ets`), messaging (`beam_message`) and
//! `term_to_binary`/`binary_to_term` (`beam_rt`).
//!
//! Ground truth for every tag value and boxed layout is the legacy C
//! `erts/emulator/beam/external.c` / `external.h` (the wire) and
//! `erts/emulator/beam/erl_term.h` / `erl_map.h` (the term layout). The C is read,
//! never modified.
//!
//! The wire layer (`tag`, `reader`, `writer`, `atom_cache`, the byte-level steps
//! of `encode`/`decode`) is safe Rust; the crate's **only** `unsafe` lives in
//! [`term`], which touches raw heap words to materialize boxed sub-terms. That
//! single deviation is mechanical and auditable: [`term`] is the one module
//! carrying `#[allow(unsafe_code)]` under the crate-wide `#![deny(unsafe_code)]`.
//!
//! Downstream code can `use beam_codec::prelude::*;`.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod atom_cache;
pub mod compress;
pub mod decode;
pub mod encode;
pub mod error;
pub mod off_heap;
pub mod reader;
pub(crate) mod ref_wire;
pub mod tag;
#[allow(unsafe_code)]
pub mod term;
pub mod writer;

pub mod prelude;

pub use crate::atom_cache::{AtomCache, AtomCacheWriter};
pub use crate::decode::{
    decode_term, decode_term_used, decode_term_with_local_node, decode_term_with_local_node_used,
    is_compressed_image, Decoder,
};
pub use crate::off_heap::OffHeapBinaries;
pub use crate::encode::{
    encode_term, encode_term_body, encode_term_body_compressed, encode_term_compressed,
    encode_term_local, encode_term_with_minor_version, AtomEncoding, Encoder, FunIdentity,
    MinorVersion,
};
pub use crate::error::{
    is_unsupported, is_unsupported_local_fun, unsupported, unsupported_local_fun, LOCAL_FUN_REASON,
    UNSUPPORTED_REASON,
};
pub use crate::reader::Reader;
pub use crate::tag::*;
pub use crate::term::{
    fun_arity_term, fun_creator_pid, fun_env, fun_function, fun_index, fun_kind_term, fun_module,
    fun_num_free_term, fun_old_index, fun_old_uniq, fun_true_arity_term, fun_uniq_bytes, FunKind,
    FunPid, FUN_DESC_WORDS_EXTERNAL, FUN_MAX_FIELD, MAX_ARG,
};
pub use crate::term::{internal_ref_id, make_internal_ref};
pub use crate::writer::Writer;
pub use beam_term::fun::{fun_desc_words, fun_kind_of_header, fun_true_arity, fun_words};
