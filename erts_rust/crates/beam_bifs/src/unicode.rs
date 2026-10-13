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

//! `unicode` module BIFs: `characters_to_binary/1,2,3` and
//! `characters_to_list/1,2`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_unicode.c` (the two
//! BIF entry points `unicode_characters_to_binary_2` / `unicode_characters_to_list_2`
//! and their helpers `utf8_need`, `do_build_utf8`, `copy_utf8_bin`,
//! `build_utf8_return`, `build_list_return`, `finalize_list_to_list`),
//! `../../lib/stdlib/src/unicode.erl` (the `/1` and `/3` wrappers,
//! `characters_to_binary_int/3`, `ml_map/3`, `i_trans*`/`o_trans`, `cbv/2`,
//! `do_i_utf8*`) and `bif.tab` (`unicode:characters_to_binary/2`,
//! `unicode:characters_to_list/2`, `unicode:bin_is_7bit/1`). The heap-binary
//! helpers live in [`crate::bits`]; raw term access in [`crate::access`]. This
//! module is safe Rust (the crate's only `unsafe` module is `access.rs`).
//!
//! # Implemented encodings
//!
//! `latin1`, `unicode`/`utf8` (the two atoms are exact aliases, as in
//! `erl_unicode.c`) and `utf16`/`utf32` (with the `{utf16,big|little}` /
//! `{utf32,big|little}` forms, where the bare atom means `big`) as **input and
//! output** encodings, for all five registered arities. An encoding argument
//! naming something else is a typed
//! [`BeamError::BadArg`](beam_common::BeamError::BadArg) — the same class the
//! oracle raises for an unknown encoding atom, and never a wrong result.
//!
//! The two wide families take different routes in the oracle and are ported
//! accordingly:
//!
//! * as an **input** encoding of `characters_to_binary/2` the C BIF **traps** to
//!   `unicode:characters_to_binary_int/2` (`erl_unicode.c:948-954`), i.e. engine 2
//!   below with an output encoding of `unicode`;
//! * as an **input** encoding of `characters_to_list/2` the C BIF traps to
//!   `unicode:characters_to_list_int/2` = "`characters_to_binary_int/2`, then
//!   read the result back as `utf8`" (`unicode.erl:849-857`), so a wide decode
//!   produces a *list* through the UTF-8 round trip;
//! * as an **output** encoding of `characters_to_binary/3` they take
//!   `characters_to_binary_int/3`'s general clause (the guard only covers
//!   `latin1`/`unicode`/`utf8` on both sides).
//!
//! The codec itself (the `<<U/utf16>>` / `<<U/utf32>>` step decoder, the `cbv/2`
//! prefix probe and the `<<One/utf16>>` builders) lives in [`wide`].
//!
//! # Two engines (both ported)
//!
//! * **Engine 1** — the C BIF path, used when the *output* encoding is
//!   `utf8`/`unicode`: `utf8_need` sizes the result, a 7-bit/already-valid fast
//!   path returns the input binary unchanged, `do_build_utf8` fills the buffer
//!   (recording a leftover partial multi-byte sequence) and `build_utf8_return`
//!   shapes the result as a binary, `{error, Converted, Rest}` or
//!   `{incomplete, Converted, Rest}`. `characters_to_list` reuses the same builder
//!   into a byte buffer, then `build_list_return` decodes it into a codepoint list
//!   (its plain-utf8-binary fast path is `utf8_to_list`).
//! * **Engine 2** — `characters_to_binary_int/3` (`ml_map` → `i_trans_chk` /
//!   `o_trans`), used when the output encoding is `latin1`. The `latin1` output
//!   limit is 255, so a codepoint above it yields `{error, Converted, [[Cp] | T]}`
//!   while a codepoint that `list_to_binary/1` cannot hold (a negative integer)
//!   raises `badarg` — both exactly as the oracle.
//!
//! `no_conversion_needed/3` (the `unicode.erl` short-circuit) is applied first for
//! every `/2` and `/3` call, including its `{latin1,latin1}` → `is_binary/1` and
//! `{latin1,utf8}`/`{utf8,latin1}` → `bin_is_7bit/1` cases. (It is never true for a
//! wide encoding: only the four `latin1`/`utf8` combinations are short-circuited.)
//!
//! # Pinned oracle behaviours worth naming
//!
//! * `characters_to_binary/2`'s second argument is the **input** encoding; the
//!   output of `/1` and `/2` is always `unicode`(utf8), so `/2` on `utf8`/`unicode`
//!   takes engine 1 and on `latin1`/`utf8` cross-encoding is decided by
//!   `no_conversion_needed/3`.
//! * A one-element list whose head is a binary is unwrapped before engine 1:
//!   `characters_to_binary([<<"AB">>])` treats `<<"AB">>` as the subject, so an
//!   error there reports a *binary* `Rest` (`{error, <<65>>, <<255,66>>}`) rather
//!   than `[[...]]`.
//! * `<<240,128,128>>` is **`{incomplete, <<>>, <<240,128,128>>}`**, not `error`:
//!   the 4-byte branch buffers the truncated sequence and `check_leftovers/2`
//!   accepts it as a valid prefix. Likewise `characters_to_list(<<240,128,128>>)`
//!   is `{incomplete, [], <<240,128,128>>}`.
//! * An overlong encoding, a lone surrogate, a code point past `U+10FFFF` and a
//!   stray continuation byte are all rejected (they are `error`, not `incomplete`).
//! * `characters_to_binary([1076,1072], latin1, utf8)` errors with the `Rest`
//!   *flattened* by `do_build_utf8` (`{error, <<>>, [1076,1072]}`), while the
//!   engine-2 counterpart nests it (`{error, <<>>, [[256]]}`) — the port keeps
//!   both shapes.
//! * A non-atom encoding argument, an unknown encoding atom, a non-list/non-binary
//!   `Data`, an improper list, a float/atom element, a big code point and a
//!   non-byte-aligned bitstring are all `badarg`.

use beam_common::Result;
use beam_term::{atom_val, signed_value, AtomTable, Eterm, NIL};

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::badarg;
use crate::registry::BifRegistry;

pub(crate) mod grapheme;
mod wide;

// ---------------------------------------------------------------------------
// Encoding selection
// ---------------------------------------------------------------------------

/// The encodings this module implements. `unicode` is an alias of `utf8`
/// (`erl_unicode.c` compares `am_unicode`/`am_utf8` to the same branch) and the
/// bare `utf16`/`utf32` atoms are aliases of their **big**-endian forms
/// (`unicode.erl:1069-1080`, `1112-1134`), so those share one variant each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Encoding {
    /// One byte per code point, `U+0000..=U+00FF`.
    Latin1,
    /// UTF-8.
    Utf8,
    /// UTF-16, big-endian (`utf16` and `{utf16,big}`).
    Utf16Big,
    /// UTF-16, little-endian (`{utf16,little}`).
    Utf16Little,
    /// UTF-32, big-endian (`utf32` and `{utf32,big}`).
    Utf32Big,
    /// UTF-32, little-endian (`{utf32,little}`).
    Utf32Little,
}

/// Parses an encoding argument.
///
/// `Ok(Some(_))` for every encoding this module implements; `Ok(None)` for any
/// **other atom** or any other two-element tuple whose tag is `utf16`/`utf32`
/// (such as `{utf16,foo}`), which the caller reports as `badarg` — the class the
/// oracle's `i_trans`/`o_trans` `function_clause` degrades to; `Err(badarg)` for
/// a non-atom, which the oracle rejects outright.
fn encoding_arg(atoms: &AtomTable, term: Eterm) -> Result<Option<Encoding>> {
    let name = |t: Eterm| -> Option<&str> {
        if access::is_atom(t) {
            atoms.name(atom_val(t))
        } else {
            None
        }
    };
    if access::is_tuple(term) {
        // `{utf16|utf32, big|little}`; anything else is an unknown encoding.
        if access::tuple_size(term) != 2 {
            return Ok(None);
        }
        let little = match name(access::tuple_element(term, 1)) {
            Some("big") => false,
            Some("little") => true,
            _ => return Ok(None),
        };
        return Ok(match name(access::tuple_element(term, 0)) {
            Some("utf16") => Some(if little {
                Encoding::Utf16Little
            } else {
                Encoding::Utf16Big
            }),
            Some("utf32") => Some(if little {
                Encoding::Utf32Little
            } else {
                Encoding::Utf32Big
            }),
            _ => None,
        });
    }
    if !access::is_atom(term) {
        return Err(badarg());
    }
    Ok(match atoms.name(atom_val(term)) {
        Some("latin1") => Some(Encoding::Latin1),
        Some("utf8") | Some("unicode") => Some(Encoding::Utf8),
        Some("utf16") => Some(Encoding::Utf16Big),
        Some("utf32") => Some(Encoding::Utf32Big),
        _ => None,
    })
}

