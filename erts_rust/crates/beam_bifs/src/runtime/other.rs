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
//! (`erl_debugger`), the code-coverage bookkeeping (`code`), the `ets` scheduler fan-in
//! (`internal_request_all/0`), the
//! tracing-state cleanup and literal-area copy protocols, and the `prim_file` NFC
//! normalizer, `os` signal handling, `net_kernel` distribution flag and
//! `error_logger` category map. Each is registered through the single
//! [`RuntimeHooks`](crate::context::RuntimeHooks) seam, so a call answers the typed
//! `unsupported_bif` capability gap until the runtime installs a hook -- never
//! `undef`, never a fabricated value. Two groups are the exception: the 8 `re` keys are
//! served by the real in-crate engine (`crate::re`) and the whole 6-key `erl_ddll`
//! surface by the loaded-driver registry + `beam_abi`'s driver loader
//! ([`crate::runtime::ddll`]), not the seam.

use crate::registry::BifRegistry;

/// The `(module, function, arity)` keys this module registers **for real** (not
/// through the `RuntimeHooks` seam): the 8 [`crate::re`] engine keys registered by
/// `crate::re::register` plus the 6 `erl_ddll` keys registered by
/// [`crate::runtime::ddll::live::register`], [`crate::runtime::ddll::info::register`] and
/// [`crate::runtime::ddll::monitor::register`].
///
/// (`erl_ddll:format_error_int/1` is the seventh real `erl_ddll` key, registered by
/// `crate::runtime::ddll::register` in the containing family, not here.)
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("erl_ddll", "demonitor", 1),
    ("erl_ddll", "info", 2),
    ("erl_ddll", "loaded_drivers", 0),
    ("erl_ddll", "monitor", 2),
    ("erl_ddll", "try_load", 3),
    ("erl_ddll", "try_unload", 2),
    ("re", "compile", 1),
    ("re", "compile", 2),
    ("re", "import", 1),
    ("re", "inspect", 2),
    ("re", "internal_run", 4),
    ("re", "run", 2),
    ("re", "run", 3),
    ("re", "version", 0),
];

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    // `ets` -- the scheduler fan-in `internal_request_all/0`; every other `ets:*`
    // key the C interior used to stand in for now has a real implementation in
    // `src/ets/` (`ets:slot/2` included).
    crate::runtime::runtime_bif!(reg, "ets", internal_request_all, 0);

    // `re` -- the 8 regular-expression keys are now real implementations served by
    // the in-crate engine (`crate::re`), not `RuntimeHooks` seam keys.
    crate::re::register(reg);

    // `erl_ddll` -- the dynamically linked driver loader. The whole family is real: the
    // error table `format_error_int/1` in `src/runtime/ddll.rs`; the loading keys
    // (`loaded_drivers/0`, `try_load/3`, `try_unload/2`) by the loaded-driver registry +
    // `beam_abi`'s driver loader in `src/runtime/ddll/live.rs`; the state report `info/2` in
    // `src/runtime/ddll/info.rs`; and the event registrations `monitor/2`/`demonitor/1` in
    // `src/runtime/ddll/monitor.rs`. No `erl_ddll` key is a `RuntimeHooks` seam key.
    crate::runtime::ddll::live::register(reg);
    crate::runtime::ddll::info::register(reg);
    crate::runtime::ddll::monitor::register(reg);

    // `code` -- the module code-coverage bookkeeping.
    crate::runtime::runtime_bif!(reg, "code", coverage_support, 0);
    crate::runtime::runtime_bif!(reg, "code", get_coverage, 2);
    crate::runtime::runtime_bif!(reg, "code", get_coverage_mode, 0);
    crate::runtime::runtime_bif!(reg, "code", get_coverage_mode, 1);
    crate::runtime::runtime_bif!(reg, "code", get_debug_info, 1);
    crate::runtime::runtime_bif!(reg, "code", reset_coverage, 1);
    crate::runtime::runtime_bif!(reg, "code", set_coverage_mode, 1);

    // `erts_trace_cleaner` -- the trace-state cleanup entry points the *collector
    // process itself* would serve; `send_trace_clean_signal/1` is a real `notsup` stub
    // in `src/runtime/erts_debug.rs`.
    crate::runtime::runtime_bif!(reg, "erts_trace_cleaner", check, 0);

    // `erts_literal_area_collector` -- literal-area copy/switch protocol;
    // `send_copy_request/3` is a real `notsup` stub in `src/runtime/erts_debug.rs`.
    crate::runtime::runtime_bif!(reg, "erts_literal_area_collector", release_area_switch, 0);

    // `prim_file` -- the filename normalizer (needs the C NFC tables).
    crate::runtime::runtime_bif!(reg, "prim_file", internal_normalize_utf8, 1);

    // `os` -- Unix signal handling.
    crate::runtime::runtime_bif!(reg, "os", set_signal, 2);

    // `net_kernel` -- the distribution unicode-IO flag.
    crate::runtime::runtime_bif!(reg, "net_kernel", dflag_unicode_io, 1);

    // `error_logger` -- the legacy warning-category map.
    crate::runtime::runtime_bif!(reg, "error_logger", warning_map, 0);

    // The 14 keys this family owns for real (see `REAL_BIFS`) are registered by `crate::re::register`
    // and the three `crate::runtime::ddll` submodule `register`s above; assert they really landed so
    // the roster cannot drift from what was registered.
    debug_assert!(
        REAL_BIFS
            .iter()
            .all(|(module, function, arity)| reg.contains(module, function, *arity)),
        "REAL_BIFS lists a key that this module does not register"
    );
    // The combined literal must cover every source roster (`crate::re`'s 8 and the three
    // `crate::runtime::ddll` submodules' 6), so a key added to any of them cannot go missing here.
    debug_assert!(
        crate::re::REAL_BIFS
            .iter()
            .chain(crate::runtime::ddll::live::REAL_BIFS)
            .chain(crate::runtime::ddll::info::REAL_BIFS)
            .chain(crate::runtime::ddll::monitor::REAL_BIFS)
            .all(|(module, function, arity)| REAL_BIFS.contains(&(*module, *function, *arity))),
        "REAL_BIFS must cover the `re` and every `erl_ddll` roster"
    );
}

