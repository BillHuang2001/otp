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
//! `do_chksum` iolist walk and the `(OldChecksum, Data)` / `*_combine/3` wrappers),
//! `../../erts/emulator/beam/erl_md5.c` + `erl_md5.h` (the `erts_md5_state` and its
//! `init`/`update`/`finish`/`hash_part`/`pad`) and the bundled zlib
//! (`../../erts/emulator/zlib/crc32.c`, `adler32.c` — the CRC-32/Adler-32
//! algorithms and the `*_combine` operator arithmetic). No external dependency is
//! added: zlib's algorithms are small and are reproduced here.
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
//! own `length == 0` workaround (`erl_bif_chksum.c:443`) that returns `C1`.

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
// CRC-32 (the reflected IEEE polynomial, zlib `crc32.c`)
// ---------------------------------------------------------------------------

/// The reflected CRC-32 polynomial (zlib `POLY`, `crc32.c:157`).
const CRC32_POLY: u32 = 0xEDB8_8320;

/// zlib's byte-wise CRC table: `crc_table[i]` is the CRC of the single byte `i`
/// (`crc32.c:248-256`).
const CRC32_TABLE: [u32; 256] = crc32_table();

const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ CRC32_POLY } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// zlib's `crc32(prev, data)` (`crc32_z`): the CRC-32 of `data` continuing from the
/// CRC-32 `prev` of the preceding bytes. A zero-length `data` is the identity.
fn crc32_step(prev: u32, data: &[u8]) -> u32 {
    let mut crc = prev ^ 0xFFFF_FFFF;
    for &byte in data {
        crc = (crc >> 8) ^ CRC32_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize];
    }
    crc ^ 0xFFFF_FFFF
}

/// `a(x) * b(x) mod p(x)` in the reflected CRC-32 field, where the lowest powers sit
/// in the most significant bit — zlib's `multmodp` (`crc32.c:163`).
///
/// `a` is a power of `x` and therefore never zero; the `a == 0` guard (matching
/// `crc32_combine_op`'s early return) keeps the loop's shift register from decaying
/// to zero, which would otherwise never satisfy the terminating condition.
const fn crc_multmodp(a: u32, b: u32) -> u32 {
    if a == 0 {
        return 0;
    }
    let mut m: u32 = 1 << 31;
    let mut p: u32 = 0;
    let mut b = b;
    loop {
        if a & m != 0 {
            p ^= b;
            if a & (m - 1) == 0 {
                break;
            }
        }
        m >>= 1;
        b = if b & 1 != 0 { (b >> 1) ^ CRC32_POLY } else { b >> 1 };
    }
    p
}

/// `x2n_table[j] = x^(2^j) mod p(x)` — zlib's `x2n_table` (`crc32.c:258-262`).
/// `x^(2^32) == x`, so the 32 entries suffice for every bit of a `u64` exponent.
const CRC32_X2N_TABLE: [u32; 32] = crc32_x2n_table();

const fn crc32_x2n_table() -> [u32; 32] {
    let mut table = [0u32; 32];
    let mut p: u32 = 1 << 30; // x^1
    table[0] = p;
    let mut n = 1;
    while n < 32 {
        p = crc_multmodp(p, p);
        table[n] = p;
        n += 1;
    }
    table
}

/// `x^(n * 2^k) mod p(x)` — zlib's `x2nmodp` (`crc32.c:184`).
fn crc_x2nmodp(n: u64, k: u32) -> u32 {
    let mut p: u32 = 1 << 31; // x^0
    let mut n = n;
    let mut k = k;
    while n != 0 {
        if n & 1 != 0 {
            p = crc_multmodp(CRC32_X2N_TABLE[(k & 31) as usize], p);
        }
        n >>= 1;
        k += 1;
    }
    p
}

