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

//! Checksum and digest BIFs for `erlang`: `crc32/1,2`, `crc32_combine/3`,
//! `adler32/1,2`, `adler32_combine/3`, `md5/1`, `md5_init/0`, `md5_update/2` and
//! `md5_final/1`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_chksum.c` (the
//! `do_chksum` iolist walk and the `(OldChecksum, Data)` / `*_combine/3` wrappers) and
//! `../../erts/emulator/zlib/crc32.c` + `adler32.c` (the CRC-32/Adler-32 algorithms and
//! the `*_combine` operator arithmetic).
//! None of the three digests is reproduced here: they are the workspace's single
//! implementations at their lowest common ancestors — `beam_deflate::checksum`'s port of
//! `crc32.c`/`adler32.c` (shared with `beam_deflate`'s own gzip/zlib trailers) and
//! `beam_common::md5`'s RFC 1321 port of `erl_md5.c` (shared with the `.beam` chunk
//! checksum and the distribution handshake digest) — and this module forwards to them.
//!
//! Every value pinned below was **probed against the system oracle** (OTP 29 /
//! erts-17.1); `tests/checksum.rs` additionally cross-checks a corpus live.
//!
//! # Roster
//!
//! All ten keys are implemented:
//!
//! | BIF | semantics |
//! |---|---|
//! | `erlang:crc32/1` | the CRC-32 of iodata (initial value 0) |
//! | `erlang:crc32/2` | continues a CRC-32 from a previous value |
//! | `erlang:crc32_combine/3` | the CRC-32 of two concatenated streams |
//! | `erlang:adler32/1` | the Adler-32 of iodata (initial value 1) |
//! | `erlang:adler32/2` | continues an Adler-32 from a previous value |
//! | `erlang:adler32_combine/3` | the Adler-32 of two concatenated streams |
//! | `erlang:md5/1` | the MD5 digest of iodata |
//! | `erlang:md5_init/0` | a fresh 96-byte `erts_md5_state` image |
//! | `erlang:md5_update/2` | runs `Data` through a context image |
//! | `erlang:md5_final/1` | finishes a context image into a 16-byte digest |
//!
//! # The input walk
//!
//! `crc32/1,2`, `adler32/1,2` and `md5/1,2` accept the same input shape: a bare
//! **byte-aligned** binary, or a `Unit = 8` iolist (`do_chksum`,
//! `erl_bif_chksum.c:53-264`). Anything else is `badarg` — including a
//! non-byte-aligned bitstring, a float/atom leaf, a list element outside `0..=255`
//! and an improper tail that is neither a list, `[]` nor a byte-aligned binary.
//!
//! # The MD5 context is byte-observable
//!
//! `md5_init/0` returns the 96-byte `erts_md5_state` image (`erl_md5.h:30-35`):
//! `[a:b:c:d]` (four little-endian `u32`s — the MD5 IV on a fresh context), then
//! `tot_len` and `len` (a little-endian `u64` each), then the 64-byte block buffer.
//! `md5_update/2` reproduces that image byte-for-byte, so
//! `md5_final(md5_update(md5_init(), D)) =:= md5(D)` holds for every `D` — with the
//! *in-progress* image observable in between, exactly as in the oracle.
//!
//! # The `*_combine/3` arithmetic
//!
//! `crc32_combine/3` is zlib's `crc32_combine` (`crc32.c:954-984`, the GF(2)
//! `x^(8*len2) mod p` operator form), **not** the older "return `crc1` for a
//! zero length" variant: `crc32_combine(C1, C2, 0)` is `C1 bxor C2` in erts-17.1.
//! `adler32_combine/3` is zlib's `adler32_combine_` (`adler32.c:133`), with OTP's
//! own `length == 0` workaround (`erl_bif_chksum.c:443`) that returns `C1`. Both are
//! [`beam_deflate::checksum`]'s and the MD5 rounds are `beam_common::md5`'s, so each
//! digest's arithmetic exists exactly once in the workspace.

use beam_common::Result;
use beam_term::{signed_value, Eterm};

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::registry::BifRegistry;

/// Registers this module's ten BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "crc32", 1, crc32_1);
    reg.register("erlang", "crc32", 2, crc32_2);
    reg.register("erlang", "crc32_combine", 3, crc32_combine_3);
    reg.register("erlang", "adler32", 1, adler32_1);
    reg.register("erlang", "adler32", 2, adler32_2);
    reg.register("erlang", "adler32_combine", 3, adler32_combine_3);
    reg.register("erlang", "md5", 1, md5_1);
    reg.register("erlang", "md5_init", 0, md5_init_0);
    reg.register("erlang", "md5_update", 2, md5_update_2);
    reg.register("erlang", "md5_final", 1, md5_final_1);
}

// ---------------------------------------------------------------------------
// Argument decoding
// ---------------------------------------------------------------------------