/// `unicode:bin_is_7bit/1` — `true` iff `term` is a byte-aligned binary whose
/// bytes are all below `0x80` (a list is never 7-bit; `erts_get_aligned_binary_bytes`
/// rejects it, as does a non-byte-aligned bitstring).
fn bin_is_7bit(term: Eterm) -> bool {
    match bits::bytes(term) {
        Ok(bytes) => bytes.iter().all(|b| b & 0x80 == 0),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// The UTF-8 / latin1 codec
// ---------------------------------------------------------------------------

/// A partial multi-byte UTF-8 sequence carried across list elements
/// (`copy_utf8_bin`'s `byte leftover[4]` plus `num_leftovers`). Only the list
/// branch of [`do_build_utf8`] buffers; a top-level binary never does.
#[derive(Clone, Copy, Default)]
struct Leftover {
    buf: [u8; 4],
    len: usize,
}

/// The outcome of trying to decode one UTF-8 sequence (`cbv/2` plus the
/// `<<U/utf8>>` pattern's validation).
enum Decode {
    /// A code point and the number of bytes it occupied.
    Ok(u32, usize),
    /// Not a valid sequence (bad continuation, overlong, surrogate, out of range,
    /// or a stray continuation byte) — `cbv` returning `false`.
    Invalid,
    /// A valid *prefix*; the payload is the number of bytes still needed.
    Incomplete(usize),
}

/// Decodes one UTF-8 sequence at the start of `bytes` (which must be non-empty).
///
/// Reproduces `copy_utf8_bin`'s validation exactly: `0xC0`/`0xC1` are overlong,
/// `0xE0` needs a second byte `>= 0xA0`, a `0xED` second byte `>= 0xA0` would be a
/// surrogate, `0xF0` needs a second byte `>= 0x90`, and the 4-byte form must stay
/// at or below `U+10FFFF`.
fn decode_one(bytes: &[u8]) -> Decode {
    let b0 = bytes[0];
    if b0 & 0x80 == 0 {
        return Decode::Ok(u32::from(b0), 1);
    }
    if b0 & 0xE0 == 0xC0 {
        if bytes.len() < 2 {
            return Decode::Incomplete(1);
        }
        if bytes[1] & 0xC0 != 0x80 || b0 < 0xC2 {
            return Decode::Invalid;
        }
        let cp = (u32::from(b0 & 0x1F) << 6) | u32::from(bytes[1] & 0x3F);
        return Decode::Ok(cp, 2);
    }
    if b0 & 0xF0 == 0xE0 {
        if bytes.len() < 2 {
            return Decode::Incomplete(2);
        }
        if bytes[1] & 0xC0 != 0x80 {
            return Decode::Invalid;
        }
        if bytes.len() < 3 {
            return Decode::Incomplete(1);
        }
        if bytes[2] & 0xC0 != 0x80 || (b0 == 0xE0 && bytes[1] < 0xA0) {
            return Decode::Invalid;
        }
        if (b0 & 0x0F) == 0x0D && (bytes[1] & 0x20) != 0 {
            // A lone surrogate, `U+D800..=U+DFFF`.
            return Decode::Invalid;
        }
        let cp = (u32::from(b0 & 0x0F) << 12)
            | (u32::from(bytes[1] & 0x3F) << 6)
            | u32::from(bytes[2] & 0x3F);
        return Decode::Ok(cp, 3);
    }
    if b0 & 0xF8 == 0xF0 {
        if bytes.len() < 2 {
            return Decode::Incomplete(3);
        }
        if bytes[1] & 0xC0 != 0x80 {
            return Decode::Invalid;
        }
        if bytes.len() < 3 {
            return Decode::Incomplete(2);
        }
        if bytes[2] & 0xC0 != 0x80 {
            return Decode::Invalid;
        }
        if bytes.len() < 4 {
            return Decode::Incomplete(1);
        }
        if bytes[3] & 0xC0 != 0x80 || (b0 == 0xF0 && bytes[1] < 0x90) {
            return Decode::Invalid;
        }
        if (b0 & 0x07) > 0x04 || ((b0 & 0x07) == 0x04 && (bytes[1] & 0x3F) > 0x0F) {
            // Above `U+10FFFF`.
            return Decode::Invalid;
        }
        let cp = (u32::from(b0 & 0x07) << 18)
            | (u32::from(bytes[1] & 0x3F) << 12)
            | (u32::from(bytes[2] & 0x3F) << 6)
            | u32::from(bytes[3] & 0x3F);
        return Decode::Ok(cp, 4);
    }
    Decode::Invalid
}

/// Appends the UTF-8 encoding of the code point `v` (`do_build_utf8`'s write
/// code), rejecting a surrogate or a value outside `U+0000..=U+10FFFF`. The code
/// point is taken as an **unsigned** value, exactly like the C `unsigned_val/1`:
/// a negative small integer is therefore rejected.
fn encode_utf8(v: i64, out: &mut Vec<u8>) -> bool {
    let x = v as u64;
    if x < 0x80 {
        out.push(x as u8);
    } else if x < 0x800 {
        out.push(((x >> 6) | 0xC0) as u8);
        out.push(((x & 0x3F) | 0x80) as u8);
    } else if x < 0x10000 {
        if (0xD800..=0xDFFF).contains(&x) {
            return false;
        }
        out.push(((x >> 12) | 0xE0) as u8);
        out.push((((x >> 6) & 0x3F) | 0x80) as u8);
        out.push(((x & 0x3F) | 0x80) as u8);
    } else if x < 0x110000 {
        out.push(((x >> 18) | 0xF0) as u8);
        out.push((((x >> 12) & 0x3F) | 0x80) as u8);
        out.push((((x >> 6) & 0x3F) | 0x80) as u8);
        out.push(((x & 0x3F) | 0x80) as u8);
    } else {
        return false;
    }
    true
}

/// `copy_utf8_bin` — appends the valid UTF-8 sequences of `source` to `target`,
/// buffering a trailing partial sequence in `leftover` when one is supplied (the
/// list branch) and counting code points in `characters`.
///
/// Returns `Err(offset)` with the offset in `source` of the first invalid
/// sequence; the bytes before it have already been appended. A buffered prefix
/// that turns out to be invalid reports offset `0`, matching the C `*err_pos = source`.
fn copy_utf8_bin(
    target: &mut Vec<u8>,
    source: &[u8],
    mut leftover: Option<&mut Leftover>,
    characters: &mut usize,
) -> std::result::Result<(), usize> {
    let mut i = 0usize;
    if let Some(lo) = leftover.as_deref_mut() {
        if lo.len != 0 {
            // Complete the buffered sequence from the head of `source`.
            let need = match lo.buf[0] {
                b if b & 0x80 == 0 => 1,
                b if b & 0xE0 == 0xC0 => 2,
                b if b & 0xF0 == 0xE0 => 3,
                _ => 4,
            };
            let from_source = need - lo.len;
            if source.len() < from_source {
                lo.buf[lo.len..lo.len + source.len()].copy_from_slice(source);
                lo.len += source.len();
                return Ok(());
            }
            lo.buf[lo.len..need].copy_from_slice(&source[..from_source]);
            let completed = lo.buf[..need].to_vec();
            lo.len = 0;
            let mut decoded = Vec::new();
            if copy_utf8_bin(&mut decoded, &completed, None, characters).is_err() {
                return Err(0);
            }
            target.extend_from_slice(&decoded);
            i = from_source;
        }
    }
    while i < source.len() {
        let b0 = source[i];
        let rem = source.len() - i;
        if b0 & 0x80 == 0 {
            target.push(b0);
            i += 1;
        } else if b0 & 0xE0 == 0xC0 {
            if let Some(lo) = leftover.as_mut() {
                if rem < 2 {
                    lo.buf[0] = b0;
                    lo.len = 1;
                    break;
                }
            }
            if rem < 2 || (source[i + 1] & 0xC0) != 0x80 || b0 < 0xC2 {
                return Err(i);
            }
            target.extend_from_slice(&source[i..i + 2]);
            i += 2;
        } else if b0 & 0xF0 == 0xE0 {
            if let Some(lo) = leftover.as_mut() {
                if rem < 3 {
                    lo.buf[..rem].copy_from_slice(&source[i..]);
                    lo.len = rem;
                    break;
                }
            }
            if rem < 3 || (source[i + 1] & 0xC0) != 0x80 || (source[i + 2] & 0xC0) != 0x80 {
                return Err(i);
            }
            if b0 == 0xE0 && source[i + 1] < 0xA0 {
                return Err(i);
            }
            if (b0 & 0x0F) == 0x0D && (source[i + 1] & 0x20) != 0 {
                return Err(i);
            }
            target.extend_from_slice(&source[i..i + 3]);
            i += 3;
        } else if b0 & 0xF8 == 0xF0 {
            if let Some(lo) = leftover.as_mut() {
                if rem < 4 {
                    lo.buf[..rem].copy_from_slice(&source[i..]);
                    lo.len = rem;
                    break;
                }
            }
            if rem < 4
                || (source[i + 1] & 0xC0) != 0x80
                || (source[i + 2] & 0xC0) != 0x80
                || (source[i + 3] & 0xC0) != 0x80
            {
                return Err(i);
            }
            if b0 == 0xF0 && source[i + 1] < 0x90 {
                return Err(i);
            }
            if (b0 & 0x07) > 0x04 || ((b0 & 0x07) == 0x04 && (source[i + 1] & 0x3F) > 0x0F) {
                return Err(i);
            }
            target.extend_from_slice(&source[i..i + 4]);
            i += 4;
        } else {
            return Err(i);
        }
        *characters += 1;
    }
    Ok(())
}

/// `check_leftovers` — `true` iff `source` is a valid *prefix* of a multi-byte
/// UTF-8 sequence (`check_leftovers/2 == 0`).
fn check_leftovers(source: &[u8]) -> bool {
    let size = source.len();
    let b0 = source[0];
    if b0 & 0xE0 == 0xC0 {
        true
    } else if b0 & 0xF0 == 0xE0 {
        size < 2 || (size < 3 && (source[1] & 0xC0) == 0x80)
    } else if b0 & 0xF8 == 0xF0 {
        size < 2
            || (size < 3 && (source[1] & 0xC0) == 0x80)
            || (size < 4 && (source[1] & 0xC0) == 0x80 && (source[2] & 0xC0) == 0x80)
    } else {
        false
    }
}

/// `aligned_binary_size` — the byte size of a byte-aligned bitstring, `None` for a
/// non-aligned bitstring or a non-bitstring.
fn aligned_binary_size(term: Eterm) -> Option<usize> {
    if !bits::is_binary(term) {
        return None;
    }
    bits::byte_size(term).ok()
}

/// `latin1_binary_need` — the UTF-8 byte count of a latin1 binary (`1` per ASCII
/// byte, `2` per byte with the high bit set).
fn latin1_binary_need(term: Eterm) -> Option<usize> {
    let data = bits::bytes(term).ok()?;
    Some(data.iter().map(|b| if b & 0x80 == 0 { 1 } else { 2 }).sum())
}

/// `utf8_need` — the exact UTF-8 byte size of `ioterm` interpreted with `latin1`
/// as the input encoding, or `None` for the C `-1` (`badarg`). Note the C quirk
/// the port preserves: a bignum element stops the walk and returns the size so far,
/// so `do_build_utf8` produces the *informative error tuple* rather than `badarg`
/// (M1 builds no bignums, so this only documents the intended behaviour).
fn utf8_need(ioterm: Eterm, latin1: bool) -> Option<usize> {
    if access::is_nil(ioterm) {
        return Some(0);
    }
    if bits::is_bitstring(ioterm) {
        return if latin1 {
            latin1_binary_need(ioterm)
        } else {
            aligned_binary_size(ioterm)
        };
    }
    if !access::is_list(ioterm) {
        return None;
    }
    let mut need = 0usize;
    let mut stack: Vec<Eterm> = vec![ioterm];
    while let Some(mut cur) = stack.pop() {
        loop {
            if access::is_nil(cur) {
                break;
            }
            if !access::is_list(cur) {
                // Improper list tail: only a bitstring is accepted.
                let x = if latin1 {
                    latin1_binary_need(cur)?
                } else {
                    aligned_binary_size(cur)?
                };
                need += x;
                break;
            }
            let obj = access::car(cur);
            let tail = access::cdr(cur);
            if access::is_nil(obj) {
                // Empty sublists are ignored.
            } else if access::is_small(obj) {
                let x = signed_value(obj) as u64;
                need += if x < 0x80 {
                    1
                } else if x < 0x800 {
                    2
                } else if x < 0x10000 {
                    3
                } else {
                    4
                };
            } else if access::is_list(obj) {
                stack.push(tail);
                cur = obj;
                continue;
            } else if bits::is_bitstring(obj) {
                need += if latin1 {
                    latin1_binary_need(obj)?
                } else {
                    aligned_binary_size(obj)?
                };
            } else if access::is_big(obj) {
                // Let `do_build_utf8` produce the error tuple (see the doc note).
                return Some(need);
            } else {
                return None;
            }
            cur = tail;
        }
    }
    Some(need)
}

/// `is_valid_utf8` — the whole of `term` is a valid UTF-8 byte-aligned binary.
fn is_valid_utf8(term: Eterm) -> Result<bool> {
    let bytes = bits::bytes(term)?;
    let mut sink = Vec::new();
    let mut characters = 0usize;
    Ok(copy_utf8_bin(&mut sink, &bytes, None, &mut characters).is_ok())
}

/// A sub-binary of `orig` starting at `offset` (`erts_make_sub_binary`; M1 copies
/// the bytes onto the heap — see `crate::bits`'s placement deviation).
fn sub_binary(ctx: &mut BifContext<'_>, orig: &[u8], offset: usize) -> Result<Eterm> {
    bits::make_binary(ctx.heap(), &orig[offset..])
}

/// `{Tag, A, B}` with `Tag` **produced** by the caller (`error`/`incomplete`/`ok`),
/// so it is interned on demand rather than looked up.
fn tuple3(ctx: &mut BifContext<'_>, tag: &str, a: Eterm, b: Eterm) -> Result<Eterm> {
    let tag = ctx.intern_atom(tag)?;
    ctx.tuple(&[tag, a, b])
}

// ---------------------------------------------------------------------------
// `do_build_utf8` — the shared builder
// ---------------------------------------------------------------------------

/// `do_build_utf8` — walks the (possibly deep) iolist `ioterm`, appending the
/// UTF-8 bytes of every character to `target`, and returns the term that was *not*
/// consumed (the C `rest_term`). On any conversion failure `err` is set and the
/// returned term is the error rest: the current list position, with every list
/// level still on the work stack re-consed onto it.
///
/// The C `left` work budget is unbounded here, so a binary is never split and the
/// `err == 0 && rest != NIL` trap is not reached from this function.
fn do_build_utf8(
    ctx: &mut BifContext<'_>,
    ioterm: Eterm,
    latin1: bool,
    target: &mut Vec<u8>,
    characters: &mut usize,
    leftover: &mut Option<Leftover>,
    err: &mut bool,
) -> Result<Eterm> {
    *err = false;
    if access::is_nil(ioterm) {
        return Ok(ioterm);
    }
    if bits::is_bitstring(ioterm) {
        let orig = match bits::bytes(ioterm) {
            Ok(bytes) => bytes,
            // A non-byte-aligned bitstring: `erts_get_aligned_binary_bytes` is NULL.
            Err(_) => {
                *err = true;
                return Ok(ioterm);
            }
        };
        if !latin1 {
            if let Err(offset) = copy_utf8_bin(target, &orig, leftover.as_mut(), characters) {
                *err = true;
                return sub_binary(ctx, &orig, offset);
            }
        } else {
            for &b in &orig {
                if b < 0x80 {
                    target.push(b);
                } else {
                    target.push((b >> 6) | 0xC0);
                    target.push((b & 0x3F) | 0x80);
                }
                *characters += 1;
            }
        }
        return Ok(NIL);
    }
    if !access::is_list(ioterm) {
        *err = true;
        return Ok(ioterm);
    }

    // A list: walk it depth-first, keeping pending tails on an explicit stack, so
    // the error rest can be rebuilt exactly as the C `done:` label does.
    let mut stack: Vec<Eterm> = Vec::new();
    let mut iot = ioterm;
    'walk: loop {
        if access::is_nil(iot) {
            match stack.pop() {
                Some(next) => {
                    iot = next;
                    continue 'walk;
                }
                None => break 'walk,
            }
        }
        if access::is_list(iot) {
            loop {
                let obj = access::car(iot);
                let tail = access::cdr(iot);
                if access::is_small(obj) {
                    if leftover.as_ref().is_some_and(|lo| lo.len != 0) {
                        // A pending partial sequence followed by an integer.
                        *err = true;
                        break 'walk;
                    }
                    let x = signed_value(obj) as u64;
                    if latin1 && x > 0xFF {
                        *err = true;
                        break 'walk;
                    }
                    if !encode_utf8(signed_value(obj), target) {
                        *err = true;
                        break 'walk;
                    }
                    *characters += 1;
                    iot = tail;
                    if !access::is_list(iot) {
                        break;
                    }
                } else if access::is_nil(obj) {
                    iot = tail;
                    if !access::is_list(iot) {
                        break;
                    }
                } else if access::is_list(obj) {
                    stack.push(tail);
                    iot = obj;
                    break;
                } else if bits::is_bitstring(obj) {
                    let rest_term = do_build_utf8(ctx, obj, latin1, target, characters, leftover, err)?;
                    if *err {
                        iot = ctx.cons(rest_term, tail)?;
                        break 'walk;
                    }
                    if !access::is_nil(rest_term) {
                        iot = ctx.cons(rest_term, tail)?;
                        break 'walk;
                    }
                    iot = tail;
                    if !access::is_list(iot) {
                        break;
                    }
                } else {
                    *err = true;
                    break 'walk;
                }
            }
            continue 'walk;
        }
        // An improper list tail: a bitstring is converted, anything else errors.
        if bits::is_bitstring(iot) {
            let rest = do_build_utf8(ctx, iot, latin1, target, characters, leftover, err)?;
            iot = rest;
            if *err {
                break 'walk;
            }
            continue 'walk;
        }
        *err = true;
        break 'walk;
    }
    while let Some(st) = stack.pop() {
        iot = ctx.cons(iot, st)?;
    }
    Ok(iot)
}

/// The leftover buffer as a byte slice (empty when there is none).
fn leftover_bytes(leftover: Option<Leftover>) -> Vec<u8> {
    leftover.map(|lo| lo.buf[..lo.len].to_vec()).unwrap_or_default()
}

/// Decodes a buffer this module produced into a list of code points. The buffer
/// only ever holds complete, valid sequences, so a failure here is impossible.
fn codepoint_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Result<Eterm> {
    let mut values = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        match decode_one(&bytes[i..]) {
            Decode::Ok(cp, len) => {
                values.push(ctx.int(i64::from(cp))?);
                i += len;
            }
            _ => return Err(badarg()),
        }
    }
    ctx.list(&values)
}

