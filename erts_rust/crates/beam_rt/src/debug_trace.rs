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

//! The runtime's **env-gated debug trace**: a diagnostic channel for the boot path and the
//! scheduler pass.
//!
//! Set `BEAM_RT_BOOT_TRACE` to any value to turn it on; every line goes to **stderr**,
//! prefixed `[rt-trace]`. With the variable unset the module costs one `env::var_os` per
//! *call site that asks* — never an unconditional write — so a normal run is silent.
//!
//! It exists because the boot sequence's failure modes (a process that is never enqueued, a
//! scheduler pass that drains with work outstanding) are not observable from the outside:
//! the trace records what the scheduler and the boot drives actually did, rather than what
//! they were meant to do. It is a **diagnostic**, not a contract — no behaviour depends on
//! it.

use std::fmt;
use std::sync::OnceLock;

/// Whether the debug trace is enabled (`BEAM_RT_BOOT_TRACE` present).
///
/// Resolved **once** per OS process: a trace switch is read from the environment at most
/// once, so a hot path pays one relaxed load after the first call.
pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BEAM_RT_BOOT_TRACE").is_some())
}

/// Writes one trace line to stderr when [`enabled`]; a no-op otherwise.
pub(crate) fn trace(args: fmt::Arguments<'_>) {
    if enabled() {
        eprintln!("[rt-trace] {args}");
    }
}