/// zlib's `crc32_combine(crc1, crc2, len2)` (`crc32.c:969-982`): the CRC-32 of two
/// concatenated streams whose CRCs are `crc1` (first stream) and `crc2` (second
/// stream, `len2` bytes long). `len2` is a byte count; `2^3 = 8` in `crc_x2nmodp`
/// turns it into the bit count.
///
/// A zero `len2` yields `crc1 bxor crc2` (the modern zlib operator form), matching
/// the oracle's `crc32_combine(C1, C2, 0)`.
fn crc32_combine(crc1: u32, crc2: u32, len2: u64) -> u32 {
    let op = crc_x2nmodp(len2, 3);
    if op == 0 {
        // `crc32_combine_op`'s guard; unreachable for a non-negative `len2`.
        return 0;
    }
    crc_multmodp(op, crc1) ^ crc2
}

// ---------------------------------------------------------------------------
// Adler-32 (zlib `adler32.c`)
// ---------------------------------------------------------------------------

/// The largest prime below 65536 (zlib `BASE`).
const ADLER_BASE: u64 = 65521;

/// The largest block length during which the two running sums stay below `2^32`
/// (zlib `NMAX`).
const ADLER_NMAX: usize = 5552;

/// `(sum1, sum2)` back into the packed Adler-32 value.
fn adler_pack(adler: u64, sum2: u64) -> u32 {
    ((adler & 0xFFFF) | ((sum2 & 0xFFFF) << 16)) as u32
}

/// zlib's `adler32(prev, data)` (`adler32_z`, `adler32.c:61`): the Adler-32 of
/// `data` continuing from `prev`.
///
/// The one-byte and shorter-than-16-byte fast paths are reproduced literally, so a
/// caller-supplied *non-canonical* accumulator (`adler32/2`) reduces exactly as the
/// oracle reduces it.
fn adler32_step(prev: u32, data: &[u8]) -> u32 {
    let mut sum2 = u64::from((prev >> 16) & 0xFFFF);
    let mut adler = u64::from(prev & 0xFFFF);
    let len = data.len();

    if len == 1 {
        adler += u64::from(data[0]);
        if adler >= ADLER_BASE {
            adler -= ADLER_BASE;
        }
        sum2 += adler;
        if sum2 >= ADLER_BASE {
            sum2 -= ADLER_BASE;
        }
        return adler_pack(adler, sum2);
    }

    if len < 16 {
        for &byte in data {
            adler += u64::from(byte);
            sum2 += adler;
        }
        if adler >= ADLER_BASE {
            adler -= ADLER_BASE;
        }
        sum2 %= ADLER_BASE;
        return adler_pack(adler, sum2);
    }

    let mut i = 0usize;
    while len - i >= ADLER_NMAX {
        let end = i + ADLER_NMAX;
        while i < end {
            adler += u64::from(data[i]);
            sum2 += adler;
            i += 1;
        }
        adler %= ADLER_BASE;
        sum2 %= ADLER_BASE;
    }
    let mut rem = len - i;
    while rem >= 16 {
        let end = i + 16;
        while i < end {
            adler += u64::from(data[i]);
            sum2 += adler;
            i += 1;
        }
        rem -= 16;
    }
    while rem > 0 {
        adler += u64::from(data[i]);
        sum2 += adler;
        i += 1;
        rem -= 1;
    }
    adler %= ADLER_BASE;
    sum2 %= ADLER_BASE;
    adler_pack(adler, sum2)
}

/// zlib's `adler32_combine_(adler1, adler2, len2)` (`adler32.c:133`): the Adler-32
/// of two concatenated streams, where `len2` is the byte length of the second.
///
/// `MOD63(len2)` reduces the length modulo `BASE` first, exactly as zlib does.
fn adler32_combine(adler1: u32, adler2: u32, len2: u64) -> u32 {
    let rem = len2 % ADLER_BASE;
    let mut sum1 = u64::from(adler1 & 0xFFFF);
    let mut sum2 = (rem * sum1) % ADLER_BASE;
    sum1 += u64::from(adler2 & 0xFFFF) + ADLER_BASE - 1;
    sum2 += u64::from((adler1 >> 16) & 0xFFFF)
        + u64::from((adler2 >> 16) & 0xFFFF)
        + ADLER_BASE
        - rem;
    if sum1 >= ADLER_BASE {
        sum1 -= ADLER_BASE;
    }
    if sum1 >= ADLER_BASE {
        sum1 -= ADLER_BASE;
    }
    if sum2 >= ADLER_BASE << 1 {
        sum2 -= ADLER_BASE << 1;
    }
    if sum2 >= ADLER_BASE {
        sum2 -= ADLER_BASE;
    }
    adler_pack(sum1, sum2)
}

