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

//! The node's `LOCAL_EXT` (`121`) hash for `term_to_binary(Term, [local])`.
//!
//! The C computes this **once per VM start** in `erts_late_init_external`
//! (`../../erts/emulator/beam/external.c:136-200`): it builds a byte buffer of
//! `time_offset(8 LE bytes) || hostname || pid-string`, then runs a Bob-Jenkins **block hash**
//! over it seeded with the low 32 bits of the VM's monotonic clock. The value is
//! node-local by design: two runs of `term_to_binary(self(), [local])` differ in exactly those
//! four bytes (see `../beam_bifs/src/binconv.rs`'s `encode_image`).
//!
//! This module reproduces that shape with real process/host state and a self-contained port of
//! the same block hash ([`block_hash`], `../../erts/emulator/beam/erl_term_hashing.c:427-501`), so a
//! `[local]` image carries four genuinely mixed bytes rather than a fabricated constant. The
//! value is computed **once** per OS process ([`node_hash`]) and is stable for the whole
//! process lifetime — the same guarantee the C's `static Uint32 local_node_hash` gives.
//!
//! The hash is NOT byte-comparable across runtimes (the seed is a clock reading), so a
//! differential case covering `[local]` must **mask** the 4 hash bytes and compare the rest of
//! the image.

use std::sync::OnceLock;

/// The one hash per OS process (the C's `static Uint32 local_node_hash`).
static HASH: OnceLock<u32> = OnceLock::new();

/// The node's `LOCAL_EXT` hash — computed once per OS process and stable thereafter.
///
/// The input bytes mirror the C's `lnid` buffer (`external.c:163-186`): an 8-byte
/// little-endian time-shaped field, then the hostname, then the decimal process id. The seed
/// is the low 32 bits of the clock reading, exactly as the C seeds with
/// `(Uint32) (mtime & 0xffffffff)`.
pub(super) fn node_hash() -> u32 {
    *HASH.get_or_init(|| {
        let clock_nanos = clock_nanos();
        let mut buf = Vec::new();
        // The "time offset" field of the C's buffer: here the full 64-bit clock reading in
        // little-endian order (the C writes its time offset the same way).
        buf.extend_from_slice(&clock_nanos.to_le_bytes());
        buf.extend_from_slice(hostname().as_bytes());
        // The C's `pidstr` is the decimal pid text (`sys_get_pid`).
        buf.extend_from_slice(std::process::id().to_string().as_bytes());
        block_hash(&buf, clock_nanos as u32)
    })
}

/// The clock reading seeding the hash: nanoseconds since the UNIX epoch (as `u64`).
///
/// A monotonic-clock reading would be preferable (the C uses `erts_get_monotonic_time`), but
/// Rust's `Instant` carries no absolute value and adding a clock dependency is out of scope, so
/// the wall clock is the reachable source. The only case `duration_since` rejects is a system
/// clock set before the epoch; the field then contributes `0` and the hash is still mixed from
/// the host and pid bytes, so it stays node-local and per-start.
fn clock_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_nanos() as u64)
        .unwrap_or(0)
}

/// The host name, resolved from real host state without a new dependency.
///
/// Fallback chain, in order: the Linux kernel hostname file
/// `/proc/sys/kernel/hostname`; the `HOSTNAME` environment variable; the `COMPUTERNAME`
/// environment variable (the Windows spelling); else the empty string — which is exactly the
/// C's own answer when `sys_get_hostname` fails (`external.c:147-153`).
fn hostname() -> String {
    if let Ok(raw) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let name = raw.trim_end_matches(|c| c == '\n' || c == '\r');
        if !name.is_empty() {
            return name.to_string();
        }
    }
    for key in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                return value;
            }
        }
    }
    String::new()
}