/// `term_to_Uint` narrowed by the C's `((u >> 16) >> 16) != 0` guard: a plain
/// non-negative integer below `2^32` (`erl_bif_chksum.c:319`). A bignum is
/// rejected: it is always outside that range, so `is_small` is an exact test here.
fn as_u32(term: Eterm) -> Option<u32> {
    if access::is_small(term) {
        u32::try_from(signed_value(term)).ok()
    } else {
        None
    }
}

/// A flattened checksum input: the bytes, plus whether `do_chksum` would have
/// performed at least one `sumfun` call.
///
/// The flag is observable only through `adler32/2`: a **zero-length** call reduces a
/// caller-supplied non-canonical accumulator, so `adler32(X, <<>>)` (one zero-length
/// call) and `adler32(X, [])` (no call at all, the `is_nil` early return) differ for
/// an `X` that is not already reduced. For every other checksum a zero-length call
/// is the identity.
struct Input {
    bytes: Vec<u8>,
    called: bool,
}

/// Flattens a checksum input into its bytes.
///
/// The acceptance rules are `do_chksum`'s (`erl_bif_chksum.c:53-264`), which mirror
/// `iolist_to_binary/1`: a bare **byte-aligned** bitstring, or a `Unit = 8` iolist
/// whose leaves are bytes (`0..=255`), byte-aligned binaries, or nested lists — a
/// cons **tail** may also be `[]`, a cons, or such a binary. A bare
/// non-byte-aligned bitstring (`crc32(<<1:3>>)`), a list element outside `0..=255`
/// (`crc32([300])`, `crc32([-1])`), a float/atom/other leaf (`crc32(5)`,
/// `md5([1.5])`) and an improper tail that is neither a list, `[]` nor a
/// byte-aligned binary (`crc32([1|2])`) are all `badarg`.
///
/// The walk uses an explicit work stack, so a deeply nested list cannot overflow the
/// Rust stack.
fn collect_input(root: Eterm) -> Result<Input> {
    if bits::is_bitstring(root) {
        if bits::bit_size(root)? % 8 != 0 {
            return Err(badarg());
        }
        // A bare binary — `<<>>` included — is one (possibly zero-length) call.
        return Ok(Input { bytes: bits::bytes(root)?, called: true });
    }
    if access::is_nil(root) {
        // The `is_nil(ioterm)` early return: no `sumfun` call, so no reduction.
        return Ok(Input { bytes: Vec::new(), called: false });
    }
    if !access::is_list(root) {
        return Err(badarg());
    }

    let mut bytes = Vec::new();
    let mut called = false;
    let mut stack = vec![root];
    while let Some(term) = stack.pop() {
        if access::is_nil(term) {
            continue;
        }
        if access::is_small(term) {
            let byte = signed_value(term);
            if !(0..=255).contains(&byte) {
                return Err(badarg());
            }
            // A flat byte run is at least one byte, hence one `sumfun` call.
            bytes.push(byte as u8);
            called = true;
            continue;
        }
        if bits::is_bitstring(term) {
            if bits::bit_size(term)? % 8 != 0 {
                return Err(badarg());
            }
            bytes.extend_from_slice(&bits::bytes(term)?);
            called = true;
            continue;
        }
        if access::is_list(term) {
            let tail = access::cdr(term);
            // `do_chksum`'s cons-tail rule: a bitstring tail is a valid iolist
            // (`[1|<<2>>]`); anything that is not `[]`, a cons or a bitstring is an
            // improper list (a byte tail is rejected — `ALLOW_BYTE_TAIL` is off).
            if !access::is_nil(tail) && !access::is_list(tail) && !bits::is_bitstring(tail) {
                return Err(badarg());
            }
            // Push the tail first so the head is processed next (LIFO).
            stack.push(tail);
            stack.push(access::car(term));
            continue;
        }
        return Err(badarg());
    }
    Ok(Input { bytes, called })
}

// ---------------------------------------------------------------------------
// CRC-32 and Adler-32 (the workspace's zlib 1.3.2 port, `beam_deflate`)
// ---------------------------------------------------------------------------

/// zlib's `crc32(prev, data)` (`crc32_z`): the CRC-32 of `data` continuing from the
/// CRC-32 `prev` of the preceding bytes. A zero-length `data` is the identity, and a
/// non-canonical `prev` is complemented on entry exactly as the C complements it.
///
/// [`beam_deflate::crc32`] is the workspace's port of
/// `../../erts/emulator/zlib/crc32.c` (the same reflected `0xEDB8_8320` table, the same
/// pre/post complement), so this is a thin forwarder: one table and one algorithm, shared
/// with the gzip/zip framings below and with `beam_deflate`'s own trailers.
///
/// `pub(crate)` because `crate::zlib`'s RFC 1952 (gzip) framing checks — the header's
/// CRC-16 and the per-member CRC-32 trailer — are the same primitive, so the tests that
/// build those members share this one checksum, never a second copy.
pub(crate) fn crc32_step(prev: u32, data: &[u8]) -> u32 {
    beam_deflate::crc32(prev, data)
}

