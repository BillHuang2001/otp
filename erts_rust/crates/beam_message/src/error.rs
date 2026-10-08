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

//! `pub(crate)` copy-failure constructors plus the public message-heap-exhaustion
//! classification -- the crate adds no error type of its own.
//!
//! Every fallible path in this crate returns the workspace error type
//! ([`BeamError`](beam_common::BeamError)); this module centralises the construction of the two
//! failures the copy engine can raise, exactly as `beam_gc::error` does for the collector, and
//! exposes the predicate [`is_message_heap_exhaustion`] so a host runtime can classify them.
//! Heap exhaustion mid-copy is a **normal** condition (the caller may grow the destination heap
//! and retry), never a panic.
//!
//! # The message-heap-exhaustion contract
//!
//! [`copy_struct`](crate::copy_struct), [`copy_struct_shared`](crate::copy_struct_shared),
//! [`copy_struct_simple`](crate::copy_struct_simple), [`send`](crate::send) and
//! [`send_fragmented`](crate::send_fragmented) copy a term into a **destination heap** (a
//! receiver's process heap, a message buffer or the runtime's term arena). When that heap cannot
//! hold the copied term, the crate reports [`BeamError::Alloc`] with [`AllocKind::Message`] and
//! [`AllocFailure::requested`](beam_common::AllocFailure) equal to
//! `words * size_of::<Eterm>()`, where `words` is the flat term-word count the copy still wanted.
//!
//! This is a **soft, recoverable** signal, not a host-level out-of-memory: a host may
//!
//! * **collect and grow** the destination heap and retry the copy, or
//! * at its configured hard limit (the emulator's `+hmax` / `process_flag(max_heap_size, _)`)
//!   **raise `error:system_limit`** -- the class `error` and reason atom `system_limit` the real
//!   emulator uses for an exceeded hard runtime limit.
//!
//! `AllocKind::Message` is deliberately the kind used here: it is *already* the load-bearing
//! "the payload did not fit the receiver heap, retry through the heap-fragment model" signal a
//! host keys on (see `beam_rt`'s message delivery), so the retry path and this classification
//! remain one contract rather than two. Test for it with [`is_message_heap_exhaustion`] rather
//! than re-deriving the `AllocKind` match at each call site.

use beam_common::{AllocFailure, AllocKind, BeamError};
use beam_term::prelude::Eterm;

/// Returns `true` iff `error` is this crate's **message-heap exhaustion**.
///
/// True exactly when `error` is [`BeamError::Alloc`] with
/// [`AllocFailure::kind`](beam_common::AllocFailure) == [`AllocKind::Message`] -- the failure this
/// crate's copy boundary raises when the destination message/term heap could not hold the copied
/// term (see the module docs for the full contract). A host may collect/grow the destination heap
/// and retry, or raise `error:system_limit` at its configured hard limit.
///
/// Returns `false` for every other error, including a different allocation class
/// ([`AllocKind::Heap`]/[`AllocKind::LargeObject`]/[`AllocKind::Carrier`]/[`AllocKind::Other`]),
/// the copy engine's malformed-graph [`BeamError::BadArg`], and every non-[`Alloc`](BeamError::Alloc)
/// variant.
///
/// # Examples
///
/// ```
/// use beam_common::{AllocFailure, AllocKind, BeamError};
/// use beam_message::is_message_heap_exhaustion;
///
/// let exhaustion = BeamError::Alloc(AllocFailure::new(AllocKind::Message, 800));
/// assert!(is_message_heap_exhaustion(&exhaustion));
///
/// let heap = BeamError::Alloc(AllocFailure::new(AllocKind::Heap, 800));
/// assert!(!is_message_heap_exhaustion(&heap));
/// ```
pub fn is_message_heap_exhaustion(error: &BeamError) -> bool {
    matches!(error, BeamError::Alloc(failure) if failure.kind == AllocKind::Message)
}

/// Builds the message-heap-exhaustion error raised while copying a term.
///
/// Reported as `BeamError::Alloc(AllocFailure::new(AllocKind::Message, bytes))` so the caller
/// can collect/grow and retry -- never a panic and never a crate-local error enum. `words` is
/// the number of term words the copy still wanted.
pub(crate) fn heap_exhausted(words: usize) -> BeamError {
    BeamError::Alloc(AllocFailure::new(
        AllocKind::Message,
        words * core::mem::size_of::<Eterm>(),
    ))
}

/// Builds the error raised for a malformed/unsupported term layout or an out-of-range
/// off-heap slot (`badarg`).
pub(crate) fn bad_term() -> BeamError {
    BeamError::BadArg
}
