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

//! The `erts_internal` runtime-coupled BIFs (the port and process-control
//! internals, the tracing machinery, code loading, ...).
//!
//! # Why these are here and not in `src/*.rs`
//!
//! The keys in this family are the `erts_internal:*` entries from
//! `../../erts/emulator/beam/bif.tab` that this crate does not implement
//! natively: they all need the **running** runtime, which lives in other crates
//! of the workspace and is unreachable from a BIF. The port machinery (`port_call/3`,
//! `port_command/3`, `port_control/3`, `port_close/1`, `port_connect/2`,
//! `port_info/1,2`, `open_port/2`), process control (`suspend_process/2`,
//! `process_flag/3`, `process_display/2`, `processes_next/1`, `system_check/1`,
//! `scheduler_wall_time/1`, `is_process_alive/2`, `is_system_process/1`,
//! `spawn_request/4`, `spawn_system_process/3`, `dist_spawn_request/4`,
//! `request_system_task/3,4`, `notify_breakpoint_hit/3`, `group_leader/2,3`),
//! garbage collection and code loading (`garbage_collect/1`,
//! `check_process_code/1`, `check_dirty_process_code/2`,
//! `is_process_executing_dirty/1`, `dirty_process_handle_signals/1`,
//! `prepare_loading/2`, `beamfile_chunk/2`, `beamfile_module_md5/1`,
//! `purge_module/2`), the ETS C interior (`ets_lookup_binary_info/2`,
//! `ets_raw_first/1`, `ets_raw_next/2`, `ets_super_user/1`), the tracing
//! machinery (`trace/3,4`, `trace_pattern/3,4`, `trace_info/3`,
//! `trace_session_create/3`, `trace_session_destroy/1`, `system_monitor/1,3`),
//! the distribution internals (`new_connection/1`, `create_dist_channel/3`,
//! `abort_pending_connection/2`, `get_dflags/0`, `get_creation/0`), the allocator
//! introspection (`gather_alloc_histograms/1`, `gather_carrier_info/1`),
//! `erase_persistent_terms/0`, `no_aux_work_threads/0` and `term_to_string/2`.
//!
//! # One seam, one honest gap
//!
//! All 56 keys resolve through the **single**
//! [`RuntimeHooks`](crate::context::RuntimeHooks) dispatcher (via
//! [`BifContext::call_runtime_bif`](crate::context::BifContext::call_runtime_bif)),
//! which is handed the `(module, function, arity)` identity verbatim so the
//! runtime dispatches on exactly the `bif.tab` key. With **no hook installed every
//! one of them answers the typed `unsupported_bif` capability gap** — never `undef`
//! (the function *is* a declared BIF; only its runtime provider is missing),
//! never a fabricated value (workspace `CONTEXT.md` §3).
//!
//! A few of these names are additionally intercepted natively by `beam_rt`
//! *before* the registry is consulted (the runtime's own fast paths); that is
//! unaffected by this registration — the key simply exists here so that a
//! registry call, or a `beam_rt` that defers to it, gets the honest gap rather
//! than a plain `undef`.
//!
//! # Roster (56 keys, all `bif.tab`-declared)
//!
//! `../../erts/emulator/beam/bif.tab` lines: 74, 79, 80, 124, 130, 170-176,
//! 178-180, 189, 191, 193, 195, 197, 199, 201-203, 216, 217, 224, 226, 282,
//! 661-663, 692, 693, 696, 697, 712, 733, 747-749, 755, 762,
//! 768-770, 787, 799-803, 810-812, 825.

