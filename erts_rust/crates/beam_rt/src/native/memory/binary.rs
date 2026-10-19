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
//! binary the process owns, in the order the payloads were allocated, and `[]` when it owns
//! none. `Ptr` is the payload's own pointer as an integer, `Size` its byte length and `Refc`
//! its total reference count — exactly the three observables
//! [`RefcBinary`](beam_alloc::RefcBinary) exposes (`data_ptr`/`len`/`refcount`).
//!
//! The roster is read by the parent module's [`snapshot`](super::RuntimeContext::snapshot),
//! which extracts the three figures out (the process table borrow must end before a reply is
//! built on the engine heap). That roster is the **deduped union** of the target's **engine
//! reservoir** — [`Machine::refc_binaries`], the one holder of the payloads the process's own
//! compiled bitstring code built — and the process's own off-heap table
//! ([`Process::off_heap_binaries`], which stays the path for a host that registers a payload on
//! the PCB directly; nothing in production does).
//!
//! The reservoir half has **two** routes, one per query shape:
//!
//! * **another** pid — [`RunnerState::reservoir_at`](crate::runner::RunnerState::reservoir_at),
//!   the engine the pass keeps in its state for every process it is not executing;
//! * the **executing** pid — the in-machine BIF door
//!   ([`in_machine_off_heap_binaries`](super::RuntimeContext::in_machine_off_heap_binaries)),
//!   because a scheduler pass takes that one engine *out* of the runner state for the whole
//!   slice. So `erlang:process_info(self(), binary)` lists the payload the process built
//!   earlier in the very slice that asks, which is the oracle's answer; the door reads the
//!   roster only when the query names `binary`, so no other item pays for it.
//!
//! A process this runtime holds no machine for — the `-eval` root process, a foreign pid —
//! answers `[]` rather than a fabricated roster. Only **compiled bitstring code** feeds the
//! reservoir: a binary a `beam_bifs` BIF builds (`binary:copy/2`, `base64:encode/1`, …) is an
//! on-heap block on this runtime, so it is not listed here (`M1_GAPS.md`
//! §`process_info/1,2`). This module only materializes the value, so it owns no state and
//! touches no runtime table.
//!
//! Each of the three integers is built with [`terms::make_integer`], so a payload pointer
//! outside the 60-bit small range materializes as an **exact bignum** rather than being
//! truncated to a wrong-sized integer.
//!
//! [`Machine::refc_binaries`]: beam_engine::Machine::refc_binaries
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
