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

//! The conventional glob-import surface for downstream crates.
//!
//! `use beam_codec::prelude::*;` brings the codec seam (the reader/writer, the
//! atom cache, the encoder/decoder and the tag constants) into scope. The
//! term-layer helpers in [`crate::term`] are deliberately **not** re-exported:
//! they are an internal stop-gap, not part of the cross-crate contract.

pub use crate::atom_cache::{AtomCache, AtomCacheWriter};
pub use crate::compress::{compress, decompress};
pub use crate::decode::{
    decode_term, decode_term_body, decode_term_body_used, decode_term_used,
    decode_term_with_local_node, decode_term_with_local_node_used, decode_term_with_options,
    decode_term_with_options_and_gate, decode_term_with_options_and_gate_used,
    decode_term_with_options_used, is_compressed_body, is_compressed_image, DecodeOptions, Decoder,
    ExportGate,
};pub use crate::encode::{
    encode_term, encode_term_body, encode_term_body_compressed, encode_term_compressed,
    encode_term_local, encode_term_with_minor_version, AtomEncoding, Encoder, FunIdentity,
    MinorVersion,
};
pub use crate::error::{
    is_unsupported, is_unsupported_local_fun, unsupported, unsupported_local_fun, LOCAL_FUN_REASON,
    UNSUPPORTED_REASON,
};
pub use crate::off_heap::OffHeapBinaries;
pub use crate::reader::Reader;
pub use crate::tag::*;
pub use crate::term::{
    fun_arity_term, fun_creator_pid, fun_env, fun_function, fun_index, fun_kind_term, fun_module,
    fun_num_free_term, fun_old_index, fun_old_uniq, fun_true_arity_term, fun_uniq_bytes, FunKind,
    FunPid, FUN_DESC_WORDS_EXTERNAL, FUN_MAX_FIELD, MAX_ARG,
};
pub use crate::writer::Writer;
pub use beam_term::fun::{fun_desc_words, fun_kind_of_header, fun_true_arity, fun_words};
