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

//! Byte-exact ground-truth vectors produced by the system oracle.
//!
//! Every vector below is a literal `term_to_binary/1` image captured from the
//! reference implementation (OTP 29.x / erts-17.1, `erl` on `PATH`). Each test
//! asserts **both** directions:
//!
//! 1. `decode_term(bytes, ..)` yields the expected term, checked through the safe
//!    public accessors (`beam_codec::term::*`, `beam_term::*`); and
//! 2. `encode_term(decoded, ..)` reproduces the exact oracle bytes.
//!
//! ## Regenerating a vector
//!
//! Run the system Erlang and paste the printed decimal byte list into the matching
//! `const`:
//!
//! ```text
//! erl -noshell -eval 'io:format("~w~n", [term_to_binary(T)])' -s init stop
//! ```
//!
//! For example
//! `erl -noshell -eval 'io:format("~w~n", [term_to_binary({ok,17})])' -s init stop`
//! prints `<<131,104,2,119,2,111,107,97,17>>`, i.e. the `OK_17` vector below.
//!
//! ## What is *not* byte-exact in the oracle
//!
//! `self()` and `make_ref()` are node-local identifiers whose ids are allocated at
//! run time, so their vectors pin **one observed generation** (node `nonode@nohost`,
//! creation `0`). The test stays exact because the codec contract is a round trip
//! over the pinned bytes, not a re-derivation of the id. The key order of a small
//! map is likewise not canonicalised before encoding, so `MAP_A_1_B_2` pins one
//! observed order; the decoder preserves the wire order and the encoder re-emits it.
//!
//! ## Binaries and bitstrings
//!
//! `BINARY_EXT` (`109`) and `BIT_BINARY_EXT` (`77`) are pinned below too, including
//! the **byte-aligned** cases the oracle encodes as `BINARY_EXT` (`<<7:16>>`) versus
//! the non-aligned ones it encodes as `BIT_BINARY_EXT` (`<<1:3>>`). The vectors were
//! captured the same way as the rest; `oracle_term_to_binary_matches_when_erl_is_available`
//! re-derives them from the system `erl` when one is on `PATH`.
//!
//! Note the ERTS storage divergence: ERTS keeps a binary larger than
//! `ERL_ONHEAP_BINARY_LIMIT` (64 bytes) off-heap as a ref-counted `Binary`, while this
//! crate materializes every binary as an on-heap `ErlHeapBits` block (see
//! `beam_codec::term::make_binary`). The **wire** image is identical, which is what
//! these vectors assert.
//!
//! ## Funs
//!
//! The fun tags `EXPORT_EXT` (`113`) and `NEW_FUN_EXT` (`112`) are pinned below too: an
//! explicit `fun lists:reverse/1`, a genuine closure (`FUN_NEW_FTEST_MAKE3`, whose
//! `uniq`/`old_uniq` are a hash of the *compiled* helper module, so the whole image
//! must be regenerated with the local toolchain), and the `LitT` literal pools of the
//! real `maps` and `gen_server` modules — the exact terms a loader must decode (with
//! the fun nested inside a compound literal) before those modules can load.
//!
//! These tests never use `unsafe`; the crate's only `unsafe` lives in
//! `beam_codec::term`. Panicking on mismatch is intended (a failure is a test
//! failure).

use beam_alloc::Heap;
use beam_codec::compress::decompress;
use beam_codec::term::{
    big_parts, bitstring_bytes, bitstring_size_bits, cons_head, cons_tail, external_pid_parts,
    external_ref_parts, flatmap_parts, float_value, fun_arity_term, fun_creator_pid, fun_env,
    fun_function, fun_index, fun_kind_term, fun_module, fun_num_free_term, fun_old_index,
    fun_old_uniq, fun_uniq_bytes, tuple_arity, tuple_element, FunKind, FunPid,
};
use beam_codec::{
    decode_term, encode_term, encode_term_body, encode_term_compressed, COMPRESSED_EXT,
    VERSION_MAGIC,
};
use beam_term::prelude::{
    atom_val, is_atom, is_list, is_nil, is_small, make_small, signed_value, Atom, AtomTable, Eterm,
    NIL,
};

// ---------------------------------------------------------------------------
// Oracle vectors — one literal `term_to_binary/1` image per term.
// ---------------------------------------------------------------------------

/// `{ok,17}`
const OK_17: &[u8] = &[131, 104, 2, 119, 2, 111, 107, 97, 17];
/// `[1,2,3]` — a proper byte list, so the oracle emits `STRING_EXT` (`107`).
const LIST_123: &[u8] = &[131, 107, 0, 3, 1, 2, 3];
/// `"abc"` — `[97,98,99]`, again `STRING_EXT`.
const STRING_ABC: &[u8] = &[131, 107, 0, 3, 97, 98, 99];
/// `#{a=>1}` — flat map with one pair.
const MAP_A_1: &[u8] = &[131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];
/// `3.14` — `NEW_FLOAT_EXT` (`70`).
const FLOAT_3_14: &[u8] = &[131, 70, 64, 9, 30, 184, 81, 235, 133, 31];
/// `true`
const ATOM_TRUE: &[u8] = &[131, 119, 4, 116, 114, 117, 101];
/// `[]` — `NIL_EXT` (`106`).
const EMPTY_LIST: &[u8] = &[131, 106];
/// `12345678901234567890123` — a positive `SMALL_BIG_EXT` (`110`) with 10 digits.
const BIG_12345: &[u8] = &[131, 110, 10, 0, 203, 68, 66, 113, 118, 78, 182, 66, 157, 2];
/// `{}` — a zero-arity `SMALL_TUPLE_EXT`.
const EMPTY_TUPLE: &[u8] = &[131, 104, 0];
/// `{1,2,3}`
const TUPLE_123: &[u8] = &[131, 104, 3, 97, 1, 97, 2, 97, 3];
/// `[a,b,c]` — not a byte list, so `LIST_EXT` (`108`) with a `NIL_EXT` tail.
const LIST_ABC: &[u8] = &[131, 108, 0, 0, 0, 3, 119, 1, 97, 119, 1, 98, 119, 1, 99, 106];
/// `-1` — negative, so `INTEGER_EXT` (`98`), not `SMALL_INTEGER_EXT`.
const NEG_ONE: &[u8] = &[131, 98, 255, 255, 255, 255];
/// `100000000000000000000` — `10^20`, a `SMALL_BIG_EXT` with 9 digits.
const BIG_1E20: &[u8] = &[131, 110, 9, 0, 0, 0, 16, 99, 45, 94, 199, 107, 5];
/// `4294967296` (`2^32`) — a five-byte `SMALL_BIG_EXT`. The value fits a 60-bit
/// small, so it decodes to one; it still re-encodes byte-identically because a small
/// outside the signed 32-bit range is encoded as a bignum again.
const BIG_2_POW_32: &[u8] = &[131, 110, 5, 0, 0, 0, 0, 0, 1];
/// `18446744073709551633` (`2^64 + 17`) — nine bytes, i.e. one full `ErtsDigit`
/// (`17`) plus a partial one (`1`); the wire keeps all eight bytes of the low digit.
const BIG_2_64_PLUS_17: &[u8] = &[131, 110, 9, 0, 17, 0, 0, 0, 0, 0, 0, 0, 1];
/// `1.0`
const FLOAT_ONE: &[u8] = &[131, 70, 63, 240, 0, 0, 0, 0, 0, 0];
/// `-0.0` — negative zero keeps its sign bit (`0x8000_0000_0000_0000`).
const FLOAT_NEG_ZERO: &[u8] = &[131, 70, 128, 0, 0, 0, 0, 0, 0, 0];
/// `'hello world'` — an atom needing no quoting on the wire.
const ATOM_HELLO_WORLD: &[u8] = &[131, 119, 11, 104, 101, 108, 108, 111, 32, 119, 111,
    114, 108, 100];