// ---------------------------------------------------------------------------
// MD5 (`erts_md5_state`, `erl_md5.c`)
// ---------------------------------------------------------------------------

/// `sizeof(erts_md5_state)` — the `md5_init/0` binary's length.
const MD5_STATE_LEN: usize = 96;
/// `MD5_DIGEST_LENGTH` — the `md5/1` and `md5_final/1` binary's length.
const MD5_DIGEST_LEN: usize = 16;

/// The 64 per-round constants `sk[i].k` of `hash_part` (`erl_md5.c:161-177`).
const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// The 64 per-round left-rotation amounts `sk[i].shift` (`erl_md5.c:161-177`).
const MD5_SHIFT: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// The C `erts_md5_state` (`erl_md5.h:30-35`), the MD5 running context.
#[derive(Clone)]
struct Md5State {
    a: u32,
    b: u32,
    c: u32,
    d: u32,
    tot_len: u64,
    len: usize,
    buf: [u8; 64],
}

impl Md5State {
    /// `erts_md5_init` (`erl_md5.c:76`): the MD5 IV, zeroed counters and buffer.
    fn new() -> Md5State {
        Md5State {
            a: 0x67452301,
            b: 0xefcdab89,
            c: 0x98badcfe,
            d: 0x10325476,
            tot_len: 0,
            len: 0,
            buf: [0u8; 64],
        }
    }

    /// Decodes the 96-byte `erts_md5_state` image.
    ///
    /// # Errors
    ///
    /// [`unsupported_bif`] when the `len` field is outside `0..=64`: the oracle
    /// reads and writes past its 64-byte buffer for such an image (undefined
    /// behaviour, `erl_bif_chksum.c:562`/`:598` never produce one), which cannot be
    /// reproduced faithfully — the crate reports the gap instead of a fabricated
    /// digest. A wrong length or a non-binary argument is [`badarg`].
    fn from_image(image: &[u8]) -> Result<Md5State> {
        if image.len() != MD5_STATE_LEN {
            return Err(badarg());
        }
        let u32_at = |o: usize| {
            u32::from_le_bytes([image[o], image[o + 1], image[o + 2], image[o + 3]])
        };
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
        if len > 64 {
            return Err(unsupported_bif());
        }
        let mut buf = [0u8; 64];
        buf.copy_from_slice(&image[32..MD5_STATE_LEN]);
        Ok(Md5State {
            a: u32_at(0),
            b: u32_at(4),
            c: u32_at(8),
            d: u32_at(12),
            tot_len: u64_at(16),
            len,
            buf,
        })
    }

    /// The 96-byte `erts_md5_state` image (the little-endian field order of the C
    /// struct).
    fn to_image(&self) -> [u8; MD5_STATE_LEN] {
        let mut image = [0u8; MD5_STATE_LEN];
        image[0..4].copy_from_slice(&self.a.to_le_bytes());
        image[4..8].copy_from_slice(&self.b.to_le_bytes());
        image[8..12].copy_from_slice(&self.c.to_le_bytes());
        image[12..16].copy_from_slice(&self.d.to_le_bytes());
        image[16..24].copy_from_slice(&self.tot_len.to_le_bytes());
        image[24..32].copy_from_slice(&(self.len as u64).to_le_bytes());
        image[32..MD5_STATE_LEN].copy_from_slice(&self.buf);
        image
    }