/// The Bob-Jenkins **block hash** (C `erts_block_hash_init` + `erts_block_hash` over a flattened
/// buffer, `erts/emulator/beam/erl_term_hashing.c:427-501`).
///
/// `bytes` is hashed in 12-byte blocks, then a final partial-block fold adds the bytes into the
/// (a, b, c) state along with the buffer's total length, and one last `MIX` produces the answer.
/// The result is the C's `ctx->c`. Deterministic for a given `(bytes, seed)` and genuinely mixed
/// (a one-bit input change propagates through the whole state).
pub(super) fn block_hash(bytes: &[u8], seed: u32) -> u32 {
    let mut chunks = bytes.chunks_exact(BLOCK_HASH_BYTES_PER_ITER);
    let (mut a, mut b, mut c) = (HCONST, HCONST, seed);
    for k in chunks.by_ref() {
        a = a.wrapping_add(u32::from_le_bytes([k[0], k[1], k[2], k[3]]));
        b = b.wrapping_add(u32::from_le_bytes([k[4], k[5], k[6], k[7]]));
        c = c.wrapping_add(u32::from_le_bytes([k[8], k[9], k[10], k[11]]));
        (a, b, c) = mix(a, b, c);
    }
    // `full_length` is the whole buffer's length, added into `c` before the final fold
    // (`block_hash_final_bytes`'s `ctx->c += full_length`).
    c = c.wrapping_add(bytes.len() as u32);
    let tail = chunks.remainder();
    // The C's fallthrough switch: bytes 10..0 land in descending shift positions, high bytes in
    // `c`, the middle four in `b`, the low four in `a`.
    if tail.len() >= 11 {
        c = c.wrapping_add((tail[10] as u32) << 24);
    }
    if tail.len() >= 10 {
        c = c.wrapping_add((tail[9] as u32) << 16);
    }
    if tail.len() >= 9 {
        c = c.wrapping_add((tail[8] as u32) << 8);
    }
    if tail.len() >= 8 {
        b = b.wrapping_add((tail[7] as u32) << 24);
    }
    if tail.len() >= 7 {
        b = b.wrapping_add((tail[6] as u32) << 16);
    }
    if tail.len() >= 6 {
        b = b.wrapping_add((tail[5] as u32) << 8);
    }
    if tail.len() >= 5 {
        b = b.wrapping_add(tail[4] as u32);
    }
    if tail.len() >= 4 {
        a = a.wrapping_add((tail[3] as u32) << 24);
    }
    if tail.len() >= 3 {
        a = a.wrapping_add((tail[2] as u32) << 16);
    }
    if tail.len() >= 2 {
        a = a.wrapping_add((tail[1] as u32) << 8);
    }
    if tail.len() >= 1 {
        a = a.wrapping_add(tail[0] as u32);
    }
    let (_, _, c) = mix(a, b, c);
    c
}

/// Bytes hashed per block in the C (`BLOCK_HASH_BYTES_PER_ITER`).
const BLOCK_HASH_BYTES_PER_ITER: usize = 12;

/// The C's `HCONST` — the golden-ratio constant the block hash's `a`/`b` start from.
const HCONST: u32 = 0x9e3779b9;

/// The C's `MIX(a, b, c)` macro (`erl_term_hashing.c:428-439`): nine rounds of
/// subtract/xor-shift, each state word updating in turn. Wrapping arithmetic is the C's
/// `Uint32` overflow.
fn mix(a: u32, b: u32, c: u32) -> (u32, u32, u32) {
    let mut a = a;
    let mut b = b;
    let mut c = c;
    a = a.wrapping_sub(b).wrapping_sub(c) ^ (c >> 13);
    b = b.wrapping_sub(c).wrapping_sub(a) ^ (a << 8);
    c = c.wrapping_sub(a).wrapping_sub(b) ^ (b >> 13);
    a = a.wrapping_sub(b).wrapping_sub(c) ^ (c >> 12);
    b = b.wrapping_sub(c).wrapping_sub(a) ^ (a << 16);
    c = c.wrapping_sub(a).wrapping_sub(b) ^ (b >> 5);
    a = a.wrapping_sub(b).wrapping_sub(c) ^ (c >> 3);
    b = b.wrapping_sub(c).wrapping_sub(a) ^ (a << 10);
    c = c.wrapping_sub(a).wrapping_sub(b) ^ (b >> 15);
    (a, b, c)
}