/// `'héllo'` — a non-ASCII atom, `SMALL_ATOM_UTF8_EXT` with the UTF-8 bytes
/// `h C3 A9 l l o`.
const ATOM_HELLO_UNICODE: &[u8] = &[131, 119, 6, 104, 195, 169, 108, 108, 111];
/// `#{}` — an empty map (`116`, count `0`).
const EMPTY_MAP: &[u8] = &[131, 116, 0, 0, 0, 0];
/// `#{a=>1,b=>2}` — one observed key order of a two-pair flat map.
const MAP_A_1_B_2: &[u8] = &[131, 116, 0, 0, 0, 2, 119, 1, 97, 97, 1, 119, 1, 98, 97, 2];
/// `self()` — `NEW_PID_EXT` (`88`) on `nonode@nohost`, id `10`, serial/creation `0`.
const PID_SELF: &[u8] = &[131, 88, 119, 13, 110, 111, 110, 111, 100, 101, 64, 110, 111,
    104, 111, 115, 116, 0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0];
/// `make_ref()` — `NEWER_REFERENCE_EXT` (`90`) with three 32-bit ids.
const REF_MAKEREF: &[u8] = &[131, 90, 0, 3, 119, 13, 110, 111, 110, 111, 100, 101, 64,
    110, 111, 104, 111, 115, 116, 0, 0, 0, 0, 0, 1, 57, 124, 156, 252, 0, 3, 130, 192,
    173, 25];
/// `[1|2]` — an improper list: `LIST_EXT` with a non-`NIL` tail.
const IMPROPER_LIST: &[u8] = &[131, 108, 0, 0, 0, 1, 97, 1, 97, 2];
/// `list_to_tuple(lists:seq(1,256))` — arity 256, so `LARGE_TUPLE_EXT` (`105`).
const LARGE_TUPLE_256: &[u8] = &[131, 105, 0, 0, 1, 0, 97, 1, 97, 2, 97, 3, 97, 4, 97,
    5, 97, 6, 97, 7, 97, 8, 97, 9, 97, 10, 97, 11, 97, 12, 97, 13, 97, 14, 97, 15, 97,
    16, 97, 17, 97, 18, 97, 19, 97, 20, 97, 21, 97, 22, 97, 23, 97, 24, 97, 25, 97, 26,
    97, 27, 97, 28, 97, 29, 97, 30, 97, 31, 97, 32, 97, 33, 97, 34, 97, 35, 97, 36, 97,
    37, 97, 38, 97, 39, 97, 40, 97, 41, 97, 42, 97, 43, 97, 44, 97, 45, 97, 46, 97, 47,
    97, 48, 97, 49, 97, 50, 97, 51, 97, 52, 97, 53, 97, 54, 97, 55, 97, 56, 97, 57, 97,
    58, 97, 59, 97, 60, 97, 61, 97, 62, 97, 63, 97, 64, 97, 65, 97, 66, 97, 67, 97, 68,
    97, 69, 97, 70, 97, 71, 97, 72, 97, 73, 97, 74, 97, 75, 97, 76, 97, 77, 97, 78, 97,
    79, 97, 80, 97, 81, 97, 82, 97, 83, 97, 84, 97, 85, 97, 86, 97, 87, 97, 88, 97, 89,
    97, 90, 97, 91, 97, 92, 97, 93, 97, 94, 97, 95, 97, 96, 97, 97, 97, 98, 97, 99, 97,
    100, 97, 101, 97, 102, 97, 103, 97, 104, 97, 105, 97, 106, 97, 107, 97, 108, 97,
    109, 97, 110, 97, 111, 97, 112, 97, 113, 97, 114, 97, 115, 97, 116, 97, 117, 97,
    118, 97, 119, 97, 120, 97, 121, 97, 122, 97, 123, 97, 124, 97, 125, 97, 126, 97,
    127, 97, 128, 97, 129, 97, 130, 97, 131, 97, 132, 97, 133, 97, 134, 97, 135, 97,
    136, 97, 137, 97, 138, 97, 139, 97, 140, 97, 141, 97, 142, 97, 143, 97, 144, 97,
    145, 97, 146, 97, 147, 97, 148, 97, 149, 97, 150, 97, 151, 97, 152, 97, 153, 97,
    154, 97, 155, 97, 156, 97, 157, 97, 158, 97, 159, 97, 160, 97, 161, 97, 162, 97,
    163, 97, 164, 97, 165, 97, 166, 97, 167, 97, 168, 97, 169, 97, 170, 97, 171, 97,
    172, 97, 173, 97, 174, 97, 175, 97, 176, 97, 177, 97, 178, 97, 179, 97, 180, 97,
    181, 97, 182, 97, 183, 97, 184, 97, 185, 97, 186, 97, 187, 97, 188, 97, 189, 97,
    190, 97, 191, 97, 192, 97, 193, 97, 194, 97, 195, 97, 196, 97, 197, 97, 198, 97,
    199, 97, 200, 97, 201, 97, 202, 97, 203, 97, 204, 97, 205, 97, 206, 97, 207, 97,
    208, 97, 209, 97, 210, 97, 211, 97, 212, 97, 213, 97, 214, 97, 215, 97, 216, 97,
    217, 97, 218, 97, 219, 97, 220, 97, 221, 97, 222, 97, 223, 97, 224, 97, 225, 97,
    226, 97, 227, 97, 228, 97, 229, 97, 230, 97, 231, 97, 232, 97, 233, 97, 234, 97,
    235, 97, 236, 97, 237, 97, 238, 97, 239, 97, 240, 97, 241, 97, 242, 97, 243, 97,
    244, 97, 245, 97, 246, 97, 247, 97, 248, 97, 249, 97, 250, 97, 251, 97, 252, 97,
    253, 97, 254, 97, 255, 98, 0, 0, 1, 0];

/// `term_to_binary(lists:duplicate(500, hello), [compressed])` — the oracle's
/// **compressed** image (OTP 29.1.1). Layout: `131, 80, <inner size:4 BE>, <zlib>`.
///
/// The 4-byte size (`0x00000DB2` = 3506) is the length of the inner term body: the
/// uncompressed `term_to_binary(lists:duplicate(500, hello))` image with its version
/// byte removed (`zlib:uncompress/1` over the stream yields exactly `inner[1..]`).
///
/// The zlib bytes are **zlib's** output. `flate2`/`miniz_oxide` would produce a
/// different — equally valid — deflate stream for the same input, so this vector
/// pins the **decoder** and the inner body, not our compressor's byte output (that is
/// covered by the round-trip tests in `roundtrip.rs`).
const COMPRESSED_DUP_500_HELLO: &[u8] = &[
    131, 80, 0, 0, 13, 178, 120, 156, 237, 197, 177, 9, 0, 32, 16, 4, 176, 183, 112, 84, 65, 228,
    192, 210, 197, 29, 192, 53, 44, 146, 38, 169, 106, 247, 244, 57, 146, 45, 73, 146, 36, 73,
    250, 168, 245, 0, 145, 146, 3, 87,
];

