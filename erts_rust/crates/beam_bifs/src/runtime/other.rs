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

//! The remaining runtime-coupled families (`erl_debugger`, `re`, `erl_ddll`, `code`, ...).
//!
//! Every key here is a declared `bif.tab` entry whose implementation needs host
//! machinery this crate does not own: the interpreter-level debugger tools
//! (`erl_debugger`), PCRE (`re`), the live driver registry (`erl_ddll`), the
//! code-coverage bookkeeping (`code`), the `ets` scheduler fan-in
//! (`internal_request_all/0`) and C-interior slot walk (`slot/2`), the
//! tracing-state cleanup and literal-area copy protocols, and the `prim_file` NFC
//! normalizer, `os` signal handling, `net_kernel` distribution flag and
//! `error_logger` category map. Each is registered through the single
//! [`RuntimeHooks`](crate::context::RuntimeHooks) seam, so a call answers the typed
//! `unsupported_bif` capability gap until the runtime installs a hook -- never
//! `undef`, never a fabricated value.

use crate::registry::BifRegistry;

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    // `ets` -- the C-only `slot/2` interior walk and the scheduler fan-in
    // `internal_request_all/0`; every other `ets:*` key the C interior used to
    // stand in for now has a real implementation in `src/ets/`.
    crate::runtime::runtime_bif!(reg, "ets", internal_request_all, 0);
    crate::runtime::runtime_bif!(reg, "ets", slot, 2);

    // `erl_debugger` -- the interpreter-level debugger tools.
    crate::runtime::runtime_bif!(reg, "erl_debugger", breakpoints, 1);
    crate::runtime::runtime_bif!(reg, "erl_debugger", breakpoints, 3);
    crate::runtime::runtime_bif!(reg, "erl_debugger", breakpoint, 3);
    crate::runtime::runtime_bif!(reg, "erl_debugger", instrumentations, 0);
    crate::runtime::runtime_bif!(reg, "erl_debugger", peek_stack_frame_slot, 4);
    crate::runtime::runtime_bif!(reg, "erl_debugger", peek_xreg, 3);
    crate::runtime::runtime_bif!(reg, "erl_debugger", register, 1);
    crate::runtime::runtime_bif!(reg, "erl_debugger", stack_frames, 2);
    crate::runtime::runtime_bif!(reg, "erl_debugger", supported, 0);
    crate::runtime::runtime_bif!(reg, "erl_debugger", toggle_instrumentations, 1);
    crate::runtime::runtime_bif!(reg, "erl_debugger", unregister, 2);
    crate::runtime::runtime_bif!(reg, "erl_debugger", whereis, 0);
    crate::runtime::runtime_bif!(reg, "erl_debugger", xregs_count, 1);

    // `re` -- the PCRE-backed regular-expression engine.
    crate::runtime::runtime_bif!(reg, "re", compile, 1);
    crate::runtime::runtime_bif!(reg, "re", compile, 2);
    crate::runtime::runtime_bif!(reg, "re", import, 1);
    crate::runtime::runtime_bif!(reg, "re", inspect, 2);
    crate::runtime::runtime_bif!(reg, "re", internal_run, 4);
    crate::runtime::runtime_bif!(reg, "re", run, 2);
    crate::runtime::runtime_bif!(reg, "re", run, 3);
    crate::runtime::runtime_bif!(reg, "re", version, 0);

    // `erl_ddll` -- the dynamically linked driver loader. `format_error_int/1` is
    // the one self-contained key and is implemented for real in `src/runtime/ddll.rs`.
    crate::runtime::runtime_bif!(reg, "erl_ddll", demonitor, 1);
    crate::runtime::runtime_bif!(reg, "erl_ddll", info, 2);
    crate::runtime::runtime_bif!(reg, "erl_ddll", loaded_drivers, 0);
    crate::runtime::runtime_bif!(reg, "erl_ddll", monitor, 2);
    crate::runtime::runtime_bif!(reg, "erl_ddll", try_load, 3);
    crate::runtime::runtime_bif!(reg, "erl_ddll", try_unload, 2);

    // `code` -- the module code-coverage bookkeeping.
    crate::runtime::runtime_bif!(reg, "code", coverage_support, 0);
    crate::runtime::runtime_bif!(reg, "code", get_coverage, 2);
    crate::runtime::runtime_bif!(reg, "code", get_coverage_mode, 0);
    crate::runtime::runtime_bif!(reg, "code", get_coverage_mode, 1);
    crate::runtime::runtime_bif!(reg, "code", get_debug_info, 1);
    crate::runtime::runtime_bif!(reg, "code", reset_coverage, 1);
    crate::runtime::runtime_bif!(reg, "code", set_coverage_mode, 1);

    // `erts_trace_cleaner` -- the trace-state cleanup signal machinery.
    crate::runtime::runtime_bif!(reg, "erts_trace_cleaner", check, 0);
    crate::runtime::runtime_bif!(reg, "erts_trace_cleaner", send_trace_clean_signal, 1);

    // `erts_literal_area_collector` -- literal-area copy/switch protocol.
    crate::runtime::runtime_bif!(reg, "erts_literal_area_collector", release_area_switch, 0);
    crate::runtime::runtime_bif!(reg, "erts_literal_area_collector", send_copy_request, 3);

    // `prim_file` -- the filename normalizer (needs the C NFC tables).
    crate::runtime::runtime_bif!(reg, "prim_file", internal_normalize_utf8, 1);

    // `os` -- Unix signal handling.
    crate::runtime::runtime_bif!(reg, "os", set_signal, 2);

    // `net_kernel` -- the distribution unicode-IO flag.
    crate::runtime::runtime_bif!(reg, "net_kernel", dflag_unicode_io, 1);

    // `error_logger` -- the legacy warning-category map.
    crate::runtime::runtime_bif!(reg, "error_logger", warning_map, 0);
}

