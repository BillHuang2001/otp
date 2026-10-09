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

//! `pub(crate)` constructors for the allocation-failure errors.
//!
//! This module adds **no** error type of its own: the binding contract (crate-layer
//! `CONTEXT.md` §3) is that `beam_common::{Result, BeamError}` is the one error
//! surface of every crate. These helpers exist only so the failure sites do not each
//! open-code an [`AllocFailure`], and they stay `pub(crate)`: dependents see the
//! resulting `BeamError` values, never the constructors.

use beam_common::{AllocFailure, AllocKind, BeamError};
use beam_term::prelude::Eterm;

/// Builds the `out_of_memory` failure for an allocation of `requested` bytes of
/// `kind`.
pub(crate) fn oom(kind: AllocKind, requested: usize) -> BeamError {
    BeamError::Alloc(AllocFailure::new(kind, requested))
}

/// A carrier could not be obtained from the OS (`mmap` failure or a
/// `max_footprint` cap). Classified as [`AllocKind::Carrier`].
pub(crate) fn carrier_failure(bytes: usize) -> BeamError {
    oom(AllocKind::Carrier, bytes)
}

/// An off-heap large object (a [`crate::binary::RefcBinary`] payload) could not be
/// allocated. Classified as [`AllocKind::LargeObject`].
///
/// The payload is an off-heap large object regardless of which allocator path served
/// it, so a failure is reported as such rather than as the underlying carrier/heap
/// class the caller cannot see.
pub(crate) fn large_object_failure(bytes: usize) -> BeamError {
    oom(AllocKind::LargeObject, bytes)
}

/// A per-process [`crate::heap::Heap`] ran out of room for `words` term words.
///
/// The word count is converted to bytes so the carried [`AllocFailure`] is uniform.
/// The conversion saturates: [`crate::heap::Heap::alloc_words`] is an `unsafe fn`
/// whose `n` the caller may pass arbitrarily, so `words * size_of::<Eterm>()` would
/// otherwise overflow — panicking in debug builds and wrapping to a bogus byte count
/// in release builds. Saturating keeps the call total: it always yields the typed
/// `AllocKind::Heap` failure, never a panic and never a wrong number.
/// The engine/GC layer treats this as *collect and retry*, **not** a fatal OOM (see
/// the crate `CONTEXT.md`, Design Decisions).
pub(crate) fn heap_exhausted(words: usize) -> BeamError {
    oom(
        AllocKind::Heap,
        words.saturating_mul(core::mem::size_of::<Eterm>()),
    )
}

/// An allocation requested an alignment the allocator cannot satisfy.
///
/// [`BeamError::BadArg`] carries no payload, so the offending alignment is only
/// observable through the debug log.
pub(crate) fn bad_align(align: usize) -> BeamError {
    beam_common::beam_debug!("beam_alloc: unsupported allocation alignment {align}");
    BeamError::BadArg
}