/// `fun lists:reverse/1` — an `EXPORT_EXT` (`113`) fun.
///
/// `erl -noshell -eval 'io:format("~w~n", [term_to_binary(fun lists:reverse/1)])' -s init stop`
///
/// Layout: `[113][module atom][function atom][arity]`.
const FUN_EXPORT_LISTS_REVERSE: &[u8] = &[
    131, 113, 119, 5, 108, 105, 115, 116, 115, 119, 7, 114, 101, 118, 101, 114, 115, 101, 97, 1,
];

/// `ftest:make3("aa","bb","cc")` — a genuine `NEW_FUN_EXT` (`112`) closure with
/// three free variables.
///
/// The helper module's source (this is what the compiler hashes into the fun's
/// `uniq`/`index`/`old_uniq` fields, so it must be recorded to regenerate the
/// vector):
///
/// ```erlang
/// -module(ftest).
/// -export([make3/3, make1/1]).
/// make3(A,B,C) -> fun() -> {A,B,C} end.
/// make1(A) -> fun(Z) -> A+Z end.
/// ```
///
/// Generate with:
///
/// ```text
/// erlc ftest.erl
/// erl -noshell -pa . -eval 'io:format("~w~n", [term_to_binary(ftest:make3("aa","bb","cc"))])' -s init stop
/// ```
///
/// Layout: `[112][size:4 BE][arity:1][uniq:16][index:4 BE][num_free:4 BE]
/// [module atom][old_index][old_uniq][creator pid (`NEW_PID_EXT` 88)][env…]`
/// with `size = 4 + payload_len` and **env order = stream order**. Unlike the
/// other vectors, `uniq`/`old_uniq` are a hash of the *compiled* `ftest.beam`, so
/// they change when the compiler does — regenerate the whole image (the round trip
/// is invisible to the codec, which simply forwards the wire fields).
const FUN_NEW_FTEST_MAKE3: &[u8] = &[
    131, 112, 0, 0, 0, 86, 0, 38, 119, 19, 142, 172, 206, 132, 125, 242, 218, 147, 83, 146, 36,
    109, 42, 0, 0, 0, 0, 0, 0, 0, 3, 119, 5, 102, 116, 101, 115, 116, 97, 0, 98, 1, 51, 184,
    156, 88, 119, 13, 110, 111, 110, 111, 100, 101, 64, 110, 111, 104, 111, 115, 116, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 107, 0, 2, 97, 97, 107, 0, 2, 98, 98, 107, 0, 2, 99, 99,
];

/// The `LitT` literal pool of the real `maps` module — a genuine OTP `.beam`
/// literal term, the exact shape a loader must decode before the module can load.
///
/// `erl -noshell -eval '{ok,{_,[{literals,L}]}} = beam_lib:chunks(code:which(maps),
/// [literals]), io:format("~w~n",[term_to_binary(L)]), halt().'`
///
/// The pool is `[ {0, [{error_info, #{module => erl_stdlib_errors}}]}, {1, #{}},
/// {2, fun maps:remove/2} ]` — a three-element list whose last element nests an
/// `EXPORT_EXT` (`113`) fun, so decoding this module's literals exercises the fun
/// path inside a compound term exactly as the loader does.
const LITERALS_MAPS: &[u8] = &[
    131, 108, 0, 0, 0, 3, 104, 2, 97, 0, 108, 0, 0, 0, 1, 104, 2, 119, 10, 101, 114, 114, 111, 114,
    95, 105, 110, 102, 111, 116, 0, 0, 0, 1, 119, 6, 109, 111, 100, 117, 108, 101, 119, 17,
    101, 114, 108, 95, 115, 116, 100, 108, 105, 98, 95, 101, 114, 114, 111, 114, 115, 106,
    104, 2, 97, 1, 116, 0, 0, 0, 0, 104, 2, 97, 2, 113, 119, 4, 109, 97, 112, 115, 119, 6,
    114, 101, 109, 111, 118, 101, 97, 2, 106,
];

