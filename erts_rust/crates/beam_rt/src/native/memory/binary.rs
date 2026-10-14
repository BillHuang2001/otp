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

//! The `binary` item of `erlang:process_info/2` — a process's off-heap refc binaries.
//!
//! The oracle answers `[{Ptr, Size, Refc}, …]`: one 3-tuple per off-heap reference-counted
//! binary the process owns, in the off-heap table's slot order, and `[]` when it owns none.
//! `Ptr` is the payload's own pointer as an integer, `Size` its byte length and `Refc` its
//! total reference count — exactly the three observables
//! [`RefcBinary`](beam_alloc::RefcBinary) exposes (`data_ptr`/`len`/`refcount`).
//!
//! The roster is read from [`Process::off_heap_binaries`] — the process's **live owned**
//! payloads, released slots skipped — by the parent module's
//! [`snapshot`](super::RuntimeContext::snapshot), which copies the three figures out (the
//! process table borrow must end before a reply is built on the engine heap). This module
//! only materializes the value, so it owns no state and touches no runtime table.
//!
//! Each of the three integers is built with [`terms::make_integer`], so a payload pointer
//! outside the 60-bit small range materializes as an **exact bignum** rather than being
//! truncated to a wrong-sized integer.
//!
//! [`Process::off_heap_binaries`]: beam_process::Process::off_heap_binaries

use beam_common::Result;
use beam_engine::terms;
use beam_term::Eterm;

use super::{RuntimeContext, Snapshot};

impl RuntimeContext<'_> {
    /// The `binary` item: the process's off-heap refc binaries as `[{Ptr, Size, Refc}, …]`.
    ///
    /// `snapshot.off_heap_binaries` is the already-extracted `(pointer, length, refcount)`
    /// roster; an empty roster yields the empty list `[]` — the oracle's answer for a
    /// process that holds no off-heap binary.
    pub(super) fn binary_value(&mut self, snapshot: &Snapshot) -> Result<Eterm> {
        let mut entries = Vec::with_capacity(snapshot.off_heap_binaries.len());
        for (ptr, size, refcount) in &snapshot.off_heap_binaries {
            let ptr = terms::make_integer(self, *ptr as i64)?;
            let size = terms::make_integer(self, *size as i64)?;
            let refcount = terms::make_integer(self, *refcount as i64)?;
            entries.push(terms::make_tuple(self, &[ptr, size, refcount])?);
        }
        terms::make_list(self, &entries)
    }
}
