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

//! The **`+JPperf` code-map feed** (contract A.8).
//!
//! The perf map is the diagnostic that names the native code a scheduled process can run,
//! so it may only ever hold ranges the baseline backend actually mapped. This is the one
//! hook that fills it, called once per module-export first entry.

use beam_loader::Module;

use crate::runtime::Runtime;

/// Snapshots the baseline JIT's **real** code ranges of `module` into the `+JPperf` map
/// (contract A.8).
///
/// The perf map is the diagnostic that names the native code a scheduled process can run, so
/// it may only ever hold ranges the baseline backend actually mapped
/// ([`baseline_code_map`](beam_engine::jit::cache::baseline_code_map) -- which compiles the
/// module on first sight, so the ranges are its own artifact's) -- never a synthesised one.
/// Diagnostic-only by contract: a disabled flag returns without compiling anything, and a
/// compile or write failure is *recorded* in the map's own diagnostic slot rather than
/// propagated, exactly as the oracle logs it and carries on (a perf file must never fail a run).
pub(super) fn record_baseline_code(runtime: &mut Runtime, module: &Module) {
    if !runtime.perf_map().config().map_enabled() {
        return;
    }
    match beam_engine::jit::cache::baseline_code_map(module) {
        Ok(regions) => {
            for region in regions {
                if let Err(error) =
                    runtime
                        .perf_map_mut()
                        .record(region.start, region.size, &region.name)
                {
                    runtime
                        .perf_map_mut()
                        .note_diagnostic(format!("perf: {error}"));
                }
            }
        }
        Err(error) => runtime
            .perf_map_mut()
            .note_diagnostic(format!("perf: no code map for this module: {error}")),
    }
}