#[cfg(test)]
mod tests {
    use crate::context::BifContext;
    use crate::registry::BifRegistry;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, Eterm, NIL};

    /// Every `RuntimeHooks`-seam key registered here is reachable and, with no
    /// [`RuntimeHooks`] hook installed, answers the typed capability gap rather than a
    /// fabricated value. The `re` and all six `erl_ddll` keys are **not** seam keys, though: they are
    /// real implementations (`crate::re`, `crate::runtime::ddll`) and are asserted to *run*
    /// (`re:run/3` returns the engine's `{match,[{2,1}]}`), not merely to be reachable.
    ///
    /// [`RuntimeHooks`]: crate::context::RuntimeHooks
    #[test]
    fn runtime_coupled_keys_answer_the_typed_gap() {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert!(reg.contains("re", "run", 3));
        // `ets:internal_request_all/0` is this file's only remaining `ets` seam key
        // (`ets:slot/2` now has a real implementation in `src/ets/`).
        assert!(reg.contains("ets", "internal_request_all", 0));
        // `os:set_signal/2`'s home is *this* file (`src/runtime/other.rs`), not
        // `src/os_bif.rs`; it is a seam key answering the typed gap with no provider
        // (see the module `CONTEXT.md`).
        assert!(reg.contains("os", "set_signal", 2));

        // `re` is the real in-crate engine: `re:run/3` is a working BIF, so its
        // result — never a typed gap — is asserted. The atoms the engine mints
        // (`match` here) must be seeded, since no `AtomInterner` is installed. The
        // subject/pattern are built with the frozen writers before the context
        // borrows the heap (`list_to_binary/1` is not registered by `super::register`
        // — it lives in `binconv.rs`).
        let mut heap = Heap::new(1024).expect("test heap");
        let subject = {
            let mut tail = NIL;
            for &b in b"xabcy".iter().rev() {
                tail = heap
                    .cons(make_small(i64::from(b)), tail)
                    .expect("the subject fits");
            }
            tail
        };
        let pattern = crate::bits::make_binary(&mut heap, b"b").expect("the pattern fits");
        let mut atoms = AtomTable::new();
        atoms.seed(["match", "nomatch", "ok", "true", "false"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let out = reg
            .call("re", "run", 3, &mut ctx, &[subject, pattern, NIL])
            .expect("re:run matches");
        assert_eq!(
            crate::print::format_term(&mut ctx, out, true).expect("the result is printable"),
            "{match,[{2,1}]}",
            "re:run/3 of \"xabcy\" / \"b\" must return the oracle's match result"
        );

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
            "os:set_signal/2 must be in the 172-key RuntimeHooks roster"
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

    /// The keys this family serves for real are exactly [`super::REAL_BIFS`]: the 8 `re` engine keys
    /// ([`crate::re::REAL_BIFS`]) plus the 6 `erl_ddll` keys
    /// ([`crate::runtime::ddll::live::REAL_BIFS`] ∪ [`crate::runtime::ddll::info::REAL_BIFS`] ∪
    /// [`crate::runtime::ddll::monitor::REAL_BIFS`]). Ties the source rosters to this file's
    /// combined literal so any of them drifting is caught here.
    #[test]
    fn the_real_roster_is_the_re_and_ddll_keys() {
        let real = super::REAL_BIFS;
        assert_eq!(real.len(), 14);
        for key in crate::re::REAL_BIFS {
            assert!(real.contains(key), "missing re key {key:?}");
        }
        for key in crate::runtime::ddll::live::REAL_BIFS
            .iter()
            .chain(crate::runtime::ddll::info::REAL_BIFS)
            .chain(crate::runtime::ddll::monitor::REAL_BIFS)
        {
            assert!(real.contains(key), "missing ddll key {key:?}");
        }
        let mut sorted = real.to_vec();
        sorted.sort_unstable();
        assert!(sorted.iter().eq(real.iter()), "REAL_BIFS is not sorted");
    }
}
