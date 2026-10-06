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
pub mod reader;
pub mod tag;
#[allow(unsafe_code)]
pub mod term;
pub mod writer;

pub mod prelude;

pub use crate::atom_cache::{AtomCache, AtomCacheWriter};
pub use crate::decode::{decode_term, Decoder};
pub use crate::encode::{
    encode_term, encode_term_body, encode_term_body_compressed, encode_term_compressed,
    encode_term_local, AtomEncoding, Encoder,
};
pub use crate::error::{
    is_unsupported, is_unsupported_local_fun, unsupported, unsupported_local_fun, LOCAL_FUN_REASON,
    UNSUPPORTED_REASON,
};
pub use crate::reader::Reader;
pub use crate::tag::*;
pub use crate::term::{
    fun_arity_term, fun_creator_pid, fun_env, fun_function, fun_index, fun_kind_term, fun_module,
    fun_num_free_term, fun_old_index, fun_old_uniq, fun_uniq_bytes, FunKind, FunPid, FUN_MAX_FIELD,
    MAX_ARG,
};
pub use crate::writer::Writer;
pub use beam_term::fun::{fun_kind_of_header, fun_words};
