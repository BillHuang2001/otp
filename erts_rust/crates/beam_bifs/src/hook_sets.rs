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

//! The curated "hook-needed" BIF sets: exactly which registered BIFs read which
//! optional [`BifContext`] seam.
//!
//! # What these sets are
//!
//! A host keeps its optional `beam_bifs` seams behind *gates*: for the curried
//! `(module, function, arity)` of a BIF call it decides whether the seam has to be
//! built and installed on the [`BifContext`] before it dispatches. Most of these
//! seams have a small, fixed consumer set, so the gate is a list membership test.
//! This module is the one authoritative home for those sets: a `&'static` table per
//! seam plus an allocation-free `needs_*` predicate over it.
//!
//! The seams covered here, each with its consumer table:
//!
//! | seam | table | keys |
//! |---|---|---|
//! | [`MemoryHook`] | [`MEMORY_HOOK_BIFS`] | 2 |
//! | [`LocalFunIdentity`] | [`LOCAL_FUN_IDENTITY_BIFS`] | 4 |
//! | [`DictHook`] | [`DICT_HOOK_BIFS`] | 7 |
//! | [`ProcessHook`] | [`PROCESS_HOOK_BIFS`] | 11 |
//! | [`PortHook`] | [`PORT_HOOK_BIFS`] | 4 |
//! | [`SendHook`] | [`SEND_HOOK_BIFS`] | 2 |
//! | [`NifLoader`] | [`NIF_LOADER_BIFS`] | 1 |
//! | [`RecordSource`] | [`RECORD_SOURCE_BIFS`] | 1 |
//! | [`ModuleInfo`] | [`MODULE_INFO_BIFS`] | 2 |
//! | [`PersistentTerm`] | [`PERSISTENT_TERM_BIFS`] | 7 |
//! | [`FunCaller`] | [`FUN_CALLER_BIFS`] | 22 |
//! | [`LocalNode`] | [`LOCAL_NODE_BIFS`] | 6 |
//! | [`RuntimeHooks`] | [`RUNTIME_HOOKS_BIFS`] | 195 |
//!
//! # Why the sets live **here**, not in the host
//!
//! The seam *consumers* are BIF implementations inside this crate. If a host
//! re-derived these sets privately it would silently **drift** the moment this crate
//! changed which BIFs read a seam, and the drift is invisible in both directions:
//!
//! * a **too-narrow** gate leaves a BIF that *does* need the seam with the typed
//!   `unsupported_bif` / `Ok(None)` capability gap — a working BIF turned into a
//!   failure;
//! * a **too-wide** gate builds an expensive host bridge on BIFs that never touch
//!   the seam.
//!
//! Keeping the one authoritative list next to the call sites makes the dependency
//! explicit and testable: the tests below pin the exact contents, and — for the
//! seams whose *whole* defining module is the seam — assert set **equality** against
//! the module's own `register`, so renaming, removing or adding a BIF on the registry
//! side fails loudly here rather than degrading at runtime. Every listed key is also
//! asserted to be registered in [`default_registry`](crate::registry::default_registry).
//!
//! # The seam that is deliberately **not** here: `AtomInterner`
//!
//! The [`AtomInterner`] seam has no bounded consumer set, so no table is exported for
//! it and one must not be invented. [`BifContext::intern_atom`] is a *primitive*
//! reached wherever a BIF turns an arbitrary runtime string into an atom term:
//! `list_to_atom/1`/`binary_to_atom/1,2` (a user-supplied name), `binary_to_term/1,2`
//! (an atom in the wire image), `records:get_definition/2` (`ctx.intern_atom` of every
//! field name of the definition), the `binary:compile_pattern/1` tags, and
//! [`crate::error::structured_error`](crate::error) — the raise helper interns an
//! arbitrary tag for every structured reason a BIF can produce. That set is not a
//! fixed roster of BIFs; it is "any BIF that may intern a name the table does not
//! already hold", which is unbounded and cannot be enumerated. A host must therefore
//! either install the seam unconditionally (a cheap, allocation-free `Rc`-rooted
//! interner, as `beam_rt` does) or accept the `TermNotFound` degradation — never gate
//! it on a fabricated finite list.
//!
//! # Adoption (host crates)
//!
//! A host that currently re-derives these gates should delegate to the predicates
//! here instead of carrying a private copy — `beam_rt::RuntimeContext`'s private
//! `uses_*` methods map one-to-one:
//!
//! | private host gate | public predicate |
//! |---|---|
//! | `RuntimeContext::uses_memory_hook` | [`needs_memory_hook`] |
//! | `RuntimeContext::uses_fun_identity` | [`needs_local_fun_identity`] |
//! | `RuntimeContext::uses_dict` | [`needs_dict_hook`] |
//! | `RuntimeContext::uses_process_hook` | [`needs_process_hook`] |
//! | `RuntimeContext::uses_nif_loader` | [`needs_nif_loader`] |
//! | `RuntimeContext::uses_record_source` | [`needs_record_source`] |
//! | `RuntimeContext::uses_send_hook` | [`needs_send_hook`] |
//! | `RuntimeContext::uses_module_info` | [`needs_module_info`] |
//! | `RuntimeContext::uses_persistent_term` | [`needs_persistent_term`] |
//! | `RuntimeContext::uses_runtime_hooks` (a 10-key subset today) | [`needs_runtime_hooks`] (the whole 195-key seam) |
//! | — (the port hook is installed unconditionally today) | [`needs_port_hook`] |
//! | — (the fun caller is gated by `reentrancy_capable(args)` today) | [`needs_fun_caller`] |
//! | — (the local node is installed unconditionally today) | [`needs_local_node`] |
//!
//! # The cost asymmetries that make the gates load-bearing
//!
//! These predicates exist because *eagerly* building a bridge is expensive — the
//! bridges that need `&mut Runtime`/`&mut ProcessTable` cannot even co-exist in one
//! `call_registered` frame — so it must not happen on the ordinary BIF dispatch path:
//!
//! * the **memory bridge** folds a `.beam` size per loaded module — one `stat`
//!   syscall each — so building it eagerly costs on the order of ~10 µs **per call**;
//! * the **fun-identity bridge** clones an `Arc` and allocates a `PathBuf`, so it too
//!   is a real allocation on every call if installed unconditionally;
//! * the **record-source**, **module-info** and **runtime-hooks** bridges borrow the
//!   code server / timer heap of the whole runtime, and the **dict**/**process**/
//!   **send** bridges need the running process — so each is built only for the keys
//!   that actually read it.
//!
//! Every predicate is an allocation-free `binary_search` membership test over the
//! `&'static` tables, so calling one on every dispatch is free.
//!
//! [`BifContext::intern_atom`]: crate::context::BifContext::intern_atom