/// The `LitT` literal pool of the real `gen_server` module — a larger pool (65
/// elements) that nests four `EXPORT_EXT` (`113`) funs (`fun
/// gen_server:format_log/1` and `/2`).
///
/// `erl -noshell -eval '{ok,{_,[{literals,L}]}} = beam_lib:chunks(code:which(gen_server),
/// [literals]), io:format("~w~n",[term_to_binary(L)]), halt().'`
const LITERALS_GEN_SERVER: &[u8] = &[
    131, 108, 0, 0, 0, 65, 104, 2, 97, 0, 108, 0, 0, 0, 1, 119, 9, 110, 111, 99, 111, 110, 110, 101,
    99, 116, 106, 104, 2, 97, 1, 108, 0, 0, 0, 1, 119, 5, 102, 108, 117, 115, 104, 106, 104,
    2, 97, 2, 104, 2, 119, 4, 101, 120, 105, 116, 119, 6, 110, 111, 114, 109, 97, 108, 104,
    2, 97, 3, 108, 0, 0, 0, 2, 104, 2, 119, 5, 97, 115, 121, 110, 99, 119, 4, 116, 114, 117,
    101, 104, 2, 119, 4, 105, 110, 102, 111, 119, 5, 102, 97, 108, 115, 101, 106, 104, 2,
    97, 4, 116, 0, 0, 0, 0, 104, 2, 97, 5, 104, 2, 119, 10, 103, 101, 110, 95, 115, 101, 114,
    118, 101, 114, 119, 14, 110, 111, 95, 104, 97, 110, 100, 108, 101, 95, 105, 110, 102,
    111, 104, 2, 97, 6, 116, 0, 0, 0, 3, 119, 12, 101, 114, 114, 111, 114, 95, 108, 111, 103,
    103, 101, 114, 116, 0, 0, 0, 2, 119, 3, 116, 97, 103, 119, 11, 119, 97, 114, 110, 105,
    110, 103, 95, 109, 115, 103, 119, 9, 114, 101, 112, 111, 114, 116, 95, 99, 98, 113, 119,
    10, 103, 101, 110, 95, 115, 101, 114, 118, 101, 114, 119, 10, 102, 111, 114, 109, 97,
    116, 95, 108, 111, 103, 97, 1, 119, 6, 100, 111, 109, 97, 105, 110, 108, 0, 0, 0, 1,
    119, 3, 111, 116, 112, 106, 119, 9, 114, 101, 112, 111, 114, 116, 95, 99, 98, 113, 119,
    10, 103, 101, 110, 95, 115, 101, 114, 118, 101, 114, 119, 10, 102, 111, 114, 109, 97,
    116, 95, 108, 111, 103, 97, 2, 104, 2, 97, 7, 116, 0, 0, 0, 3, 119, 4, 108, 105, 110, 101,
    98, 0, 0, 9, 137, 119, 4, 102, 105, 108, 101, 107, 0, 14, 103, 101, 110, 95, 115, 101,
    114, 118, 101, 114, 46, 101, 114, 108, 119, 3, 109, 102, 97, 104, 3, 119, 10, 103, 101,
    110, 95, 115, 101, 114, 118, 101, 114, 119, 15, 116, 114, 121, 95, 104, 97, 110, 100,
    108, 101, 95, 105, 110, 102, 111, 97, 3, 104, 2, 97, 8, 104, 2, 119, 2, 111, 107, 119,
    2, 111, 107, 104, 2, 97, 9, 108, 0, 0, 0, 1, 104, 2, 119, 3, 97, 98, 115, 119, 4, 116,
    114, 117, 101, 106, 104, 2, 97, 10, 107, 0, 42, 42, 68, 66, 71, 42, 32, 126, 116, 112,
    32, 115, 101, 110, 116, 32, 126, 116, 112, 32, 116, 111, 32, 126, 116, 119, 44, 32, 110,
    101, 119, 32, 115, 116, 97, 116, 101, 32, 126, 116, 112, 126, 110, 104, 2, 97, 11, 107,
    0, 25, 42, 68, 66, 71, 42, 32, 126, 116, 112, 32, 110, 101, 119, 32, 115, 116, 97, 116,
    101, 32, 126, 116, 112, 126, 110, 104, 2, 97, 12, 107, 0, 33, 42, 68, 66, 71, 42, 32,
    126, 116, 112, 32, 103, 111, 116, 32, 99, 97, 108, 108, 32, 126, 116, 112, 32, 102, 114,
    111, 109, 32, 126, 116, 119, 126, 110, 104, 2, 97, 13, 107, 0, 24, 42, 68, 66, 71, 42,
    32, 126, 116, 112, 32, 103, 111, 116, 32, 99, 97, 115, 116, 32, 126, 116, 112, 126, 110,
    104, 2, 97, 14, 107, 0, 19, 42, 68, 66, 71, 42, 32, 126, 116, 112, 32, 103, 111, 116,
    32, 126, 116, 112, 126, 110, 104, 2, 97, 15, 107, 0, 20, 42, 68, 66, 71, 42, 32, 126,
    116, 112, 32, 100, 98, 103, 32, 32, 126, 116, 112, 126, 110, 104, 2, 97, 16, 104, 2,
    119, 10, 103, 101, 110, 95, 115, 101, 114, 118, 101, 114, 119, 9, 116, 101, 114, 109, 105,
    110, 97, 116, 101, 104, 2, 97, 17, 116, 0, 0, 0, 3, 119, 12, 101, 114, 114, 111, 114,
    95, 108, 111, 103, 103, 101, 114, 116, 0, 0, 0, 2, 119, 3, 116, 97, 103, 119, 5, 101,
    114, 114, 111, 114, 119, 9, 114, 101, 112, 111, 114, 116, 95, 99, 98, 113, 119, 10, 103,
    101, 110, 95, 115, 101, 114, 118, 101, 114, 119, 10, 102, 111, 114, 109, 97, 116, 95,
    108, 111, 103, 97, 1, 119, 6, 100, 111, 109, 97, 105, 110, 108, 0, 0, 0, 1, 119, 3, 111,
    116, 112, 106, 119, 9, 114, 101, 112, 111, 114, 116, 95, 99, 98, 113, 119, 10, 103, 101,
    110, 95, 115, 101, 114, 118, 101, 114, 119, 10, 102, 111, 114, 109, 97, 116, 95, 108,
    111, 103, 97, 2, 104, 2, 97, 18, 116, 0, 0, 0, 3, 119, 4, 108, 105, 110, 101, 98, 0,
    0, 10, 225, 119, 4, 102, 105, 108, 101, 107, 0, 14, 103, 101, 110, 95, 115, 101, 114, 118,
    101, 114, 46, 101, 114, 108, 119, 3, 109, 102, 97, 104, 3, 119, 10, 103, 101, 110, 95,
    115, 101, 114, 118, 101, 114, 119, 10, 101, 114, 114, 111, 114, 95, 105, 110, 102, 111,
    97, 7, 104, 2, 97, 19, 108, 0, 0, 0, 2, 119, 18, 99, 117, 114, 114, 101, 110, 116, 95,
    115, 116, 97, 99, 107, 116, 114, 97, 99, 101, 119, 15, 114, 101, 103, 105, 115, 116, 101,
    114, 101, 100, 95, 110, 97, 109, 101, 106, 104, 2, 97, 20, 116, 0, 0, 0, 4, 119, 11,
    99, 104, 97, 114, 115, 95, 108, 105, 109, 105, 116, 119, 9, 117, 110, 108, 105, 109, 105,
    116, 101, 100, 119, 5, 100, 101, 112, 116, 104, 119, 9, 117, 110, 108, 105, 109, 105,
    116, 101, 100, 119, 8, 101, 110, 99, 111, 100, 105, 110, 103, 119, 4, 117, 116, 102, 56,
    119, 11, 115, 105, 110, 103, 108, 101, 95, 108, 105, 110, 101, 119, 5, 102, 97, 108, 115,
    101, 104, 2, 97, 21, 107, 0, 9, 46, 32, 76, 97, 98, 101, 108, 58, 32, 104, 2, 97, 22,
    108, 0, 0, 0, 1, 107, 0, 1, 46, 106, 104, 2, 97, 23, 107, 0, 9, 46, 32, 83, 116, 97, 116,
    101, 58, 32, 104, 2, 97, 24, 107, 0, 16, 46, 32, 76, 97, 115, 116, 32, 109, 101, 115,
    115, 97, 103, 101, 58, 32, 104, 2, 97, 25, 107, 0, 10, 46, 32, 82, 101, 97, 115, 111,
    110, 58, 32, 104, 2, 97, 26, 107, 0, 12, 32, 116, 101, 114, 109, 105, 110, 97, 116, 105,
    110, 103, 104, 2, 97, 27, 107, 0, 15, 71, 101, 110, 101, 114, 105, 99, 32, 115, 101, 114,
    118, 101, 114, 32, 104, 2, 97, 28, 107, 0, 21, 46, 32, 85, 110, 104, 97, 110, 100, 108,
    101, 100, 32, 109, 101, 115, 115, 97, 103, 101, 58, 32, 104, 2, 97, 29, 107, 0, 25, 85,
    110, 100, 101, 102, 105, 110, 101, 100, 32, 104, 97, 110, 100, 108, 101, 95, 105, 110,
    102, 111, 32, 105, 110, 32, 104, 2, 97, 30, 108, 0, 0, 0, 1, 107, 0, 2, 126, 110, 106,
    104, 2, 97, 31, 107, 0, 20, 42, 42, 32, 80, 114, 111, 99, 101, 115, 115, 32, 108, 97,
    98, 101, 108, 32, 61, 61, 32, 104, 2, 97, 32, 107, 0, 7, 44, 126, 110, 32, 32, 32, 32,
    104, 2, 97, 33, 108, 0, 0, 0, 1, 107, 0, 3, 93, 126, 110, 106, 104, 2, 97, 34, 107, 0,
    15, 42, 42, 32, 76, 111, 103, 32, 61, 61, 126, 110, 42, 42, 32, 91, 104, 2, 97, 35, 107,
    0, 2, 126, 110, 104, 2, 97, 36, 107, 0, 35, 126, 110, 42, 42, 32, 82, 101, 97, 115, 111,
    110, 32, 102, 111, 114, 32, 116, 101, 114, 109, 105, 110, 97, 116, 105, 111, 110, 32,
    61, 61, 126, 110, 42, 42, 32, 104, 2, 97, 37, 107, 0, 26, 126, 110, 42, 42, 32, 87, 104,
    101, 110, 32, 83, 101, 114, 118, 101, 114, 32, 115, 116, 97, 116, 101, 32, 61, 61, 32,
    104, 2, 97, 38, 107, 0, 23, 42, 42, 32, 76, 97, 115, 116, 32, 109, 101, 115, 115, 97,
    103, 101, 32, 105, 110, 32, 119, 97, 115, 32, 104, 2, 97, 39, 107, 0, 14, 32, 116, 101,
    114, 109, 105, 110, 97, 116, 105, 110, 103, 32, 10, 104, 2, 97, 40, 107, 0, 18, 42, 42,
    32, 71, 101, 110, 101, 114, 105, 99, 32, 115, 101, 114, 118, 101, 114, 32, 104, 2, 97,
    41, 107, 0, 54, 42, 42, 32, 85, 110, 100, 101, 102, 105, 110, 101, 100, 32, 104, 97, 110,
    100, 108, 101, 95, 105, 110, 102, 111, 32, 105, 110, 32, 126, 112, 126, 110, 42, 42,
    32, 85, 110, 104, 97, 110, 100, 108, 101, 100, 32, 109, 101, 115, 115, 97, 103, 101, 58,
    32, 104, 2, 97, 42, 104, 2, 106, 106, 104, 2, 97, 43, 107, 0, 6, 32, 76, 111, 103, 58,
    32, 104, 2, 97, 44, 107, 0, 1, 46, 104, 2, 97, 45, 107, 0, 13, 32, 115, 116, 97, 99, 107,
    116, 114, 97, 99, 101, 58, 32, 104, 2, 97, 46, 107, 0, 8, 32, 67, 108, 105, 101, 110,
    116, 32, 104, 2, 97, 47, 107, 0, 34, 32, 67, 108, 105, 101, 110, 116, 32, 126, 48, 112,
    32, 105, 115, 32, 114, 101, 109, 111, 116, 101, 32, 111, 110, 32, 110, 111, 100, 101,
    32, 126, 48, 112, 46, 104, 2, 97, 48, 107, 0, 20, 32, 67, 108, 105, 101, 110, 116, 32,
    126, 48, 112, 32, 105, 115, 32, 100, 101, 97, 100, 46, 104, 2, 97, 49, 107, 0, 3, 42,
    42, 32, 104, 2, 97, 50, 107, 0, 13, 32, 115, 116, 97, 99, 107, 116, 114, 97, 99, 101, 126,
    110, 104, 2, 97, 51, 107, 0, 10, 42, 42, 32, 67, 108, 105, 101, 110, 116, 32, 104, 2,
    97, 52, 107, 0, 35, 42, 42, 32, 67, 108, 105, 101, 110, 116, 32, 126, 112, 32, 105, 115,
    32, 114, 101, 109, 111, 116, 101, 32, 111, 110, 32, 110, 111, 100, 101, 32, 126, 112,
    126, 110, 104, 2, 97, 53, 107, 0, 22, 42, 42, 32, 67, 108, 105, 101, 110, 116, 32, 126,
    112, 32, 105, 115, 32, 100, 101, 97, 100, 126, 110, 104, 2, 97, 54, 107, 0, 1, 112, 104,
    2, 97, 55, 107, 0, 1, 80, 104, 2, 97, 56, 107, 0, 1, 48, 104, 2, 97, 57, 107, 0, 1,
    116, 104, 2, 97, 58, 107, 0, 25, 83, 116, 97, 116, 117, 115, 32, 102, 111, 114, 32, 103,
    101, 110, 101, 114, 105, 99, 32, 115, 101, 114, 118, 101, 114, 104, 2, 97, 59, 107, 0,
    5, 83, 116, 97, 116, 101, 104, 2, 97, 60, 107, 0, 13, 76, 111, 103, 103, 101, 100, 32,
    101, 118, 101, 110, 116, 115, 104, 2, 97, 61, 107, 0, 6, 80, 97, 114, 101, 110, 116, 104,
    2, 97, 62, 107, 0, 6, 83, 116, 97, 116, 117, 115, 104, 2, 97, 63, 108, 0, 0, 0, 6, 104,
    2, 119, 11, 104, 97, 110, 100, 108, 101, 95, 105, 110, 102, 111, 97, 2, 104, 2, 119, 15,
    104, 97, 110, 100, 108, 101, 95, 99, 111, 110, 116, 105, 110, 117, 101, 97, 2, 104, 2,
    119, 9, 116, 101, 114, 109, 105, 110, 97, 116, 101, 97, 2, 104, 2, 119, 11, 99, 111, 100,
    101, 95, 99, 104, 97, 110, 103, 101, 97, 3, 104, 2, 119, 13, 102, 111, 114, 109, 97,
    116, 95, 115, 116, 97, 116, 117, 115, 97, 1, 104, 2, 119, 13, 102, 111, 114, 109, 97,
    116, 95, 115, 116, 97, 116, 117, 115, 97, 2, 106, 104, 2, 97, 64, 108, 0, 0, 0, 9, 104,
    2, 119, 4, 105, 110, 105, 116, 97, 1, 104, 2, 119, 11, 104, 97, 110, 100, 108, 101, 95,
    99, 97, 108, 108, 97, 3, 104, 2, 119, 11, 104, 97, 110, 100, 108, 101, 95, 99, 97, 115,
    116, 97, 2, 104, 2, 119, 11, 104, 97, 110, 100, 108, 101, 95, 105, 110, 102, 111, 97,
    2, 104, 2, 119, 15, 104, 97, 110, 100, 108, 101, 95, 99, 111, 110, 116, 105, 110, 117,
    101, 97, 2, 104, 2, 119, 9, 116, 101, 114, 109, 105, 110, 97, 116, 101, 97, 2, 104, 2,
    119, 11, 99, 111, 100, 101, 95, 99, 104, 97, 110, 103, 101, 97, 3, 104, 2, 119, 13, 102,
    111, 114, 109, 97, 116, 95, 115, 116, 97, 116, 117, 115, 97, 2, 104, 2, 119, 13, 102,
    111, 114, 109, 97, 116, 95, 115, 116, 97, 116, 117, 115, 97, 1, 106, 106,
];
// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decodes `bytes`, asserts the encoder is byte-exact, and hands back the heap, the
/// atom table and the term (the term borrows the heap's and table's storage, so all
/// three are returned — callers must bind the heap, even as `_heap`).
fn round_trip(bytes: &[u8]) -> (Heap, AtomTable, Eterm) {
    let mut heap = Heap::new(1024).expect("a 1024-word heap is always available");
    let mut atoms = AtomTable::new();
    let term = decode_term(bytes, &mut heap, &mut atoms)
        .unwrap_or_else(|err| panic!("decoding {bytes:?} failed: {err:?}"));
    let reencoded = encode_term(term, &atoms).expect("a decoded term always re-encodes");
    assert_eq!(
        reencoded.as_slice(),
        bytes,
        "re-encoding must reproduce the oracle bytes exactly"
    );
    (heap, atoms, term)
}

