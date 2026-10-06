//! The conventional glob-import surface for downstream crates.
//!
//! `use beam_codec::prelude::*;` brings the codec seam (the reader/writer, the
//! atom cache, the encoder/decoder and the tag constants) into scope. The
//! term-layer helpers in [`crate::term`] are deliberately **not** re-exported:
//! they are an internal stop-gap, not part of the cross-crate contract.

pub use crate::atom_cache::{AtomCache, AtomCacheWriter};
pub use crate::compress::{compress, decompress};
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