/// The registered BIFs that read the [`MemoryHook`] seam — the only consumers of
/// [`BifContext::memory`](crate::context::BifContext::memory).
///
/// Sorted by `(module, function, arity)`; see [`needs_memory_hook`].
pub const MEMORY_HOOK_BIFS: &[(&str, &str, u32)] = &[
    ("erlang", "memory", 0),
    ("erlang", "memory", 1),
];

/// The registered BIFs that read the [`LocalFunIdentity`] seam — the only callers
/// reaching `binconv::encode_body`, which installs a local fun's `NEW_FUN_EXT`
/// identity on the external-term encoder.
///
/// `erlang:term_to_binary/1,2` reach it through `encode_image`/`encode_body`;
/// `erlang:external_size/1,2` reach it through `plain_image_of`, which shares the
/// same `encode_body`. Sorted by `(module, function, arity)`; see
/// [`needs_local_fun_identity`].
pub const LOCAL_FUN_IDENTITY_BIFS: &[(&str, &str, u32)] = &[
    ("erlang", "external_size", 1),
    ("erlang", "external_size", 2),
    ("erlang", "term_to_binary", 1),
    ("erlang", "term_to_binary", 2),
];

/// The registered BIFs that read the [`DictHook`] seam — the whole process-dictionary
/// surface of `src/dict.rs`, each through a `dict_*` accessor.
///
/// The set is exactly `dict::register`'s roster (7 keys): every dict BIF routes
/// through the seam, so the module's registration and this table are asserted equal.
/// Sorted by `(module, function, arity)`; see [`needs_dict_hook`].
pub const DICT_HOOK_BIFS: &[(&str, &str, u32)] = &[
    ("erlang", "erase", 0),
    ("erlang", "erase", 1),
    ("erlang", "get", 0),
    ("erlang", "get", 1),
    ("erlang", "get_keys", 0),
    ("erlang", "get_keys", 1),
    ("erlang", "put", 2),
];

/// The registered BIFs that read the [`ProcessHook`] seam — the whole
/// process/name/global-state surface of `src/proc_reg.rs`, each through a `proc_*`
/// accessor.
///
/// The set is exactly `proc_reg::register`'s roster (11 keys): every key routes
/// through the seam, so the module's registration and this table are asserted equal.
/// Sorted by `(module, function, arity)`; see [`needs_process_hook`].
pub const PROCESS_HOOK_BIFS: &[(&str, &str, u32)] = &[
    ("erlang", "group_leader", 0),
    ("erlang", "is_process_alive", 1),
    ("erlang", "ports", 0),
    ("erlang", "process_flag", 2),
    ("erlang", "process_info", 1),
    ("erlang", "process_info", 2),
    ("erlang", "processes", 0),
    ("erlang", "register", 2),
    ("erlang", "registered", 0),
    ("erlang", "unregister", 1),
    ("erlang", "whereis", 1),
];

/// The registered BIFs that read the [`PortHook`] seam — the four `src/port.rs`
/// operations that consult the driver (open a port, close it, read its info).
///
/// `erlang:port_close/1` returns `true`, and `port_info/1,2` render the hook's
/// `{Item, Value}` pairs; `erlang:list_to_port/1`/`port_to_list/1` are **pure**
/// (`#Port<0.Num>` text) and do not read the seam, so they are deliberately absent
/// even though `port::register` installs them. Sorted by `(module, function,
/// arity)`; see [`needs_port_hook`].
pub const PORT_HOOK_BIFS: &[(&str, &str, u32)] = &[
    ("erlang", "open_port", 2),
    ("erlang", "port_close", 1),
    ("erlang", "port_info", 1),
    ("erlang", "port_info", 2),
];

/// The registered BIFs that read the [`SendHook`] seam — `erlang:send/2` and its
/// quoted alias `erlang:'!'/2`, the same `src/proc.rs` implementation.
///
/// `erlang:self/0` shares the module but reads only the injected
/// [`BifContext::self_pid`](crate::context::BifContext::self_pid), so it is absent.
/// Sorted by `(module, function, arity)`; see [`needs_send_hook`].
pub const SEND_HOOK_BIFS: &[(&str, &str, u32)] = &[
    ("erlang", "!", 2),
    ("erlang", "send", 2),
];

/// The registered BIFs that read the [`NifLoader`] seam — `erlang:load_nif/2`, the
/// only consumer of [`BifContext::nif_loader`](crate::context::BifContext::nif_loader).
///
/// The set is exactly `nif::register`'s roster (one key). Sorted by `(module,
/// function, arity)`; see [`needs_nif_loader`].
pub const NIF_LOADER_BIFS: &[(&str, &str, u32)] = &[("erlang", "load_nif", 2)];

/// The registered BIFs that read the [`RecordSource`] seam — `records:get_definition/2`,
/// the only consumer of
/// [`BifContext::record_definition`](crate::context::BifContext::record_definition).
///
/// Every other `records:*` BIF operates on a record term it was handed (or on a
/// `Definition` the caller already resolved); only `get_definition/2` resolves a
/// definition by `(Module, Name)` through the hook. Sorted by `(module, function,
/// arity)`; see [`needs_record_source`].
pub const RECORD_SOURCE_BIFS: &[(&str, &str, u32)] = &[("records", "get_definition", 2)];

/// The registered BIFs that read the [`ModuleInfo`] seam — `erlang:get_module_info/1,2`,
/// the whole `src/modinfo.rs` surface, through
/// [`BifContext::module_info`](crate::context::BifContext::module_info).
///
/// The set is exactly `modinfo::register`'s roster (2 keys), so the module's
/// registration and this table are asserted equal. Sorted by `(module, function,
/// arity)`; see [`needs_module_info`].
pub const MODULE_INFO_BIFS: &[(&str, &str, u32)] = &[
    ("erlang", "get_module_info", 1),
    ("erlang", "get_module_info", 2),
];

/// The registered BIFs that read the [`PersistentTerm`] seam — the whole
/// `persistent_term:*` surface of `src/persistent_term.rs`, each through a
/// `persistent_term_*` accessor.
///
/// The set is exactly `persistent_term::register`'s roster (7 keys), so the module's
/// registration and this table are asserted equal. Sorted by `(module, function,
/// arity)`; see [`needs_persistent_term`].
pub const PERSISTENT_TERM_BIFS: &[(&str, &str, u32)] = &[
    ("persistent_term", "erase", 1),
    ("persistent_term", "get", 0),
    ("persistent_term", "get", 1),
    ("persistent_term", "get", 2),
    ("persistent_term", "info", 0),
    ("persistent_term", "put", 2),
    ("persistent_term", "put_new", 2),
];