    /// `erts_md5_update` (`erl_md5.c:87`): appends `msg` to the context.
    fn update(&mut self, msg: &[u8]) {
        let mut part = 0usize;
        let mut left = msg.len();

        if self.len > 0 {
            let copy_size = 64 - self.len;
            if copy_size < left {
                self.buf[self.len..64].copy_from_slice(&msg[part..part + copy_size]);
                part += copy_size;
                left -= copy_size;
                let block = self.buf;
                self.hash_part(&block);
                self.len = 0;
                self.tot_len += 64;
            } else {
                self.buf[self.len..self.len + left].copy_from_slice(&msg[part..part + left]);
                self.len += left;
                return;
            }
        }

        while left >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&msg[part..part + 64]);
            self.hash_part(&block);
            part += 64;
            left -= 64;
            self.tot_len += 64;
        }

        if left > 0 {
            self.buf[..left].copy_from_slice(&msg[part..part + left]);
            self.len = left;
        }
    }

    /// `erts_md5_finish` (`erl_md5.c:125`): pads, hashes, and returns the 16-byte
    /// little-endian digest of the four state words.
    fn finish(&self) -> [u8; MD5_DIGEST_LEN] {
        let mut pad_buf = [0u8; 128];
        let mut left = self.pad(&mut pad_buf);
        let mut state = self.clone();
        let mut off = 0usize;
        while left > 0 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&pad_buf[off..off + 64]);
            state.hash_part(&block);
            left -= 64;
            off += 64;
        }
        let mut out = [0u8; MD5_DIGEST_LEN];
        out[0..4].copy_from_slice(&state.a.to_le_bytes());
        out[4..8].copy_from_slice(&state.b.to_le_bytes());
        out[8..12].copy_from_slice(&state.c.to_le_bytes());
        out[12..16].copy_from_slice(&state.d.to_le_bytes());
        out
    }

    /// `pad` (`erl_md5.c:222`): `0x80`, zeros to the 56-byte boundary, then the
    /// 64-bit little-endian *bit* length. Returns the number of bytes written
    /// (always a multiple of 64).
    fn pad(&self, out: &mut [u8; 128]) -> usize {
        let size = self.len;
        let filled = (size + 1) % 64;
        let zeroes = (64 - 8 - filled) & (64 - 1);
        out[..size].copy_from_slice(&self.buf[..size]);
        out[size] = 0x80;
        // `out[size + 1 .. size + 1 + zeroes]` is already zero.
        let pos = size + 1 + zeroes;
        let tot_bit_size = (self.tot_len + size as u64).wrapping_mul(8);
        out[pos..pos + 8].copy_from_slice(&tot_bit_size.to_le_bytes());
        pos + 8
    }

    /// `hash_part` (`erl_md5.c:156`): folds one 64-byte block into the state.
    fn hash_part(&mut self, block: &[u8; 64]) {
        let mut a = self.a;
        let mut b = self.b;
        let mut c = self.c;
        let mut d = self.d;

        for i in 0..64usize {
            let (f, g) = if i <= 15 {
                (d ^ (b & (c ^ d)), i)
            } else if i <= 31 {
                (c ^ (d & (b ^ c)), (5 * i + 1) & 0xF)
            } else if i <= 47 {
                (b ^ c ^ d, (3 * i + 5) & 0xF)
            } else {
                (c ^ (b | !d), (7 * i) & 0xF)
            };
            let word = u32::from_le_bytes([
                block[g * 4],
                block[g * 4 + 1],
                block[g * 4 + 2],
                block[g * 4 + 3],
            ]);
            let f = f
                .wrapping_add(a)
                .wrapping_add(MD5_K[i])
                .wrapping_add(word)
                .rotate_left(MD5_SHIFT[i]);

            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f);
        }

        self.a = self.a.wrapping_add(a);
        self.b = self.b.wrapping_add(b);
        self.c = self.c.wrapping_add(c);
        self.d = self.d.wrapping_add(d);
    }
}

/// `erts_md5` (`erl_md5.c:146`): the MD5 digest of a whole message.
fn md5_digest(data: &[u8]) -> [u8; MD5_DIGEST_LEN] {
    let mut state = Md5State::new();
    state.update(data);
    state.finish()
}

/// Decodes a context-image argument: a byte-aligned binary of exactly 96 bytes.
fn decode_context(term: Eterm) -> Result<Md5State> {
    let image = bits::bytes(term)?;
    Md5State::from_image(&image)
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
    bits::make_binary(ctx.heap(), &md5_digest(&input.bytes))
}

/// `md5_init()` (`erl_bif_chksum.c:538`) — a fresh 96-byte context image.
fn md5_init_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    bits::make_binary(ctx.heap(), &Md5State::new().to_image())
}