// ---------------------------------------------------------------------------
// Engine 1 — output encoding `utf8`/`unicode`
// ---------------------------------------------------------------------------

/// `unicode_characters_to_binary_2` with an unbounded work budget: converts
/// `subject` to UTF-8 (`latin1` = the *input* encoding is latin1) and shapes the
/// result (`build_utf8_return`).
fn engine1_binary(ctx: &mut BifContext<'_>, subject: Eterm, latin1: bool) -> Result<Eterm> {
    let need = match utf8_need(subject, latin1) {
        Some(need) => need,
        None => return Err(badarg()),
    };
    if bits::is_binary(subject)
        && aligned_binary_size(subject) == Some(need)
        && (latin1 || is_valid_utf8(subject)?)
    {
        // Already the right bytes: hand the binary back untouched.
        return Ok(subject);
    }

    let mut target = Vec::with_capacity(need);
    let mut characters = 0usize;
    let mut leftover: Option<Leftover> = Some(Leftover::default());
    let mut err = false;
    let mut rest = do_build_utf8(ctx, subject, latin1, &mut target, &mut characters, &mut leftover, &mut err)?;
    // With an unbounded budget `do_build_utf8` consumes the whole subject; the C
    // `err == 0 && rest != NIL` trap would continue on the tail, so do the same.
    while !err && !access::is_nil(rest) {
        rest = do_build_utf8(ctx, rest, latin1, &mut target, &mut characters, &mut leftover, &mut err)?;
    }

    let lo = leftover_bytes(leftover);
    let converted = bits::make_binary(ctx.heap(), &target)?;
    if err {
        if lo.is_empty() {
            return tuple3(ctx, "error", converted, rest);
        }
        let leftover_bin = bits::make_binary(ctx.heap(), &lo)?;
        let with_nil = ctx.cons(rest, NIL)?;
        let rest = ctx.cons(leftover_bin, with_nil)?;
        return tuple3(ctx, "error", converted, rest);
    }
    if lo.is_empty() {
        return Ok(converted);
    }
    // A valid prefix was buffered: `incomplete`, unless it is not a valid prefix.
    let leftover_bin = bits::make_binary(ctx.heap(), &lo)?;
    let reason = if check_leftovers(&lo) { "incomplete" } else { "error" };
    tuple3(ctx, reason, converted, leftover_bin)
}

/// `build_list_return` + `finalize_list_to_list`: converts `subject` to a code
/// point list, or `{error|incomplete, list, Rest}`.
fn engine1_list(ctx: &mut BifContext<'_>, subject: Eterm, latin1: bool) -> Result<Eterm> {
    let need = match utf8_need(subject, latin1) {
        Some(need) => need,
        None => return Err(badarg()),
    };
    let mut target = Vec::with_capacity(need);
    let mut characters = 0usize;
    let mut leftover: Option<Leftover> = Some(Leftover::default());
    let mut err = false;
    let mut rest = do_build_utf8(ctx, subject, latin1, &mut target, &mut characters, &mut leftover, &mut err)?;
    while !err && !access::is_nil(rest) {
        rest = do_build_utf8(ctx, rest, latin1, &mut target, &mut characters, &mut leftover, &mut err)?;
    }

    let lo = leftover_bytes(leftover);
    let state;
    if err {
        state = "error";
        if !lo.is_empty() {
            let leftover_bin = bits::make_binary(ctx.heap(), &lo)?;
            let with_nil = ctx.cons(rest, NIL)?;
            rest = ctx.cons(leftover_bin, with_nil)?;
        }
    } else if access::is_nil(rest) && !lo.is_empty() {
        let leftover_bin = bits::make_binary(ctx.heap(), &lo)?;
        state = if check_leftovers(&lo) { "incomplete" } else { "error" };
        rest = leftover_bin;
    } else {
        state = "";
    }

    let converted = codepoint_list(ctx, &target)?;
    if state.is_empty() {
        return Ok(converted);
    }
    tuple3(ctx, state, converted, rest)
}

/// `utf8_to_list` / `do_bif_utf8_to_list` — the fast path for a plain, byte-aligned
/// UTF-8 binary: analyse the input, decode the accepted prefix and report the tail.
fn utf8_to_list(ctx: &mut BifContext<'_>, bin: Eterm) -> Result<Eterm> {
    let bytes = bits::bytes(bin)?;
    let mut i = 0usize;
    let mut state = "ok";
    while i < bytes.len() {
        match decode_one(&bytes[i..]) {
            Decode::Ok(_, len) => i += len,
            Decode::Invalid => {
                state = "error";
                break;
            }
            Decode::Incomplete(_) => {
                state = "incomplete";
                break;
            }
        }
    }
    let converted = codepoint_list(ctx, &bytes[..i])?;
    if state == "ok" {
        return Ok(converted);
    }
    let rest = bits::make_binary(ctx.heap(), &bytes[i..])?;
    // An "incomplete" that is not a valid prefix is really an error.
    let state = if state == "incomplete" && !check_leftovers(&bytes[i..]) {
        "error"
    } else {
        state
    };
    tuple3(ctx, state, converted, rest)
}

// ---------------------------------------------------------------------------
// Engine 2 — `characters_to_binary_int/3` (`ml_map` + trans coders)
// ---------------------------------------------------------------------------

/// The `latin1`-only input transcoders (`i_trans_chk`).
enum InTrans {
    /// The decoded code points.
    Codepoints(Vec<i64>),
    /// `{error, Translated, Rest}`.
    Error { translated: Vec<i64>, rest: Eterm },
    /// `{incomplete, Translated, Rest, Missing}`.
    Incomplete {
        translated: Vec<i64>,
        rest: Eterm,
        missing: usize,
    },
}

/// The `o_trans` result for a single code point.
enum OutTransInt {
    /// The encoded bytes.
    Bin(Vec<u8>),
    /// `{error, _, [Part]}` — an unencodable code point (the `<<One/utf8>>` raise
    /// that `do_o_binary2` catches).
    ErrorPart,
    /// A `badarg` raise (`list_to_binary/1` rejecting the code point).
    Raise,
}

/// What `ml_map/3`'s `Fun` receives.
enum Part<'a> {
    Bin(&'a [u8]),
    Int(i64),
}

/// One `Fun(Part, Accum)` application.
enum MapStep {
    Bin(Vec<u8>),
    Error { converted: Vec<u8>, rest: Eterm },
    Incomplete {
        converted: Vec<u8>,
        rest: Eterm,
        missing: usize,
    },
}

/// The `ml_map/3` return value.
enum MlResult {
    Bin(Vec<u8>),
    /// `{incomplete, Accum, Inc, X}` from the `[]`-with-pending clause.
    Incomplete {
        accum: Vec<u8>,
        rest: Eterm,
        missing: usize,
    },
    /// `{error, Accum, Rest}`.
    Error { accum: Vec<u8>, rest: Eterm },
}