/// The registered BIFs that read the [`FunCaller`] seam — every `lists`/`maps`
/// function that **applies a fun argument** back into the runtime.
///
/// The `lists` 18: `map/2`, `filter/2`, `foldl/3`, `foldr/3`, `foreach/2`, `sort/2`,
/// `usort/2`, `flatmap/2`, `zipwith/3`, `mapfoldl/3`, `mapfoldr/3`, `all/2`, `any/2`,
/// `takewhile/2`, `dropwhile/2`, `splitwith/2`, `partition/2`, `keymap/3`. The `maps` 4:
/// `map/2`, `fold/3`, `filter/2`, `foreach/2`. Every one of them registers a `BifFn`
/// whose body reaches [`BifContext::call_fun`](crate::context::BifContext::call_fun)
/// directly or through the shared `call_predicate`/`fun_order` helpers. The
/// non-fun-taking `lists`/`maps` fast paths in the same modules do not read the seam.
///
/// Note: a host that gates the fun caller cannot use this table alone — it must also
/// keep any *argument*-based guard it has (a `fun` argument is what makes a call
/// re-entrant for ETS-cell purposes). This table answers only "does this key read the
/// seam". Sorted by `(module, function, arity)`; see [`needs_fun_caller`].
pub const FUN_CALLER_BIFS: &[(&str, &str, u32)] = &[
    ("lists", "all", 2),
    ("lists", "any", 2),
    ("lists", "dropwhile", 2),
    ("lists", "filter", 2),
    ("lists", "flatmap", 2),
    ("lists", "foldl", 3),
    ("lists", "foldr", 3),
    ("lists", "foreach", 2),
    ("lists", "keymap", 3),
    ("lists", "map", 2),
    ("lists", "mapfoldl", 3),
    ("lists", "mapfoldr", 3),
    ("lists", "partition", 2),
    ("lists", "sort", 2),
    ("lists", "splitwith", 2),
    ("lists", "takewhile", 2),
    ("lists", "usort", 2),
    ("lists", "zipwith", 3),
    ("maps", "filter", 2),
    ("maps", "fold", 3),
    ("maps", "foreach", 2),
    ("maps", "map", 2),
];

/// The registered BIFs that read the [`LocalNode`] seam — the BIFs that need the
/// local node's name/creation, through
/// [`BifContext::local_node`](crate::context::BifContext::local_node).
///
/// `erlang:node/0` is the local node's name atom; `erlang:node/1` reads it for an
/// **internal** pid/port/reference (an external one carries its own node in the term);
/// `erlang:term_to_binary/1,2` and `erlang:external_size/1,2` read it in `encode_body`
/// to install the node identity for an internal pid/port id. Sorted by `(module,
/// function, arity)`; see [`needs_local_node`].
pub const LOCAL_NODE_BIFS: &[(&str, &str, u32)] = &[
    ("erlang", "external_size", 1),
    ("erlang", "external_size", 2),
    ("erlang", "node", 0),
    ("erlang", "node", 1),
    ("erlang", "term_to_binary", 1),
    ("erlang", "term_to_binary", 2),
];