/// zlib's `crc32_combine(crc1, crc2, len2)` (`crc32.c:969-982`): the CRC-32 of two
/// concatenated streams whose CRCs are `crc1` (first stream) and `crc2` (second stream,
/// `len2` bytes long). `len2` is a byte count; zlib's `x2nmodp(len2, 3)` turns it into
/// the bit count.
///
/// A zero `len2` yields `crc1 bxor crc2` (the modern zlib operator form), matching
/// the oracle's `crc32_combine(C1, C2, 0)` — **not** the pre-1.3 "return `crc1`" variant.
fn crc32_combine(crc1: u32, crc2: u32, len2: u64) -> u32 {
    beam_deflate::crc32_combine(crc1, crc2, len2)
}

/// The largest prime below 65536 (zlib `BASE`).
const ADLER_BASE: u64 = 65521;

/// `(sum1, sum2)` back into the packed Adler-32 value.
fn adler_pack(sum1: u64, sum2: u64) -> u32 {
    ((sum1 & 0xFFFF) | ((sum2 & 0xFFFF) << 16)) as u32
}

/// zlib's `adler32(prev, data)` (`adler32_z`, `adler32.c:61`): the Adler-32 of
/// `data` continuing from `prev`.
///
/// Two short arms stay here because zlib's own shortcuts are **observable** through
/// `adler32/2`, which accepts a *non-canonical* accumulator, while
/// [`beam_deflate::adler32`] — the bulk path below, zlib's `NMAX`-batched loop — folds
/// both sums modulo `BASE` at every chunk boundary:
///
/// * `len == 0` — zlib's `len < 16` arm runs for a zero-length call too, so the
///   accumulator **is** folded: the oracle's `adler32(4294967295, <<>>)` is `917518`.
///   (zlib's `buf == Z_NULL` special case — `1` — never arises here: `do_chksum` always
///   passes a real buffer.)
/// * `len == 1` — the byte-at-a-time fast path folds each sum with a **single**
///   conditional subtract, so a non-canonical `sum2` can leave `sum2 >= BASE` and the
///   packed value is then not its `% BASE` reduction: the oracle's
///   `adler32(4294967025, <<255>>)` is `4294901744` (`sum2` 65534), where a modulo fold
///   would answer `917488`.
///
/// `pub(crate)` so the crate's other Adler-32 users reach the one implementation rather
/// than a second copy (the `erlang:adler32/1,2` and `adler32_combine/3` BIFs below).
pub(crate) fn adler32_step(prev: u32, data: &[u8]) -> u32 {
    let mut sum1 = u64::from(prev & 0xFFFF);
    let mut sum2 = u64::from((prev >> 16) & 0xFFFF);
    if data.is_empty() {
        // `len < 16` with `len == 0`: fold, then pack.
        if sum1 >= ADLER_BASE {
            sum1 -= ADLER_BASE;
        }
        return adler_pack(sum1, sum2 % ADLER_BASE);
    }
    if data.len() == 1 {
        // The C's `len == 1` arm, literally: one conditional subtract each.
        sum1 += u64::from(data[0]);
        if sum1 >= ADLER_BASE {
            sum1 -= ADLER_BASE;
        }
        sum2 += sum1;
        if sum2 >= ADLER_BASE {
            sum2 -= ADLER_BASE;
        }
        return adler_pack(sum1, sum2);
    }
    beam_deflate::adler32(prev, data)
}

/// zlib's `adler32_combine_(adler1, adler2, len2)` (`adler32.c:133`): the Adler-32
/// of two concatenated streams, where `len2` is the byte length of the second.
///
/// `MOD63(len2)` reduces the length modulo `BASE` first, exactly as zlib does, and the
/// second stream's length is folded into the first stream's `sum2` — including zlib's
/// `sum1 | (sum2 << 16)` packing, whose **unmasked** `sum1` a non-canonical pair can
/// push past 16 bits (the oracle's `adler32_combine(4294901759, 4294967295, 5)` is
/// `6094860`, not the `6029324` a masked packing would give).
fn adler32_combine(adler1: u32, adler2: u32, len2: u64) -> u32 {
    beam_deflate::adler32_combine(adler1, adler2, len2)
}

// ---------------------------------------------------------------------------
// MD5 (`erts_md5_state`, `erl_md5.h`)
// ---------------------------------------------------------------------------

/// `sizeof(erts_md5_state)` (`erl_md5.h:30-35`) — the `md5_init/0` binary's length.
const MD5_STATE_LEN: usize = 96;

