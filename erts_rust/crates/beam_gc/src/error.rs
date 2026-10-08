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

//! `pub(crate)` GC allocation-failure constructors — the crate adds no error type of its own.
//!
//! Every fallible GC path returns the workspace error type
//! ([`BeamError`](beam_common::BeamError)); this module only centralises the
//! construction of the one GC-specific failure the collector can raise.

use beam_common::{AllocFailure, AllocKind, BeamError};
use beam_term::prelude::Eterm;

/// Builds the heap-exhaustion error raised while copying a term.
///
/// Heap exhaustion is a **normal** condition, reported as
/// `BeamError::Alloc(beam_common::AllocFailure::new(beam_common::AllocKind::Heap, bytes))`
/// so the caller can collect and retry — never a panic and never a crate-local
/// error enum. `words` is the number of term words the copy wanted.
pub(crate) fn heap_exhausted(words: usize) -> BeamError {
    BeamError::Alloc(AllocFailure::new(
        AllocKind::Heap,
        words * core::mem::size_of::<Eterm>(),
    ))
}