/// The name of an atom term, resolved through the table the decoder interned into.
fn atom_name(atoms: &AtomTable, term: Eterm) -> String {
    assert!(is_atom(term), "expected an atom, got {term:?}");
    atoms
        .name(atom_val(term))
        .expect("a decoded atom is interned in the table")
        .to_string()
}

/// The name of an already-resolved [`Atom`], e.g. one an accessor returned.
fn interned_atom_name(atoms: &AtomTable, atom: Atom) -> String {
    atoms
        .name(atom)
        .expect("a decoded atom is interned in the table")
        .to_string()
}

/// The value of a small-integer term.
fn small_value(term: Eterm) -> i64 {
    assert!(is_small(term), "expected a small integer, got {term:?}");
    signed_value(term)
}

/// Walks a cons spine, returning its heads and its final tail (`NIL` for a proper
/// list, the improper tail otherwise).
fn spine(mut cursor: Eterm) -> (Vec<Eterm>, Eterm) {
    let mut heads = Vec::new();
    while is_list(cursor) {
        heads.push(cons_head(cursor).expect("a cons cell has a head"));
        cursor = cons_tail(cursor).expect("a cons cell has a tail");
    }
    (heads, cursor)
}

/// The values of a list of small integers.
fn small_list(term: Eterm) -> Vec<i64> {
    let (heads, tail) = spine(term);
    assert!(is_nil(tail), "expected a proper list, tail is {tail:?}");
    heads.into_iter().map(small_value).collect()
}