/// Decodes the 96-byte `erts_md5_state` image into a `beam_common::md5` context.
///
/// The MD5 arithmetic is the workspace's single implementation (`beam_common::md5`, the
/// lowest common ancestor this crate shares with the `.beam`-checksum and distribution
/// digest users); this module owns only the C struct's byte layout — a term/ABI concern,
/// since the image is observable as a binary.
///
/// `a`/`b`/`c`/`d` are four little-endian `u32`s at offsets 0/4/8/12, `tot_len` a
/// little-endian `u64` at 16, `len` a little-endian `u64` at 24, and the 64-byte block
/// buffer at 32. All 64 buffer bytes are stored verbatim (not just `len` of them), so a
/// hand-crafted image round-trips byte-exactly.
///
/// # Errors
///
/// [`badarg`] when the image is not exactly [`MD5_STATE_LEN`] bytes. [`unsupported_bif`]
/// when the `len` field does not fit a `usize` or is outside `0..=64`: the oracle reads and
/// writes past its 64-byte buffer for such an image (undefined behaviour,
/// `erl_bif_chksum.c:562`/`:598` never produce one), which cannot be reproduced faithfully
/// — the crate reports the gap instead of a fabricated digest.
fn from_image(image: &[u8]) -> Result<beam_common::md5::Md5> {
    if image.len() != MD5_STATE_LEN {
        return Err(badarg());
    }
    let u32_at =
        |o: usize| u32::from_le_bytes([image[o], image[o + 1], image[o + 2], image[o + 3]]);
    let u64_at = |o: usize| {
        u64::from_le_bytes([
            image[o],
            image[o + 1],
            image[o + 2],
            image[o + 3],
            image[o + 4],
            image[o + 5],
            image[o + 6],
            image[o + 7],
        ])
    };
    let len = usize::try_from(u64_at(24)).map_err(|_| unsupported_bif())?;
    if len > beam_common::md5::BLOCK_LEN {
        return Err(unsupported_bif());
    }
    let mut buf = [0u8; beam_common::md5::BLOCK_LEN];
    buf.copy_from_slice(&image[32..MD5_STATE_LEN]);
    let state = [u32_at(0), u32_at(4), u32_at(8), u32_at(12)];
    beam_common::md5::Md5::from_parts(state, u64_at(16), len, buf)
}

/// The 96-byte `erts_md5_state` image (the little-endian field order of the C struct).
///
/// The inverse of [`from_image`]: the four state words, `tot_len`, `len`, then all 64 bytes
/// of the block buffer.
fn to_image(state: &beam_common::md5::Md5) -> [u8; MD5_STATE_LEN] {
    let mut image = [0u8; MD5_STATE_LEN];
    for (index, word) in state.state().iter().enumerate() {
        image[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    image[16..24].copy_from_slice(&state.total_len().to_le_bytes());
    image[24..32].copy_from_slice(&(state.buffered_len() as u64).to_le_bytes());
    image[32..MD5_STATE_LEN].copy_from_slice(state.buffer());
    image
}

/// Decodes a context-image argument: a byte-aligned binary of exactly 96 bytes.
fn decode_context(term: Eterm) -> Result<beam_common::md5::Md5> {
    let image = bits::bytes(term)?;
    from_image(&image)
}

// ---------------------------------------------------------------------------
// The BIFs
// ---------------------------------------------------------------------------

/// `crc32(IoData)` (`erl_bif_chksum.c:289`) — the CRC-32 with initial value 0.
///
/// `crc32(<<>>)` and `crc32([])` are `0`; `crc32(<<"123456789">>)` is `3421780262`.
fn crc32_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let input = collect_input(args[0])?;
    ctx.int(i64::from(crc32_step(0, &input.bytes)))
}

/// `crc32(OldCrc, IoData)` (`erl_bif_chksum.c:312`) — continues a CRC-32.
///
/// `OldCrc` is a plain integer in `0..=2^32 - 1`; a negative, fractional or
/// out-of-range value is `badarg`. `crc32(X, <<>>)` and `crc32(X, [])` are both `X`.
fn crc32_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let prev = as_u32(args[0]).ok_or_else(badarg)?;
    let input = collect_input(args[1])?;
    ctx.int(i64::from(crc32_step(prev, &input.bytes)))
}

/// `crc32_combine(Crc1, Crc2, Len2)` (`erl_bif_chksum.c:339`) — the CRC-32 of two
/// concatenated streams. All three arguments are plain integers below `2^32`
/// (`Len2` is the second stream's byte length).
fn crc32_combine_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let crc1 = as_u32(args[0]).ok_or_else(badarg)?;
    let crc2 = as_u32(args[1]).ok_or_else(badarg)?;
    let len2 = as_u32(args[2]).ok_or_else(badarg)?;
    ctx.int(i64::from(crc32_combine(crc1, crc2, u64::from(len2))))
}

/// `adler32(IoData)` (`erl_bif_chksum.c:369`) — the Adler-32 with initial value 1.
///
/// `adler32(<<>>)` and `adler32([])` are `1`; `adler32(<<"abc">>)` is `38600999`.
fn adler32_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let input = collect_input(args[0])?;
    // The C seeds with `adler32(0, NULL, 0)`, which zlib answers with `1`
    // (`adler32.c:81`, the deferred initial value).
    let sum = if input.called { adler32_step(1, &input.bytes) } else { 1 };
    ctx.int(i64::from(sum))
}

/// `adler32(OldAdler, IoData)` (`erl_bif_chksum.c:392`) — continues an Adler-32.
///
/// `OldAdler` is a plain integer in `0..=2^32 - 1`. `adler32(X, [])` is `X`
/// (no call at all), while `adler32(X, <<>>)` performs one **zero-length** update
/// that reduces a non-canonical `X` — both oracle-exact.
fn adler32_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let prev = as_u32(args[0]).ok_or_else(badarg)?;
    let input = collect_input(args[1])?;
    let sum = if input.called { adler32_step(prev, &input.bytes) } else { prev };
    ctx.int(i64::from(sum))
}