use crate::registry::BifRegistry;
use crate::runtime::runtime_bif;

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    runtime_bif!(reg, "erts_internal", abort_pending_connection, 2);
    runtime_bif!(reg, "erts_internal", beamfile_chunk, 2);
    runtime_bif!(reg, "erts_internal", beamfile_module_md5, 1);
    runtime_bif!(reg, "erts_internal", check_dirty_process_code, 2);
    runtime_bif!(reg, "erts_internal", check_process_code, 1);
    runtime_bif!(reg, "erts_internal", create_dist_channel, 3);
    runtime_bif!(reg, "erts_internal", dirty_process_handle_signals, 1);
    runtime_bif!(reg, "erts_internal", dist_spawn_request, 4);
    runtime_bif!(reg, "erts_internal", erase_persistent_terms, 0);
    runtime_bif!(reg, "erts_internal", ets_lookup_binary_info, 2);
    runtime_bif!(reg, "erts_internal", ets_raw_first, 1);
    runtime_bif!(reg, "erts_internal", ets_raw_next, 2);
    runtime_bif!(reg, "erts_internal", ets_super_user, 1);
    runtime_bif!(reg, "erts_internal", garbage_collect, 1);
    runtime_bif!(reg, "erts_internal", gather_alloc_histograms, 1);
    runtime_bif!(reg, "erts_internal", gather_carrier_info, 1);
    runtime_bif!(reg, "erts_internal", get_creation, 0);
    runtime_bif!(reg, "erts_internal", get_dflags, 0);
    runtime_bif!(reg, "erts_internal", group_leader, 2);
    runtime_bif!(reg, "erts_internal", group_leader, 3);
    runtime_bif!(reg, "erts_internal", is_process_alive, 2);
    runtime_bif!(reg, "erts_internal", is_process_executing_dirty, 1);
    runtime_bif!(reg, "erts_internal", is_system_process, 1);
    runtime_bif!(reg, "erts_internal", new_connection, 1);
    runtime_bif!(reg, "erts_internal", no_aux_work_threads, 0);
    runtime_bif!(reg, "erts_internal", notify_breakpoint_hit, 3);
    runtime_bif!(reg, "erts_internal", open_port, 2);
    runtime_bif!(reg, "erts_internal", port_call, 3);
    runtime_bif!(reg, "erts_internal", port_close, 1);
    runtime_bif!(reg, "erts_internal", port_command, 3);
    runtime_bif!(reg, "erts_internal", port_connect, 2);
    runtime_bif!(reg, "erts_internal", port_control, 3);
    runtime_bif!(reg, "erts_internal", port_info, 1);
    runtime_bif!(reg, "erts_internal", port_info, 2);
    runtime_bif!(reg, "erts_internal", prepare_loading, 2);
    runtime_bif!(reg, "erts_internal", process_display, 2);
    runtime_bif!(reg, "erts_internal", processes_next, 1);
    runtime_bif!(reg, "erts_internal", process_flag, 3);
    runtime_bif!(reg, "erts_internal", purge_module, 2);
    runtime_bif!(reg, "erts_internal", request_system_task, 3);
    runtime_bif!(reg, "erts_internal", request_system_task, 4);
    runtime_bif!(reg, "erts_internal", scheduler_wall_time, 1);
    runtime_bif!(reg, "erts_internal", spawn_request, 4);
    runtime_bif!(reg, "erts_internal", spawn_system_process, 3);
    runtime_bif!(reg, "erts_internal", suspend_process, 2);
    runtime_bif!(reg, "erts_internal", system_check, 1);
    runtime_bif!(reg, "erts_internal", system_monitor, 1);
    runtime_bif!(reg, "erts_internal", system_monitor, 3);
    runtime_bif!(reg, "erts_internal", term_to_string, 2);
    runtime_bif!(reg, "erts_internal", trace, 3);
    runtime_bif!(reg, "erts_internal", trace, 4);
    runtime_bif!(reg, "erts_internal", trace_info, 3);
    runtime_bif!(reg, "erts_internal", trace_pattern, 3);
    runtime_bif!(reg, "erts_internal", trace_pattern, 4);
    runtime_bif!(reg, "erts_internal", trace_session_create, 3);
    runtime_bif!(reg, "erts_internal", trace_session_destroy, 1);
}

#[cfg(test)]
mod tests {
    use crate::context::BifContext;
    use crate::registry::BifRegistry;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    /// The family registers its keys and, with no `RuntimeHooks` installed, every
    /// one of them answers the typed `unsupported_bif` capability gap (never a
    /// plain `undef`, never a fabricated value).
    #[test]
    fn registers_seam_backed_keys_that_gap_honestly() {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert!(reg.contains("erts_internal", "garbage_collect", 1));

        let mut heap = Heap::new(8).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let arg = ctx.int(0).expect("small int");
        match reg.call("erts_internal", "garbage_collect", 1, &mut ctx, &[arg]) {
            Err(beam_common::BeamError::Raise(r)) => {
                assert_eq!(r.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected unsupported_bif, got {other:?}"),
        }
    }
}
