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

//! Collection statistics: the [`GcStats`] counters accumulated across collections.

/// Accumulated garbage-collection counters for a process heap.
///
/// Cumulative counters (`minor_collections`, `major_collections`) span the whole
/// collector lifetime; the per-collection counters are reported for the most recent
/// collection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcStats {
    /// Number of term words copied into the to-space.
    pub words_copied: usize,
    /// Number of term words reclaimed (freed rather than copied).
    pub words_reclaimed: usize,
    /// Number of boxed objects and cons cells copied.
    pub objects_copied: usize,
    /// Number of minor (young-only) collections performed.
    pub minor_collections: u64,
    /// Number of major (fullsweep) collections performed.
    pub major_collections: u64,
}
