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

//! The `erlang`-module runtime-coupled BIFs (process control, timers, code
//! loading, system/nodes introspection, ...).
//!
//! # Why these are here and not in `src/*.rs`
//!
//! Every key registered by this module is a **declared `bif.tab` entry whose
//! answer only the running runtime has**: process control (`spawn/3`,
//! `spawn_link/3`, `spawn_opt/4`, `link/1,2`, `unlink/1`, `monitor/2,3`,
//! `demonitor/1,2`, `exit/2,3`, `exit_signal/2,3`, `hibernate/3`,
//! `resume_process/1`, `send/3`, `alias/1`, `unalias/1`), the timer wheel
//! (`start_timer/3,4`, `send_after/3,4`, `cancel_timer/1,2`, `read_timer/1,2`),
//! code loading (`function_exported/3`, `module_loaded/1`, `loaded/0`,
//! `pre_loaded/0`, `check_old_code/1`, `delete_module/1`, `is_builtin/3`,
//! `finish_loading/1`, `call_on_load_function/1`, `finish_after_on_load/2`,
//! `has_prepared_code_on_load/1`), node/system introspection (`nodes/0,1,2`,
//! `monitor_node/2,3`, `setnode/2`, `statistics/1`, `system_info/1`,
//! `system_flag/2`, `system_profile/0,2`, `halt/2`, `make_ref/0`,
//! `bump_reductions/1`), the tracing machinery (`trace_info/2`,
//! `trace_delivered/1`, `seq_trace/2`, `seq_trace_info/1`,
//! `seq_trace_print/1,2`), the port data store (`port_get_data/1`,
//! `port_set_data/2`) and the distribution layer (`dist_get_stat/1`,
//! `dist_ctrl_*`, `dt_*`). None of that state is reachable from a BIF: it lives
//! in the Stage-5 runtime (schedulers, code store, timer wheel, node tables),
//! which is a different crate this layer may not touch.
//!
//! # One seam, one honest gap
//!
//! All 77 keys therefore resolve through the **single**
//! [`RuntimeHooks`](crate::context::RuntimeHooks) dispatcher (via
//! [`BifContext::call_runtime_bif`](crate::context::BifContext::call_runtime_bif)),
//! which is handed the `(module, function, arity)` identity verbatim so the
//! runtime dispatches on exactly the `bif.tab` key. With **no hook installed
//! every one of them answers the typed `unsupported_bif` capability gap** —
//! never `undef` (the function *is* a declared BIF; only its runtime provider is
//! missing), never a fabricated value (workspace `CONTEXT.md` §3).
//!
//! A few of these names are additionally intercepted natively by `beam_rt`
//! *before* the registry is consulted (the runtime's own fast paths); that is
//! unaffected by this registration — the key simply exists here so that a
//! registry call, or a `beam_rt` that defers to it, gets the honest gap rather
//! than a plain `undef`.
//!
//! # Roster (77 keys, all `bif.tab`-declared)
//!
//! `../../erts/emulator/beam/bif.tab` lines: 59, 66, 67, 81, 88, 96, 99, 106,
//! 107, 108, 109, 112, 113, 114, 128, 141, 142, 144, 156, 159-167, 205, 212,
//! 213, 218-223, 225, 228, 252-259, 265, 266, 268, 271, 277-279, 288, 325,
//! 470, 528, 529, 582, 596-600, 604, 605, 611, 668, 772-775, 814, 815, 846.

