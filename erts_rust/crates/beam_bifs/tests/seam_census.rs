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
//! The **capability-gap census** of [`default_registry()`] — the drift-proof answer
//! to "which registered BIFs have no answer today, and why not".
//!
//! `tests/roster_audit.rs` pins *which* registered key reads which optional
//! [`BifContext`] seam; `tests/bif_tab.rs` pins the declared `bif.tab` surface. This
//! file pins the third and sharpest question: what a key actually **answers** when
//! **no** optional seam is installed — the typed `unsupported_bif` capability gap, a
//! closed in-crate answer, a documented default, or a documented constant/raise.
//!
//! The registry always registers; it never promises a servable answer. A key that
//! answers the typed gap is honest (workspace `CONTEXT.md` §3: never a wrong value),
//! but the *set* of such keys is the crate's real coverage figure — it must be
//! measured, not assumed, and it must not move silently. Every row below is one
//! `(module, function, arity)` with its class and its **pinned observed outcome**
//! (OTP-29.1.1-verified where a value is involved).
//!
//! # The four classes
//!
//! * **A — closed in-crate (4 rows).** Keys that once forwarded to a seam and now
//!   answer a real value with no host at all: `erts_debug:instructions/0`,
//!   `erts_debug:dist_ext_to_term/2`, `prim_file:internal_normalize_utf8/1` and
//!   `erlang:fun_info_mfa/1` (which was never a seam key). Each is asserted
//!   **registered**, **not** a member of any exported `hook_sets` table (every
//!   `needs_*` predicate is `false`), and to produce a **non-gap** value.
//! * **B — seam-blocked (211 rows).** Keys that read one of the eleven seams whose
//!   uninstalled default is the typed gap, so they answer `error:unsupported_bif`
//!   today. Membership is **derived**, never restated: the union of the exported
//!   `hook_sets` tables that gate a roster key, deduped, minus the documented
//!   default-valued exceptions (class D). The derivation is asserted by
//!   [`the_class_b_rows_are_the_seam_union_minus_the_documented_defaults`], so adding,
//!   removing or re-gating a seam key fails here until the census is updated.
//! * **C — out of scope (23 rows).** Keys that answer a documented constant or raise
//!   **regardless of any seam**, because the rewrite models no such capability: the
//!   thirteen `erl_debugger` keys (a stock build has no debugger), the `erts_debug`
//!   `notsup`/constant stubs (`interpreter_size/0`, `lcnt_*`, `disassemble/1`, the
//!   `available_internal_state` gate) and the two collector RPCs that are restricted
//!   to the emulator's own processes. Each carries the one precise sentence of why.
//! * **D — documented defaults (8 rows).** Keys that *do* read a seam but whose
//!   uninstalled default is a documented value rather than the gap: `LocalNode`
//!   falls back to `(nonode@nohost, 0)`, `LocalFunIdentity`/`RecordSource` answer
//!   `Ok(None)`, `ExportGate` is simply skipped. Pinning these separately is what
//!   lets the class-B equation stay exact instead of "everything minus a hand list".
//!
//! # What is *not* claimed here
//!
//! This is a `beam_bifs`-level census: `default_registry()` with **no** host seams.
//! It is **not** a statement about a running node — `beam_rt` installs most of these
//! seams, and its native floor answers some keys *before* the registry is consulted,
//! so "in this file's class B" does not mean "unanswerable at runtime". Conversely a
//! key in class A/C is host-independent: no seam can change its answer.
//!
//! Two rows are **not called**: `erlang:halt/2` and
//! `erts_internal:spawn_system_process/3` (see [`SKIPPED`]). Their registration and
//! seam membership are still pinned, but a census must never be the thing that
//! terminates the test VM if a host hook ever reaches them.
//!
//! # How each outcome is observed
//!
//! Every row is driven through the **public** API only — [`default_registry`] +
//! [`BifRegistry::call`] over a real [`beam_alloc::Heap`], a seeded
//! [`beam_term::AtomTable`] and a real [`TableRegistry`], with **no** optional
//! `BifContext` seam installed (the `ets` table registry is not one of the seams: it
//! is the table store an `ets:foldl/3` needs to reach its fun caller at all).
//! Arguments are built per key by [`args_for`]; a key that only reaches its seam with
//! a *valid* argument gets one (see [`a_fun_taking_key_pins_both_legs`] for the other
//! leg — an invalid fun must fail the clause guard, not reach the seam).
use std::collections::BTreeSet;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_codec::term::{bitstring_bytes, make_binary, make_flatmap, make_fun_external};
use beam_common::{BeamError, ExitReason};
use beam_process::{PortId, ProcessId};
use beam_term::{atom_val, AtomTable, Eterm, NIL};

/// A `(module, function, arity)` census key.
type Key = (&'static str, &'static str, u32);

/// A `needs_*` predicate from [`beam_bifs::hook_sets`].
type Pred = fn(&str, &str, u32) -> bool;

/// The census class of a row — see the module documentation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    /// Closed in-crate: answers a real value with no seam installed.
    A,
    /// Seam-blocked: the typed `unsupported_bif` gap with no seam installed.
    B,
    /// Out of scope: a documented constant or raise, whatever seam exists.
    C,
    /// A seam with a documented non-gap default (`Ok(None)`, a fallback identity, a
    /// skipped check).
    D,
}

/// A value predicate: does `term` (built on `atoms`' namespace) match the pin?
type Check = fn(&AtomTable, Eterm) -> bool;