// ---------------------------------------------------------------------------
// Seeded vectors (were documented in this file's plan before the bodies landed)
// ---------------------------------------------------------------------------

#[test]
fn seed_tuple_ok_17() {
    let (_heap, atoms, term) = round_trip(OK_17);
    assert_eq!(tuple_arity(term).unwrap(), 2);
    assert_eq!(atom_name(&atoms, tuple_element(term, 0).unwrap()), "ok");
    assert_eq!(tuple_element(term, 1).unwrap(), make_small(17));
    assert_eq!(small_value(tuple_element(term, 1).unwrap()), 17);
}

#[test]
fn seed_byte_lists_are_string_ext() {
    // `[1,2,3]` and `"abc"` are both proper byte lists, so the oracle emits
    // `STRING_EXT`; both must decode to a cons list and re-encode byte-identically.
    let (_heap, _atoms, term) = round_trip(LIST_123);
    assert!(is_list(term));
    assert_eq!(small_list(term), vec![1, 2, 3]);

    let (_heap, _atoms, term) = round_trip(STRING_ABC);
    assert_eq!(small_list(term), vec![97, 98, 99]);
}

#[test]
fn seed_map_a_to_1() {
    let (_heap, atoms, term) = round_trip(MAP_A_1);
    let (keys, values) = flatmap_parts(term).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(values.len(), 1);
    assert_eq!(atom_name(&atoms, keys[0]), "a");
    assert_eq!(small_value(values[0]), 1);
}

#[test]
fn seed_float_3_14() {
    let (_heap, _atoms, term) = round_trip(FLOAT_3_14);
    // Compared by bit pattern so the comparison is exact.
    assert_eq!(float_value(term).unwrap().to_bits(), 3.14_f64.to_bits());
}

#[test]
fn seed_atom_true() {
    let (_heap, atoms, term) = round_trip(ATOM_TRUE);
    assert_eq!(atom_name(&atoms, term), "true");
}

#[test]
fn seed_empty_list() {
    let (_heap, _atoms, term) = round_trip(EMPTY_LIST);
    assert!(is_nil(term));
    assert_eq!(term, NIL);
}

#[test]
fn seed_bignum_12345678901234567890123() {
    let (_heap, _atoms, term) = round_trip(BIG_12345);
    assert!(!is_small(term), "10^23-sized values never fit a 60-bit small");
    let (negative, digits) = big_parts(term).unwrap();
    assert!(!negative);
    // `ErtsDigit`s (`u64`), least significant first — one digit per word, so the ten
    // wire bytes group into one full digit and one partial (`big.h:29`).
    assert_eq!(digits, &[0x42B6_4E76_7142_44CB, 0x029D]);
}

// ---------------------------------------------------------------------------
// Newly generated vectors
// ---------------------------------------------------------------------------

#[test]
fn empty_tuple() {
    let (_heap, _atoms, term) = round_trip(EMPTY_TUPLE);
    assert_eq!(tuple_arity(term).unwrap(), 0);
}

#[test]
fn small_tuple_1_2_3() {
    let (_heap, _atoms, term) = round_trip(TUPLE_123);
    assert_eq!(tuple_arity(term).unwrap(), 3);
    for (index, expected) in [1_i64, 2, 3].into_iter().enumerate() {
        assert_eq!(small_value(tuple_element(term, index).unwrap()), expected);
    }
}

#[test]
fn list_of_atoms_is_list_ext() {
    let (_heap, atoms, term) = round_trip(LIST_ABC);
    let (heads, tail) = spine(term);
    assert_eq!(heads.len(), 3);
    for (head, expected) in heads.iter().zip(["a", "b", "c"]) {
        assert_eq!(atom_name(&atoms, *head), expected);
    }
    assert!(is_nil(tail));
}

#[test]
fn negative_one_is_integer_ext() {
    let (_heap, _atoms, term) = round_trip(NEG_ONE);
    assert_eq!(small_value(term), -1);
}

#[test]
fn bignum_1e20() {
    let (_heap, _atoms, term) = round_trip(BIG_1E20);
    assert!(!is_small(term));
    let (negative, digits) = big_parts(term).unwrap();
    assert!(!negative);
    // The nine wire bytes are one full `ErtsDigit` plus the partial digit `5`.
    assert_eq!(digits, &[0x6BC7_5E2D_6310_0000, 0x5]);
}

#[test]
fn bignum_2_pow_32_decodes_to_a_small() {
    let (_heap, _atoms, term) = round_trip(BIG_2_POW_32);
    assert!(is_small(term), "2^32 fits a 60-bit small");
    assert_eq!(small_value(term), 4_294_967_296);
}

#[test]
fn bignum_2_64_plus_17_keeps_the_full_low_digit() {
    let (_heap, _atoms, term) = round_trip(BIG_2_64_PLUS_17);
    assert!(!is_small(term), "2^64 + 17 exceeds a 60-bit small");
    let (negative, digits) = big_parts(term).unwrap();
    assert!(!negative);
    assert_eq!(digits, &[17, 1]);
}

#[test]
fn bignum_2_pow_3000_uses_large_big() {
    // The oracle's `term_to_binary(1 bsl 3000)`: `LARGE_BIG_EXT` (`111`) with a
    // 376-byte magnitude, i.e. `[111, 0, 0, 1, 120, 0, <375 zero bytes>, 1]`.
    let mut image = vec![131u8, 111, 0, 0, 1, 120, 0];
    image.extend(std::iter::repeat(0u8).take(375));
    image.push(1);

    let (_heap, _atoms, term) = round_trip(&image);
    let (negative, digits) = big_parts(term).unwrap();
    assert!(!negative);
    // 376 bytes = 47 `ErtsDigit`s: 46 zero ones holding the zero bytes, plus the
    // partial digit carrying the leading `0x01` byte at its most significant position.
    assert_eq!(digits.len(), 47);
    assert!(digits[..46].iter().all(|&digit| digit == 0));
    assert_eq!(digits[46], 0x0100_0000_0000_0000);
}

#[test]
fn floats_one_and_negative_zero() {
    let (_heap, _atoms, term) = round_trip(FLOAT_ONE);
    assert_eq!(float_value(term).unwrap().to_bits(), 1.0_f64.to_bits());

    let (_heap, _atoms, term) = round_trip(FLOAT_NEG_ZERO);
    let value = float_value(term).unwrap();
    assert_eq!(value.to_bits(), (-0.0_f64).to_bits());
    assert!(value.is_sign_negative(), "-0.0 keeps its sign bit");
    assert_eq!(value, 0.0);
}