#[cfg(test)]
mod tests {
    use crate::context::BifContext;
    use crate::registry::BifRegistry;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{AtomTable, Eterm};

    /// Every key registered here is reachable and, with no [`RuntimeHooks`] hook
    /// installed, answers the typed capability gap rather than a fabricated value.
    ///
    /// [`RuntimeHooks`]: crate::context::RuntimeHooks
    #[test]
    fn runtime_coupled_keys_answer_the_typed_gap() {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert!(reg.contains("re", "run", 3));
        assert!(reg.contains("ets", "slot", 2));
        // `os:set_signal/2`'s home is *this* file (`src/runtime/other.rs`), not
        // `src/os_bif.rs`; it is a seam key answering the typed gap with no provider
        // (see the module `CONTEXT.md`).
        assert!(reg.contains("os", "set_signal", 2));

        let mut heap = Heap::new(8).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let args = vec![ctx.int(0).expect("small int"); 3];
        match reg.call("re", "run", 3, &mut ctx, &args) {
            Err(beam_common::BeamError::Raise(r)) => {
                assert_eq!(r.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected unsupported_bif, got {other:?}"),
        }

        let args = vec![ctx.int(0).expect("small int"); 2];
        match reg.call("os", "set_signal", 2, &mut ctx, &args) {
            Err(beam_common::BeamError::Raise(r)) => {
                assert_eq!(r.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected unsupported_bif, got {other:?}"),
        }
    }

    /// `os:set_signal/2` is a **seam key and nothing more** on this side of the seam.
    ///
    /// This node owns the key + the forward: `src/runtime/other.rs` registers it, the
    /// `RUNTIME_HOOKS_BIFS` roster (the one authoritative set, in `crate::hook_sets`)
    /// contains it, and the body only calls
    /// [`BifContext::call_runtime_bif`](crate::context::BifContext::call_runtime_bif).
    /// With no `RuntimeHooks` installed **every** argument shape — a plausible small-int
    /// pair *and* a pair of `[]`, which no signal name or disposition could ever be —
    /// answers the same typed `unsupported_bif`: the key interprets no argument itself
    /// (never `badarg`), and never fabricates the oracle's `ok`.
    ///
    /// The *provider* (the `("os", "set_signal", 2)` arm of `RuntimeHooks::call`, over
    /// `beam_io::signal::SignalFacility`) lives in `beam_rt`, not here — see the module
    /// `CONTEXT.md`.
    #[test]
    fn os_set_signal_is_a_roster_seam_key_forwarder() {
        assert!(
            crate::hook_sets::needs_runtime_hooks("os", "set_signal", 2),
            "os:set_signal/2 must be in the 187-key RuntimeHooks roster"
        );

        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert!(reg.contains("os", "set_signal", 2));

        let mut heap = Heap::new(16).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        for args in [
            vec![ctx.int(0).expect("small int"); 2],
            vec![Eterm::default(); 2],
        ] {
            match reg.call("os", "set_signal", 2, &mut ctx, &args) {
                Err(beam_common::BeamError::Raise(r)) => assert_eq!(
                    r.reason,
                    beam_common::ExitReason::atom("unsupported_bif"),
                    "the seam gap, never badarg and never a fabricated `ok`"
                ),
                other => panic!("expected unsupported_bif, got {other:?}"),
            }
        }
    }
}