/// The fixed parts of `characters_to_binary_int/3`'s closure environment.
struct MapEnv {
    in_enc: Encoding,
    /// `true` for `i_trans_chk` (the `latin1` output encoding).
    chk: bool,
    /// The `Limit` the integer parts are checked against.
    limit: i64,
    /// The output encoding — `o_trans(OutEncoding)`.
    out_enc: Encoding,
}

/// One decode step of `enc`'s decoder at the head of `bytes` (the Erlang
/// `<<U/utf16>>`/`<<U/utf32>>`/`<<U/utf8>>` binary-pattern match, with `cbv/2`
/// standing in when it fails). `latin1` never reaches here — [`in_trans`]
/// short-circuits it with `binary_to_list/1`.
fn in_step(enc: Encoding, bytes: &[u8]) -> wide::Step {
    match enc {
        Encoding::Utf16Big => wide::decode_utf16(bytes, false),
        Encoding::Utf16Little => wide::decode_utf16(bytes, true),
        Encoding::Utf32Big => wide::decode_utf32(bytes, false),
        Encoding::Utf32Little => wide::decode_utf32(bytes, true),
        Encoding::Latin1 | Encoding::Utf8 => match decode_one(bytes) {
            Decode::Ok(cp, len) => wide::Step::Ok(cp, len),
            Decode::Invalid => wide::Step::Invalid,
            Decode::Incomplete(missing) => wide::Step::Incomplete(missing),
        },
    }
}

/// `i_trans` / `i_trans_chk` for every implemented input encoding.
fn in_trans(ctx: &mut BifContext<'_>, env: &MapEnv, bytes: &[u8]) -> Result<InTrans> {
    if env.in_enc == Encoding::Latin1 {
        // `binary_to_list/1` — every byte is a code point, never an error.
        return Ok(InTrans::Codepoints(bytes.iter().map(|&b| i64::from(b)).collect()));
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        match in_step(env.in_enc, &bytes[i..]) {
            wide::Step::Ok(cp, len) => {
                if env.chk && cp > 255 {
                    return Ok(InTrans::Error {
                        translated: out,
                        rest: bits::make_binary(ctx.heap(), &bytes[i..])?,
                    });
                }
                out.push(i64::from(cp));
                i += len;
            }
            wide::Step::Invalid => {
                return Ok(InTrans::Error {
                    translated: out,
                    rest: bits::make_binary(ctx.heap(), &bytes[i..])?,
                });
            }
            wide::Step::Incomplete(missing) => {
                return Ok(InTrans::Incomplete {
                    translated: out,
                    rest: bits::make_binary(ctx.heap(), &bytes[i..])?,
                    missing,
                });
            }
        }
    }
    Ok(InTrans::Codepoints(out))
}

/// `o_trans` applied to one code point.
fn out_trans_one(env: &MapEnv, value: i64) -> OutTransInt {
    match env.out_enc {
        Encoding::Latin1 => {
            if (0..=255).contains(&value) {
                OutTransInt::Bin(vec![value as u8])
            } else {
                // `list_to_binary/1` raises; `characters_to_binary/3` turns it into `badarg`.
                OutTransInt::Raise
            }
        }
        Encoding::Utf8 => {
            let mut out = Vec::new();
            if encode_utf8(value, &mut out) {
                OutTransInt::Bin(out)
            } else {
                OutTransInt::ErrorPart
            }
        }
        // `<<One/utf16>>` &c.: an unencodable code point raises inside
        // `do_o_binary2`, which catches it into `{error, <<>>, [One|T]}`.
        Encoding::Utf16Big => wide::encode_utf16(value, false).map_or(OutTransInt::ErrorPart, OutTransInt::Bin),
        Encoding::Utf16Little => wide::encode_utf16(value, true).map_or(OutTransInt::ErrorPart, OutTransInt::Bin),
        Encoding::Utf32Big => wide::encode_utf32(value, false).map_or(OutTransInt::ErrorPart, OutTransInt::Bin),
        Encoding::Utf32Little => wide::encode_utf32(value, true).map_or(OutTransInt::ErrorPart, OutTransInt::Bin),
    }
}

/// `o_trans` applied to a translated list of code points.
fn out_trans(env: &MapEnv, cps: &[i64]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(cps.len());
    for &cp in cps {
        match out_trans_one(env, cp) {
            OutTransInt::Bin(bytes) => out.extend_from_slice(&bytes),
            // `do_o_binary`'s error tuple is spliced into a binary, which raises;
            // `list_to_binary/1` raises directly.
            OutTransInt::ErrorPart | OutTransInt::Raise => return Err(badarg()),
        }
    }
    Ok(out)
}

/// `Fun(Part, Accum)` from `characters_to_binary_int/3`.
fn map_fun(ctx: &mut BifContext<'_>, env: &MapEnv, part: Part<'_>, accum: Vec<u8>) -> Result<MapStep> {
    match part {
        Part::Bin(bytes) => match in_trans(ctx, env, bytes)? {
            InTrans::Codepoints(cps) => {
                let mut out = accum;
                out.extend_from_slice(&out_trans(env, &cps)?);
                Ok(MapStep::Bin(out))
            }
            InTrans::Error { translated, rest } => {
                let mut out = accum;
                out.extend_from_slice(&out_trans(env, &translated)?);
                Ok(MapStep::Error { converted: out, rest })
            }
            InTrans::Incomplete {
                translated,
                rest,
                missing,
            } => {
                let mut out = accum;
                out.extend_from_slice(&out_trans(env, &translated)?);
                Ok(MapStep::Incomplete {
                    converted: out,
                    rest,
                    missing,
                })
            }
        },
        Part::Int(value) => {
            // Over the limit (or a bignum, which cannot be encoded): `{error, Accum, [Part]}`.
            if value > env.limit {
                let one = ctx.int(value)?;
                return Ok(MapStep::Error {
                    converted: accum,
                    rest: ctx.list(&[one])?,
                });
            }
            match out_trans_one(env, value) {
                OutTransInt::Bin(bytes) => {
                    let mut out = accum;
                    out.extend_from_slice(&bytes);
                    Ok(MapStep::Bin(out))
                }
                OutTransInt::ErrorPart => {
                    let one = ctx.int(value)?;
                    Ok(MapStep::Error {
                        converted: accum,
                        rest: ctx.list(&[one])?,
                    })
                }
                OutTransInt::Raise => Err(badarg()),
            }
        }
    }
}

/// `ml_map/3` — the iolist walk of `characters_to_binary_int/3`, with the pending
/// partial sequence carried exactly as the Erlang `{{Incomplete, Missing}, Accum}`
/// accumulator does.
fn ml_map(
    ctx: &mut BifContext<'_>,
    env: &MapEnv,
    mut cur: Eterm,
    mut acc: Vec<u8>,
    mut pending: Option<(Eterm, usize)>,
) -> Result<MlResult> {
    loop {
        if access::is_list(cur) {
            let part = access::car(cur);
            let tail = access::cdr(cur);

            if let Some((incomplete, missing)) = pending {
                // Clause 3: a pending partial sequence followed by an integer.
                if access::is_integer(part) {
                    let rest = ctx.cons(incomplete, cur)?;
                    return Ok(MlResult::Error { accum: acc, rest });
                }
                // Clause 5: fill the pending sequence from this binary.
                if bits::is_binary(part) {
                    let bytes = bits::bytes(part)?;
                    let incomplete_bytes = bits::bytes(incomplete)?;
                    if bytes.len() >= missing {
                        let mut new_part = incomplete_bytes;
                        new_part.extend_from_slice(&bytes[..missing]);
                        let trailing = &bytes[missing..];
                        let new_part = bits::make_binary(ctx.heap(), &new_part)?;
                        let trailing = bits::make_binary(ctx.heap(), trailing)?;
                        let tail2 = ctx.cons(trailing, tail)?;
                        cur = ctx.cons(new_part, tail2)?;
                        pending = None;
                        continue;
                    }
                    let mut new_incomplete = incomplete_bytes;
                    new_incomplete.extend_from_slice(&bytes);
                    let term = bits::make_binary(ctx.heap(), &new_incomplete)?;
                    pending = Some((term, missing - bytes.len()));
                    cur = tail;
                    continue;
                }
            }

            // Clause 4: an integer part.
            if access::is_integer(part) {
                if access::is_small(part) {
                    let value = signed_value(part);
                    match map_fun(ctx, env, Part::Int(value), acc)? {
                        MapStep::Bin(out) => {
                            acc = out;
                            cur = tail;
                            continue;
                        }
                        MapStep::Error { converted, rest } => {
                            // `{error, Converted, [Rest | T]}`.
                            let rest = ctx.cons(rest, tail)?;
                            return Ok(MlResult::Error { accum: converted, rest });
                        }
                        MapStep::Incomplete { .. } => {
                            // An integer part is never incomplete.
                            return Err(badarg());
                        }
                    }
                }
                // A bignum is always above the limit: `{error, Accum, [[Part] | T]}`.
                let one = ctx.list(&[part])?;
                let rest = ctx.cons(one, tail)?;
                return Ok(MlResult::Error { accum: acc, rest });
            }

            // Clauses 6/7: a binary part (> 8192 bytes is split first).
            if bits::is_binary(part) {
                let bytes = bits::bytes(part)?;
                if bytes.len() > 8192 {
                    let new_part = bits::make_binary(ctx.heap(), &bytes[..8192])?;
                    let trailing = bits::make_binary(ctx.heap(), &bytes[8192..])?;
                    let tail2 = ctx.cons(trailing, tail)?;
                    cur = ctx.cons(new_part, tail2)?;
                    continue;
                }
                match map_fun(ctx, env, Part::Bin(&bytes), acc)? {
                    MapStep::Bin(out) => {
                        acc = out;
                        cur = tail;
                        continue;
                    }
                    MapStep::Incomplete {
                        converted,
                        rest,
                        missing,
                    } => {
                        pending = Some((rest, missing));
                        acc = converted;
                        cur = tail;
                        continue;
                    }
                    MapStep::Error { converted, rest } => {
                        let rest = ctx.cons(rest, tail)?;
                        return Ok(MlResult::Error { accum: converted, rest });
                    }
                }
            }

            // Clause 8: a list part (`[]` is a list in Erlang, so this covers an
            // empty sublist too, which then hits clause 1/2 for its own value).
            if access::is_nil(part) || access::is_list(part) {
                match ml_map(ctx, env, part, acc, pending)? {
                    MlResult::Bin(out) => {
                        acc = out;
                        cur = tail;
                        continue;
                    }
                    MlResult::Incomplete {
                        accum,
                        rest,
                        missing,
                    } => {
                        pending = Some((rest, missing));
                        acc = accum;
                        cur = tail;
                        continue;
                    }
                    MlResult::Error { accum, rest } => {
                        let rest = ctx.cons(rest, tail)?;
                        return Ok(MlResult::Error { accum, rest });
                    }
                }
            }

            // No clause matches: `function_clause` ⇒ the caller raises `badarg`.
            return Err(badarg());
        }

        if access::is_nil(cur) {
            // Clauses 1/2.
            return Ok(match pending {
                Some((incomplete, missing)) => MlResult::Incomplete {
                    accum: acc,
                    rest: incomplete,
                    missing,
                },
                None => MlResult::Bin(acc),
            });
        }

        // Clauses 9/10/11: a bare binary (or `badarg` for anything else).
        if bits::is_binary(cur) {
            let bytes = bits::bytes(cur)?;
            if let Some((incomplete, missing)) = pending {
                if bytes.len() >= missing {
                    let incomplete_tail = ctx.cons(cur, NIL)?;
                    cur = ctx.cons(incomplete, incomplete_tail)?;
                    pending = None;
                    continue;
                }
                let mut merged = bits::bytes(incomplete)?;
                merged.extend_from_slice(&bytes);
                let term = bits::make_binary(ctx.heap(), &merged)?;
                return Ok(MlResult::Incomplete {
                    accum: acc,
                    rest: term,
                    missing: missing - bytes.len(),
                });
            }
            if bytes.len() > 8192 {
                let new_part = bits::make_binary(ctx.heap(), &bytes[..8192])?;
                let trailing = bits::make_binary(ctx.heap(), &bytes[8192..])?;
                let trailing_tail = ctx.cons(trailing, NIL)?;
                cur = ctx.cons(new_part, trailing_tail)?;
                continue;
            }
            return Ok(match map_fun(ctx, env, Part::Bin(&bytes), acc)? {
                MapStep::Bin(out) => MlResult::Bin(out),
                MapStep::Incomplete {
                    converted,
                    rest,
                    missing,
                } => MlResult::Incomplete {
                    accum: converted,
                    rest,
                    missing,
                },
                MapStep::Error { converted, rest } => MlResult::Error {
                    accum: converted,
                    rest,
                },
            });
        }
        return Err(badarg());
    }
}

