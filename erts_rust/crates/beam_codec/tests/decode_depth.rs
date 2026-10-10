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

//! The decode **nesting-depth bound**: a deeply nested image must yield a typed
//! [`beam_common::BeamError`], never a native stack overflow.
//!
//! [`Decoder::decode`](beam_codec::Decoder::decode) is recursive, so an image such as
//! `<<131,(binary:copy(<<104,1>>,27500))/binary,106>>` (~55 KB of nested 1-tuples)
//! would otherwise exhaust the native stack and **abort the process** — a Rust stack
//! overflow (`SIGABRT`), not a typed error. The decoder now refuses any term nested
//! deeper than `MAX_DECODE_DEPTH` (1500; see `src/decode/mod.rs`) with
//! `BeamError::SystemLimit`. This file pins that guarantee:
//!
//! * a nesting depth comfortably under the bound decodes normally;
//! * a nesting depth over the bound is the typed `SystemLimit` — and each over-limit
//!   case runs on a deliberately small (2 MiB) thread, where an *unguarded*
//!   regression would blow the stack and abort the whole test process, so the guard
//!   cannot silently disappear;
//! * the bound covers **depth only** — a 1,000,000-element list and a
//!   100,000-element tuple (both decoded iteratively) still decode;
//! * the exact hostile image from the bug report is a typed error.
//!
//! `MAX_DECODE_DEPTH` is private; the depths below sit on either side of it (1000 <
//! 1500 < 8000 < 27500) and are chosen so the measurements in `src/decode/mod.rs`
//! hold: on a 2 MiB stack the *unguarded* recursion overflows at a nesting depth of
//! about 6250, so 8000 and 27500 would each abort without the guard.

use std::thread;

use beam_alloc::Heap;
use beam_codec::{
    decode_term, LARGE_TUPLE_EXT, LIST_EXT, NIL_EXT, SMALL_TUPLE_EXT, VERSION_MAGIC,
};
use beam_common::BeamError;
use beam_term::prelude::{is_list, Eterm};
use beam_term::AtomTable;

/// The stack given to every worker thread that decodes an over-limit image.
///
/// 2 MiB is the Rust default for spawned threads and four times *smaller* than the
/// 8 MiB main thread, so it is the tight case the guard must survive — and the case
/// where an unguarded regression aborts (the recursion `SIGABRT`s at a nesting depth
/// of ~6250 here, well under the 8000 the over-limit tests feed it).
const SMALL_STACK_BYTES: usize = 2 << 20;

/// Decode outcome, computed inside the worker thread so the test only ever moves
/// owned, `Send` data across the `join` boundary.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The image decoded; the payload describes the top-level term's shape.
    Decoded(String),
    /// The image was refused by the depth bound with `BeamError::SystemLimit`.
    SystemLimit,
    /// The image was refused with some other typed error; carries its `Debug` form.
    Failed(String),
}

/// Runs `decode_term(image)` on a fresh thread with `stack_bytes` of stack over a
/// `heap_words`-word decode heap, and classifies the outcome.
///
/// Giving the thread an explicit (small) stack is the point: a regression that
/// removed the depth guard would overflow it and abort the whole test process, which
/// is precisely the failure these tests exist to catch.
fn decode_on_stack(stack_bytes: usize, image: Vec<u8>, heap_words: usize) -> Verdict {
    thread::Builder::new()
        .stack_size(stack_bytes)
        .spawn(move || {
            let mut heap = Heap::new(heap_words).expect("the carrier allocator never fails for these sizes");
            let mut atoms = AtomTable::new();
            match decode_term(&image, &mut heap, &mut atoms) {
                Ok(term) => Verdict::Decoded(describe(term)),
                Err(BeamError::SystemLimit) => Verdict::SystemLimit,
                Err(err) => Verdict::Failed(format!("{err:?}")),
            }
        })
        .expect("spawning a thread with an explicit stack size never fails")
        .join()
        .expect("the decode worker must not panic")
}

/// A short shape tag for a decoded top-level term, used only in assertion messages.
fn describe(term: Eterm) -> String {
    if let Ok(arity) = beam_codec::term::tuple_arity(term) {
        return format!("tuple[{arity}]");
    }
    if is_list(term) {
        return "list".to_string();
    }
    "other".to_string()
}