/// `md5_update(Context, IoData)` (`erl_bif_chksum.c:550`) — the context image with
/// `IoData` folded in. `Context` must be a 96-byte binary; anything else is `badarg`.
fn md5_update_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut state = decode_context(args[0])?;
    let input = collect_input(args[1])?;
    state.update(&input.bytes);
    bits::make_binary(ctx.heap(), &state.to_image())
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
            md5_digest(b"abc"),
            [
                144, 1, 80, 152, 60, 210, 79, 176, 214, 150, 63, 125, 40, 225, 127, 114
            ]
        );
        assert_eq!(
            md5_digest(b""),
            [212, 29, 140, 217, 143, 0, 178, 4, 233, 128, 9, 152, 236, 248, 66, 126]
        );
        // >64 bytes exercise the whole-block path.
        assert_eq!(
            md5_digest(&[b'A'; 100]),
            [138, 220, 89, 55, 230, 53, 246, 201, 175, 100, 111, 11, 35, 86, 15, 174]
        );
    }

    #[test]
    fn md5_context_round_trips() {
        // The initial image is the IV in little-endian, then 80 zero bytes.
        let fresh = Md5State::new().to_image();
        let mut expected = [0u8; MD5_STATE_LEN];
        expected[..16].copy_from_slice(&[
            1, 35, 69, 103, 137, 171, 205, 239, 254, 220, 186, 152, 118, 84, 50, 16,
        ]);
        assert_eq!(fresh, expected);

        // update(init, "abc"): len = 3 in the buffer, IV untouched.
        let mut state = Md5State::new();
        state.update(b"abc");
        let image = state.to_image();
        assert_eq!(image[..16], expected[..16], "a sub-block update keeps the IV");
        assert_eq!(&image[16..24], &[0u8; 8], "tot_len is still 0");
        assert_eq!(&image[24..32], &[3, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&image[32..35], b"abc");
        assert_eq!(state.finish(), md5_digest(b"abc"));

        // A full block appends `tot_len` and leaves `len` at 0; 32 + 32 leaves 64.
        let mut state = Md5State::new();
        state.update(&[b'A'; 64]);
        let image = state.to_image();
        assert_eq!(&image[16..24], &[64, 0, 0, 0, 0, 0, 0, 0], "one hashed block");
        assert_eq!(&image[24..32], &[0u8; 8], "the block buffer is empty");
        assert_eq!(state.finish(), md5_digest(&[b'A'; 64]));

        let mut state = Md5State::new();
        state.update(&[b'A'; 32]);
        state.update(&[b'B'; 32]);
        let image = state.to_image();
        assert_eq!(&image[16..24], &[0u8; 8], "the block is buffered, not hashed");
        assert_eq!(&image[24..32], &[64, 0, 0, 0, 0, 0, 0, 0]);
        let mut joined = vec![b'A'; 32];
        joined.extend_from_slice(&[b'B'; 32]);
        assert_eq!(state.finish(), md5_digest(&joined));

        // Any split of the same data gives the same digest.
        for split in [0usize, 1, 63, 64, 65, 100, 128] {
            let data: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
            let mut state = Md5State::new();
            state.update(&data[..split.min(data.len())]);
            state.update(&data[split.min(data.len())..]);
            assert_eq!(state.finish(), md5_digest(&data), "split at {split}");
        }
    }

    #[test]
    fn md5_update_rejects_an_out_of_range_len_field() {
        // A hand-crafted image whose `len` field is 100: the oracle reads/writes
        // past its 64-byte buffer (undefined behaviour), which is reported as a
        // typed `unsupported_bif` rather than reproduced.
        let mut image = Md5State::new().to_image();
        image[24..32].copy_from_slice(&100u64.to_le_bytes());
        assert!(matches!(Md5State::from_image(&image), Err(BeamError::Raise(_))));
        // A `len` of 64 (which the oracle itself produces) is accepted.
        image[24..32].copy_from_slice(&64u64.to_le_bytes());
        assert!(Md5State::from_image(&image).is_ok());
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