/// `adler32_combine(Adler1, Adler2, Len2)` (`erl_bif_chksum.c:419`) — the Adler-32
/// of two concatenated streams.
///
/// A `Len2` of `0` returns `Adler1` unchanged — OTP's own workaround around zlib's
/// `adler32_combine` (`erl_bif_chksum.c:443`).
fn adler32_combine_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let adler1 = as_u32(args[0]).ok_or_else(badarg)?;
    let adler2 = as_u32(args[1]).ok_or_else(badarg)?;
    let len2 = as_u32(args[2]).ok_or_else(badarg)?;
    let sum = if len2 == 0 { adler1 } else { adler32_combine(adler1, adler2, u64::from(len2)) };
    ctx.int(i64::from(sum))
}

/// `md5(IoData)` (`erl_bif_chksum.c:454`) — the MD5 digest as a 16-byte binary.
///
/// `md5(<<"abc">>)` is `<<144,1,80,152,60,210,79,176,214,150,63,125,40,225,127,114>>`.
fn md5_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let input = collect_input(args[0])?;
    bits::make_binary(ctx.heap(), &beam_common::md5::digest(&input.bytes))
}

/// `md5_init()` (`erl_bif_chksum.c:538`) — a fresh 96-byte context image.
fn md5_init_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    bits::make_binary(ctx.heap(), &to_image(&beam_common::md5::Md5::new()))
}

/// `md5_update(Context, IoData)` (`erl_bif_chksum.c:550`) — the context image with
/// `IoData` folded in. `Context` must be a 96-byte binary; anything else is `badarg`.
fn md5_update_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut state = decode_context(args[0])?;
    let input = collect_input(args[1])?;
    state.update(&input.bytes);
    bits::make_binary(ctx.heap(), &to_image(&state))
}