/// `[VERSION_MAGIC] ++ [SMALL_TUPLE_EXT,1]*depth ++ [NIL]`: `depth` nested 1-tuples
/// wrapping the empty list — exactly the shape of the reported hostile image.
fn nested_tuple_image(depth: usize) -> Vec<u8> {
    let mut image = Vec::with_capacity(1 + depth * 2 + 1);
    image.push(VERSION_MAGIC);
    for _ in 0..depth {
        image.push(SMALL_TUPLE_EXT);
        image.push(1);
    }
    image.push(NIL_EXT);
    image
}

/// `[VERSION_MAGIC][LIST_EXT][len:4][NIL; len][NIL]`: a **wide** proper list of `len`
/// empty lists (a deep list spine is not built: `LIST_EXT` elements are decoded in a
/// loop, so this never nests).
fn wide_list_image(len: usize) -> Vec<u8> {
    let mut image = Vec::with_capacity(7 + len);
    image.push(VERSION_MAGIC);
    image.push(LIST_EXT);
    image.extend_from_slice(&(len as u32).to_be_bytes());
    image.resize(image.len() + len, NIL_EXT);
    image.push(NIL_EXT);
    image
}

/// `[VERSION_MAGIC][LARGE_TUPLE_EXT][arity:4][NIL; arity]`: a **wide** tuple of
/// `arity` empty lists (a tuple's elements are decoded in a loop, so this never
/// nests either).
fn wide_tuple_image(arity: usize) -> Vec<u8> {
    let mut image = Vec::with_capacity(5 + arity);
    image.push(VERSION_MAGIC);
    image.push(LARGE_TUPLE_EXT);
    image.extend_from_slice(&(arity as u32).to_be_bytes());
    image.resize(image.len() + arity, NIL_EXT);
    image
}

/// (a) A depth comfortably under `MAX_DECODE_DEPTH` (1000 < 1500) decodes normally.
///
/// Run on the small stack too, so this doubles as a check that a legitimate
/// (well-under-the-limit) nesting comfortably fits.
#[test]
fn nesting_under_the_limit_decodes() {
    let image = nested_tuple_image(1000);
    assert_eq!(
        decode_on_stack(SMALL_STACK_BYTES, image, 1 << 16),
        Verdict::Decoded("tuple[1]".to_string()),
        "a 1000-deep nested tuple is under the bound and must decode normally",
    );
}

/// (b) A depth over `MAX_DECODE_DEPTH` is the typed `SystemLimit`, never an abort.
///
/// The 8000-deep image is fed to a 2 MiB thread: an unguarded decoder would overflow
/// that stack (measured cliff ≈ 6250) and abort the process, so this test fails
/// loudly — not quietly — if the guard is ever removed.
#[test]
fn nesting_over_the_limit_is_a_typed_error() {
    let image = nested_tuple_image(8000);
    assert_eq!(
        decode_on_stack(SMALL_STACK_BYTES, image, 1 << 16),
        Verdict::SystemLimit,
        "an 8000-deep nested tuple must be refused with BeamError::SystemLimit",
    );
}

/// (c) The bound is on **depth**, never on length: a 1,000,000-element list and a
/// 100,000-element tuple both decode, because their elements are read iteratively.
#[test]
fn a_huge_list_and_a_huge_tuple_still_decode() {
    // 1,000,000 cons cells = 2,000,000 words; 3M words is comfortable headroom.
    let list = wide_list_image(1_000_000);
    assert_eq!(
        decode_on_stack(SMALL_STACK_BYTES, list, 3 << 20),
        Verdict::Decoded("list".to_string()),
        "a 1,000,000-element list is wide, not deep, and must decode",
    );

    let tuple = wide_tuple_image(100_000);
    assert_eq!(
        decode_on_stack(SMALL_STACK_BYTES, tuple, 1 << 18),
        Verdict::Decoded("tuple[100000]".to_string()),
        "a 100,000-element tuple is wide, not deep, and must decode",
    );
}

/// (d) The exact hostile image from the bug report — ~55 KB of nested 1-tuples that
/// aborted the process with a stack overflow — is now a typed error.
#[test]
fn the_reported_hostile_image_is_a_typed_error() {
    // `<<131,(binary:copy(<<104,1>>,27500))/binary,106>>`.
    let image = nested_tuple_image(27_500);
    assert_eq!(
        decode_on_stack(SMALL_STACK_BYTES, image, 1 << 16),
        Verdict::SystemLimit,
        "the reported hostile image must be refused with BeamError::SystemLimit, not abort",
    );
}