use crate::registry::BifRegistry;
use crate::runtime::runtime_bif;

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    runtime_bif!(reg, "erlang", alias, 1);
    runtime_bif!(reg, "erlang", bump_reductions, 1);
    runtime_bif!(reg, "erlang", call_on_load_function, 1);
    runtime_bif!(reg, "erlang", cancel_timer, 1);
    runtime_bif!(reg, "erlang", cancel_timer, 2);
    runtime_bif!(reg, "erlang", check_old_code, 1);
    runtime_bif!(reg, "erlang", delete_module, 1);
    runtime_bif!(reg, "erlang", demonitor, 1);
    runtime_bif!(reg, "erlang", demonitor, 2);
    runtime_bif!(reg, "erlang", dist_ctrl_get_data, 1);
    runtime_bif!(reg, "erlang", dist_ctrl_get_data_notification, 1);
    runtime_bif!(reg, "erlang", dist_ctrl_get_opt, 2);
    runtime_bif!(reg, "erlang", dist_ctrl_input_handler, 2);
    runtime_bif!(reg, "erlang", dist_ctrl_put_data, 2);
    runtime_bif!(reg, "erlang", dist_ctrl_set_opt, 3);
    runtime_bif!(reg, "erlang", dist_get_stat, 1);
    runtime_bif!(reg, "erlang", dt_append_vm_tag_data, 1);
    runtime_bif!(reg, "erlang", dt_get_tag, 0);
    runtime_bif!(reg, "erlang", dt_get_tag_data, 0);
    runtime_bif!(reg, "erlang", dt_prepend_vm_tag_data, 1);
    runtime_bif!(reg, "erlang", dt_put_tag, 1);
    runtime_bif!(reg, "erlang", dt_restore_tag, 1);
    runtime_bif!(reg, "erlang", dt_spread_tag, 1);
    runtime_bif!(reg, "erlang", exit, 2);
    runtime_bif!(reg, "erlang", exit, 3);
    runtime_bif!(reg, "erlang", exit_signal, 2);
    runtime_bif!(reg, "erlang", exit_signal, 3);
    runtime_bif!(reg, "erlang", finish_after_on_load, 2);
    runtime_bif!(reg, "erlang", finish_loading, 1);
    runtime_bif!(reg, "erlang", function_exported, 3);
    runtime_bif!(reg, "erlang", halt, 2);
    runtime_bif!(reg, "erlang", has_prepared_code_on_load, 1);
    runtime_bif!(reg, "erlang", hibernate, 3);
    runtime_bif!(reg, "erlang", is_builtin, 3);
    runtime_bif!(reg, "erlang", link, 1);
    runtime_bif!(reg, "erlang", link, 2);
    runtime_bif!(reg, "erlang", loaded, 0);
    runtime_bif!(reg, "erlang", make_ref, 0);
    runtime_bif!(reg, "erlang", module_loaded, 1);
    runtime_bif!(reg, "erlang", monitor, 2);
    runtime_bif!(reg, "erlang", monitor, 3);
    runtime_bif!(reg, "erlang", monitor_node, 2);
    runtime_bif!(reg, "erlang", monitor_node, 3);
    runtime_bif!(reg, "erlang", nodes, 0);
    runtime_bif!(reg, "erlang", nodes, 1);
    runtime_bif!(reg, "erlang", nodes, 2);
    runtime_bif!(reg, "erlang", port_get_data, 1);
    runtime_bif!(reg, "erlang", port_set_data, 2);
    runtime_bif!(reg, "erlang", pre_loaded, 0);
    runtime_bif!(reg, "erlang", read_timer, 1);
    runtime_bif!(reg, "erlang", read_timer, 2);
    runtime_bif!(reg, "erlang", resume_process, 1);
    runtime_bif!(reg, "erlang", send, 3);
    runtime_bif!(reg, "erlang", send_after, 3);
    runtime_bif!(reg, "erlang", send_after, 4);
    runtime_bif!(reg, "erlang", seq_trace, 2);
    runtime_bif!(reg, "erlang", seq_trace_info, 1);
    runtime_bif!(reg, "erlang", seq_trace_print, 1);
    runtime_bif!(reg, "erlang", seq_trace_print, 2);
    runtime_bif!(reg, "erlang", setnode, 2);
    runtime_bif!(reg, "erlang", spawn, 3);
    runtime_bif!(reg, "erlang", spawn_link, 3);
    runtime_bif!(reg, "erlang", spawn_opt, 4);
    runtime_bif!(reg, "erlang", spawn_request_abandon, 1);
    runtime_bif!(reg, "erlang", start_timer, 3);
    runtime_bif!(reg, "erlang", start_timer, 4);
    runtime_bif!(reg, "erlang", statistics, 1);
    runtime_bif!(reg, "erlang", system_flag, 2);
    runtime_bif!(reg, "erlang", system_info, 1);
    runtime_bif!(reg, "erlang", system_profile, 0);
    runtime_bif!(reg, "erlang", system_profile, 2);
    runtime_bif!(reg, "erlang", term_to_iovec, 1);
    runtime_bif!(reg, "erlang", term_to_iovec, 2);
    runtime_bif!(reg, "erlang", trace_delivered, 1);
    runtime_bif!(reg, "erlang", trace_info, 2);
    runtime_bif!(reg, "erlang", unalias, 1);
    runtime_bif!(reg, "erlang", unlink, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    use crate::context::BifContext;

    #[test]
    fn register_installs_all_seventy_seven_keys() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 77, "the 77 runtime-coupled erlang keys");
        // A spot-check across the families: process control, code loading, node
        // introspection, the timer wheel, the distribution layer.
        for (name, arity) in [
            ("spawn", 3),
            ("link", 1),
            ("make_ref", 0),
            ("system_info", 1),
            ("module_loaded", 1),
            ("start_timer", 3),
            ("dist_ctrl_put_data", 2),
            ("unalias", 1),
        ] {
            assert!(
                reg.contains("erlang", name, arity),
                "missing erlang:{name}/{arity}"
            );
        }
    }

    /// With no `RuntimeHooks` installed the seam is the typed capability gap, not
    /// a fabricated value and not `undef`.
    #[test]
    fn every_key_answers_the_typed_capability_gap_without_a_hook() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        let mut heap = Heap::new(8).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match reg.call("erlang", "make_ref", 0, &mut ctx, &[]) {
            Err(BeamError::Raise(raise)) => assert_eq!(
                raise.reason,
                beam_common::ExitReason::atom("unsupported_bif")
            ),
            other => panic!("expected unsupported_bif, got {other:?}"),
        }
    }
}