/// `md5_final(Context)` (`erl_bif_chksum.c:589`) — the 16-byte digest of a context
/// image. `Context` must be a 96-byte binary; anything else is `badarg`. The
/// argument is read, never mutated (the C works on a copy).
fn md5_final_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let state = decode_context(args[0])?;
    bits::make_binary(ctx.heap(), &state.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    fn heap() -> Heap {
        Heap::new(1 << 16).expect("test heap")
    }

    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = heap();
        let atoms = AtomTable::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    fn binary(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
        bits::make_binary(ctx.heap(), bytes).expect("the binary fits")
    }

    #[test]
    fn crc32_known_vectors() {
        // The canonical CRC-32 check value (zlib: `crc32(0, "123456789")`).
        assert_eq!(crc32_step(0, b"123456789"), 3_421_780_262);
        assert_eq!(crc32_step(0, b""), 0);
        assert_eq!(crc32_step(0, b"abc"), 891_568_578);
        // A continuation is the same as one call over the concatenation.
        let first = crc32_step(0, b"1234");
        assert_eq!(crc32_step(first, b"56789"), 3_421_780_262);
    }

    #[test]
    fn crc32_combine_matches_the_oracle_operator_form() {
        let c1 = 2_615_402_659; // crc32(<<"1234">>)
        let c2 = 320_708_720; // crc32(<<"56789">>)
        assert_eq!(crc32_combine(c1, c2, 5), 3_421_780_262);
        // A zero length is `c1 bxor c2` (modern zlib), not `c1`.
        assert_eq!(crc32_combine(c1, c2, 0), c1 ^ c2);
        assert_eq!(crc32_combine(c1, c2, 1), 1_549_382_082);
        assert_eq!(crc32_combine(c1, c2, 9), 135_094_578);
        assert_eq!(crc32_combine(c1, 0, 0), c1);
        assert_eq!(crc32_combine(0, c2, 0), c2);
        assert_eq!(crc32_combine(0, 0, 0), 0);
    }

    #[test]
    fn adler32_known_vectors() {
        assert_eq!(adler32_step(1, b"abc"), 38_600_999);
        assert_eq!(adler32_step(0, b"abc"), 38_404_390);
        assert_eq!(adler32_step(1, b"123456789"), 152_961_502);
        // The continuation matches a single call over the concatenation.
        let first = adler32_step(1, b"1234");
        assert_eq!(adler32_step(first, b"56789"), 152_961_502);
        // A zero-length step reduces a non-canonical accumulator.
        assert_eq!(adler32_step(u32::MAX, &[]), 917_518);
        assert_eq!(adler32_step(u32::MAX, b"a"), 8_192_111);
        // `NMAX` blocking does not change the result.
        let big = vec![255u8; 6000];
        assert_eq!(adler32_step(u32::MAX, &big), 3_580_123_639);
    }

    #[test]
    fn adler32_combine_matches_the_oracle() {
        let a1 = 33_030_347; // adler32(<<"1234">>)
        let a2 = 53_739_796; // adler32(<<"56789">>)
        assert_eq!(adler32_combine(a1, a2, 5), 152_961_502);
        // zlib reduces the length modulo BASE.
        assert_eq!(adler32_combine(a1, a2, 5 + ADLER_BASE), 152_961_502);
    }

    #[test]
    fn md5_known_vectors() {
        assert_eq!(
            beam_common::md5::digest(b"abc"),
            [
                144, 1, 80, 152, 60, 210, 79, 176, 214, 150, 63, 125, 40, 225, 127, 114
            ]
        );
        assert_eq!(
            beam_common::md5::digest(b""),
            [212, 29, 140, 217, 143, 0, 178, 4, 233, 128, 9, 152, 236, 248, 66, 126]
        );
        // >64 bytes exercise the whole-block path.
        assert_eq!(
            beam_common::md5::digest(&[b'A'; 100]),
            [138, 220, 89, 55, 230, 53, 246, 201, 175, 100, 111, 11, 35, 86, 15, 174]
        );
    }

    /// A digest as lowercase hex.
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A deterministic message whose byte `i` is `i % 251` (the `tests/checksum.rs`
    /// corpus convention).
    fn patterned(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// The `pad` boundary, pinned against independently computed digests.
    ///
    /// With `size` buffered bytes, `pad` derives `filled = (size + 1) % 64` and the
    /// zero run `(56 - filled) mod 64` (the C writes that as the unsigned
    /// `(64 - 8 - filled) & (64 - 1)`). For a message whose length is `56..=63` beyond
    /// a whole number of 64-byte blocks the 8-byte length field no longer fits the
    /// final block, so the subtraction underflows — which a checked Rust `usize`
    /// subtraction cannot express. Every length whose byte count is `0..=64` or
    /// `120..=128` (mod 64) is pinned here, which covers the whole boundary window
    /// plus the explicit `55`/`56`/`63`/`64`-byte cases.
    ///
    /// The goldens are `md5sum` outputs over the same byte pattern, cross-checked
    /// against Python's `hashlib`; MD5 is RFC 1321, so the digest is not in doubt and
    /// every one of these inputs is an independently known-answer vector.
    #[test]
    fn md5_pad_boundary_lengths_match_known_vectors() {
        const KNOWN: &[(usize, &str)] = &[
            (0, "d41d8cd98f00b204e9800998ecf8427e"),
            (1, "93b885adfe0da089cdf634904fd59f71"),
            (2, "441077cc9e57554dd476bdfb8b8b8102"),
            (3, "b95f67f61ebb03619622d798f45fc2d3"),
            (4, "37b59afd592725f9305e484a5d7f5168"),
            (5, "d05374dc381d9b52806446a71c8e79b1"),
            (6, "d15ae53931880fd7b724dd7888b4b4ed"),
            (7, "9aa461e1eca4086f9230aa49c90b0c61"),
            (8, "3677509751ccf61539174d2b9635a7bf"),
            (9, "a6e7d3b46fdfaf0bde2a1f832a00d2de"),
            (10, "c56bd5480f6e5413cb62a0ad9666613a"),
            (11, "5b86fa8ad8f4357ea417214182177be8"),
            (12, "50a73d7013e9803e3b20888f8fcafb15"),
            (13, "b20d4797e23eea3ea5778970d2e226f3"),
            (14, "aa541e601b7b9ddd0504d19866350d4e"),
            (15, "58b7ce493ac99c66058538dacb1e3c94"),
            (16, "1ac1ef01e96caf1be0d329331a4fc2a8"),
            (17, "1bdd36b0a024c90db383512607293692"),
            (18, "633ab81aea5942052b794524e1a28477"),
            (19, "2d325313eb5df436c078435fa0f5eff1"),
            (20, "1549d1aae20214e065ab4b76aaac89a8"),
            (21, "7e437c81824d3982e70c88b5da8ea94b"),
            (22, "2f5f7e7216832ae19c353023618a35a8"),
            (23, "6535e52506c27eaa1033891ff4f3a74e"),
            (24, "8bd9c8efbbac58748951ca5a45cfd386"),
            (25, "d983c63bf41853056787fe1bb764dbff"),
            (26, "b4f24c1219fb00d081c4020c56263451"),
            (27, "b0ae6708c5e1be10668f57d3916cf423"),
            (28, "ba7bb5ad4dba5bde028703007969cb25"),
            (29, "ea880e16eac1b1488aff8a25d11d6271"),
            (30, "c7172f0903c4919eb232f18ab7a30c42"),
            (31, "e9e77893ba926e732f483282f416ffac"),
            (32, "b4ffcb23737cec315a4a4d1aa2a620ce"),
            (33, "5506a276a0a9acc3093f9169c73cf8c5"),
            (34, "e5a849897d9cc0b25b286c1f0bfb50e3"),
            (35, "f54fa30ea7b26d3e11c54d3c8451bcf0"),
            (36, "07602fe0229e486957081a49e3f06f83"),
            (37, "7c4bba98253ca834bf9ed43fd8b2f959"),
            (38, "cf8df427548bbfdb1e11143fdf008b85"),
            (39, "1431a6895a8f435755395f9ba83e76bf"),
            (40, "30dd5e4cae35ba892cc66d7736723980"),
            (41, "8ee247a1063931bedaf4c2fa3e4e261a"),
            (42, "c32ceee2d2245df8589f94fcda0c9f2c"),
            (43, "f25fa0e071d1f1cdc6632c6b673bccd5"),
            (44, "370491b643e97577f4f74bd88576d1ec"),
            (45, "b292bf16e3aafaf41f19c921068214f8"),
            (46, "52921aae5ccc9b6e8e45853419d0c80f"),
            (47, "f1375be31969155ef76f04741cd861d7"),
            (48, "04605ca542b2d82b9886a4b4b9acfb1c"),
            (49, "fa887ba0fa491faaacbb82bc5fefcd5b"),
            (50, "06470e932ad7c7cedf548b5ccb9d4806"),
            (51, "ad130b245e2dd894267cb0ddc532d169"),
            (52, "a9eeb95053682248608e97d79e89ca82"),
            (53, "cc26a3dc608268b98ecd1f3946c4b718"),
            (54, "33dd62a2df6538daf1cf821d9cde61f9"),
            (55, "6912ee65fff2d9f9ce2508cddf8bcda0"),
            (56, "51fdd1acda72405dfdfa03fcb85896d7"),
            (57, "5320ef4c17ef34a0cf2db763338d25eb"),
            (58, "9f4f41b5cde885f94cfc0e06e78f929d"),
            (59, "e39965bc00ecacd90fd875f77eff499a"),
            (60, "63ed72093ae09e2c8553ee069e63d702"),
            (61, "0d08fc14ac5baa37792377355dbad0ae"),
            (62, "f3cdffe2e160a061754a06dafcfd688b"),
            (63, "48a6295221902e8e0938f773a7185e72"),
            (64, "b2d3f56bc197fd985d5965079b5e7148"),
            (120, "b7ba1efc6022e9ed272f00b8831e26e6"),
            (121, "b0b2d719a838db877b6d6571a39a1cdc"),
            (122, "800aa956ec16f603ecdba66c2dc6e4cf"),
            (123, "8827d2778287c58a242acd4c549beb31"),
            (124, "cfbc5aa0b61103c1a982d8927b26f575"),
            (125, "a1f5b691f74f566a2be1765731084f8a"),
            (126, "80749be03f5724fa4ca0aef8909379b7"),
            (127, "8402b21e7bc7906493bae0dac017f1f9"),
            (128, "37eff01866ba3f538421b30b7cbefcac"),
            (1000, "a24f1e3ef66950e1327f210e3997ba2c"),
            (4096, "a0c16616c91907bd14e999986cf822d5"),
        ];

        for &(len, expected) in KNOWN {
            let data = patterned(len);
            assert_eq!(hex(&beam_common::md5::digest(&data)), expected, "md5/1 of a {len}-byte input");
            // `md5_init`/`md5_update`/`md5_final` pad through the same `pad`; an
            // irregular chunking must not change the digest.
            let mut state = beam_common::md5::Md5::new();
            for chunk in data.chunks(7) {
                state.update(chunk);
            }
            assert_eq!(
                hex(&state.finish()),
                expected,
                "the streaming digest of a {len}-byte input"
            );
        }

        // The window the defect lived in is really covered: `filled = (len + 1) % 64`
        // is `55..=64` exactly for a 54..=63-byte buffered remainder.
        for len in 54..=63 {
            assert!(
                KNOWN.iter().any(|&(n, _)| n == len),
                "the boundary window is not covered at {len} bytes"
            );
        }
    }

    /// The RFC 1321 Appendix A.5 test suite, plus the classic 10^6-byte `"a"` vector
    /// (multi-block, and its padded length field has a non-zero high word).
    #[test]
    fn md5_rfc_1321_suite_vectors() {
        let cases: [(&[u8], &str); 6] = [
            (b"a", "0cc175b9c0f1b6a831c399e269772661"),
            (b"message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (b"abcdefghijklmnopqrstuvwxyz", "c3fcd3d76192e4007dfb496cca67e13b"),
            (
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
            // `md5("")` and `md5("abc")` are byte-pinned in `md5_known_vectors`.
            (&[], "d41d8cd98f00b204e9800998ecf8427e"),
        ];
        for (data, expected) in cases {
            assert_eq!(hex(&beam_common::md5::digest(data)), expected, "md5/1 of {:?}", data);
        }

        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(hex(&beam_common::md5::digest(&million_a)), "7707d6ae4e027c70eea2a935c2296f21");
        // A split that leaves the last chunk at the pad boundary.
        let mut state = beam_common::md5::Md5::new();
        state.update(&million_a[..999_993]);
        state.update(&million_a[999_993..]);
        assert_eq!(hex(&state.finish()), "7707d6ae4e027c70eea2a935c2296f21");
    }

    #[test]
    fn md5_context_round_trips() {
        // The initial image is the IV in little-endian, then 80 zero bytes.
        let fresh = to_image(&beam_common::md5::Md5::new());
        let mut expected = [0u8; MD5_STATE_LEN];
        expected[..16].copy_from_slice(&[
            1, 35, 69, 103, 137, 171, 205, 239, 254, 220, 186, 152, 118, 84, 50, 16,
        ]);
        assert_eq!(fresh, expected);

        // update(init, "abc"): len = 3 in the buffer, IV untouched.
        let mut state = beam_common::md5::Md5::new();
        state.update(b"abc");
        let image = to_image(&state);
        assert_eq!(image[..16], expected[..16], "a sub-block update keeps the IV");
        assert_eq!(&image[16..24], &[0u8; 8], "tot_len is still 0");
        assert_eq!(&image[24..32], &[3, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&image[32..35], b"abc");
        assert_eq!(state.finish(), beam_common::md5::digest(b"abc"));

        // A full block appends `tot_len` and leaves `len` at 0; 32 + 32 leaves 64.
        let mut state = beam_common::md5::Md5::new();
        state.update(&[b'A'; 64]);
        let image = to_image(&state);
        assert_eq!(&image[16..24], &[64, 0, 0, 0, 0, 0, 0, 0], "one hashed block");
        assert_eq!(&image[24..32], &[0u8; 8], "the block buffer is empty");
        assert_eq!(state.finish(), beam_common::md5::digest(&[b'A'; 64]));

        let mut state = beam_common::md5::Md5::new();
        state.update(&[b'A'; 32]);
        state.update(&[b'B'; 32]);
        let image = to_image(&state);
        assert_eq!(&image[16..24], &[0u8; 8], "the block is buffered, not hashed");
        assert_eq!(&image[24..32], &[64, 0, 0, 0, 0, 0, 0, 0]);
        let mut joined = vec![b'A'; 32];
        joined.extend_from_slice(&[b'B'; 32]);
        assert_eq!(state.finish(), beam_common::md5::digest(&joined));

        // Any split of the same data gives the same digest.
        for split in [0usize, 1, 63, 64, 65, 100, 128] {
            let data: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
            let mut state = beam_common::md5::Md5::new();
            state.update(&data[..split.min(data.len())]);
            state.update(&data[split.min(data.len())..]);
            assert_eq!(state.finish(), beam_common::md5::digest(&data), "split at {split}");
        }
    }

    #[test]
    fn md5_update_rejects_an_out_of_range_len_field() {
        // A hand-crafted image whose `len` field is 100: the oracle reads/writes
        // past its 64-byte buffer (undefined behaviour), which is reported as a
        // typed `unsupported_bif` rather than reproduced.
        let mut image = to_image(&beam_common::md5::Md5::new());
        image[24..32].copy_from_slice(&100u64.to_le_bytes());
        assert!(matches!(from_image(&image), Err(BeamError::Raise(_))));
        // A `len` of 64 (which the oracle itself produces) is accepted.
        image[24..32].copy_from_slice(&64u64.to_le_bytes());
        assert!(from_image(&image).is_ok());
    }

    #[test]
    fn collect_input_accepts_the_iolist_shapes() {
        with_ctx(|ctx| {
            assert_eq!(collect_input(NIL).unwrap().bytes, Vec::<u8>::new());
            assert!(!collect_input(NIL).unwrap().called, "`[]` performs no call");

            let empty = binary(ctx, &[]);
            let empty_input = collect_input(empty).unwrap();
            assert!(empty_input.bytes.is_empty());
            assert!(empty_input.called, "`<<>>` performs a zero-length call");

            let inner = ctx.list(&[make_small(4)]).unwrap();
            let mid = binary(ctx, &[2, 3]);
            let nested = ctx
                .list(&[make_small(1), mid, inner])
                .unwrap();
            assert_eq!(collect_input(nested).unwrap().bytes, vec![1, 2, 3, 4]);

            // The cons-tail rules: a bitstring tail is accepted, a byte tail is not.
            let tail_binary = binary(ctx, &[2]);
            let bitstring_tail = ctx.cons(make_small(1), tail_binary).unwrap();
            assert_eq!(collect_input(bitstring_tail).unwrap().bytes, vec![1, 2]);
            let byte_tail = ctx.cons(make_small(1), make_small(2)).unwrap();
            assert!(matches!(collect_input(byte_tail), Err(BeamError::BadArg)));

            // A bare byte-aligned binary, and a bare non-byte-aligned bitstring.
            let odd_bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 3).unwrap();
            assert!(matches!(collect_input(odd_bits), Err(BeamError::BadArg)));
            assert!(matches!(collect_input(make_small(5)), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn as_u32_is_the_c_uint_check() {
        assert_eq!(as_u32(make_small(0)), Some(0));
        assert_eq!(as_u32(make_small(u32::MAX as i64)), Some(u32::MAX));
        assert_eq!(as_u32(make_small(u32::MAX as i64 + 1)), None);
        assert_eq!(as_u32(make_small(-1)), None);
        assert_eq!(as_u32(NIL), None);
    }
}