#[test]
fn atoms_with_space_and_unicode() {
    let (_heap, atoms, term) = round_trip(ATOM_HELLO_WORLD);
    assert_eq!(atom_name(&atoms, term), "hello world");

    let (_heap, atoms, term) = round_trip(ATOM_HELLO_UNICODE);
    assert_eq!(atom_name(&atoms, term), "héllo");
}

#[test]
fn empty_map() {
    let (_heap, _atoms, term) = round_trip(EMPTY_MAP);
    let (keys, values) = flatmap_parts(term).unwrap();
    assert!(keys.is_empty());
    assert!(values.is_empty());
}

#[test]
fn map_with_two_pairs() {
    let (_heap, atoms, term) = round_trip(MAP_A_1_B_2);
    let (keys, values) = flatmap_parts(term).unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(values.len(), 2);
    assert_eq!(atom_name(&atoms, keys[0]), "a");
    assert_eq!(small_value(values[0]), 1);
    assert_eq!(atom_name(&atoms, keys[1]), "b");
    assert_eq!(small_value(values[1]), 2);
}

#[test]
fn external_pid_self() {
    let (_heap, atoms, term) = round_trip(PID_SELF);
    let (node, number, serial, creation) = external_pid_parts(term).unwrap();
    assert_eq!(atom_name(&atoms, node), "nonode@nohost");
    assert_eq!((number, serial, creation), (10, 0, 0));
}

#[test]
fn external_reference() {
    let (_heap, atoms, term) = round_trip(REF_MAKEREF);
    let (node, ids, creation) = external_ref_parts(term).unwrap();
    assert_eq!(atom_name(&atoms, node), "nonode@nohost");
    assert_eq!(ids, &[0x0001_397C, 0x9CFC_0003, 0x82C0_AD19]);
    assert_eq!(creation, 0);
}

#[test]
fn improper_list() {
    let (_heap, _atoms, term) = round_trip(IMPROPER_LIST);
    assert!(is_list(term));
    let (heads, tail) = spine(term);
    assert_eq!(heads.len(), 1);
    assert_eq!(small_value(heads[0]), 1);
    assert!(!is_nil(tail), "the tail is the improper element 2");
    assert_eq!(small_value(tail), 2);
}

#[test]
fn large_tuple_arity_256_is_large_tuple_ext() {
    let (_heap, _atoms, term) = round_trip(LARGE_TUPLE_256);
    assert_eq!(tuple_arity(term).unwrap(), 256);
    for index in 0..256 {
        assert_eq!(
            small_value(tuple_element(term, index).unwrap()),
            index as i64 + 1
        );
    }
}

// ---------------------------------------------------------------------------
// COMPRESSED (`80`) — oracle vectors
// ---------------------------------------------------------------------------

#[test]
fn seed_compressed_image_decodes_and_its_inner_body_matches() {
    let mut heap = Heap::new(4096).expect("a 4096-word heap holds 500 cons cells");
    let mut atoms = AtomTable::new();
    let term = decode_term(COMPRESSED_DUP_500_HELLO, &mut heap, &mut atoms)
        .expect("the oracle compressed image must decode");

    // `[hello, hello, …]` — 500 atoms, proper tail.
    let (heads, tail) = spine(term);
    assert!(is_nil(tail), "the inner list must be proper");
    assert_eq!(heads.len(), 500);
    for head in heads {
        assert!(is_atom(head), "expected an atom element, got {head:?}");
        assert_eq!(atom_name(&atoms, head), "hello");
    }

    // The inner body — the zlib stream, decompressed — is byte-identical to our own
    // uncompressed encoding of the same term. This pins the term layer even though
    // the zlib bytes themselves are implementation-defined.
    let inner = decompress(&COMPRESSED_DUP_500_HELLO[6..]).expect("the stream is valid zlib");
    assert_eq!(inner.len(), 3506, "the declared inner size");
    assert_eq!(encode_term_body(term, &atoms).unwrap(), inner);

    // Re-compressing produces a well-formed wrapper whose inner body is the same.
    let image = encode_term_compressed(term, &atoms, 6).unwrap();
    assert_eq!(image[0], VERSION_MAGIC);
    assert_eq!(image[1], COMPRESSED_EXT);
    let mut heap2 = Heap::new(4096).expect("a 4096-word heap holds 500 cons cells");
    let mut atoms2 = AtomTable::new();
    let term2 = decode_term(&image, &mut heap2, &mut atoms2).expect("our image must decode");
    assert_eq!(encode_term_body(term2, &atoms2).unwrap(), inner);
}

#[test]
fn compressed_option_falls_back_when_it_does_not_help() {
    // The C only emits the `COMPRESSED` wrapper when zlib actually shrinks the term;
    // otherwise it reverts to the plain image (`external.c:2284-2293`). `{ok,17}` is
    // small enough that `term_to_binary({ok,17}, [compressed])` is byte-identical to
    // the uncompressed `OK_17` vector (verified with the oracle).
    //
    // Our `encode_term_compressed` deliberately *always* writes the wrapper — the
    // caller asked for compression — so it is not the same bytes for this term; the
    // oracle's (uncompressed) image must nevertheless decode, which this asserts.
    let (_heap, _atoms, term) = round_trip(OK_17);
    assert_eq!(tuple_arity(term).unwrap(), 2);
}

// ---------------------------------------------------------------------------
// Binaries and bitstrings — oracle vectors
// ---------------------------------------------------------------------------

#[test]
fn binary_hi_is_binary_ext() {
    // `<<"hi">>` → `[109][len:4 BE][data]`.
    const BINARY_HI: &[u8] = &[131, 109, 0, 0, 0, 2, 104, 105];
    let (_heap, _atoms, term) = round_trip(BINARY_HI);
    assert_eq!(bitstring_size_bits(term).unwrap(), 16);
    assert_eq!(bitstring_bytes(term).unwrap(), (&[104u8, 105][..], 0, 16));
}

#[test]
fn bit_aligned_bitstring_is_binary_ext() {
    // `<<7:16>>` is byte-aligned, so the oracle still emits `BINARY_EXT` (`109`).
    const BIT_7_16: &[u8] = &[131, 109, 0, 0, 0, 2, 0, 7];
    let (_heap, _atoms, term) = round_trip(BIT_7_16);
    assert_eq!(bitstring_size_bits(term).unwrap(), 16);
    assert_eq!(bitstring_bytes(term).unwrap(), (&[0u8, 7][..], 0, 16));
}

#[test]
fn empty_binary_is_binary_ext() {
    // `<<>>` → `[109][len:4 BE = 0]`.
    const EMPTY_BINARY: &[u8] = &[131, 109, 0, 0, 0, 0];
    let (_heap, _atoms, term) = round_trip(EMPTY_BINARY);
    assert_eq!(bitstring_size_bits(term).unwrap(), 0);
    assert_eq!(bitstring_bytes(term).unwrap(), (&[][..], 0, 0));
}

#[test]
fn non_aligned_bitstrings_are_bit_binary_ext() {
    // `[77][len:4 BE][bits:1][data]` — the length field precedes the trailing-bit
    // count, and `data` is MSB-first inside each byte.
    for &(image, n_bits) in &[
        (&[131u8, 77, 0, 0, 0, 1, 1, 128][..], 1usize), // <<1:1>>
        (&[131, 77, 0, 0, 0, 1, 3, 0b0010_0000], 3),    // <<1:3>>
        (&[131, 77, 0, 0, 0, 2, 4, 0, 0x50], 12),       // <<5:12>>
        (&[131, 77, 0, 0, 0, 2, 5, 0, 0x18], 13),       // <<3:13>>
    ] {
        let (_heap, _atoms, term) = round_trip(image);
        assert_eq!(bitstring_size_bits(term).unwrap(), n_bits);
    }
}