/// The pinned outcome of a census row.
#[derive(Clone, Copy)]
enum Expect {
    /// `BeamError::Raise` whose reason is this atom — the typed gap
    /// (`unsupported_bif`) and the two documented `undef`/`notsup` refusals.
    Raise(&'static str),
    /// `BeamError::BadArg` — the plain argument error.
    BadArg,
    /// A value satisfying the predicate, with a human description.
    Value(Check, &'static str),
    /// Not called: registration and seam membership are pinned, the outcome is not
    /// (see the row's `why`).
    Skip,
}

/// One census row.
struct Row {
    module: &'static str,
    function: &'static str,
    arity: u32,
    class: Class,
    expect: Expect,
    why: &'static str,
}

/// A class-A row: closed in-crate, answering `check` (described by `label`).
macro_rules! closed {
    ($module:literal, $function:literal, $arity:literal, $check:expr, $label:literal, $why:literal) => {
        Row {
            module: $module,
            function: $function,
            arity: $arity,
            class: Class::A,
            expect: Expect::Value($check, $label),
            why: $why,
        }
    };
}

/// A class-B row: seam-gated, the typed `unsupported_bif` gap when no seam is
/// installed.
macro_rules! seam {
    ($module:literal, $function:literal, $arity:literal, $why:literal) => {
        Row {
            module: $module,
            function: $function,
            arity: $arity,
            class: Class::B,
            expect: Expect::Raise("unsupported_bif"),
            why: $why,
        }
    };
}

/// A class-C row: a documented constant/raise no seam can change.
macro_rules! out_of_scope {
    ($module:literal, $function:literal, $arity:literal, $expect:expr, $why:literal) => {
        Row {
            module: $module,
            function: $function,
            arity: $arity,
            class: Class::C,
            expect: $expect,
            why: $why,
        }
    };
}

/// A class-D row: a seam whose uninstalled default is a documented value.
macro_rules! default_value {
    ($module:literal, $function:literal, $arity:literal, $expect:expr, $why:literal) => {
        Row {
            module: $module,
            function: $function,
            arity: $arity,
            class: Class::D,
            expect: $expect,
            why: $why,
        }
    };
}

/// The nine seams plus the provider contracts the seventeen `BifContext` seams are
/// reached through: every exported [`hook_sets`] table with its predicate.
///
/// The list is the *whole* exported set, so a new table added there shows up in
/// [`reads_a_seam`] immediately (and a class-A/class-C row that starts reading it
/// fails the disjointness assertion).
const SEAM_TABLES: &[(&str, &[Key], Pred)] = &[
    ("MEMORY_HOOK_BIFS", MEMORY_HOOK_BIFS, needs_memory_hook),
    (
        "LOCAL_FUN_IDENTITY_BIFS",
        LOCAL_FUN_IDENTITY_BIFS,
        needs_local_fun_identity,
    ),
    ("DICT_HOOK_BIFS", DICT_HOOK_BIFS, needs_dict_hook),
    ("PROCESS_HOOK_BIFS", PROCESS_HOOK_BIFS, needs_process_hook),
    ("PORT_HOOK_BIFS", PORT_HOOK_BIFS, needs_port_hook),
    ("SEND_HOOK_BIFS", SEND_HOOK_BIFS, needs_send_hook),
    ("NIF_LOADER_BIFS", NIF_LOADER_BIFS, needs_nif_loader),
    ("EXPORT_GATE_BIFS", EXPORT_GATE_BIFS, needs_export_gate),
    ("RECORD_SOURCE_BIFS", RECORD_SOURCE_BIFS, needs_record_source),
    ("MODULE_INFO_BIFS", MODULE_INFO_BIFS, needs_module_info),
    (
        "PERSISTENT_TERM_BIFS",
        PERSISTENT_TERM_BIFS,
        needs_persistent_term,
    ),
    ("FUN_CALLER_BIFS", FUN_CALLER_BIFS, needs_fun_caller),
    ("LOCAL_NODE_BIFS", LOCAL_NODE_BIFS, needs_local_node),
    ("RUNTIME_HOOKS_BIFS", RUNTIME_HOOKS_BIFS, needs_runtime_hooks),
];

/// The eleven tables whose uninstalled default **is** the typed `unsupported_bif`
/// gap — the class-B universe (deduped at run time by the coverage test).
///
/// Deliberately absent: [`EXPORT_GATE_BIFS`] (the gate is *skipped* with none
/// installed, not refused), [`RECORD_SOURCE_BIFS`] and [`MODULE_INFO_BIFS`]'s
/// `Ok(None)` half — `records:get_definition/2` renders `badarg` — and
/// [`LOCAL_NODE_BIFS`], whose default is the `(nonode@nohost, 0)` fallback. Those
/// keys are class-D rows instead, exactly so the equation below stays exact.
const CLASS_B_TABLES: &[(&str, &[Key], Pred)] = &[
    ("RUNTIME_HOOKS_BIFS", RUNTIME_HOOKS_BIFS, needs_runtime_hooks),
    ("MEMORY_HOOK_BIFS", MEMORY_HOOK_BIFS, needs_memory_hook),
    ("DICT_HOOK_BIFS", DICT_HOOK_BIFS, needs_dict_hook),
    ("PROCESS_HOOK_BIFS", PROCESS_HOOK_BIFS, needs_process_hook),
    ("PORT_HOOK_BIFS", PORT_HOOK_BIFS, needs_port_hook),
    ("SEND_HOOK_BIFS", SEND_HOOK_BIFS, needs_send_hook),
    ("NIF_LOADER_BIFS", NIF_LOADER_BIFS, needs_nif_loader),
    (
        "PERSISTENT_TERM_BIFS",
        PERSISTENT_TERM_BIFS,
        needs_persistent_term,
    ),
    ("MODULE_INFO_BIFS", MODULE_INFO_BIFS, needs_module_info),
    ("FUN_CALLER_BIFS", FUN_CALLER_BIFS, needs_fun_caller),
    (
        "LOCAL_FUN_IDENTITY_BIFS",
        LOCAL_FUN_IDENTITY_BIFS,
        needs_local_fun_identity,
    ),
];

/// The rows whose call is **not** made — see the module documentation.
///
/// A census must not be able to terminate the test VM. `erlang:halt/2` exists to
/// stop the node and `erts_internal:spawn_system_process/3` exists to create an
/// unkillable system process; today both are pure seam forwarders (so both would
/// answer the typed gap), but that safety rests only on "no host hook is installed",
/// and a future in-crate implementation would turn this file into a VM killer. Their
/// registration and `RUNTIME_HOOKS_BIFS` membership are pinned instead, and the
/// outcome stays the documented behaviour of the seam they forward to.
const SKIPPED: &[Key] = &[
    ("erlang", "halt", 2),
    ("erts_internal", "spawn_system_process", 3),
];

/// The seam-consumer table that lists `key`, if any.
fn reads_a_seam(key: Key) -> Option<&'static str> {
    SEAM_TABLES
        .iter()
        .find(|(_, _, predicate)| predicate(key.0, key.1, key.2))
        .map(|(name, _, _)| *name)
}

// ---------------------------------------------------------------------------
// The census.
//
// ~240 rows, one per registered key with a capability-gap-relevant answer. The
// class-B block is the `hook_sets` union minus the class-D rows (asserted at run
// time, not by eye); the class-A, class-C and class-D rows are hand-written because
// each one's answer is a distinct fact about the code.
// ---------------------------------------------------------------------------

const CENSUS: &[Row] = &[
    // -- Class A: closed in-crate -------------------------------------------
    closed!(
        "erts_debug",
        "instructions",
        0,
        check_instructions_list,
        "a 248-element list of instruction rows",
        "the generated `beam_makeops` roster (`src/runtime/instructions.rs`) is crate data"
    ),
    closed!(
        "erts_debug",
        "dist_ext_to_term",
        2,
        check_small_five,
        "the integer 5",
        "the distribution decode over the shared `beam_codec` decoder (`decode_dist_ext_image`)"
    ),
    closed!(
        "prim_file",
        "internal_normalize_utf8",
        1,
        check_normalized_193,
        "the list [193]",
        "the composition-only Unicode normalizer behind `src/unicode/normalize/`"
    ),
    closed!(
        "erlang",
        "fun_info_mfa",
        1,
        check_fun_info_mfa,
        "{m,f_2,2}",
        "pure term introspection over the frozen fun interior (`src/fun_info.rs`); never a seam key"
    ),
    // -- Class B: seam-blocked ----------------------------------------------
    // The five `LocalFunIdentity` *renderers*: with no identity seam a local fun has
    // no `#Fun<Module.OldIndex.OldUniq>` text to print, so the renderer answers the
    // typed gap (an `External`/`Imported` fun never reaches the seam and renders).
    seam!("erlang", "fun_to_list", 1, "LocalFunIdentity seam (`hook_sets::LOCAL_FUN_IDENTITY_BIFS`): the fun text is `erl_printf_term.c`'s `FUN_DEF` arm, which needs the defining module, `old_index` and `old_uniq` — on the seam for a `make_fun3` fun"),
    seam!("erlang", "display", 1, "LocalFunIdentity seam (`hook_sets::LOCAL_FUN_IDENTITY_BIFS`): `display/1` is the same `FUN_DEF` text on the output sink"),
    seam!("io", "format", 2, "LocalFunIdentity seam (`hook_sets::LOCAL_FUN_IDENTITY_BIFS`): `~w`/`~p` render a fun through the one shared `fun_text` leaf"),
    seam!("io_lib", "format", 2, "LocalFunIdentity seam (`hook_sets::LOCAL_FUN_IDENTITY_BIFS`): `~w`/`~p` render a fun through the one shared `fun_text` leaf"),
    seam!("io_lib", "format", 3, "LocalFunIdentity seam (`hook_sets::LOCAL_FUN_IDENTITY_BIFS`): `~w`/`~p` render a fun through the one shared `fun_text` leaf"),    seam!("code", "coverage_support", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("code", "get_coverage", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("code", "get_coverage_mode", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("code", "get_coverage_mode", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("code", "get_debug_info", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("code", "reset_coverage", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("code", "set_coverage_mode", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "!", 2, "SendHook seam: delivery needs the target mailbox and the scheduler wake-up"),
    seam!("erlang", "alias", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "bump_reductions", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "call_on_load_function", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "cancel_timer", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "cancel_timer", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "check_old_code", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "delete_module", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "demonitor", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "demonitor", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "dist_ctrl_get_data", 1, "RuntimeHooks forwarder: the oracle's unconditional `notsup` is the *provider's* answer (`beam_rt`'s bridge), so with no seam installed this side reports the typed gap"),
    seam!("erlang", "dist_ctrl_get_data_notification", 1, "RuntimeHooks forwarder: the oracle's unconditional `notsup` is the *provider's* answer (`beam_rt`'s bridge), so with no seam installed this side reports the typed gap"),
    seam!("erlang", "dist_ctrl_get_opt", 2, "RuntimeHooks forwarder: the oracle's unconditional `notsup` is the *provider's* answer (`beam_rt`'s bridge), so with no seam installed this side reports the typed gap"),
    seam!("erlang", "dist_ctrl_input_handler", 2, "RuntimeHooks forwarder: the oracle's unconditional `notsup` is the *provider's* answer (`beam_rt`'s bridge), so with no seam installed this side reports the typed gap"),
    seam!("erlang", "dist_ctrl_put_data", 2, "RuntimeHooks forwarder: the oracle's unconditional `notsup` is the *provider's* answer (`beam_rt`'s bridge), so with no seam installed this side reports the typed gap"),
    seam!("erlang", "dist_ctrl_set_opt", 3, "RuntimeHooks forwarder: the oracle's unconditional `notsup` is the *provider's* answer (`beam_rt`'s bridge), so with no seam installed this side reports the typed gap"),
    seam!("erlang", "dist_get_stat", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "dt_append_vm_tag_data", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "dt_get_tag", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "dt_get_tag_data", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "dt_prepend_vm_tag_data", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "dt_put_tag", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "dt_restore_tag", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "dt_spread_tag", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "erase", 0, "DictHook seam: the process dictionary is a process GC sub-rootset (`beam_process::Dict`)"),
    seam!("erlang", "erase", 1, "DictHook seam: the process dictionary is a process GC sub-rootset (`beam_process::Dict`)"),
    seam!("erlang", "exit", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "exit", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "exit_signal", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "exit_signal", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "finish_after_on_load", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "finish_loading", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "function_exported", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "get", 0, "DictHook seam: the process dictionary is a process GC sub-rootset (`beam_process::Dict`)"),
    seam!("erlang", "get", 1, "DictHook seam: the process dictionary is a process GC sub-rootset (`beam_process::Dict`)"),
    seam!("erlang", "get_keys", 0, "DictHook seam: the process dictionary is a process GC sub-rootset (`beam_process::Dict`)"),
    seam!("erlang", "get_keys", 1, "DictHook seam: the process dictionary is a process GC sub-rootset (`beam_process::Dict`)"),
    seam!("erlang", "get_module_info", 1, "ModuleInfo seam: the code server's per-module introspection"),
    seam!("erlang", "get_module_info", 2, "ModuleInfo seam: the code server's per-module introspection"),
    seam!("erlang", "group_leader", 0, "ProcessHook seam: the name registry and the process table are Stage-5 state"),
    Row {
        module: "erlang",
        function: "halt",
        arity: 2,
        class: Class::B,
        expect: Expect::Skip,
        why: "a `RuntimeHooks` forwarder (so the typed gap today), but an in-crate `halt` would stop the test VM: listed, never called",
    },
    seam!("erlang", "has_prepared_code_on_load", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "hibernate", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "is_process_alive", 1, "ProcessHook seam: the name registry and the process table are Stage-5 state"),
    seam!("erlang", "link", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "link", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "load_nif", 2, "NifLoader seam: `dlopen` and the NIF C ABI binding are Stage-5 machinery"),
    seam!("erlang", "loaded", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "make_ref", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "memory", 0, "MemoryHook seam: the allocator's figures are the runtime's own accounting"),
    seam!("erlang", "memory", 1, "MemoryHook seam: the allocator's figures are the runtime's own accounting"),
    seam!("erlang", "module_loaded", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "monitor", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "monitor", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "monitor_node", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "monitor_node", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "nodes", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "nodes", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "nodes", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "open_port", 2, "PortHook seam: a port is an OS process + driver + a port-table slot (Stage-5)"),
    seam!("erlang", "port_close", 1, "PortHook seam: a port is an OS process + driver + a port-table slot (Stage-5)"),
    seam!("erlang", "port_get_data", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "port_info", 1, "PortHook seam: a port is an OS process + driver + a port-table slot (Stage-5)"),
    seam!("erlang", "port_info", 2, "PortHook seam: a port is an OS process + driver + a port-table slot (Stage-5)"),
    seam!("erlang", "port_set_data", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "ports", 0, "ProcessHook seam: the name registry and the process table are Stage-5 state"),
    seam!("erlang", "pre_loaded", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "process_flag", 2, "ProcessHook seam: the name registry and the process table are Stage-5 state"),
    seam!("erlang", "process_info", 1, "ProcessHook seam: the name registry and the process table are Stage-5 state"),
    seam!("erlang", "process_info", 2, "ProcessHook seam: the name registry and the process table are Stage-5 state"),
    seam!("erlang", "processes", 0, "ProcessHook seam: the name registry and the process table are Stage-5 state"),
    seam!("erlang", "put", 2, "DictHook seam: the process dictionary is a process GC sub-rootset (`beam_process::Dict`)"),
    seam!("erlang", "read_timer", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "read_timer", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "register", 2, "ProcessHook seam: the name registry and the process table are Stage-5 state"),
    seam!("erlang", "registered", 0, "ProcessHook seam: the name registry and the process table are Stage-5 state"),
    seam!("erlang", "resume_process", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "send", 2, "SendHook seam: delivery needs the target mailbox and the scheduler wake-up"),
    seam!("erlang", "send", 3, "SendHook seam: delivery needs the target mailbox and the scheduler wake-up"),
    seam!("erlang", "send_after", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "send_after", 4, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "seq_trace", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "seq_trace_info", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "seq_trace_print", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "seq_trace_print", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "setnode", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "spawn", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "spawn_link", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "spawn_opt", 4, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "spawn_request_abandon", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "start_timer", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "start_timer", 4, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "statistics", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "system_flag", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "system_info", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "system_profile", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "system_profile", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "trace_delivered", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "trace_info", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "unalias", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "unlink", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erlang", "unregister", 1, "ProcessHook seam: the name registry and the process table are Stage-5 state"),
    seam!("erlang", "whereis", 1, "ProcessHook seam: the name registry and the process table are Stage-5 state"),
    seam!("error_logger", "warning_map", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_debug", "breakpoint", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_debug", "dirty", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_debug", "dirty_cpu", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_debug", "dirty_io", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "abort_pending_connection", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "beamfile_chunk", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "beamfile_module_md5", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "check_dirty_process_code", 2, "RuntimeHooks forwarder: the oracle's unconditional `notsup` is the *provider's* answer (`beam_rt`'s bridge), so with no seam installed this side reports the typed gap"),
    seam!("erts_internal", "check_process_code", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "create_dist_channel", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "dirty_process_handle_signals", 1, "RuntimeHooks forwarder: the oracle's unconditional `notsup` is the *provider's* answer (`beam_rt`'s bridge), so with no seam installed this side reports the typed gap"),
    seam!("erts_internal", "dist_spawn_request", 4, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "erase_persistent_terms", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "ets_lookup_binary_info", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "ets_super_user", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "garbage_collect", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "gather_alloc_histograms", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "gather_carrier_info", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "get_creation", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "get_dflags", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "group_leader", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "group_leader", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "is_process_alive", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "is_process_executing_dirty", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "is_system_process", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "new_connection", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "no_aux_work_threads", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "notify_breakpoint_hit", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "open_port", 2, "PortHook seam: a port is an OS process + driver + a port-table slot (Stage-5)"),
    seam!("erts_internal", "port_call", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "port_close", 1, "PortHook seam: a port is an OS process + driver + a port-table slot (Stage-5)"),
    seam!("erts_internal", "port_command", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "port_connect", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "port_control", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "port_info", 1, "PortHook seam: a port is an OS process + driver + a port-table slot (Stage-5)"),
    seam!("erts_internal", "port_info", 2, "PortHook seam: a port is an OS process + driver + a port-table slot (Stage-5)"),
    seam!("erts_internal", "prepare_loading", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "process_display", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "process_flag", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "processes_next", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "purge_module", 2, "RuntimeHooks forwarder: the oracle's unconditional `notsup` is the *provider's* answer (`beam_rt`'s bridge), so with no seam installed this side reports the typed gap"),
    seam!("erts_internal", "request_system_task", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "request_system_task", 4, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "scheduler_wall_time", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "spawn_request", 4, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    Row {
        module: "erts_internal",
        function: "spawn_system_process",
        arity: 3,
        class: Class::B,
        expect: Expect::Skip,
        why: "a `RuntimeHooks` forwarder (so the typed gap today), but it exists to create an unkillable system process: listed, never called",
    },
    seam!("erts_internal", "suspend_process", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "system_check", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "system_monitor", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "system_monitor", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "term_to_string", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "trace", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "trace", 4, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "trace_info", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "trace_pattern", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "trace_pattern", 4, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "trace_session_create", 3, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_internal", "trace_session_destroy", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("erts_literal_area_collector", "release_area_switch", 0, "RuntimeHooks forwarder: the oracle's unconditional `notsup` is the *provider's* answer (`beam_rt`'s bridge), so with no seam installed this side reports the typed gap"),
    seam!("erts_trace_cleaner", "check", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("ets", "foldl", 3, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("ets", "foldr", 3, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("ets", "give_away", 3, "SendHook seam: delivery needs the target mailbox and the scheduler wake-up"),
    seam!("ets", "internal_request_all", 0, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("lists", "all", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "any", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "dropwhile", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "filter", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "flatmap", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "foldl", 3, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "foldr", 3, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "foreach", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "keymap", 3, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "map", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "mapfoldl", 3, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "mapfoldr", 3, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "partition", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "sort", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "splitwith", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "takewhile", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "usort", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("lists", "zipwith", 3, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("maps", "filter", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("maps", "fold", 3, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("maps", "foreach", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("maps", "map", 2, "FunCaller seam: a `BifFn` cannot execute BEAM code, so applying the fun needs the engine"),
    seam!("net_kernel", "dflag_unicode_io", 1, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("os", "set_signal", 2, "RuntimeHooks seam (`hook_sets::RUNTIME_HOOKS_BIFS`): the answer is live VM state only the runtime holds"),
    seam!("persistent_term", "erase", 1, "PersistentTerm seam: a node-global store outside every process heap"),
    seam!("persistent_term", "get", 0, "PersistentTerm seam: a node-global store outside every process heap"),
    seam!("persistent_term", "get", 1, "PersistentTerm seam: a node-global store outside every process heap"),
    seam!("persistent_term", "get", 2, "PersistentTerm seam: a node-global store outside every process heap"),
    seam!("persistent_term", "info", 0, "PersistentTerm seam: a node-global store outside every process heap"),
    seam!("persistent_term", "put", 2, "PersistentTerm seam: a node-global store outside every process heap"),
    seam!("persistent_term", "put_new", 2, "PersistentTerm seam: a node-global store outside every process heap"),
    // -- Class C: out of scope ----------------------------------------------
    out_of_scope!(
        "erl_debugger",
        "supported",
        0,
        Expect::Value(check_atom_false, "the atom false"),
        "`erts_debugger_flags` is 0 in a stock build, so `erl_debugger_supported_0` answers `false`"
    ),
    out_of_scope!(
        "erl_debugger",
        "instrumentations",
        0,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erl_debugger",
        "toggle_instrumentations",
        1,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erl_debugger",
        "register",
        1,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erl_debugger",
        "unregister",
        2,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erl_debugger",
        "whereis",
        0,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erl_debugger",
        "breakpoint",
        3,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erl_debugger",
        "breakpoints",
        1,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erl_debugger",
        "breakpoints",
        3,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erl_debugger",
        "stack_frames",
        2,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erl_debugger",
        "peek_stack_frame_slot",
        4,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erl_debugger",
        "peek_xreg",
        3,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erl_debugger",
        "xregs_count",
        1,
        Expect::Raise("undef"),
        "`BIF_UNDEF_IF_NO_DEBUGGER_SUPPORT` precedes any argument inspection: a stock build raises `undef`"
    ),
    out_of_scope!(
        "erts_debug",
        "disassemble",
        1,
        Expect::Value(check_atom_false, "the atom false"),
        "the `BEAMASM` arm is a bare `BIF_RET(am_false)` before the argument is looked at (`beam_debug.c:396`)"
    ),
    out_of_scope!(
        "erts_debug",
        "interpreter_size",
        0,
        Expect::Value(check_small_zero, "the integer 0"),
        "a `BEAMASM` build has no dispatch table to measure, so the C answers `make_small(0)` (`beam_debug.c:517`)"
    ),
    out_of_scope!(
        "erts_debug",
        "lcnt_clear",
        0,
        Expect::Value(check_atom_error, "the atom error"),
        "without `ERTS_ENABLE_LOCK_COUNT` it is a bare `BIF_RET(am_error)` (`erl_bif_info.c:6311`)"
    ),
    out_of_scope!(
        "erts_debug",
        "lcnt_collect",
        0,
        Expect::Value(check_atom_error, "the atom error"),
        "without `ERTS_ENABLE_LOCK_COUNT` it is a bare `BIF_RET(am_error)` (`erl_bif_info.c:6322`)"
    ),
    out_of_scope!(
        "erts_debug",
        "lcnt_control",
        1,
        Expect::BadArg,
        "the whole body is under `#ifdef ERTS_ENABLE_LOCK_COUNT`; the trailing `badarg` is then the only outcome (`erl_bif_info.c:6357`)"
    ),
    out_of_scope!(
        "erts_debug",
        "lcnt_control",
        2,
        Expect::BadArg,
        "the whole body is under `#ifdef ERTS_ENABLE_LOCK_COUNT`; the trailing `badarg` is then the only outcome (`erl_bif_info.c:6384`)"
    ),
    out_of_scope!(
        "erts_debug",
        "get_internal_state",
        1,
        Expect::Raise("undef"),
        "`available_internal_state` starts 0 and a gate-off call is `EXC_UNDEF` before the argument (`erl_bif_info.c:4341`)"
    ),
    out_of_scope!(
        "erts_debug",
        "set_internal_state",
        2,
        Expect::Raise("undef"),
        "as `get_internal_state/1`: gate-off is `undef`; `{available_internal_state, Bool}` is the sole gate-free arm"
    ),
    out_of_scope!(
        "erts_trace_cleaner",
        "send_trace_clean_signal",
        1,
        Expect::Raise("notsup"),
        "restricted to the emulator's own cleaner process: `EXC_NOTSUP` before any argument check (`erl_bif_trace.c:649`)"
    ),
    out_of_scope!(
        "erts_literal_area_collector",
        "send_copy_request",
        3,
        Expect::Raise("notsup"),
        "restricted to the emulator's own collector process: `EXC_NOTSUP` before any argument check (`beam_bif_load.c:1749`)"
    ),
    // -- Class D: documented defaults ---------------------------------------
    default_value!(
        "erlang",
        "node",
        0,
        Expect::Value(check_atom_nonode, "the atom nonode@nohost"),
        "`LocalNode` with no hook falls back to the M1 fixed identity `(nonode@nohost, 0)` (`BifContext::local_node`)"
    ),
    default_value!(
        "erlang",
        "node",
        1,
        Expect::Value(check_atom_nonode, "the atom nonode@nohost"),
        "`LocalNode`'s fallback identity is what an internal pid's node renders as (`BifContext::local_node`)"
    ),
    default_value!(
        "erlang",
        "term_to_binary",
        1,
        Expect::Value(check_external_image, "an external format image"),
        "`LocalFunIdentity` answers `Ok(None)` with no hook, which only affects a `make_fun3` closure; a plain term encodes"
    ),
    default_value!(
        "erlang",
        "term_to_binary",
        2,
        Expect::Value(check_external_image, "an external format image"),
        "`LocalFunIdentity`'s `Ok(None)` default plus `LocalNode`'s fallback identity cover the two `/2` options in use"
    ),
    default_value!(
        "erlang",
        "external_size",
        1,
        Expect::Value(check_small_three, "the integer 3"),
        "`external_size/1` of the small integer 1: the encoder needs no fun identity, so `Ok(None)` suffices"
    ),
    default_value!(
        "erlang",
        "external_size",
        2,
        Expect::Value(check_small_three, "the integer 3"),
        "as `external_size/1`: the `LocalFunIdentity` default only refuses a `make_fun3` closure, never a plain term"
    ),
    default_value!(
        "erlang",
        "binary_to_term",
        2,
        Expect::Value(check_small_five, "the integer 5"),
        "`ExportGate` is *skipped*, not refused, with no hook installed: `[safe]`/`[]` decode the same image"
    ),
    default_value!(
        "records",
        "get_definition",
        2,
        Expect::BadArg,
        "`RecordSource` answers `Ok(None)` with no hook, which `records:get_definition/2` renders as the oracle's `badarg`"
    ),
];

// ---------------------------------------------------------------------------
// Value predicates (the class-A / class-C / class-D pins).
//
// Each one is deliberately structural rather than word-identical: a result term is
// built on a *different* heap from any expected term, so only the shape and the
// interesting scalars are pinned.
// ---------------------------------------------------------------------------

/// Whether `term` is the atom `name` in `atoms`' namespace.
fn is_atom_named(atoms: &AtomTable, term: Eterm, name: &str) -> bool {
    beam_term::is_atom(term) && atoms.name(atom_val(term)) == Some(name)
}

/// The atom `false` (a documented constant).
fn check_atom_false(atoms: &AtomTable, term: Eterm) -> bool {
    is_atom_named(atoms, term, "false")
}

/// The atom `error` (the `lcnt_*` constants).
fn check_atom_error(atoms: &AtomTable, term: Eterm) -> bool {
    is_atom_named(atoms, term, "error")
}

/// The atom `nonode@nohost` (the `LocalNode` fallback identity).
fn check_atom_nonode(atoms: &AtomTable, term: Eterm) -> bool {
    is_atom_named(atoms, term, "nonode@nohost")
}

/// The small integer 0 (`erts_debug:interpreter_size/0`).
fn check_small_zero(_atoms: &AtomTable, term: Eterm) -> bool {
    beam_term::is_small(term) && beam_term::signed_value(term) == 0
}

/// The small integer 3 (`erlang:external_size/1,2` of `1`).
fn check_small_three(_atoms: &AtomTable, term: Eterm) -> bool {
    beam_term::is_small(term) && beam_term::signed_value(term) == 3
}

/// The small integer 5 (`<<131,97,5>>` decoded).
fn check_small_five(_atoms: &AtomTable, term: Eterm) -> bool {
    beam_term::is_small(term) && beam_term::signed_value(term) == 5
}

/// A binary in the external term format (the version byte `131`).
fn check_external_image(_atoms: &AtomTable, term: Eterm) -> bool {
    match bitstring_bytes(term) {
        Ok((bytes, 0, len)) => len > 0 && bytes.first() == Some(&131),
        _ => false,
    }
}

/// The 248-element instruction list `erts_debug:instructions/0` renders.
fn check_instructions_list(_atoms: &AtomTable, term: Eterm) -> bool {
    proper_list_len(term) == Some(248)
}

/// The list `[193]` — `<<65,204,129>>` (a + U+0301) normalizes to one composed byte.
fn check_normalized_193(_atoms: &AtomTable, term: Eterm) -> bool {
    match proper_list_len(term) {
        Some(1) => match beam_codec::term::cons_head(term) {
            Ok(head) => beam_term::is_small(head) && beam_term::signed_value(head) == 193,
            Err(_) => false,
        },
        _ => false,
    }
}

/// The `{m, f_2, 2}` triple `erlang:fun_info_mfa/1` renders for the census' arity-2
/// external fun.
fn check_fun_info_mfa(atoms: &AtomTable, term: Eterm) -> bool {
    if !matches!(beam_codec::term::tuple_arity(term), Ok(3)) {
        return false;
    }
    let (Ok(module), Ok(function), Ok(arity)) = (
        beam_codec::term::tuple_element(term, 0),
        beam_codec::term::tuple_element(term, 1),
        beam_codec::term::tuple_element(term, 2),
    ) else {
        return false;
    };
    is_atom_named(atoms, module, "m")
        && is_atom_named(atoms, function, "f_2")
        && beam_term::is_small(arity)
        && beam_term::signed_value(arity) == 2
}

/// The length of a proper list, or `None` for a non-list/improper one.
///
/// `beam_term::car`/`cdr` are `unsafe` (the frozen-layout readers this test crate
/// must not call), so the walk goes through the *safe* `beam_codec::term` accessors.
fn proper_list_len(mut term: Eterm) -> Option<usize> {
    let mut len = 0;
    while beam_term::is_list(term) {
        term = beam_codec::term::cons_tail(term).ok()?;
        len += 1;
    }
    (term == NIL).then_some(len)
}

// ---------------------------------------------------------------------------
// Driving a row.
// ---------------------------------------------------------------------------

/// The atom names every row's arguments and pins resolve.
///
/// Only a *seeded* name resolves without an [`AtomInterner`](beam_bifs::AtomInterner)
/// hook, and this file installs no seam at all — so the seed is deliberately one name
/// wider than the arguments use.
const SEEDED_ATOMS: &[&str] = &[
    "a",
    "m",
    "f_1",
    "f_2",
    "f_3",
    "set",
    "spawn",
    "status",
    "trap_exit",
    "gift",
    "total",
    "local",
    "module",
    "name",
    "nonode@nohost",
    "error",
    "undefined",
    "true",
    "false",
];

/// A heap big enough for every row's result (the widest is the 248-element
/// instruction list and the `ets` copy-out).
fn test_heap() -> Heap {
    Heap::new(1 << 14).expect("test heap")
}

/// The seeded atom table.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(SEEDED_ATOMS.iter().copied());
    atoms
}

/// A seeded atom term.
fn atom(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name)
        .unwrap_or_else(|e| panic!("the atom {name} must be seeded: {e:?}"))
}

/// A proper list of the first `arity` positive small integers.
fn small_ints(ctx: &mut BifContext<'_>, arity: u32) -> Vec<Eterm> {
    (1..=arity)
        .map(|i| ctx.int(i64::from(i)).expect("a small int fits"))
        .collect()
}

/// A real `EXPORT_EXT` fun of `arity` on the calling heap.
///
/// A fun-taking BIF validates its argument with the oracle's `is_function(F, Arity)`
/// clause-head guard, so nothing less reaches the [`FunCaller`](beam_bifs::FunCaller)
/// seam (see [`a_fun_taking_key_pins_both_legs`]).
fn external_fun(ctx: &mut BifContext<'_>, arity: usize) -> Eterm {
    let function = match arity {
        1 => "f_1",
        2 => "f_2",
        _ => "f_3",
    };
    let module = atom_val(ctx.atom("m").expect("seeded"));
    let function = atom_val(ctx.atom(function).expect("seeded"));
    make_fun_external(ctx.heap(), arity, module, function).expect("an external fun fits")
}

/// A `FunKind::Local` (`make_fun3`) fun: the header word plus the
/// `[uniq, index]` descriptor, with no module/name anywhere in the term.
///
/// The block is **leaked** deliberately: it must stay readable for the whole call,
/// and a census row's argument vector cannot own the storage it points at.
fn local_fun() -> Eterm {
    let words: &'static mut [Eterm] = Box::leak(
        vec![
            Eterm::from_raw(beam_term::FUN_SUBTAG | (2usize << 8)),
            Eterm::from_raw(0), // uniq
            Eterm::from_raw(0), // index
        ]
        .into_boxed_slice(),
    );
    beam_term::make_boxed(words.as_ptr())
}
/// The character list `bytes` on the calling heap.
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let elems: Vec<Eterm> = bytes.iter().map(|&b| ctx.int(i64::from(b)).expect("fits")).collect();
    ctx.list(&elems).expect("a short list fits")
}
/// The list `[1, 2]` on the calling heap.
fn list12(ctx: &mut BifContext<'_>) -> Eterm {
    let one = ctx.int(1).expect("fits");
    let two = ctx.int(2).expect("fits");
    ctx.list(&[one, two]).expect("a two-element list fits")
}

/// The arguments a census row is called with.
///
/// Most keys are seam forwarders that inspect nothing before the seam, so a
/// placeholder argument vector is enough; the arms below are the keys that validate
/// their arguments **first** and would answer `badarg`/`function_clause` instead of
/// reaching the seam ("only a *valid* argument reaches the seam" — see the module
/// documentation).
fn args_for(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
) -> Vec<Eterm> {
    let pid = ProcessId::new(7, 0).to_eterm();
    let port = PortId::new(1).to_eterm();
    match (module, function, arity) {
        // -- `ProcessHook`: a non-atom name / non-pid id is the C's `badarg` ----
        ("erlang", "whereis" | "unregister", 1) => vec![atom(ctx, "a")],
        ("erlang", "register", 2) => vec![atom(ctx, "a"), pid],
        ("erlang", "is_process_alive", 1) | ("erlang", "process_info", 1) => vec![pid],
        ("erlang", "process_info", 2) => vec![pid, atom(ctx, "status")],
        ("erlang", "process_flag", 2) => vec![atom(ctx, "trap_exit"), pid],
        // -- `PortHook`: the two spellings, each with a well-formed port argument --
        ("erlang" | "erts_internal", "port_close" | "port_info", 1) => vec![port],
        ("erlang" | "erts_internal", "port_info", 2) => vec![port, atom(ctx, "name")],
        ("erlang" | "erts_internal", "open_port", 2) => {
            let spawn = atom(ctx, "spawn");
            let command = make_binary(ctx.heap(), b"cmd").expect("a short binary fits");
            let name = ctx.tuple(&[spawn, command]).expect("a 2-tuple fits");
            vec![name, NIL]
        }
        // -- `SendHook`: the destination must be deliverable -------------------
        ("erlang", "send" | "!", 2) => {
            let message = ctx.int(1).expect("fits");
            vec![pid, message]
        }
        ("erlang", "send", 3) => {
            let message = ctx.int(1).expect("fits");
            vec![pid, message, NIL]
        }
        ("ets", "give_away", 3) => {
            let handle = new_table(reg, ctx);
            vec![handle, pid, atom(ctx, "gift")]
        }
        // -- `NifLoader`: only a character list or a binary is a path ----------
        ("erlang", "load_nif", 2) => {
            let path = ctx.list(&[ctx.int(120).expect("fits")]).expect("fits");
            vec![path, NIL]
        }
        // -- `ModuleInfo`: the module and item are atoms ----------------------
        ("erlang", "get_module_info", 1) => vec![atom(ctx, "a")],
        ("erlang", "get_module_info", 2) => vec![atom(ctx, "a"), atom(ctx, "module")],
        // -- `MemoryHook`: the item is an atom --------------------------------
        ("erlang", "memory", 1) => vec![atom(ctx, "total")],
        // -- `FunCaller`: `lists` 18 ------------------------------------------
        (
            "lists",
            "map" | "filter" | "foreach" | "flatmap" | "all" | "any" | "takewhile" | "dropwhile"
            | "splitwith" | "partition",
            2,
        ) => {
            let fun = external_fun(ctx, 1);
            let list = list12(ctx);
            vec![fun, list]
        }
        ("lists", "sort" | "usort", 2) => {
            let fun = external_fun(ctx, 2);
            let list = list12(ctx);
            vec![fun, list]
        }
        ("lists", "foldl" | "foldr" | "mapfoldl" | "mapfoldr", 3) => {
            let fun = external_fun(ctx, 2);
            let acc = ctx.int(0).expect("fits");
            let list = list12(ctx);
            vec![fun, acc, list]
        }
        ("lists", "zipwith", 3) => {
            let fun = external_fun(ctx, 2);
            let left = list12(ctx);
            let right = list12(ctx);
            vec![fun, left, right]
        }
        ("lists", "keymap", 3) => {
            // `keymap(Fun, N, TupleList)` — `Fun/1`, an integer position, tuples.
            let fun = external_fun(ctx, 1);
            let position = ctx.int(1).expect("fits");
            let key = ctx.int(7).expect("fits");
            let tuple = ctx.tuple(&[key, key]).expect("fits");
            let list = ctx.list(&[tuple]).expect("fits");
            vec![fun, position, list]
        }
        // -- `FunCaller`: `maps` 4 (`fold/3`'s fun is arity 3) ----------------
        ("maps", "map" | "filter" | "foreach", 2) => {
            let fun = external_fun(ctx, 2);
            let map = one_pair_map(ctx);
            vec![fun, map]
        }
        ("maps", "fold", 3) => {
            let fun = external_fun(ctx, 3);
            let acc = ctx.int(0).expect("fits");
            let map = one_pair_map(ctx);
            vec![fun, acc, map]
        }
        // -- `FunCaller`: `ets` 2 (the fold's argument order is `(Fun, Acc, Tab)`)
        ("ets", "foldl" | "foldr", 3) => {
            let fun = external_fun(ctx, 2);
            let acc = ctx.int(0).expect("fits");
            vec![fun, acc, new_table(reg, ctx)]
        }
        // -- Class-D keys whose `/2` forms need a well-formed option list -----
        ("erlang", "term_to_binary" | "external_size", 2) => {
            let term = ctx.int(1).expect("fits");
            vec![term, NIL]
        }
        ("erlang", "binary_to_term", 2) => {
            let image = make_binary(ctx.heap(), &[131, 97, 5]).expect("fits");
            vec![image, NIL]
        }
        ("erlang", "node", 1) => vec![pid],
        // -- `erlang:fun_info_mfa/1` needs a fun, not a placeholder -----------
        ("erlang", "fun_info_mfa", 1) => vec![external_fun(ctx, 2)],
        // -- Class-A decoders -------------------------------------------------
        ("erts_debug", "dist_ext_to_term", 2) => {
            let empty = ctx.tuple(&[]).expect("fits");
            let image = make_binary(ctx.heap(), &[131, 97, 5]).expect("fits");
            vec![empty, image]
        }
        ("prim_file", "internal_normalize_utf8", 1) => {
            // `<<65,204,129>>` — `a` followed by U+0301, which composes to `å`.
            let image = make_binary(ctx.heap(), &[65, 204, 129]).expect("fits");
            vec![image]
        }
        // -- `LocalFunIdentity` renderers: only a *local* fun reaches the seam ---
        ("erlang", "display", 1) | ("erlang", "fun_to_list", 1) => vec![local_fun()],
        ("io", "format", 2) | ("io_lib", "format", 2) => {
            let format = byte_list(ctx, b"~w");
            vec![format, ctx.list(&[local_fun()]).expect("fits")]
        }
        ("io_lib", "format", 3) => {
            let format = byte_list(ctx, b"~w");
            vec![format, ctx.list(&[local_fun()]).expect("fits"), NIL]
        }
        _ => small_ints(ctx, arity),
    }
}

/// The map `#{1 => 2}` on the calling heap.
fn one_pair_map(ctx: &mut BifContext<'_>) -> Eterm {
    let key = ctx.int(1).expect("fits");
    let value = ctx.int(2).expect("fits");
    make_flatmap(ctx.heap(), &[(key, value)]).expect("a one-pair map fits")
}

/// A fresh `set` table holding one `{1, 2}` object, through the registry itself.
fn new_table(reg: &BifRegistry, ctx: &mut BifContext<'_>) -> Eterm {
    let name = atom(ctx, "a");
    let kind = atom(ctx, "set");
    let options = ctx.list(&[kind]).expect("fits");
    let handle = reg
        .call("ets", "new", 2, ctx, &[name, options])
        .expect("ets:new/2");
    let key = ctx.int(1).expect("fits");
    let value = ctx.int(2).expect("fits");
    let object = ctx.tuple(&[key, value]).expect("fits");
    reg.call("ets", "insert", 2, ctx, &[handle, object])
        .expect("ets:insert/2");
    handle
}

/// Calls `row`'s key over a fresh heap, a seeded atom table and a real table
/// registry, with **no** optional seam installed, and checks the outcome against the
/// row's pin.
///
/// The call and the comparison happen inside this function because a result term
/// points into the heap that built it: checking it after the heap is dropped would
/// read freed storage.
///
/// # Errors
///
/// The human-readable mismatch, naming the row, its class, its `why` and what was
/// observed instead.
fn verify_row(reg: &BifRegistry, row: &Row) -> Result<(), String> {
    let label = format!(
        "{}:{}/{} [{:?}]",
        row.module, row.function, row.arity, row.class
    );
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let args = args_for(reg, &mut ctx, row.module, row.function, row.arity);
    let result = reg.call(row.module, row.function, row.arity, &mut ctx, &args);
    match (&row.expect, result) {
        (Expect::Skip, _) => {
            if SKIPPED.contains(&(row.module, row.function, row.arity)) {
                Ok(())
            } else {
                Err(format!("{label} is a skip row but is not listed in SKIPPED"))
            }
        }
        (Expect::Raise(reason), Err(BeamError::Raise(raise))) => {
            if raise.reason == ExitReason::atom(*reason) {
                Ok(())
            } else {
                Err(format!(
                    "{label} must raise {reason} ({}), got the raise {:?}",
                    row.why, raise.reason
                ))
            }
        }
        (Expect::Raise(reason), other) => Err(format!(
            "{label} must raise {reason} ({}), got {other:?}",
            row.why
        )),
        (Expect::BadArg, Err(BeamError::BadArg)) => Ok(()),
        (Expect::BadArg, other) => Err(format!(
            "{label} must answer `BeamError::BadArg` ({}), got {other:?}",
            row.why
        )),
        (Expect::Value(check, description), Ok(term)) => {
            if check(ctx.atoms(), term) {
                Ok(())
            } else {
                Err(format!(
                    "{label} must answer {description} ({}), got a different term",
                    row.why
                ))
            }
        }
        (Expect::Value(_, description), Err(err)) => Err(format!(
            "{label} must answer {description} ({}), got the error {err:?}",
            row.why
        )),
    }
}

/// Every row of `class`.
fn rows_of(class: Class) -> impl Iterator<Item = &'static Row> {
    CENSUS.iter().filter(move |row| row.class == class)
}

/// The census keys of `class`.
fn keys_of(class: Class) -> BTreeSet<Key> {
    rows_of(class)
        .map(|row| (row.module, row.function, row.arity))
        .collect()
}

// ---------------------------------------------------------------------------
// Assertions.
// ---------------------------------------------------------------------------

/// **Guard 1** — every row names a key the registry actually holds, and no key is
/// listed twice (a duplicated row would let a stale pin hide behind a fresh one).
#[test]
fn every_row_is_registered_and_unique() {
    let reg = default_registry();
    let mut seen: BTreeSet<Key> = BTreeSet::new();
    for row in CENSUS {
        let key = (row.module, row.function, row.arity);
        assert!(
            reg.contains(row.module, row.function, row.arity),
            "the census names {}:{}/{} which `default_registry()` does not hold",
            row.module,
            row.function,
            row.arity
        );
        assert!(
            seen.insert(key),
            "{}:{}/{} is listed twice in the census",
            row.module,
            row.function,
            row.arity
        );
    }
    // The two skipped rows must be in the file as rows, not merely in `SKIPPED`.
    for &key in SKIPPED {
        assert!(
            seen.contains(&key),
            "{key:?} is declared skipped but has no census row"
        );
    }
}

/// **Guard 2** — every row's observed outcome is exactly the pinned one.
///
/// Equality is on the error *class and reason* (a `Raise`'s reason atom, or
/// `BadArg`) and, for a value, on the row's predicate — never on a term word, which
/// is legitimately heap-dependent.
#[test]
fn every_row_answers_its_pinned_outcome() {
    let reg = default_registry();
    let mut failures: Vec<String> = Vec::new();
    for row in CENSUS {
        if let Err(mismatch) = verify_row(&reg, row) {
            failures.push(mismatch);
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} census rows disagree with the code:\n  {}",
        failures.len(),
        CENSUS.len(),
        failures.join("\n  ")
    );
}

/// **Guard 3 (coverage)** — the class-B rows are *exactly* the union of the eleven
/// gap-defaulting `hook_sets` tables, deduped, minus the documented default rows.
///
/// This is the drift alarm the whole file exists for: a key added to (or removed
/// from) a seam table, or a key whose seam grows an in-crate implementation, breaks
/// this equation until the census — and its class — is updated.
#[test]
fn the_class_b_rows_are_the_seam_union_minus_the_documented_defaults() {
    let mut universe: BTreeSet<Key> = BTreeSet::new();
    for (name, table, predicate) in CLASS_B_TABLES {
        for &key in *table {
            assert!(
                predicate(key.0, key.1, key.2),
                "{name} lists {key:?} but its own `needs_*` predicate rejects it"
            );
            universe.insert(key);
        }
    }
    let defaults = keys_of(Class::D);
    let expected: BTreeSet<Key> = universe.difference(&defaults).copied().collect();
    let class_b = keys_of(Class::B);
    let missing: Vec<&Key> = expected.difference(&class_b).collect();
    let extra: Vec<&Key> = class_b.difference(&expected).collect();
    assert!(
        missing.is_empty(),
        "seam-gated keys missing from class B: {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "class-B rows that no gap-defaulting seam table lists: {extra:?}"
    );
    // The overlap is exactly the four `LOCAL_FUN_IDENTITY_BIFS` **encoders**, whose
    // uninstalled default is `Ok(None)`, so they are class-D rows; the table's five
    // renderers default to the typed gap instead and are ordinary class-B rows.
    let overlap: BTreeSet<Key> = universe.intersection(&defaults).copied().collect();
    let encoders: BTreeSet<Key> = [
        ("erlang", "term_to_binary", 1),
        ("erlang", "term_to_binary", 2),
        ("erlang", "external_size", 1),
        ("erlang", "external_size", 2),
    ]
    .into_iter()
    .collect();
    let local_fun_identity: BTreeSet<Key> = LOCAL_FUN_IDENTITY_BIFS.iter().copied().collect();
    assert_eq!(
        overlap, encoders,
        "the class-D rows that a class-B table lists must be exactly the four `LOCAL_FUN_IDENTITY_BIFS` encoders"
    );
    assert!(
        overlap.is_subset(&local_fun_identity),
        "every overlapping class-D row must be a `LOCAL_FUN_IDENTITY_BIFS` member"
    );
    // ... and every class-D row *does* read some seam: none of them is a stray.
    for &key in &defaults {
        assert!(
            reads_a_seam(key).is_some(),
            "{key:?} is a class-D row but reads no seam"
        );
    }
}

/// **Guard 4 (disjointness)** — a class-A or class-C row is *host-independent*: no
/// `hook_sets` table may list it, so no seam can change its answer.
#[test]
fn class_a_and_class_c_are_disjoint_from_the_seam_union() {
    for class in [Class::A, Class::C] {
        for row in rows_of(class) {
            let key = (row.module, row.function, row.arity);
            assert_eq!(
                reads_a_seam(key),
                None,
                "{:?} row {}:{}/{} must read no seam (it is listed by {})",
                class,
                row.module,
                row.function,
                row.arity,
                reads_a_seam(key).unwrap_or("-")
            );
        }
    }
    // Class D is disjoint from classes A, B and C by construction (a key is one row).
    assert_eq!(keys_of(Class::A).intersection(&keys_of(Class::B)).count(), 0);
    assert_eq!(keys_of(Class::A).intersection(&keys_of(Class::C)).count(), 0);
    assert_eq!(keys_of(Class::A).intersection(&keys_of(Class::D)).count(), 0);
    assert_eq!(keys_of(Class::B).intersection(&keys_of(Class::C)).count(), 0);
    assert_eq!(keys_of(Class::B).intersection(&keys_of(Class::D)).count(), 0);
    assert_eq!(keys_of(Class::C).intersection(&keys_of(Class::D)).count(), 0);
}

/// The census' own witness figures — the direction of a drift, not a second source
/// of truth (the coverage guards above are authoritative).
///
/// | class | rows | meaning |
/// |---|---|---|
/// | A | 4 | closed in-crate |
/// | B | 211 | `215` seam-table keys − the `4` `LOCAL_FUN_IDENTITY_BIFS` defaults |
/// | C | 23 | `erl_debugger` 13 + `erts_debug` 8 + the 2 collector RPCs |
/// | D | 8 | `LOCAL_NODE_BIFS` 6 + `RECORD_SOURCE_BIFS` 1 + `EXPORT_GATE_BIFS` 1 |
#[test]
fn the_class_counts_are_pinned() {
    let counts = [
        (Class::A, 4usize),
        (Class::B, 211),
        (Class::C, 23),
        (Class::D, 8),
    ];
    for (class, expected) in counts {
        assert_eq!(
            rows_of(class).count(),
            expected,
            "{class:?} drifted from {expected} rows — re-derive it from the code, do not patch the number"
        );
    }
    assert_eq!(
        CENSUS.len(),
        counts.iter().map(|(_, n)| n).sum::<usize>(),
        "every row must carry exactly one class"
    );
}

/// The other leg of a `FUN_CALLER_BIFS` key: an **invalid** fun never reaches the
/// seam.
///
/// A fun-taking BIF's clause-head guard runs first, so the typed gap the census pins
/// is only reachable with a *valid* fun of the right arity — the two families even
/// report the guard failure differently (`lists.erl` raises `function_clause`,
/// `maps.erl` raises `badarg`), which is why the census' argument builder goes to the
/// trouble of minting a real `EXPORT_EXT` fun.
#[test]
fn a_fun_taking_key_pins_both_legs() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let not_a_fun = ctx.int(5).expect("fits");
    let list = list12(&mut ctx);

    // Leg 1 — a valid fun: the typed gap (already pinned by the census row).
    let fun = external_fun(&mut ctx, 1);
    match reg.call("lists", "map", 2, &mut ctx, &[fun, list]) {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason,
            ExitReason::atom("unsupported_bif"),
            "a valid fun must reach the FunCaller seam"
        ),
        other => panic!("lists:map/2 with a valid fun must be the typed gap, got {other:?}"),
    }

    // Leg 2 — the clause guard, before the seam: `lists` raises `function_clause` …
    match reg.call("lists", "map", 2, &mut ctx, &[not_a_fun, list]) {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason,
            ExitReason::atom("function_clause"),
            "lists:map/2's guard is the oracle's function_clause"
        ),
        other => panic!("lists:map/2 with a non-fun must raise function_clause, got {other:?}"),
    }

    // … and `maps` raises the plain `badarg` (a different family, a different guard).
    let map = one_pair_map(&mut ctx);
    match reg.call("maps", "map", 2, &mut ctx, &[not_a_fun, map]) {
        Err(BeamError::BadArg) => {}
        other => panic!("maps:map/2 with a non-fun must be badarg, got {other:?}"),
    }
}

/// The `LocalNode` seam's **`local_node_hash`** half has no fallback, so
/// `term_to_binary(Term, [local])` is the typed gap even though the identity half does
/// fall back to `nonode@nohost`.
///
/// This is the one place where a class-D key's seam is *partly* a gap: the census row
/// pins the `[]`-options leg (a value), and this test pins the `[local]` leg (the
/// gap) — the two together are the seam's complete uninstalled behaviour.
#[test]
fn the_local_option_is_the_documented_gap() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let term = ctx.int(1).expect("fits");
    let local = atom(&mut ctx, "local");
    let options = ctx.list(&[local]).expect("fits");
    match reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, options]) {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason,
            ExitReason::atom("unsupported_bif"),
            "`[local]` needs a `local_node_hash` no host-side seam can invent"
        ),
        other => panic!("term_to_binary(1, [local]) must be the typed gap, got {other:?}"),
    }
}