/// The registered BIFs that read the [`RuntimeHooks`] seam — every key registered
/// through the `runtime_bif!` macro in `src/runtime/`, each bridging to
/// [`BifContext::call_runtime_bif`](crate::context::BifContext::call_runtime_bif).
///
/// The union of the four `runtime_bif!` family modules: `runtime/erlang.rs` (77),
/// `runtime/erts_internal.rs` (56), `runtime/erts_debug.rs` (18) and
/// `runtime/other.rs` (44) = **195** keys. The `erlang:memory/0,1` pair in
/// `runtime/memory.rs` is registered against the separate [`MEMORY_HOOK_BIFS`] seam,
/// the 13 `atomics`/`counters` resource-object keys are real implementations
/// registered by `runtime/atomics.rs` ([`crate::runtime::atomics::REAL_BIFS`]), and
/// `erl_ddll:format_error_int/1` is a real implementation registered by
/// `runtime/ddll.rs` ([`crate::runtime::ddll::REAL_BIFS`]) — so none of those groups
/// is here.
///
/// The set is exactly `runtime::register`'s roster minus those three groups, so the
/// module's registration and this table are asserted **equal** — the strongest drift
/// guard in this module, and the reason the whole table can be trusted to be complete
/// rather than merely sound. Sorted by `(module, function, arity)`; see
/// [`needs_runtime_hooks`].
pub const RUNTIME_HOOKS_BIFS: &[(&str, &str, u32)] = &[
    ("code", "coverage_support", 0),
    ("code", "get_coverage", 2),
    ("code", "get_coverage_mode", 0),
    ("code", "get_coverage_mode", 1),
    ("code", "get_debug_info", 1),
    ("code", "reset_coverage", 1),
    ("code", "set_coverage_mode", 1),
    ("erl_ddll", "demonitor", 1),
    ("erl_ddll", "info", 2),
    ("erl_ddll", "loaded_drivers", 0),
    ("erl_ddll", "monitor", 2),
    ("erl_ddll", "try_load", 3),
    ("erl_ddll", "try_unload", 2),
    ("erl_debugger", "breakpoint", 3),
    ("erl_debugger", "breakpoints", 1),
    ("erl_debugger", "breakpoints", 3),
    ("erl_debugger", "instrumentations", 0),
    ("erl_debugger", "peek_stack_frame_slot", 4),
    ("erl_debugger", "peek_xreg", 3),
    ("erl_debugger", "register", 1),
    ("erl_debugger", "stack_frames", 2),
    ("erl_debugger", "supported", 0),
    ("erl_debugger", "toggle_instrumentations", 1),
    ("erl_debugger", "unregister", 2),
    ("erl_debugger", "whereis", 0),
    ("erl_debugger", "xregs_count", 1),
    ("erlang", "alias", 1),
    ("erlang", "bump_reductions", 1),
    ("erlang", "call_on_load_function", 1),
    ("erlang", "cancel_timer", 1),
    ("erlang", "cancel_timer", 2),
    ("erlang", "check_old_code", 1),
    ("erlang", "delete_module", 1),
    ("erlang", "demonitor", 1),
    ("erlang", "demonitor", 2),
    ("erlang", "dist_ctrl_get_data", 1),
    ("erlang", "dist_ctrl_get_data_notification", 1),
    ("erlang", "dist_ctrl_get_opt", 2),
    ("erlang", "dist_ctrl_input_handler", 2),
    ("erlang", "dist_ctrl_put_data", 2),
    ("erlang", "dist_ctrl_set_opt", 3),
    ("erlang", "dist_get_stat", 1),
    ("erlang", "dt_append_vm_tag_data", 1),
    ("erlang", "dt_get_tag", 0),
    ("erlang", "dt_get_tag_data", 0),
    ("erlang", "dt_prepend_vm_tag_data", 1),
    ("erlang", "dt_put_tag", 1),
    ("erlang", "dt_restore_tag", 1),
    ("erlang", "dt_spread_tag", 1),
    ("erlang", "exit", 2),
    ("erlang", "exit", 3),
    ("erlang", "exit_signal", 2),
    ("erlang", "exit_signal", 3),
    ("erlang", "finish_after_on_load", 2),
    ("erlang", "finish_loading", 1),
    ("erlang", "function_exported", 3),
    ("erlang", "halt", 2),
    ("erlang", "has_prepared_code_on_load", 1),
    ("erlang", "hibernate", 3),
    ("erlang", "is_builtin", 3),
    ("erlang", "link", 1),
    ("erlang", "link", 2),
    ("erlang", "loaded", 0),
    ("erlang", "make_ref", 0),
    ("erlang", "module_loaded", 1),
    ("erlang", "monitor", 2),
    ("erlang", "monitor", 3),
    ("erlang", "monitor_node", 2),
    ("erlang", "monitor_node", 3),
    ("erlang", "nodes", 0),
    ("erlang", "nodes", 1),
    ("erlang", "nodes", 2),
    ("erlang", "port_get_data", 1),
    ("erlang", "port_set_data", 2),
    ("erlang", "pre_loaded", 0),
    ("erlang", "read_timer", 1),
    ("erlang", "read_timer", 2),
    ("erlang", "resume_process", 1),
    ("erlang", "send", 3),
    ("erlang", "send_after", 3),
    ("erlang", "send_after", 4),
    ("erlang", "seq_trace", 2),
    ("erlang", "seq_trace_info", 1),
    ("erlang", "seq_trace_print", 1),
    ("erlang", "seq_trace_print", 2),
    ("erlang", "setnode", 2),
    ("erlang", "spawn", 3),
    ("erlang", "spawn_link", 3),
    ("erlang", "spawn_opt", 4),
    ("erlang", "spawn_request_abandon", 1),
    ("erlang", "start_timer", 3),
    ("erlang", "start_timer", 4),
    ("erlang", "statistics", 1),
    ("erlang", "system_flag", 2),
    ("erlang", "system_info", 1),
    ("erlang", "system_profile", 0),
    ("erlang", "system_profile", 2),
    ("erlang", "term_to_iovec", 1),
    ("erlang", "term_to_iovec", 2),
    ("erlang", "trace_delivered", 1),
    ("erlang", "trace_info", 2),
    ("erlang", "unalias", 1),
    ("erlang", "unlink", 1),
    ("error_logger", "warning_map", 0),
    ("erts_debug", "breakpoint", 2),
    ("erts_debug", "copy_shared", 2),
    ("erts_debug", "dirty", 3),
    ("erts_debug", "dirty_cpu", 2),
    ("erts_debug", "dirty_io", 2),
    ("erts_debug", "disassemble", 1),
    ("erts_debug", "dist_ext_to_term", 2),
    ("erts_debug", "get_internal_state", 1),
    ("erts_debug", "instructions", 0),
    ("erts_debug", "interpreter_size", 0),
    ("erts_debug", "lcnt_clear", 0),
    ("erts_debug", "lcnt_collect", 0),
    ("erts_debug", "lcnt_control", 1),
    ("erts_debug", "lcnt_control", 2),
    ("erts_debug", "map_info", 1),
    ("erts_debug", "set_internal_state", 2),
    ("erts_debug", "size_shared", 1),
    ("erts_debug", "unaligned_bitstring", 2),
    ("erts_internal", "abort_pending_connection", 2),
    ("erts_internal", "beamfile_chunk", 2),
    ("erts_internal", "beamfile_module_md5", 1),
    ("erts_internal", "check_dirty_process_code", 2),
    ("erts_internal", "check_process_code", 1),
    ("erts_internal", "create_dist_channel", 3),
    ("erts_internal", "dirty_process_handle_signals", 1),
    ("erts_internal", "dist_spawn_request", 4),
    ("erts_internal", "erase_persistent_terms", 0),
    ("erts_internal", "ets_lookup_binary_info", 2),
    ("erts_internal", "ets_raw_first", 1),
    ("erts_internal", "ets_raw_next", 2),
    ("erts_internal", "ets_super_user", 1),
    ("erts_internal", "garbage_collect", 1),
    ("erts_internal", "gather_alloc_histograms", 1),
    ("erts_internal", "gather_carrier_info", 1),
    ("erts_internal", "get_creation", 0),
    ("erts_internal", "get_dflags", 0),
    ("erts_internal", "group_leader", 2),
    ("erts_internal", "group_leader", 3),
    ("erts_internal", "is_process_alive", 2),
    ("erts_internal", "is_process_executing_dirty", 1),
    ("erts_internal", "is_system_process", 1),
    ("erts_internal", "new_connection", 1),
    ("erts_internal", "no_aux_work_threads", 0),
    ("erts_internal", "notify_breakpoint_hit", 3),
    ("erts_internal", "open_port", 2),
    ("erts_internal", "port_call", 3),
    ("erts_internal", "port_close", 1),
    ("erts_internal", "port_command", 3),
    ("erts_internal", "port_connect", 2),
    ("erts_internal", "port_control", 3),
    ("erts_internal", "port_info", 1),
    ("erts_internal", "port_info", 2),
    ("erts_internal", "prepare_loading", 2),
    ("erts_internal", "process_display", 2),
    ("erts_internal", "process_flag", 3),
    ("erts_internal", "processes_next", 1),
    ("erts_internal", "purge_module", 2),
    ("erts_internal", "request_system_task", 3),
    ("erts_internal", "request_system_task", 4),
    ("erts_internal", "scheduler_wall_time", 1),
    ("erts_internal", "spawn_request", 4),
    ("erts_internal", "spawn_system_process", 3),
    ("erts_internal", "suspend_process", 2),
    ("erts_internal", "system_check", 1),
    ("erts_internal", "system_monitor", 1),
    ("erts_internal", "system_monitor", 3),
    ("erts_internal", "term_to_string", 2),
    ("erts_internal", "trace", 3),
    ("erts_internal", "trace", 4),
    ("erts_internal", "trace_info", 3),
    ("erts_internal", "trace_pattern", 3),
    ("erts_internal", "trace_pattern", 4),
    ("erts_internal", "trace_session_create", 3),
    ("erts_internal", "trace_session_destroy", 1),
    ("erts_literal_area_collector", "release_area_switch", 0),
    ("erts_literal_area_collector", "send_copy_request", 3),
    ("erts_trace_cleaner", "check", 0),
    ("erts_trace_cleaner", "send_trace_clean_signal", 1),
    ("ets", "internal_request_all", 0),
    ("ets", "slot", 2),
    ("net_kernel", "dflag_unicode_io", 1),
    ("os", "set_signal", 2),
    ("prim_file", "internal_normalize_utf8", 1),
    ("re", "compile", 1),
    ("re", "compile", 2),
    ("re", "import", 1),
    ("re", "inspect", 2),
    ("re", "internal_run", 4),
    ("re", "run", 2),
    ("re", "run", 3),
    ("re", "version", 0),
];