#[test]
fn large_binary_round_trips_through_the_wire() {
    // `list_to_binary(lists:seq(1,100))` — 100 bytes, past `ERL_ONHEAP_BINARY_LIMIT`
    // (64): ERTS stores it off-heap as a ref-counted binary, but the wire image is the
    // same `[109][len:4 BE][data]` and this crate materializes it on-heap.
    let mut image = vec![131u8, 109, 0, 0, 0, 100];
    image.extend(1u8..=100);
    let (_heap, _atoms, term) = round_trip(&image);
    assert_eq!(bitstring_size_bits(term).unwrap(), 800);
}

// ---------------------------------------------------------------------------
// Funs — oracle vectors (`EXPORT_EXT` `113` / `NEW_FUN_EXT` `112`)
// ---------------------------------------------------------------------------

/// Recursively collects every fun term reachable inside `term`, descending through
/// tuples, cons spines and maps — the shape a `.beam` literal pool (`LitT`) has.
fn collect_funs(term: Eterm, out: &mut Vec<Eterm>) {
    if fun_kind_term(term).is_ok() {
        out.push(term);
        return;
    }
    // Walk a cons spine; the final (non-cons) tail is handled below.
    let mut cursor = term;
    while is_list(cursor) {
        collect_funs(cons_head(cursor).unwrap(), out);
        cursor = cons_tail(cursor).unwrap();
    }
    if let Ok(arity) = tuple_arity(cursor) {
        for index in 0..arity {
            collect_funs(tuple_element(cursor, index).unwrap(), out);
        }
    } else if let Ok((keys, values)) = flatmap_parts(cursor) {
        for key in keys {
            collect_funs(*key, out);
        }
        for value in values {
            collect_funs(*value, out);
        }
    }
}

#[test]
fn fun_export_lists_reverse_is_export_ext() {
    // `fun lists:reverse/1` — `EXPORT_EXT` (`113`): `[module][function][arity]`.
    let (_heap, atoms, term) = round_trip(FUN_EXPORT_LISTS_REVERSE);
    assert_eq!(fun_kind_term(term).unwrap(), FunKind::External);
    assert_eq!(fun_arity_term(term).unwrap(), 1);
    assert_eq!(fun_num_free_term(term).unwrap(), 0);
    assert_eq!(interned_atom_name(&atoms, fun_module(term).unwrap()), "lists");
    assert_eq!(interned_atom_name(&atoms, fun_function(term).unwrap()), "reverse");
    // An external fun carries none of the closure-only fields.
    assert!(fun_env(term).unwrap().is_empty());
    assert!(fun_index(term).is_err());
    assert!(fun_creator_pid(term).is_err());
}

#[test]
fn fun_new_ftest_make3_is_new_fun_ext() {
    // `ftest:make3("aa","bb","cc")` — `NEW_FUN_EXT` (`112`): a real closure with
    // three free variables. The env order on the wire is the env order (`env[k]` is
    // the `k`-th stream element), so the three strings must come back in order.
    let (_heap, mut atoms, term) = round_trip(FUN_NEW_FTEST_MAKE3);
    assert_eq!(fun_kind_term(term).unwrap(), FunKind::Imported);
    assert_eq!(fun_arity_term(term).unwrap(), 0);
    assert_eq!(fun_num_free_term(term).unwrap(), 3);
    assert_eq!(interned_atom_name(&atoms, fun_module(term).unwrap()), "ftest");
    // An imported fun has no exported function name.
    assert!(fun_function(term).is_err());
    assert_eq!(fun_index(term).unwrap(), 0);
    assert_eq!(fun_old_index(term).unwrap(), 0);
    assert_eq!(fun_old_uniq(term).unwrap(), 0x0133_B89C);
    // The 16-byte `uniq` is the compiled `ftest.beam`'s hash (see the const doc).
    assert_eq!(
        fun_uniq_bytes(term).unwrap(),
        [
            0x26, 0x77, 0x13, 0x8E, 0xAC, 0xCE, 0x84, 0x7D, 0xF2, 0xDA, 0x93, 0x53, 0x92, 0x24,
            0x6D, 0x2A,
        ]
    );
    let node = atoms.intern("nonode@nohost");
    assert_eq!(
        fun_creator_pid(term).unwrap(),
        FunPid {
            node,
            number: 0,
            serial: 0,
            creation: 0,
        }
    );
    let env = fun_env(term).unwrap();
    assert_eq!(env.len(), 3);
    for (slot, expected) in env.iter().zip([[97_i64, 97], [98, 98], [99, 99]]) {
        assert_eq!(small_list(*slot), expected, "env order must follow the wire");
    }
}

#[test]
fn literals_maps_contains_a_nested_export_fun() {
    // The `maps` module's real `LitT` literal pool — the exact shape a loader must
    // decode before the module can load. Its last element nests `fun maps:remove/2`.
    let (_heap, atoms, term) = round_trip(LITERALS_MAPS);
    let (heads, tail) = spine(term);
    assert!(is_nil(tail), "the literal pool is a proper list");
    assert_eq!(heads.len(), 3);

    // `{2, fun maps:remove/2}`.
    let last = heads[2];
    assert_eq!(tuple_arity(last).unwrap(), 2);
    assert_eq!(small_value(tuple_element(last, 0).unwrap()), 2);
    let fun = tuple_element(last, 1).unwrap();
    assert_eq!(fun_kind_term(fun).unwrap(), FunKind::External);
    assert_eq!(fun_arity_term(fun).unwrap(), 2);
    assert_eq!(interned_atom_name(&atoms, fun_module(fun).unwrap()), "maps");
    assert_eq!(interned_atom_name(&atoms, fun_function(fun).unwrap()), "remove");
}

#[test]
fn literals_gen_server_decodes_with_its_export_funs() {
    // The `gen_server` module's literal pool (65 elements) — a larger real-module
    // case that nests four `EXPORT_EXT` funs. Its pool needs more than the default
    // test heap, so it is decoded with a generously sized one.
    let mut heap = Heap::new(1 << 16).expect("a 65536-word heap is available");
    let mut atoms = AtomTable::new();
    let term = decode_term(LITERALS_GEN_SERVER, &mut heap, &mut atoms)
        .expect("a real module's literal pool must decode");
    assert_eq!(
        encode_term(term, &atoms).expect("a decoded literal pool re-encodes"),
        LITERALS_GEN_SERVER,
        "re-encoding must reproduce the oracle bytes exactly"
    );

    let (heads, tail) = spine(term);
    assert!(is_nil(tail), "the literal pool is a proper list");
    assert_eq!(heads.len(), 65);

    let mut funs = Vec::new();
    collect_funs(term, &mut funs);
    let mut arities = Vec::new();
    for &fun in &funs {
        assert_eq!(fun_kind_term(fun).unwrap(), FunKind::External);
        assert_eq!(interned_atom_name(&atoms, fun_module(fun).unwrap()), "gen_server");
        assert_eq!(interned_atom_name(&atoms, fun_function(fun).unwrap()), "format_log");
        arities.push(fun_arity_term(fun).unwrap());
    }
    assert_eq!(funs.len(), 4, "the pool nests four funs, arities {arities:?}");
    assert!(arities.contains(&1) && arities.contains(&2));
}