/// The `{InTrans, Limit}`/`o_trans` environment of `characters_to_binary_int/3`
/// (`unicode.erl:891-896`): the checker and the 255 limit come from a `latin1`
/// **output**, otherwise the *input* being `latin1` still caps the integer parts
/// at 255.
fn engine2_env(in_enc: Encoding, out_enc: Encoding) -> MapEnv {
    let chk = out_enc == Encoding::Latin1;
    let limit = if chk || in_enc == Encoding::Latin1 {
        255
    } else {
        0x10FFFF
    };
    MapEnv {
        in_enc,
        chk,
        limit,
        out_enc,
    }
}

/// The raw outcome of `characters_to_binary_int/3` — the unbuilt form of what
/// [`engine2_binary`] shapes into a term, so `characters_to_list_int/2` can read
/// the converted binary back as a list ([`ctl_wide`]).
enum Engine2 {
    /// The converted bytes.
    Bin(Vec<u8>),
    /// `{incomplete, Accum, Inc}` (the `Missing` count is dropped, as in the
    /// Erlang `case Res of … {incomplete,A,B,_} -> {incomplete,A,B}`).
    Incomplete { accum: Vec<u8>, rest: Eterm },
    /// `{error, Accum, Rest}`.
    Error { accum: Vec<u8>, rest: Eterm },
}

/// `ml_map/3` over the `characters_to_binary_int/3` closure, unbuilt.
fn engine2_run(
    ctx: &mut BifContext<'_>,
    data: Eterm,
    in_enc: Encoding,
    out_enc: Encoding,
) -> Result<Engine2> {
    let env = engine2_env(in_enc, out_enc);
    Ok(match ml_map(ctx, &env, data, Vec::new(), None)? {
        MlResult::Bin(bytes) => Engine2::Bin(bytes),
        MlResult::Incomplete { accum, rest, .. } => Engine2::Incomplete { accum, rest },
        MlResult::Error { accum, rest } => Engine2::Error { accum, rest },
    })
}

/// `characters_to_binary_int/3` for any output encoding (`latin1`'s
/// `i_trans_chk`/255-limit special case is decided by [`engine2_env`]).
fn engine2_binary(
    ctx: &mut BifContext<'_>,
    data: Eterm,
    in_enc: Encoding,
    out_enc: Encoding,
) -> Result<Eterm> {
    match engine2_run(ctx, data, in_enc, out_enc)? {
        Engine2::Bin(bytes) => bits::make_binary(ctx.heap(), &bytes),
        Engine2::Incomplete { accum, rest } => {
            let converted = bits::make_binary(ctx.heap(), &accum)?;
            tuple3(ctx, "incomplete", converted, rest)
        }
        Engine2::Error { accum, rest } => {
            let converted = bits::make_binary(ctx.heap(), &accum)?;
            tuple3(ctx, "error", converted, rest)
        }
    }
}

// ---------------------------------------------------------------------------
// `no_conversion_needed/3`
// ---------------------------------------------------------------------------

/// `unicode:no_conversion_needed/3` — the short-circuit that returns the input
/// unchanged. `Ok(Some(_))` means "return this term".
fn no_conversion_needed(data: Eterm, in_enc: Encoding, out_enc: Encoding) -> Option<Eterm> {
    if in_enc == Encoding::Latin1 && out_enc == Encoding::Latin1 {
        return if bits::is_binary(data) { Some(data) } else { None };
    }
    let cross = (in_enc == Encoding::Latin1 && out_enc == Encoding::Utf8)
        || (in_enc == Encoding::Utf8 && out_enc == Encoding::Latin1);
    if cross && bin_is_7bit(data) {
        return Some(data);
    }
    None
}

// ---------------------------------------------------------------------------
// The BIFs
// ---------------------------------------------------------------------------

/// The subject of engine 1: `[Bin]` (a one-element list whose head is a bitstring)
/// is unwrapped to `Bin`, exactly as `unicode_characters_to_binary_2` does — which
/// is why such a call reports a binary `Rest`.
fn subject_of(data: Eterm) -> Eterm {
    if access::is_list(data) && bits::is_bitstring(access::car(data)) && access::is_nil(access::cdr(data))
    {
        access::car(data)
    } else {
        data
    }
}

/// `characters_to_binary(Data, InEncoding, OutEncoding)`.
///
/// The oracle's guard clause (`unicode.erl:882-889`) sends the six
/// `latin1`/`unicode`/`utf8` × `unicode`/`utf8` combinations to the C BIF engine
/// ([`engine1_binary`]); every other pair — including every wide encoding on
/// either side — takes `characters_to_binary_int/3`'s general clause
/// ([`engine2_binary`]).
fn ctb(ctx: &mut BifContext<'_>, data: Eterm, in_enc: Encoding, out_enc: Encoding) -> Result<Eterm> {
    if let Some(unchanged) = no_conversion_needed(data, in_enc, out_enc) {
        return Ok(unchanged);
    }
    let narrow_utf8 = out_enc == Encoding::Utf8
        && (in_enc == Encoding::Latin1 || in_enc == Encoding::Utf8);
    if narrow_utf8 {
        return engine1_binary(ctx, subject_of(data), in_enc == Encoding::Latin1);
    }
    engine2_binary(ctx, data, in_enc, out_enc)
}

/// `unicode:characters_to_binary/1` — `characters_to_binary(Data, unicode, unicode)`.
fn characters_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctb(ctx, args[0], Encoding::Utf8, Encoding::Utf8)
}

/// `unicode:characters_to_binary/2` — the input encoding; the output is `unicode`.
fn characters_to_binary_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let in_enc = encoding_arg(ctx.atoms(), args[1])?.ok_or_else(badarg)?;
    ctb(ctx, args[0], in_enc, Encoding::Utf8)
}

/// `unicode:characters_to_binary/3`.
fn characters_to_binary_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let in_enc = encoding_arg(ctx.atoms(), args[1])?.ok_or_else(badarg)?;
    let out_enc = encoding_arg(ctx.atoms(), args[2])?.ok_or_else(badarg)?;
    ctb(ctx, args[0], in_enc, out_enc)
}

/// `characters_to_list(Data, InEncoding)`.
///
/// `latin1` and `utf8`/`unicode` are the C BIF's own paths; a wide encoding makes
/// the BIF **trap** to `unicode:characters_to_list_int/2`
/// (`erl_unicode.c:1139-1142`), i.e. `do_characters_to_list/2`
/// (`unicode.erl:849-857`): convert to a UTF-8 binary, then read *that* back as a
/// `utf8` list, keeping the error/incomplete tag and the untranslated `Rest`.
fn ctl(ctx: &mut BifContext<'_>, data: Eterm, in_enc: Encoding) -> Result<Eterm> {
    match in_enc {
        Encoding::Latin1 => engine1_list(ctx, data, true),
        Encoding::Utf8 => {
            if bits::is_bitstring(data) {
                // The plain-utf8-binary fast path (`utf8_to_list`); a non-byte-aligned
                // bitstring is `badarg`.
                if aligned_binary_size(data).is_none() {
                    return Err(badarg());
                }
                return utf8_to_list(ctx, data);
            }
            engine1_list(ctx, data, false)
        }
        _ => ctl_wide(ctx, data, in_enc),
    }
}

/// `unicode:characters_to_list_int/2` for a wide input encoding.
fn ctl_wide(ctx: &mut BifContext<'_>, data: Eterm, in_enc: Encoding) -> Result<Eterm> {
    /// `unicode:characters_to_list(Bin, utf8)` — the converted binary is valid
    /// UTF-8 by construction, so this is the plain-list fast path.
    fn read_back(ctx: &mut BifContext<'_>, accum: &[u8]) -> Result<Eterm> {
        let converted = bits::make_binary(ctx.heap(), accum)?;
        ctl(ctx, converted, Encoding::Utf8)
    }
    match engine2_run(ctx, data, in_enc, Encoding::Utf8)? {
        Engine2::Bin(accum) => read_back(ctx, &accum),
        Engine2::Incomplete { accum, rest } => {
            let list = read_back(ctx, &accum)?;
            tuple3(ctx, "incomplete", list, rest)
        }
        Engine2::Error { accum, rest } => {
            let list = read_back(ctx, &accum)?;
            tuple3(ctx, "error", list, rest)
        }
    }
}

/// `unicode:characters_to_list/1` — `characters_to_list(Data, unicode)`.
fn characters_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctl(ctx, args[0], Encoding::Utf8)
}

/// `unicode:characters_to_list/2`.
fn characters_to_list_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let in_enc = encoding_arg(ctx.atoms(), args[1])?.ok_or_else(badarg)?;
    ctl(ctx, args[0], in_enc)
}

/// `unicode:bin_is_7bit/1` — `true` iff `Bin` is a byte-aligned binary every byte
/// of which is below `0x80`, and `false` for any other term.
///
/// Reproduces the oracle's `bin_is_7bit` exactly (`../../erts/emulator/beam/
/// erl_unicode.c:888-909`): `erts_get_aligned_binary_bytes` returns `NULL` for a
/// list, a non-byte-aligned bitstring and every non-binary, so those are `false`
/// (`bin_is_7bit(<<1,2,127>>)` ⇒ `true`, `bin_is_7bit(<<1,128>>)` ⇒ `false`,
/// `bin_is_7bit(<<1:1>>)` ⇒ `false`, `bin_is_7bit([1,2])` ⇒ `false`,
/// `bin_is_7bit(5)` ⇒ `false`). Never raises — the argument is *not* validated.
fn bin_is_7bit_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(bin_is_7bit(args[0]))
}

