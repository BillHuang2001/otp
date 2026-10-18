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

//! The **per-tag minimal valid image** table, shared by the two tag-space suites
//! (`tag_coverage.rs` and `tag_space_audit.rs`) through `mod tag_images;`.
//!
//! It lives in a submodule rather than in either test binary so the 256-byte sweep can
//! reuse these images instead of re-deriving 37 hand-written wire layouts: a second copy
//! would be free to drift from the one `every_covered_tag_decodes` pins.

use beam_codec::Tag;

/// The sysname bytes of the smallest node atom (`SMALL_ATOM_UTF8_EXT`, `"n"`).
pub const NODE_N: [u8; 3] = [119, 1, b'n'];

/// A **minimal valid image** for `tag`, or `None` for the tags whose minimal form
/// needs context the plain `decode_term` entry cannot supply (`COMPRESSED`,
/// `LOCAL_EXT`, `ATOM_CACHE_REF` — each handled by the caller with the state it
/// needs), and for the three uncovered tags (never a term, so no valid image exists).
///
/// Every image is `[VERSION_MAGIC][tag]…` and trailing-byte-free, so it can be decoded
/// through the strict `decode_term` (which calls `Decoder::finish`).
pub fn minimal_image(tag: Tag) -> Option<Vec<u8>> {
    let mut image = vec![beam_codec::VERSION_MAGIC];
    match tag {
        Tag::SmallInteger => image.extend_from_slice(&[97, 0]),
        Tag::Integer => image.extend_from_slice(&[98, 0, 0, 0, 1]),
        // `"1.0e+00"` NUL-padded to the fixed 31-byte `FLOAT_EXT` field (a spelling
        // `sys_chars_to_double` accepts, `sys_float.c:72-140`).
        Tag::Float => {
            image.push(99);
            let text = b"1.0e+00";
            image.extend_from_slice(text);
            image.extend(std::iter::repeat(0).take(31 - text.len()));
        }
        Tag::Atom => image.extend_from_slice(&[100, 0, 1, b'a']),
        Tag::SmallAtom => image.extend_from_slice(&[115, 1, b'a']),
        Tag::AtomUtf8 => image.extend_from_slice(&[118, 0, 1, b'a']),
        Tag::SmallAtomUtf8 => image.extend_from_slice(&[119, 1, b'a']),
        // `REFERENCE_EXT`: `[node][id:4][creation:1]`.
        Tag::Reference => {
            image.push(101);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 42, 0]);
        }
        // `NEW_REFERENCE_EXT`: `[len:2][node][creation:1][ids:len×4]`.
        Tag::NewReference => {
            image.push(114);
            image.extend_from_slice(&[0, 1]);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0]);
            image.extend_from_slice(&[0, 0, 0, 1]);
        }
        // `NEWER_REFERENCE_EXT`: `[len:2][node][creation:4][ids:len×4]`.
        Tag::NewerReference => {
            image.push(90);
            image.extend_from_slice(&[0, 1]);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 0]);
            image.extend_from_slice(&[0, 0, 0, 1]);
        }
        // `PORT_EXT`: `[node][id:4][creation:1]`.
        Tag::Port => {
            image.push(102);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 7, 0]);
        }
        // `NEW_PORT_EXT`: `[node][id:4][creation:4]`.
        Tag::NewPort => {
            image.push(89);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 7, 0, 0, 0, 0]);
        }
        // `V4_PORT_EXT`: `[node][number:8][creation:4]`.
        Tag::V4Port => {
            image.push(120);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0]);
        }
        // `PID_EXT`: `[node][number:4][serial:4][creation:1]`.
        Tag::Pid => {
            image.push(103);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 10, 0, 0, 0, 20, 0]);
        }
        // `NEW_PID_EXT`: `[node][number:4][serial:4][creation:4]`.
        Tag::NewPid => {
            image.push(88);
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 10, 0, 0, 0, 20]);
            image.extend_from_slice(&[0, 0, 0, 0]);
        }
        Tag::SmallTuple => image.extend_from_slice(&[104, 0]),
        Tag::LargeTuple => image.extend_from_slice(&[105, 0, 0, 0, 0]),
        Tag::Nil => image.push(106),
        Tag::String => image.extend_from_slice(&[107, 0, 0]),
        // `LIST_EXT`: `[length:4][elements][tail]` — one cons (`1`) whose tail is `NIL`.
        Tag::List => image.extend_from_slice(&[108, 0, 0, 0, 1, 97, 1, 106]),
        // `SMALL_BIG_EXT`: sign `0`, one magnitude byte `5`.
        Tag::SmallBig => image.extend_from_slice(&[110, 1, 0, 5]),
        // `LARGE_BIG_EXT`: 32-bit count `1`, sign `0`, one magnitude byte `5`.
        Tag::LargeBig => image.extend_from_slice(&[111, 0, 0, 0, 1, 0, 5]),
        Tag::NewFloat => {
            image.push(70);
            image.extend_from_slice(&1.5f64.to_be_bytes());
        }
        Tag::Binary => image.extend_from_slice(&[109, 0, 0, 0, 0]),
        // `BIT_BINARY_EXT`: 1 byte, 1 significant bit (0b1 in the top bit).
        Tag::BitBinary => image.extend_from_slice(&[77, 0, 0, 0, 1, 1, 0x80]),
        Tag::Map => image.extend_from_slice(&[116, 0, 0, 0, 0]),
        // `EXPORT_EXT`: `[module atom][function atom][arity term]`.
        Tag::Export => {
            image.push(113);
            image.extend_from_slice(&[119, 1, b'm']);
            image.extend_from_slice(&[119, 1, b'f']);
            image.extend_from_slice(&[97, 1]);
        }
        // `NEW_FUN_EXT` with `num_free = 0` — no environment to follow.
        Tag::NewFun => {
            image.push(112);
            image.extend_from_slice(&[0, 0, 0, 0]); // `size` (read and ignored since OTP 23)
            image.push(2); // arity (8-bit header field)
            image.extend_from_slice(&[0u8; 16]); // uniq
            image.extend_from_slice(&[0, 0, 0, 0]); // index
            image.extend_from_slice(&[0, 0, 0, 0]); // num_free
            image.extend_from_slice(&[119, 1, b'm']); // module atom
            image.extend_from_slice(&[97, 0]); // old_index (small)
            image.extend_from_slice(&[97, 0]); // old_uniq (small)
            image.extend_from_slice(&[88]); // creator pid: NEW_PID_EXT
            image.extend_from_slice(&NODE_N);
            image.extend_from_slice(&[0, 0, 0, 10, 0, 0, 0, 20]);
            image.extend_from_slice(&[0, 0, 0, 0]);
        }
        // `RECORD_EXT` with zero fields: only the 2-word definition follows.
        Tag::Record => {
            image.push(67);
            image.extend_from_slice(&[0, 0, 0, 0]); // num_fields
            image.push(0); // flags: not exported
            image.extend_from_slice(&[119, 1, b'm']); // module atom
            image.extend_from_slice(&[119, 1, b'r']); // name atom
        }
        // The wrapper/contextual tags have no plain minimal image. `ATOM_INTERNAL_REF2`
        // (73) / `ATOM_INTERNAL_REF3` (75) name an atom **by index**, so a valid image
        // needs a non-empty atom table — a caller that seeds one supplies the image.
        Tag::Compressed
        | Tag::Local
        | Tag::AtomCacheRef
        | Tag::AtomInternalRef2
        | Tag::AtomInternalRef3 => return None,
        // Uncovered tags: never a term, so no valid image exists.
        Tag::Fun | Tag::NewCache | Tag::DistHeader => return None,
    }
    assert_eq!(
        Tag::from_byte(image[1]),
        Some(tag),
        "the minimal image for {tag:?} does not start with its own tag"
    );
    Some(image)
}