/// Whether `(module, function, arity)` is one of the [`MEMORY_HOOK_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_memory_hook(module: &str, function: &str, arity: u32) -> bool {
    MEMORY_HOOK_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`LOCAL_FUN_IDENTITY_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_local_fun_identity(module: &str, function: &str, arity: u32) -> bool {
    LOCAL_FUN_IDENTITY_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`DICT_HOOK_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_dict_hook(module: &str, function: &str, arity: u32) -> bool {
    DICT_HOOK_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`PROCESS_HOOK_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_process_hook(module: &str, function: &str, arity: u32) -> bool {
    PROCESS_HOOK_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`PORT_HOOK_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_port_hook(module: &str, function: &str, arity: u32) -> bool {
    PORT_HOOK_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`SEND_HOOK_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_send_hook(module: &str, function: &str, arity: u32) -> bool {
    SEND_HOOK_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`NIF_LOADER_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_nif_loader(module: &str, function: &str, arity: u32) -> bool {
    NIF_LOADER_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`RECORD_SOURCE_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_record_source(module: &str, function: &str, arity: u32) -> bool {
    RECORD_SOURCE_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`MODULE_INFO_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_module_info(module: &str, function: &str, arity: u32) -> bool {
    MODULE_INFO_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`PERSISTENT_TERM_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_persistent_term(module: &str, function: &str, arity: u32) -> bool {
    PERSISTENT_TERM_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`FUN_CALLER_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_fun_caller(module: &str, function: &str, arity: u32) -> bool {
    FUN_CALLER_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`LOCAL_NODE_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_local_node(module: &str, function: &str, arity: u32) -> bool {
    LOCAL_NODE_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

/// Whether `(module, function, arity)` is one of the [`RUNTIME_HOOKS_BIFS`].
///
/// Allocation-free (a `binary_search` over the `&'static` table), so it is safe to
/// call on every BIF dispatch.
#[must_use]
pub fn needs_runtime_hooks(module: &str, function: &str, arity: u32) -> bool {
    RUNTIME_HOOKS_BIFS
        .binary_search(&(module, function, arity))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::registry::{default_registry, BifRegistry};

    /// A `needs_*` predicate.
    type Pred = fn(&str, &str, u32) -> bool;

    /// Every exported table with its name and predicate, so the shape tests are
    /// written once. The name is only for assertion messages.
    const TABLES: &[(&str, &[(&str, &str, u32)], Pred)] = &[
        ("MEMORY_HOOK_BIFS", MEMORY_HOOK_BIFS, needs_memory_hook),
        (
            "LOCAL_FUN_IDENTITY_BIFS",
            LOCAL_FUN_IDENTITY_BIFS,
            needs_local_fun_identity,
        ),
        ("DICT_HOOK_BIFS", DICT_HOOK_BIFS, needs_dict_hook),
        (
            "PROCESS_HOOK_BIFS",
            PROCESS_HOOK_BIFS,
            needs_process_hook,
        ),
        ("PORT_HOOK_BIFS", PORT_HOOK_BIFS, needs_port_hook),
        ("SEND_HOOK_BIFS", SEND_HOOK_BIFS, needs_send_hook),
        ("NIF_LOADER_BIFS", NIF_LOADER_BIFS, needs_nif_loader),
        (
            "RECORD_SOURCE_BIFS",
            RECORD_SOURCE_BIFS,
            needs_record_source,
        ),
        ("MODULE_INFO_BIFS", MODULE_INFO_BIFS, needs_module_info),
        (
            "PERSISTENT_TERM_BIFS",
            PERSISTENT_TERM_BIFS,
            needs_persistent_term,
        ),
        ("FUN_CALLER_BIFS", FUN_CALLER_BIFS, needs_fun_caller),
        ("LOCAL_NODE_BIFS", LOCAL_NODE_BIFS, needs_local_node),
        (
            "RUNTIME_HOOKS_BIFS",
            RUNTIME_HOOKS_BIFS,
            needs_runtime_hooks,
        ),
    ];

    /// Every per-module `register` body
    /// [`BifRegistry::with_core_bifs`](crate::registry::BifRegistry::with_core_bifs)
    /// runs, **in its exact order**, with a name for assertion messages.
    /// `runtime::register` is listed once (it fans out to its own family modules).
    const REGISTERS: &[(&str, fn(&mut BifRegistry))] = &[
        ("arith", crate::arith::register),
        ("bool", crate::bool::register),
        ("compare", crate::compare::register),
        ("type_test", crate::type_test::register),
        ("term_ops", crate::term_ops::register),
        ("convert", crate::convert::register),
        ("lists", crate::lists::register),
        ("maps", crate::maps::register),
        ("math", crate::math::register),
        ("binary", crate::binary::register),
        ("binconv", crate::binconv::register),
        ("unicode", crate::unicode::register),
        ("time", crate::time::register),
        ("phash", crate::phash::register),
        ("proc", crate::proc::register),
        ("dict", crate::dict::register),
        ("reason", crate::reason::register),
        ("string", crate::string::register),
        ("io", crate::io::register),
        ("ets", crate::ets::register),
        ("port", crate::port::register),
        ("records", crate::records::register),
        ("nif", crate::nif::register),
        ("checksum", crate::checksum::register),
        ("calendar", crate::calendar::register),
        ("os_bif", crate::os_bif::register),
        ("fun_info", crate::fun_info::register),
        ("term_conv", crate::term_conv::register),
        ("erts_internal", crate::erts_internal::register),
        ("modinfo", crate::modinfo::register),
        ("match_spec", crate::match_spec::register),
        ("packet", crate::packet::register),
        ("proc_reg", crate::proc_reg::register),
        ("persistent_term", crate::persistent_term::register),
        ("runtime", crate::runtime::register),
    ];

    /// The `(module, function, arity)` keys a `register` body installs, sorted.
    fn keys_of(register: impl FnOnce(&mut BifRegistry)) -> Vec<(String, String, u32)> {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        let mut keys = reg.names();
        keys.sort();
        keys
    }

    /// A table's keys, as owned sorted triples (the same order `binary_search` uses).
    fn sorted_keys(table: &[(&str, &str, u32)]) -> Vec<(String, String, u32)> {
        let mut keys: Vec<(String, String, u32)> = table
            .iter()
            .map(|&(m, f, a)| (m.to_string(), f.to_string(), a))
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn memory_hook_bifs_are_exactly_the_memory_pair() {
        assert_eq!(
            MEMORY_HOOK_BIFS,
            &[("erlang", "memory", 0), ("erlang", "memory", 1)]
        );
    }

    #[test]
    fn local_fun_identity_bifs_are_exactly_the_encoders() {
        // `encode_body` (binconv.rs) reads `ctx.local_fun_identity()`; it is reached
        // by `term_to_binary/1,2` (via `encode_image`) and by `external_size/1,2`
        // (via `plain_image_of`). Sorted by `(module, function, arity)`.
        assert_eq!(
            LOCAL_FUN_IDENTITY_BIFS,
            &[
                ("erlang", "external_size", 1),
                ("erlang", "external_size", 2),
                ("erlang", "term_to_binary", 1),
                ("erlang", "term_to_binary", 2),
            ]
        );
    }

    #[test]
    fn dict_hook_bifs_are_exactly_the_dict_roster() {
        assert_eq!(
            DICT_HOOK_BIFS,
            &[
                ("erlang", "erase", 0),
                ("erlang", "erase", 1),
                ("erlang", "get", 0),
                ("erlang", "get", 1),
                ("erlang", "get_keys", 0),
                ("erlang", "get_keys", 1),
                ("erlang", "put", 2),
            ]
        );
    }

    #[test]
    fn process_hook_bifs_are_exactly_the_proc_reg_roster() {
        assert_eq!(
            PROCESS_HOOK_BIFS,
            &[
                ("erlang", "group_leader", 0),
                ("erlang", "is_process_alive", 1),
                ("erlang", "ports", 0),
                ("erlang", "process_flag", 2),
                ("erlang", "process_info", 1),
                ("erlang", "process_info", 2),
                ("erlang", "processes", 0),
                ("erlang", "register", 2),
                ("erlang", "registered", 0),
                ("erlang", "unregister", 1),
                ("erlang", "whereis", 1),
            ]
        );
    }

    #[test]
    fn port_hook_bifs_are_the_four_driver_operations() {
        // `list_to_port/1` and `port_to_list/1` are pure and deliberately absent.
        assert_eq!(
            PORT_HOOK_BIFS,
            &[
                ("erlang", "open_port", 2),
                ("erlang", "port_close", 1),
                ("erlang", "port_info", 1),
                ("erlang", "port_info", 2),
            ]
        );
    }

    #[test]
    fn send_hook_bifs_are_both_send_names() {
        // `erlang:self/0` shares the module but reads only the injected pid.
        assert_eq!(
            SEND_HOOK_BIFS,
            &[("erlang", "!", 2), ("erlang", "send", 2)]
        );
    }

    #[test]
    fn nif_loader_bifs_are_load_nif_2() {
        assert_eq!(NIF_LOADER_BIFS, &[("erlang", "load_nif", 2)]);
    }

    #[test]
    fn record_source_bifs_are_get_definition_2() {
        assert_eq!(RECORD_SOURCE_BIFS, &[("records", "get_definition", 2)]);
    }

    #[test]
    fn module_info_bifs_are_the_two_get_module_info_arities() {
        assert_eq!(
            MODULE_INFO_BIFS,
            &[
                ("erlang", "get_module_info", 1),
                ("erlang", "get_module_info", 2),
            ]
        );
    }

    #[test]
    fn persistent_term_bifs_are_exactly_the_seven() {
        assert_eq!(
            PERSISTENT_TERM_BIFS,
            &[
                ("persistent_term", "erase", 1),
                ("persistent_term", "get", 0),
                ("persistent_term", "get", 1),
                ("persistent_term", "get", 2),
                ("persistent_term", "info", 0),
                ("persistent_term", "put", 2),
                ("persistent_term", "put_new", 2),
            ]
        );
    }

    #[test]
    fn fun_caller_bifs_are_the_fun_taking_surface() {
        // The 18 `lists` + 4 `maps` functions that apply a fun argument.
        assert_eq!(
            FUN_CALLER_BIFS,
            &[
                ("lists", "all", 2),
                ("lists", "any", 2),
                ("lists", "dropwhile", 2),
                ("lists", "filter", 2),
                ("lists", "flatmap", 2),
                ("lists", "foldl", 3),
                ("lists", "foldr", 3),
                ("lists", "foreach", 2),
                ("lists", "keymap", 3),
                ("lists", "map", 2),
                ("lists", "mapfoldl", 3),
                ("lists", "mapfoldr", 3),
                ("lists", "partition", 2),
                ("lists", "sort", 2),
                ("lists", "splitwith", 2),
                ("lists", "takewhile", 2),
                ("lists", "usort", 2),
                ("lists", "zipwith", 3),
                ("maps", "filter", 2),
                ("maps", "fold", 3),
                ("maps", "foreach", 2),
                ("maps", "map", 2),
            ]
        );
    }

    #[test]
    fn local_node_bifs_are_the_two_node_arities_and_the_encoders() {
        assert_eq!(
            LOCAL_NODE_BIFS,
            &[
                ("erlang", "external_size", 1),
                ("erlang", "external_size", 2),
                ("erlang", "node", 0),
                ("erlang", "node", 1),
                ("erlang", "term_to_binary", 1),
                ("erlang", "term_to_binary", 2),
            ]
        );
    }

    /// The tables are `binary_search`-ed, so each must be strictly ascending.
    #[test]
    fn every_table_is_sorted_and_duplicate_free() {
        for (name, table, _) in TABLES {
            for window in table.windows(2) {
                assert!(
                    window[0] < window[1],
                    "{name} must be strictly ascending, got {:?} then {:?}",
                    window[0],
                    window[1]
                );
            }
        }
    }

    /// Every listed key answers `true` through its predicate.
    #[test]
    fn predicates_accept_every_listed_key() {
        for (name, table, predicate) in TABLES {
            for (module, function, arity) in *table {
                assert!(
                    predicate(module, function, *arity),
                    "{name}'s predicate must accept the listed key {module}:{function}/{arity}"
                );
            }
        }
    }

    /// Drift guard: every listed key is actually registered, so renaming/removing a
    /// BIF on the registry side fails here instead of degrading at runtime.
    #[test]
    fn every_listed_key_is_registered() {
        let reg = default_registry();
        for (name, table, _) in TABLES {
            for (module, function, arity) in *table {
                assert!(
                    reg.contains(module, function, *arity),
                    "{name} lists {module}:{function}/{arity}, which is not registered"
                );
            }
        }
    }

    /// Strong drift guard for the seams whose **whole** defining module is the seam:
    /// the table is asserted **equal** to the module's own `register` roster, so a BIF
    /// added to (or removed from) the module cannot silently desynchronize the gate.
    #[test]
    fn whole_module_seams_match_their_registers_exactly() {
        let cases: &[(&str, &[(&str, &str, u32)], fn(&mut BifRegistry))] = &[
            ("DICT_HOOK_BIFS", DICT_HOOK_BIFS, crate::dict::register),
            (
                "PROCESS_HOOK_BIFS",
                PROCESS_HOOK_BIFS,
                crate::proc_reg::register,
            ),
            ("NIF_LOADER_BIFS", NIF_LOADER_BIFS, crate::nif::register),
            (
                "MODULE_INFO_BIFS",
                MODULE_INFO_BIFS,
                crate::modinfo::register,
            ),
            (
                "PERSISTENT_TERM_BIFS",
                PERSISTENT_TERM_BIFS,
                crate::persistent_term::register,
            ),
        ];
        for (name, table, register) in cases {
            assert_eq!(
                sorted_keys(table),
                keys_of(*register),
                "{name} must equal the keys its defining module registers"
            );
        }
    }

    /// The strongest guard: [`RUNTIME_HOOKS_BIFS`] is asserted **equal** to the roster
    /// of `src/runtime/`'s four `runtime_bif!` families, so the table is provably
    /// complete, not merely sound.
    ///
    /// `runtime::register` additionally installs the `erlang:memory/0,1` pair (which
    /// reads the separate [`MemoryHook`] seam), the 13 real `atomics`/`counters`
    /// resource-object keys and the real `erl_ddll:format_error_int/1` (which do not
    /// go through the `RuntimeHooks` seam at all), so all three groups are filtered out
    /// first.
    #[test]
    fn runtime_hooks_bifs_is_exactly_the_runtime_bif_roster() {
        let mut expected = keys_of(crate::runtime::register);
        expected.retain(|(m, f, a)| !MEMORY_HOOK_BIFS.contains(&(m.as_str(), f.as_str(), *a)));
        expected.retain(|(m, f, a)| {
            !crate::runtime::atomics::REAL_BIFS.contains(&(m.as_str(), f.as_str(), *a))
        });
        expected.retain(|(m, f, a)| {
            !crate::runtime::ddll::REAL_BIFS.contains(&(m.as_str(), f.as_str(), *a))
        });
        assert_eq!(
            sorted_keys(RUNTIME_HOOKS_BIFS),
            expected,
            "RUNTIME_HOOKS_BIFS must equal the runtime_bif! roster \
             (minus the memory pair and the real atomics/counters + erl_ddll keys)"
        );
    }

    /// The `runtime_bif!` roster size, pinned so a bulk edit cannot slip through
    /// unnoticed (the equality guard above is the authoritative check; this makes the
    /// magnitude explicit).
    #[test]
    fn runtime_hooks_bifs_has_the_expected_size() {
        assert_eq!(RUNTIME_HOOKS_BIFS.len(), 195);
    }

    /// The module-doc table at the top of this file is the human-readable record of the
    /// seam → table → key-count mapping; the tables are the code. This ties the two
    /// together by **parsing the doc out of this very source file**, so a count that
    /// drifts from its table (or a table missing from the doc) fails the build instead
    /// of misleading a host that sizes its gates from the doc.
    ///
    /// The doc row shape is `//! | [\`Seam\`] | [\`TABLE_BIFS\`] | N |`; rows naming a
    /// non-`_BIFS` symbol (the "private host gate → predicate" table) are ignored, and
    /// the "every exported table" assertion below keeps that filter from hiding a row
    /// that should have been matched.
    #[test]
    fn the_module_doc_table_counts_match_the_tables() {
        let src = include_str!("hook_sets.rs");
        let mut documented: Vec<(&str, usize)> = Vec::new();
        for line in src.lines() {
            let Some(row) = line.strip_prefix("//! |") else {
                continue;
            };
            let columns: Vec<&str> = row.split('|').map(str::trim).collect();
            if columns.len() < 3 {
                continue;
            }
            let Some(name) = columns[1]
                .strip_prefix("[`")
                .and_then(|cell| cell.strip_suffix("`]"))
            else {
                continue;
            };
            if !name.ends_with("_BIFS") {
                continue;
            }
            let count = columns[2]
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("the doc's key-count cell for {name} is not a number"));
            documented.push((name, count));
        }

        assert_eq!(
            documented.len(),
            TABLES.len(),
            "the module doc must document every seam table exactly once (documented: {documented:?})"
        );
        for (name, count) in documented {
            let (_, table, _) = TABLES
                .iter()
                .find(|(table_name, _, _)| *table_name == name)
                .unwrap_or_else(|| {
                    panic!("the doc documents `{name}`, which is not an exported seam table")
                });
            assert_eq!(
                count,
                table.len(),
                "`{name}` is documented as {count} keys but the table holds {}",
                table.len()
            );
        }
    }

    /// The registration bodies are **pairwise disjoint**: [`BifRegistry::register`]
    /// keeps the *last* implementation registered for a key, so an overlap between two
    /// bodies silently *shadows* the earlier one — the later implementation wins and the
    /// earlier is unreachable, with nothing else failing.
    ///
    /// This is the guard against the ordering trap `with_core_bifs` sets up: the `ets`
    /// bridge (a key-per-verb body) runs before the `runtime` family, so a key both
    /// bodies installed would resolve to the runtime one rather than to the ets bridge,
    /// and vice versa for any earlier/later pair.
    #[test]
    fn no_registration_body_shadows_another() {
        let mut owner: std::collections::HashMap<(String, String, u32), &str> =
            std::collections::HashMap::new();
        for (name, register) in REGISTERS {
            for key in keys_of(*register) {
                if let Some(previous) = owner.insert(key.clone(), name) {
                    panic!(
                        "{key:?} is installed by both `{previous}` and `{name}`: \
                         `register` keeps the last one, so `{previous}`'s implementation is shadowed"
                    );
                }
            }
        }
    }

    /// [`REGISTERS`] is the whole of `with_core_bifs`: running exactly those bodies in
    /// order reproduces the default registry key-for-key, so the disjointness guard
    /// above cannot miss a body (and a body dropped from the list fails here).
    #[test]
    fn the_registration_bodies_reproduce_the_default_registry() {
        let mut reg = BifRegistry::new();
        for (_, register) in REGISTERS {
            register(&mut reg);
        }
        assert_eq!(
            reg.names(),
            default_registry().names(),
            "REGISTERS must run exactly the bodies with_core_bifs runs, in the same order"
        );
        assert_eq!(reg.len(), BifRegistry::with_core_bifs().len());
    }

    /// Near misses (wrong arity / wrong module / a different BIF) answer `false`.
    #[test]
    fn predicates_reject_near_misses() {
        let cases: &[(Pred, &str, &[(&str, &str, u32)])] = &[
            (
                needs_memory_hook,
                "needs_memory_hook",
                &[
                    ("erlang", "memory", 2),         // wrong arity
                    ("erlang", "system_info", 1),    // a different erlang BIF
                    ("lists", "memory", 1),          // wrong module
                    ("erlang", "binary_to_term", 1), // another encoder, no memory seam
                ],
            ),
            (
                needs_local_fun_identity,
                "needs_local_fun_identity",
                &[
                    ("erlang", "term_to_binary", 0), // wrong arity
                    ("erlang", "term_to_binary", 3), // wrong arity
                    ("erlang", "external_size", 0),  // wrong arity
                    ("erlang", "binary_to_term", 1), // the decode direction is not an encoder
                    ("lists", "term_to_binary", 1),  // wrong module
                    ("erlang", "-", 2),              // an unrelated operator BIF
                ],
            ),
            (
                needs_dict_hook,
                "needs_dict_hook",
                &[
                    ("erlang", "put", 3),       // wrong arity
                    ("erlang", "erase", 2),     // there is no erase/2
                    ("erlang", "memory", 0),    // a different erlang BIF
                    ("proc_lib", "put", 2),     // wrong module
                    ("erlang", "process_info", 1), // the process hook, not the dict
                ],
            ),
            (
                needs_process_hook,
                "needs_process_hook",
                &[
                    ("erlang", "whereis", 0),        // wrong arity
                    ("erlang", "process_info", 3),   // wrong arity
                    ("erlang", "group_leader", 1),   // wrong arity
                    ("erlang", "nodes", 0),          // the runtime-hooks seam, not this one
                    ("erlang", "get_module_info", 1), // the module-info seam
                ],
            ),
            (
                needs_port_hook,
                "needs_port_hook",
                &[
                    ("erlang", "open_port", 1),    // wrong arity
                    ("erlang", "port_info", 3),    // wrong arity
                    // `list_to_port/1`/`port_to_list/1` are pure: no PortHook.
                    ("erlang", "list_to_port", 1),
                    ("erlang", "port_to_list", 1),
                    ("erts_internal", "port_close", 1), // the runtime-hooks seam
                ],
            ),
            (
                needs_send_hook,
                "needs_send_hook",
                &[
                    ("erlang", "send", 3), // wrong arity; a runtime-hooks key
                    ("erlang", "self", 0), // the same module, but reads only the pid
                    ("erlang", "!", 1),    // wrong arity
                    ("maps", "send", 2),   // wrong module
                ],
            ),
            (
                needs_nif_loader,
                "needs_nif_loader",
                &[
                    ("erlang", "load_nif", 1), // wrong arity
                    ("erlang", "load_nif", 3), // wrong arity
                    ("code", "load_nif", 2),   // wrong module
                ],
            ),
            (
                needs_record_source,
                "needs_record_source",
                &[
                    ("records", "get_definition", 1), // wrong arity
                    ("records", "get", 2),            // reads a record term, not the hook
                    ("records", "is_exported", 1),    // reads a record term, not the hook
                    ("erlang", "get_definition", 2),  // wrong module
                ],
            ),
            (
                needs_module_info,
                "needs_module_info",
                &[
                    ("erlang", "get_module_info", 0), // wrong arity
                    ("erlang", "get_module_info", 3), // wrong arity
                    ("code", "get_module_info", 1),   // wrong module
                ],
            ),
            (
                needs_persistent_term,
                "needs_persistent_term",
                &[
                    ("persistent_term", "put", 3),        // wrong arity
                    ("persistent_term", "erase", 0),      // wrong arity
                    ("persistent_term", "put_new", 3),    // wrong arity
                    ("persistent_term", "keys", 0),       // not a registered key
                    ("erlang", "persistent_term", 1),     // wrong module
                ],
            ),
            (
                needs_fun_caller,
                "needs_fun_caller",
                &[
                    ("lists", "map", 3),     // wrong arity
                    ("lists", "foldl", 2),   // wrong arity
                    ("lists", "usort", 1),   // the non-fun arity
                    ("lists", "sort", 1),    // the non-fun arity
                    ("lists", "reverse", 1), // no fun argument
                    ("maps", "next", 1),     // takes no fun
                    ("maps", "merge", 2),    // takes no fun
                    ("sets", "map", 2),      // wrong module
                ],
            ),
            (
                needs_local_node,
                "needs_local_node",
                &[
                    ("erlang", "node", 2),                  // wrong arity
                    ("erlang", "term_to_binary", 0),         // wrong arity
                    ("erlang", "list_to_pid", 1),            // does not read the node
                    ("erlang", "local_node", 0),             // not a registered key
                    ("erlang", "open_port", 2),              // the port hook
                ],
            ),
            (
                needs_runtime_hooks,
                "needs_runtime_hooks",
                &[
                    ("erlang", "start_timer", 2),   // wrong arity
                    ("erlang", "nodes", 3),         // wrong arity
                    ("erlang", "memory", 0),        // the memory hook, not this seam
                    ("lists", "reverse", 1),        // a plain BIF
                    ("ets", "lookup", 2),           // the ets bridge, not this seam
                    ("erlang", "get_module_info", 1), // the module-info seam
                ],
            ),
        ];
        for (predicate, name, misses) in cases {
            for (module, function, arity) in *misses {
                assert!(
                    !predicate(module, function, *arity),
                    "{name} must reject the near miss {module}:{function}/{arity}"
                );
            }
        }
    }
}