/// Registers the `unicode` module BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("unicode", "characters_to_binary", 1, characters_to_binary_1);
    reg.register("unicode", "characters_to_binary", 2, characters_to_binary_2);
    reg.register("unicode", "characters_to_binary", 3, characters_to_binary_3);
    reg.register("unicode", "characters_to_list", 1, characters_to_list_1);
    reg.register("unicode", "characters_to_list", 2, characters_to_list_2);
    reg.register("unicode", "bin_is_7bit", 1, bin_is_7bit_1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};
    use std::cell::RefCell;

    /// Every name the test tables are seeded with, in this order.
    const NAMES: &[&str] = &[
        "ok", "error", "incomplete", "latin1", "utf8", "unicode", "foo", "true", "false", "big",
        "little", "utf16", "utf32",
    ];

    fn table() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(NAMES.iter().copied());
        atoms
    }

    // A per-thread scratch heap used only to build argument terms, so `bin(..)`
    // can be called inside an expression that already borrows the context.
    thread_local! {
        static SCRATCH: RefCell<Heap> = RefCell::new(Heap::new(1 << 16).expect("scratch heap"));
    }

    /// A binary term of `bytes` on the per-thread scratch heap.
    fn bin(bytes: &[u8]) -> Eterm {
        SCRATCH.with(|h| bits::make_binary(&mut h.borrow_mut(), bytes).expect("the binary fits"))
    }

    /// A bitstring term of `bits` bits on the per-thread scratch heap.
    fn bitstring(data: &[u8], bits: usize) -> Eterm {
        SCRATCH.with(|h| {
            bits::make_bitstring(&mut h.borrow_mut(), data, bits).expect("the bitstring fits")
        })
    }

    /// Runs `f` over a fresh context (no optional seams installed).
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(1 << 12).expect("test heap");
        let atoms = table();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// Calls `unicode:<name>/<arity>` through the real registry.
    fn call(ctx: &mut BifContext<'_>, name: &str, args: &[Eterm]) -> Result<Eterm> {
        let registry = crate::registry::default_registry();
        let f = registry
            .lookup("unicode", name, args.len() as u32)
            .expect("the BIF is registered");
        f(ctx, args)
    }

    fn enc(ctx: &BifContext<'_>, name: &str) -> Eterm {
        ctx.atom(name).expect("seeded atom")
    }

    /// A structural rendering that does not depend on `print.rs`: atoms by name,
    /// binaries as `#{…}`, lists as `[…]`, tuples as `{…}`, integers in decimal.
    fn describe(ctx: &BifContext<'_>, term: Eterm) -> String {
        if access::is_atom(term) {
            return ctx
                .atoms()
                .name(atom_val(term))
                .unwrap_or("?")
                .to_string();
        }
        if access::is_small(term) {
            return signed_value(term).to_string();
        }
        if access::is_nil(term) {
            return "[]".to_string();
        }
        if bits::is_binary(term) {
            let bytes = bits::bytes(term).expect("a binary");
            let parts: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
            return format!("#{{{}}}", parts.join(","));
        }
        if access::is_tuple(term) {
            let parts: Vec<String> = (0..access::tuple_size(term))
                .map(|i| describe(ctx, access::tuple_element(term, i)))
                .collect();
            return format!("{{{}}}", parts.join(","));
        }
        if access::is_list(term) {
            let mut parts = Vec::new();
            let mut cur = term;
            while access::is_list(cur) {
                parts.push(describe(ctx, access::car(cur)));
                cur = access::cdr(cur);
            }
            if access::is_nil(cur) {
                return format!("[{}]", parts.join(","));
            }
            return format!("[{}|{}]", parts.join(","), describe(ctx, cur));
        }
        "?".to_string()
    }

    /// Calls `f`, then describes the result (or the typed error).
    fn show(ctx: &mut BifContext<'_>, name: &str, args: &[Eterm]) -> String {
        match call(ctx, name, args) {
            Ok(term) => describe(ctx, term),
            Err(BeamError::BadArg) => "badarg".to_string(),
            Err(other) => format!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn ascii_round_trips() {
        with_ctx(|ctx| {
            let data = ctx.list(&[ctx.int(97).unwrap(), ctx.int(98).unwrap(), ctx.int(99).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[data]), "#{97,98,99}");
            // A binary in is already the right bytes.
            assert_eq!(show(ctx, "characters_to_binary", &[bin(b"abc")]), "#{97,98,99}");
            assert_eq!(show(ctx, "characters_to_list", &[bin(b"abc")]), "[97,98,99]");
            let u = enc(ctx, "unicode");
            assert_eq!(
                show(ctx, "characters_to_list", &[bin(b"abc"), u]),
                "[97,98,99]"
            );
        });
    }

    #[test]
    fn multi_byte_code_points_encode_and_decode() {
        with_ctx(|ctx| {
            // 2-byte (`U+0440`), 3-byte (`U+03C0`, `U+20AC`), 4-byte (`U+1F600`).
            for (cp, bytes) in [
                (0x440i64, vec![0xD1, 0x80]),
                (0x3C0, vec![0xCF, 0x80]),
                (0x20AC, vec![0xE2, 0x82, 0xAC]),
                (0x1F600, vec![0xF0, 0x9F, 0x98, 0x80]),
            ] {
                let data = ctx.list(&[ctx.int(cp).unwrap()]).unwrap();
                let encoded = call(ctx, "characters_to_binary", &[data]).unwrap();
                assert!(bits::is_binary(encoded));
                assert_eq!(bits::bytes(encoded).unwrap(), bytes, "encoding U+{cp:X}");
                // and back again
                assert_eq!(
                    show(ctx, "characters_to_list", &[bin(&bytes)]),
                    format!("[{cp}]"),
                    "decoding U+{cp:X}"
                );
            }
            // A pair of 2-byte code points.
            let pair = ctx
                .list(&[ctx.int(1076).unwrap(), ctx.int(1072).unwrap()])
                .unwrap();
            assert_eq!(
                show(ctx, "characters_to_binary", &[pair]),
                "#{208,180,208,176}"
            );
            assert_eq!(
                show(ctx, "characters_to_list", &[bin(&[208, 180, 208, 176])]),
                "[1076,1072]"
            );
        });
    }

    #[test]
    fn latin1_input_and_output() {
        with_ctx(|ctx| {
            let l = enc(ctx, "latin1");
            let u = enc(ctx, "unicode");
            let one = ctx.list(&[ctx.int(255).unwrap()]).unwrap();
            // latin1 in → utf8 out (output defaults to unicode).
            assert_eq!(show(ctx, "characters_to_binary", &[one, l]), "#{195,191}");
            // utf8 out, latin1 in, via the explicit /3 form.
            assert_eq!(show(ctx, "characters_to_binary", &[one, u, l]), "#{255}");
            // A latin1 byte with the high bit set becomes two bytes.
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[200]), l]), "#{195,136}");
            // The {latin1,latin1} pair is identity for a binary...
            assert_eq!(
                show(ctx, "characters_to_binary", &[bin(&[65, 255]), l, l]),
                "#{65,255}"
            );
            // ...and the list form still passes both bytes through.
            let high =
                ctx.list(&[ctx.int(65).unwrap(), ctx.int(255).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[high, l, l]), "#{65,255}");
            // latin1 input for characters_to_list is one byte per code point.
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[255, 254]), l]), "[255,254]");
            // A latin1 list input keeps its code points verbatim.
            let ascii = ctx.list(&[ctx.int(97).unwrap(), ctx.int(98).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_list", &[ascii, l]), "[97,98]");
        });
    }

    #[test]
    fn cross_encoding_and_empty_input() {
        with_ctx(|ctx| {
            let l = enc(ctx, "latin1");
            let u = enc(ctx, "unicode");
            let utf8 = enc(ctx, "utf8");
            // Empty input in every form.
            assert_eq!(show(ctx, "characters_to_binary", &[NIL]), "#{}");
            assert_eq!(show(ctx, "characters_to_list", &[NIL]), "[]");
            assert_eq!(show(ctx, "characters_to_binary", &[NIL, l, l]), "#{}");
            // Explicit unicode(utf8) in and out.
            let cyrillic = ctx
                .list(&[ctx.int(1076).unwrap(), ctx.int(1072).unwrap()])
                .unwrap();
            assert_eq!(
                show(ctx, "characters_to_binary", &[cyrillic, u, utf8]),
                "#{208,180,208,176}"
            );
            // A one-element list holding a binary is unwrapped before engine 1.
            let wrapped = ctx.list(&[bin(&[97, 208])]).unwrap();
            assert_eq!(
                show(ctx, "characters_to_binary", &[wrapped]),
                "{incomplete,#{97},#{208}}"
            );
        });
    }

    #[test]
    fn error_results() {
        with_ctx(|ctx| {
            // Engine 1: the subject is the binary, so Rest is a binary.
            assert_eq!(
                show(ctx, "characters_to_binary", &[bin(&[65, 255, 66])]),
                "{error,#{65},#{255,66}}"
            );
            assert_eq!(
                show(ctx, "characters_to_binary", &[bin(&[65, 255, 66]), enc(ctx, "utf8")]),
                "{error,#{65},#{255,66}}"
            );
            // A one-element list with a binary head is unwrapped too.
            let wrapped = ctx.list(&[bin(&[65, 255, 66])]).unwrap();
            assert_eq!(
                show(ctx, "characters_to_binary", &[wrapped, enc(ctx, "utf8")]),
                "{error,#{65},#{255,66}}"
            );
            // An overlong encoding is an error (not incomplete).
            assert_eq!(
                show(ctx, "characters_to_list", &[bin(&[192, 175])]),
                "{error,[],#{192,175}}"
            );
            // A leftover partial sequence followed by an integer.
            let list = ctx
                .list(&[ctx.int(97).unwrap(), bin(&[208]), ctx.int(98).unwrap()])
                .unwrap();
            assert_eq!(
                show(ctx, "characters_to_list", &[list, enc(ctx, "utf8")]),
                "{error,[97],[#{208},[98]]}"
            );
            // Engine 2: a code point above the latin1 limit nests the Rest.
            let big = ctx.list(&[ctx.int(256).unwrap()]).unwrap();
            assert_eq!(
                show(ctx, "characters_to_binary", &[big, enc(ctx, "unicode"), enc(ctx, "latin1")]),
                "{error,#{},[[256]]}"
            );
            let mixed = ctx
                .list(&[ctx.int(65).unwrap(), ctx.int(256).unwrap(), ctx.int(66).unwrap()])
                .unwrap();
            assert_eq!(
                show(
                    ctx,
                    "characters_to_binary",
                    &[mixed, enc(ctx, "unicode"), enc(ctx, "latin1")]
                ),
                "{error,#{65},[[256],66]}"
            );
            // Engine 2 with a utf8 binary whose tail is not latin1.
            assert_eq!(
                show(
                    ctx,
                    "characters_to_binary",
                    &[bin(&[65, 208, 180]), enc(ctx, "utf8"), enc(ctx, "latin1")]
                ),
                "{error,#{65},#{208,180}}"
            );
            // Engine 2 raises badarg for a negative code point (list_to_binary).
            let negative = ctx.list(&[ctx.int(-1).unwrap()]).unwrap();
            assert_eq!(
                show(
                    ctx,
                    "characters_to_binary",
                    &[negative, enc(ctx, "unicode"), enc(ctx, "latin1")]
                ),
                "badarg"
            );
        });
    }

    #[test]
    fn incomplete_results() {
        with_ctx(|ctx| {
            let utf8 = enc(ctx, "utf8");
            // A truncated 2-byte sequence.
            assert_eq!(
                show(ctx, "characters_to_binary", &[bin(&[65, 208]), utf8]),
                "{incomplete,#{65},#{208}}"
            );
            // A truncated 3-byte sequence.
            assert_eq!(
                show(ctx, "characters_to_binary", &[bin(&[65, 226, 130]), utf8]),
                "{incomplete,#{65},#{226,130}}"
            );
            // A truncated 4-byte sequence is *incomplete*, not an error.
            assert_eq!(
                show(ctx, "characters_to_binary", &[bin(&[240, 128, 128]), utf8]),
                "{incomplete,#{},#{240,128,128}}"
            );
            assert_eq!(
                show(ctx, "characters_to_list", &[bin(&[240, 128, 128]), utf8]),
                "{incomplete,[],#{240,128,128}}"
            );
            // Two list elements combining into one sequence.
            let split = ctx.list(&[bin(&[208]), bin(&[180])]).unwrap();
            assert_eq!(
                show(ctx, "characters_to_binary", &[split]),
                "#{208,180}"
            );
        });
    }

    #[test]
    fn badarg_cases() {
        with_ctx(|ctx| {
            let foo = enc(ctx, "foo");
            let utf8 = enc(ctx, "utf8");
            let l = enc(ctx, "latin1");
            let one = ctx.list(&[ctx.int(1).unwrap()]).unwrap();
            // Unknown / non-atom encodings.
            assert_eq!(show(ctx, "characters_to_binary", &[one, foo]), "badarg");
            assert_eq!(show(ctx, "characters_to_binary", &[one, foo, l]), "badarg");
            assert_eq!(show(ctx, "characters_to_binary", &[one, utf8, foo]), "badarg");
            assert_eq!(show(ctx, "characters_to_list", &[one, foo]), "badarg");
            assert_eq!(show(ctx, "characters_to_binary", &[one, make_small(5)]), "badarg");
            // Data that is not a chardata shape at all.
            assert_eq!(show(ctx, "characters_to_binary", &[make_small(42)]), "badarg");
            assert_eq!(show(ctx, "characters_to_list", &[make_small(42)]), "badarg");
            // A non-byte-aligned bitstring.
            assert_eq!(show(ctx, "characters_to_binary", &[bitstring(&[0x80], 1), utf8]), "badarg");
            assert_eq!(show(ctx, "characters_to_list", &[bitstring(&[0x80], 1), utf8]), "badarg");
            // A bad element.
            let f = ctx.make_float(1.0).unwrap();
            let floats = ctx.list(&[f]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[floats]), "badarg");
            let atoms = ctx.list(&[enc(ctx, "ok")]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[atoms]), "badarg");
            // An improper list.
            let improper = ctx.cons(ctx.int(65).unwrap(), ctx.int(98).unwrap()).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[improper, utf8]), "badarg");
            // A lone surrogate is an error, not badarg.
            assert_eq!(
                show(ctx, "characters_to_binary", &[bin(&[237, 160, 128]), utf8]),
                "{error,#{},#{237,160,128}}"
            );
            // Above U+10FFFF.
            assert_eq!(
                show(ctx, "characters_to_binary", &[bin(&[244, 143, 191, 192]), utf8]),
                "{error,#{},#{244,143,191,192}}"
            );
        });
    }

    #[test]
    fn registry_has_the_registered_arities() {
        let registry = crate::registry::default_registry();
        for (name, arity) in [
            ("characters_to_binary", 1),
            ("characters_to_binary", 2),
            ("characters_to_binary", 3),
            ("characters_to_list", 1),
            ("characters_to_list", 2),
            ("bin_is_7bit", 1),
        ] {
            assert!(
                registry.contains("unicode", name, arity),
                "unicode:{name}/{arity} must be registered"
            );
        }
    }

    #[test]
    fn bin_is_7bit_matches_the_oracle() {
        with_ctx(|ctx| {
            // Oracle-pinned values (`erl -noshell -eval 'unicode:bin_is_7bit(…).'`).
            assert_eq!(show(ctx, "bin_is_7bit", &[bin(&[1, 2, 127])]), "true");
            assert_eq!(show(ctx, "bin_is_7bit", &[bin(&[1, 128])]), "false");
            assert_eq!(show(ctx, "bin_is_7bit", &[bin(&[])]), "true");
            assert_eq!(show(ctx, "bin_is_7bit", &[bin(&[0])]), "true");
            assert_eq!(show(ctx, "bin_is_7bit", &[bin(&[255])]), "false");
            // A non-byte-aligned bitstring is `false`.
            assert_eq!(show(ctx, "bin_is_7bit", &[bitstring(&[0x80], 1)]), "false");
            assert_eq!(show(ctx, "bin_is_7bit", &[bitstring(&[0x00, 0x80], 9)]), "false");
            // Non-binary terms are `false`, never `badarg`.
            let list = ctx.list(&[ctx.int(1).unwrap(), ctx.int(2).unwrap()]).unwrap();
            assert_eq!(show(ctx, "bin_is_7bit", &[list]), "false");
            assert_eq!(show(ctx, "bin_is_7bit", &[make_small(5)]), "false");
            assert_eq!(show(ctx, "bin_is_7bit", &[enc(ctx, "ok")]), "false");
            let one_f = ctx.make_float(1.0).unwrap();
            assert_eq!(show(ctx, "bin_is_7bit", &[one_f]), "false");
        });
    }

    // -----------------------------------------------------------------------
    // utf16 / utf32 (the wide encodings)
    // -----------------------------------------------------------------------
    //
    // Every expectation below is a verbatim transcription of the system oracle
    // (OTP 29 / erts-17.1) — see `../unicode.rs`'s module docs and the live `erl`
    // cross-check in `tests/unicode_wide.rs`. `#{…}` is a binary, `[…]` a
    // list and `{…}` a tuple in `describe`'s rendering.

    /// The `{utf16|utf32, big|little}` encoding tuple.
    fn enc_pair(ctx: &mut BifContext<'_>, name: &str, endian: &str) -> Eterm {
        let tag = enc(ctx, name);
        let end = enc(ctx, endian);
        ctx.tuple(&[tag, end]).expect("the tuple fits")
    }

    #[test]
    fn utf16_input_matches_the_oracle() {
        with_ctx(|ctx| {
            let b = enc(ctx, "utf16");
            let big = enc_pair(ctx, "utf16", "big");
            let lit = enc_pair(ctx, "utf16", "little");
            // The bare atom is an alias of the big-endian form.
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65, 0, 66]), b]), "#{65,66}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65, 0, 66]), big]), "#{65,66}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[65, 0, 66, 0]), lit]), "#{65,66}");
            // A BOM is just U+FEFF.
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[254, 255, 0, 65]), b]), "#{239,187,191,65}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[255, 254, 65, 0]), lit]), "#{239,187,191,65}");
            // A surrogate pair becomes one code point; a lone surrogate does not.
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[216, 60, 222, 0]), b]), "#{240,159,136,128}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[60, 216, 0, 222]), lit]), "#{240,159,136,128}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[216, 0, 0, 65]), b]), "{error,#{},#{216,0,0,65}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[220, 0]), b]), "{error,#{},#{220,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[216, 60, 65, 66]), b]), "{error,#{},#{216,60,65,66}}");
            // A code unit whose lead byte is not a surrogate lead byte is plain.
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 216]), b]), "#{195,152}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[255, 254]), b]), "#{239,191,190}");
            // Truncated input is `incomplete`, never `error`; a lone lead byte
            // is over-reported as needing 3 (big) / 1 (little).
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65, 0]), b]), "{incomplete,#{65},#{0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0]), b]), "{incomplete,#{},#{0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[216, 0]), b]), "{incomplete,#{},#{216,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65, 216, 0]), b]), "{incomplete,#{65},#{216,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[216, 60, 222]), b]), "{incomplete,#{},#{216,60,222}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[216]), b]), "{incomplete,#{},#{216}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[255]), b]), "{incomplete,#{},#{255}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[220]), b]), "{error,#{},#{220}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[60, 216, 0, 222, 65]), lit]), "{incomplete,#{240,159,136,128},#{65}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[255]), lit]), "{incomplete,#{},#{255}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 220]), lit]), "{error,#{},#{0,220}}");
            // Empty input, and a list is code points regardless of the encoding.
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[]), b]), "#{}");
            let empty = ctx.list(&[]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[empty, b]), "#{}");
            let ab = ctx.list(&[ctx.int(65).unwrap(), ctx.int(66).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[ab, b]), "#{65,66}");
            let emoji = ctx.list(&[ctx.int(0x1F600).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[emoji, b]), "#{240,159,152,128}");
            // An out-of-range / surrogate / negative integer nests the Rest twice
            // (`ml_map`'s `{error, Accum, [Rest | T]}` around `Fun`'s `[Part]`).
            let over = ctx.list(&[ctx.int(0x110000).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[over, b]), "{error,#{},[[1114112]]}");
            let sur = ctx.list(&[ctx.int(0xD800).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[sur, b]), "{error,#{},[[55296]]}");
            let neg = ctx.list(&[ctx.int(-1).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[neg, b]), "{error,#{},[[-1]]}");
            // Mixed/nested shapes, including a binary part.
            let a_inner = ctx.list(&[ctx.int(65).unwrap()]).unwrap();
            let b_inner = ctx.list(&[ctx.int(66).unwrap()]).unwrap();
            let nested = ctx.list(&[a_inner, b_inner]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[nested, b]), "#{65,66}");
            let mixed = ctx.list(&[ctx.int(65).unwrap(), bin(&[0, 66])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[mixed, b]), "#{65,66}");
            let one_bin = ctx.list(&[bin(&[0, 65])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[one_bin, b]), "#{65}");
            let improper_bin = ctx.cons(ctx.int(65).unwrap(), bin(&[0, 66])).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[improper_bin, b]), "#{65,66}");
            let bits = ctx.list(&[ctx.int(65).unwrap(), bin(&[0, 66])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[bits, b]), "#{65,66}");
            // Errors that are `badarg`, not tuples.
            let one_float = ctx.make_float(1.0).unwrap();
            let floats = ctx.list(&[one_float]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[floats, b]), "badarg");
            let atoms = ctx.list(&[enc(ctx, "ok")]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[atoms, b]), "badarg");
            let improper = ctx.cons(ctx.int(65).unwrap(), ctx.int(66).unwrap()).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[improper, b]), "badarg");
            // A non-byte-aligned bitstring is not a binary, so no `ml_map` clause
            // matches (the oracle's `function_clause` ⇒ `badarg`).
            assert_eq!(show(ctx, "characters_to_binary", &[bitstring(&[0, 65, 0], 20), b]), "badarg");
            assert_eq!(show(ctx, "characters_to_list", &[bitstring(&[0, 65, 0], 20), b]), "badarg");
        });
    }

    #[test]
    fn utf16_parts_carry_a_pending_partial_code_point() {
        with_ctx(|ctx| {
            let b = enc(ctx, "utf16");
            let pair = ctx.list(&[bin(&[216, 60]), bin(&[222, 0])]).unwrap();
            // `ml_map`'s pending accumulator splices the halves together.
            assert_eq!(show(ctx, "characters_to_binary", &[pair, b]), "#{240,159,136,128}");
            let half = ctx.list(&[bin(&[216]), bin(&[60, 222, 0])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[half, b]), "#{240,159,136,128}");
            let two = ctx.list(&[bin(&[0, 65]), bin(&[0, 66])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[two, b]), "#{65,66}");
            let split = ctx.list(&[bin(&[0]), bin(&[65, 0, 66])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[split, b]), "#{65,66}");
            let short = ctx.list(&[bin(&[0, 65]), bin(&[216, 60, 222, 0])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[short, b]), "#{65,240,159,136,128}");
            // A pending partial sequence followed by an integer is the
            // `{error, Accum, [Incomplete | Whole]}` clause.
            let bad = ctx.list(&[bin(&[0, 65]), bin(&[216, 60]), ctx.int(66).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[bad, b]), "{error,#{65},[#{216,60},66]}");
            let list = ctx.list(&[bin(&[0, 65]), bin(&[220, 0]), bin(&[0, 66])]).unwrap();
            assert_eq!(show(ctx, "characters_to_list", &[list, b]), "{error,[65],[#{220,0},#{0,66}]}");
            let elem = ctx.list(&[bin(&[0, 220, 0])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[elem, b]), "{incomplete,#{195,156},#{0}}");
        });
    }

    #[test]
    fn utf32_input_matches_the_oracle() {
        with_ctx(|ctx| {
            let b = enc(ctx, "utf32");
            let big = enc_pair(ctx, "utf32", "big");
            let lit = enc_pair(ctx, "utf32", "little");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0, 65]), b]), "#{65}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0, 65]), big]), "#{65}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[65, 0, 0, 0]), lit]), "#{65}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0, 65, 0, 0, 0, 66]), b]), "#{65,66}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[65, 0, 0, 0, 66, 0, 0, 0]), lit]), "#{65,66}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 254, 255]), b]), "#{239,187,191}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 255, 255]), b]), "#{239,191,191}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0, 0]), b]), "#{0}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0, 0]), lit]), "#{0}");
            // A surrogate or a value above U+10FFFF is an error; the `cbv/2`
            // clauses decide the truncated forms.
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 216, 0]), b]), "{error,#{},#{0,0,216,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 216, 0, 0]), lit]), "{error,#{},#{0,216,0,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 17, 0, 0]), b]), "{error,#{},#{0,17,0,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 17, 0, 0, 65]), b]), "{error,#{},#{0,17,0,0,65}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0]), b]), "{incomplete,#{},#{0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0]), b]), "{incomplete,#{},#{0,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0]), b]), "{incomplete,#{},#{0,0,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 17, 0]), b]), "{error,#{},#{0,17,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0]), lit]), "{incomplete,#{},#{0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[65, 0, 0]), lit]), "{incomplete,#{},#{65,0,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[65, 216, 1]), lit]), "{incomplete,#{},#{65,216,1}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[254, 255, 0]), lit]), "{error,#{},#{254,255,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[255, 255, 0]), lit]), "{error,#{},#{255,255,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[1, 255, 0]), lit]), "{incomplete,#{},#{1,255,0}}");
            // Splitting a code point across two parts uses the pending path too.
            let split = ctx.list(&[bin(&[0, 0]), bin(&[0, 65])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[split, b]), "#{65}");
            let short = ctx.list(&[bin(&[0]), bin(&[0, 65])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[short, b]), "{incomplete,#{},#{0,0,65}}");
            let part = ctx.list(&[bin(&[0, 0, 0]), bin(&[65])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[part, b]), "#{65}");
            let one_bin = ctx.list(&[bin(&[0, 0, 0, 65])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[one_bin, b]), "#{65}");
        });
    }

    #[test]
    fn wide_output_encodings_match_the_oracle() {
        with_ctx(|ctx| {
            let u = enc(ctx, "unicode");
            let l = enc(ctx, "latin1");
            let f = enc(ctx, "utf8");
            let b16 = enc(ctx, "utf16");
            let b16l = enc_pair(ctx, "utf16", "little");
            let b16b = enc_pair(ctx, "utf16", "big");
            let b32 = enc(ctx, "utf32");
            let b32l = enc_pair(ctx, "utf32", "little");
            let ab = ctx.list(&[ctx.int(65).unwrap(), ctx.int(66).unwrap()]).unwrap();
            let emoji = ctx.list(&[ctx.int(0x1F600).unwrap()]).unwrap();
            // Out = utf16/utf32 (the general `characters_to_binary_int/3` clause).
            assert_eq!(show(ctx, "characters_to_binary", &[ab, l, b16]), "#{0,65,0,66}");
            assert_eq!(show(ctx, "characters_to_binary", &[ab, l, b16l]), "#{65,0,66,0}");
            assert_eq!(show(ctx, "characters_to_binary", &[emoji, u, b16]), "#{216,61,222,0}");
            assert_eq!(show(ctx, "characters_to_binary", &[ab, l, b32]), "#{0,0,0,65,0,0,0,66}");
            assert_eq!(show(ctx, "characters_to_binary", &[ab, l, b32l]), "#{65,0,0,0,66,0,0,0}");
            assert_eq!(show(ctx, "characters_to_binary", &[emoji, u, b32l]), "#{0,246,1,0}");
            // An unencodable integer: the Rest nests twice (limited first, then
            // guarded by the out-transcoder).
            let over = ctx.list(&[ctx.int(0x110000).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[over, u, b16]), "{error,#{},[[1114112]]}");
            assert_eq!(show(ctx, "characters_to_binary", &[over, u, b32]), "{error,#{},[[1114112]]}");
            let sur = ctx.list(&[ctx.int(0xD800).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[sur, u, b16]), "{error,#{},[[55296]]}");
            assert_eq!(show(ctx, "characters_to_binary", &[sur, u, b32]), "{error,#{},[[55296]]}");
            let neg = ctx.list(&[ctx.int(-2).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[neg, u, b16]), "{error,#{},[[-2]]}");
            let too_big = ctx.list(&[ctx.int(256).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[too_big, l, b16]), "{error,#{},[[256]]}");
            // A wide input encoding on either side of `/3`.
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65, 0, 66]), b16, f]), "#{65,66}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65]), b16, u]), "#{65}");
            assert_eq!(show(ctx, "characters_to_binary", &[ab, f, b16]), "#{0,65,0,66}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(b"AB"), f, b16]), "#{0,65,0,66}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65]), b16, b16]), "#{0,65}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65]), b16, b16l]), "#{65,0}");
            assert_eq!(show(ctx, "characters_to_binary", &[ab, b16l, b16b]), "#{0,65,0,66}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0, 65]), b32, b16]), "#{0,65}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65]), b16, b32]), "#{0,0,0,65}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0, 65]), b32, b32l]), "#{65,0,0,0}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65]), b16, b32]), "#{0,0,0,65}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[216, 60, 222, 0]), b16, b16l]), "#{60,216,0,222}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[216, 60, 222, 0]), b16, b32]), "#{0,1,242,0}");
            // A `latin1` output caps the code points at 255.
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65, 0, 66]), b16, l]), "#{65,66}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[216, 60, 222, 0]), b16, l]), "{error,#{},#{216,60,222,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65, 1, 0]), b16, l]), "{error,#{65},#{1,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0, 65]), b32, l]), "#{65}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0]), b32, l]), "{incomplete,#{},#{0,0,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0]), b32l, l]), "{incomplete,#{},#{0,0,0}}");
            // Incomplete/error shapes for the wide input.
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65, 216]), b16, l]), "{incomplete,#{65},#{216}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65, 216]), b16, f]), "{incomplete,#{65},#{216}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65, 220, 0]), b16, f]), "{error,#{65},#{220,0}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 17, 0, 0]), b32, f]), "{error,#{},#{0,17,0,0}}");
            // utf8 input with a wide output, split across parts.
            let split = ctx.list(&[bin(&[195]), bin(&[169])]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[split, f, b16]), "#{0,233}");
            assert_eq!(show(ctx, "characters_to_binary", &[split, f, l]), "#{233}");
            let inline = ctx.list(&[bin(&[0, 65]), ctx.int(66).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[inline, b16, b16]), "#{0,65,0,66}");
        });
    }

    #[test]
    fn wide_characters_to_list_matches_the_oracle() {
        with_ctx(|ctx| {
            let b = enc(ctx, "utf16");
            let b32 = enc(ctx, "utf32");
            let lit = enc_pair(ctx, "utf16", "little");
            let lit32 = enc_pair(ctx, "utf32", "little");
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[0, 65, 0, 66]), b]), "[65,66]");
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[65, 0]), lit]), "[65]");
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[0, 0, 0, 65]), b32]), "[65]");
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[65, 0, 0, 0]), lit32]), "[65]");
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[216, 60, 222, 0]), b]), "[127488]");
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[]), b]), "[]");
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[]), lit32]), "[]");
            let empty = ctx.list(&[]).unwrap();
            assert_eq!(show(ctx, "characters_to_list", &[empty, b]), "[]");
            let ab = ctx.list(&[ctx.int(65).unwrap(), ctx.int(66).unwrap()]).unwrap();
            assert_eq!(show(ctx, "characters_to_list", &[ab, b]), "[65,66]");
            let mixed = ctx.list(&[ctx.int(65).unwrap(), bin(&[0, 66])]).unwrap();
            assert_eq!(show(ctx, "characters_to_list", &[mixed, b]), "[65,66]");
            // The wide `Rest` is carried untranslated, and a top-level binary
            // reports it bare while a list input keeps the `[Rest|T]` nesting.
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[0, 65, 216]), b]), "{incomplete,[65],#{216}}");
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[0, 65, 220, 0]), b]), "{error,[65],#{220,0}}");
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[0, 17, 0, 0]), b32]), "{error,[],#{0,17,0,0}}");
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[0]), b]), "{incomplete,[],#{0}}");
            assert_eq!(show(ctx, "characters_to_list", &[bin(&[0, 0, 0, 65, 0, 0]), b32]), "{incomplete,[65],#{0,0}}");
            let one = ctx.list(&[bin(&[0])]).unwrap();
            assert_eq!(show(ctx, "characters_to_list", &[one, b]), "{incomplete,[],#{0}}");
            let pair = ctx.list(&[bin(&[216, 60]), bin(&[222, 0])]).unwrap();
            assert_eq!(show(ctx, "characters_to_list", &[pair, b]), "[127488]");
            let half = ctx.list(&[bin(&[216]), bin(&[60, 222, 0])]).unwrap();
            assert_eq!(show(ctx, "characters_to_list", &[half, b]), "[127488]");
        });
    }

    #[test]
    fn wide_encoding_arguments_are_validated_like_the_oracle() {
        with_ctx(|ctx| {
            let one = bin(&[1]);
            let ab = ctx.list(&[ctx.int(65).unwrap()]).unwrap();
            let foo = enc(ctx, "foo");
            let b16 = enc(ctx, "utf16");
            let b32 = enc(ctx, "utf32");
            // An unknown atom, and any malformed `{wide, endian}` tuple.
            assert_eq!(show(ctx, "characters_to_binary", &[one, foo]), "badarg");
            assert_eq!(show(ctx, "characters_to_list", &[one, foo]), "badarg");
            let foo_pair = enc_pair(ctx, "utf16", "foo");
            let foo32 = enc_pair(ctx, "utf32", "foo");
            assert_eq!(show(ctx, "characters_to_binary", &[one, foo_pair]), "badarg");
            assert_eq!(show(ctx, "characters_to_binary", &[one, foo32]), "badarg");
            assert_eq!(show(ctx, "characters_to_list", &[one, foo_pair]), "badarg");
            assert_eq!(show(ctx, "characters_to_binary", &[ab, enc(ctx, "latin1"), foo]), "badarg");
            assert_eq!(show(ctx, "characters_to_binary", &[ab, enc(ctx, "latin1"), foo_pair]), "badarg");
            let one_tuple = ctx.tuple(&[b16]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[one, one_tuple]), "badarg");
            let three = ctx.tuple(&[b16, enc(ctx, "big"), enc(ctx, "big")]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[one, three]), "badarg");
            let non_atom = ctx.list(&[ctx.int(5).unwrap()]).unwrap();
            let bad_tag = ctx.tuple(&[non_atom, enc(ctx, "big")]).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[one, bad_tag]), "badarg");
            // A bare non-atom encoding.
            let five = ctx.int(5).unwrap();
            assert_eq!(show(ctx, "characters_to_binary", &[one, five]), "badarg");
            assert_eq!(show(ctx, "characters_to_list", &[one, five]), "badarg");
            // The wide atoms themselves are accepted (so the failures above are
            // about the *shape* of the argument, not the encoding name); a
            // one-byte input shows the `cbv/2` asymmetry (`utf16` reports it as
            // a prefix, `utf32` does not).
            assert_eq!(show(ctx, "characters_to_binary", &[one, b16]), "{incomplete,#{},#{1}}");
            assert_eq!(show(ctx, "characters_to_binary", &[one, b32]), "{error,#{},#{1}}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 65]), b16]), "#{65}");
            assert_eq!(show(ctx, "characters_to_binary", &[bin(&[0, 0, 0, 65]), b32]), "#{65}");
        });
    }
}
