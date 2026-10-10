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
//! Of the 56 keys, **54** resolve through the **single**
//! [`RuntimeHooks`](crate::context::RuntimeHooks) dispatcher (via
//! [`BifContext::call_runtime_bif`](crate::context::BifContext::call_runtime_bif)),
//! which is handed the `(module, function, arity)` identity verbatim so the
//! runtime dispatches on exactly the `bif.tab` key. With **no hook installed every
//! one of them answers the typed `unsupported_bif` capability gap** — never `undef`
//! (the function *is* a declared BIF; only its runtime provider is missing),
//! never a fabricated value (workspace `CONTEXT.md` §3).
//!
//! The other **two** keys — `ets_raw_first/1` and `ets_raw_next/2` — are served
//! **for real** ([`REAL_BIFS`]): the C declares them as plain aliases of
//! `ets:first/1` and `ets:next/2`, *not* a storage-order walk
//! (`../../erts/emulator/beam/erl_db_hash.c:4188` — "raw_first same as first";
//! `../../erts/emulator/beam/erl_db_tree.c:539` — "raw_next same as next"), so
//! they **delegate** to this crate's own `ets` family ([`ets_registry`]) and
//! therefore cannot drift from `ets:first/1`/`ets:next/2`.
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

use std::sync::OnceLock;

use beam_common::Result;
use beam_term::Eterm;

use crate::context::BifContext;
use crate::registry::BifRegistry;
use crate::runtime::runtime_bif;

/// The `(module, function, arity)` keys this module registers **for real** (not
/// through the `RuntimeHooks` seam).
///
/// Sorted by `(module, function, arity)`. These are the two `ets`-interior
/// navigation keys, whose C implementations are plain aliases of `ets:first/1` and
/// `ets:next/2`; [`register`] asserts every one of them is actually present after
/// registration.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("erts_internal", "ets_raw_first", 1),
    ("erts_internal", "ets_raw_next", 2),
];

/// The `ets:*` family, registered once.
///
/// `erts_internal:ets_raw_first/1` and `ets_raw_next/2` are aliases of
/// `ets:first/1` and `ets:next/2` in the C (`erl_db_hash.c:4188`, `erl_db_tree.c:539`),
/// so they dispatch into this crate's own `ets` implementations instead of
/// re-resolving a table handle: equality with `ets:first/1`/`ets:next/2` then holds
/// by construction, errors included.
fn ets_registry() -> &'static BifRegistry {
    static ETS: OnceLock<BifRegistry> = OnceLock::new();
    ETS.get_or_init(|| {
        let mut reg = BifRegistry::new();
        crate::ets::register(&mut reg);
        reg
    })
}

/// `erts_internal:ets_raw_first(Tab)` — C `db_raw_first_*` (`erl_db_hash.c:4188`:
/// "raw_first same as first"; `erl_db_tree.c:539`).
fn ets_raw_first_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ets_registry().call("ets", "first", 1, ctx, args)
}

/// `erts_internal:ets_raw_next(Tab, Key)` — C `db_raw_next_*` (`erl_db_tree.c:539`:
/// "raw_next same as next").
fn ets_raw_next_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ets_registry().call("ets", "next", 2, ctx, args)
}

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
    reg.register("erts_internal", "ets_raw_first", 1, ets_raw_first_1);
    reg.register("erts_internal", "ets_raw_next", 2, ets_raw_next_2);
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
    // `REAL_BIFS` is the declared roster of the keys served by real implementations
    // rather than the seam; keep it honest by checking it against what was just
    // registered.
    debug_assert!(
        REAL_BIFS
            .iter()
            .all(|(module, function, arity)| reg.contains(module, function, *arity)),
        "REAL_BIFS lists a key that this module does not register"
    );
}

#[cfg(test)]
mod tests {
    use crate::context::BifContext;
    use crate::ets::TableRegistry;
    use crate::registry::BifRegistry;
    use beam_alloc::Heap;
    use beam_common::Result;
    use beam_process::ProcessId;
    use beam_term::{AtomTable, Eterm};

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

    /// [`super::REAL_BIFS`] lists exactly the keys this module serves with a real
    /// implementation instead of the `RuntimeHooks` seam, and every one of them is
    /// registered by [`super::register`] (the two must never drift apart).
    #[test]
    fn real_bifs_lists_the_keys_this_module_serves_for_real() {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert_eq!(
            super::REAL_BIFS,
            &[
                ("erts_internal", "ets_raw_first", 1),
                ("erts_internal", "ets_raw_next", 2),
            ]
        );
        for (module, function, arity) in super::REAL_BIFS {
            assert!(
                reg.contains(module, function, *arity),
                "REAL_BIFS lists {module}:{function}/{arity}, which register does not register"
            );
        }
        // Both keys live in the shared `ets`-family registry, not here: the two
        // registered `BifFn`s are the delegating wrappers.
        let ets = super::ets_registry();
        for function in ["first", "next"] {
            assert!(
                ets.contains("ets", function, if function == "first" { 1 } else { 2 }),
                "the delegated ets:{function} is missing from the family registry"
            );
        }
    }

    /// Renders the observable outcome of a call: a small integer, an atom's name,
    /// the raw image of anything else, or the error's `Debug` image — so a value
    /// and an error class are both distinguishable and comparable.
    fn outcome(r: Result<Eterm>, atoms: &AtomTable) -> String {
        match r {
            Ok(t) if crate::access::is_small(t) => format!("int({})", beam_term::signed_value(t)),
            Ok(t) if crate::access::is_atom(t) => format!(
                "atom({})",
                atoms.name(beam_term::atom_val(t)).unwrap_or("<unknown>")
            ),
            Ok(t) => format!("term({t:?})"),
            Err(e) => format!("err({e:?})"),
        }
    }

    /// The atoms the navigation fixtures need.
    fn nav_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "$end_of_table",
            "t",
            "set",
            "ordered_set",
            "bag",
            "duplicate_bag",
            "not_a_table",
            "foo",
            "true",
        ]);
        atoms
    }

    /// `ets:new(t, [Kind])` — the unnamed-table handle.
    fn new_table(bifs: &BifRegistry, ctx: &mut BifContext<'_>, kind: &str) -> Eterm {
        let name = ctx.atom("t").expect("seeded");
        let kind = ctx.atom(kind).expect("seeded");
        let opts = ctx.list(&[kind]).expect("option list");
        bifs.call("ets", "new", 2, ctx, &[name, opts])
            .expect("ets:new")
    }

    /// Inserts `{K, K}` for every key in `keys`.
    fn insert_keys(bifs: &BifRegistry, ctx: &mut BifContext<'_>, table: Eterm, keys: &[i64]) {
        for key in keys {
            let key = ctx.int(*key).expect("small int");
            let object = ctx.tuple(&[key, key]).expect("the {K, K} object");
            bifs.call("ets", "insert", 2, ctx, &[table, object])
                .expect("ets:insert");
        }
    }

    /// The `'$end_of_table'` atom.
    fn end_of_table(ctx: &mut BifContext<'_>) -> Eterm {
        ctx.atom("$end_of_table").expect("seeded")
    }

    /// Walks the whole table from its first key with `next`, through either the
    /// plain `ets` verbs (`raw == false`) or the `erts_internal:ets_raw_*` ones
    /// (`raw == true`), returning the visited keys in visit order and the
    /// terminating outcome.
    ///
    /// A walk that does not terminate within the table's key budget is a hard
    /// failure (never an infinite loop).
    fn walk(
        bifs: &BifRegistry,
        ctx: &mut BifContext<'_>,
        atoms: &AtomTable,
        table: Eterm,
        raw: bool,
    ) -> (Vec<String>, String) {
        let end = end_of_table(ctx);
        let mut current = if raw {
            bifs.call("erts_internal", "ets_raw_first", 1, ctx, &[table])
        } else {
            bifs.call("ets", "first", 1, ctx, &[table])
        };
        let mut visited = Vec::new();
        for _ in 0..64 {
            match current {
                Ok(key) if beam_term::compare::eq(key, end, atoms) => {
                    return (visited, outcome(Ok(key), atoms));
                }
                Ok(key) => {
                    visited.push(outcome(Ok(key), atoms));
                    current = if raw {
                        bifs.call("erts_internal", "ets_raw_next", 2, ctx, &[table, key])
                    } else {
                        bifs.call("ets", "next", 2, ctx, &[table, key])
                    };
                }
                Err(err) => return (visited, format!("err({err:?})")),
            }
        }
        panic!("the walk of {table:?} did not terminate");
    }

    /// `erts_internal:ets_raw_first/1` and `ets_raw_next/2` are **exact aliases** of
    /// `ets:first/1` and `ets:next/2` (C `erl_db_hash.c:4188` "raw_first same as
    /// first"; `erl_db_tree.c:539` "raw_next same as next") — for every table kind,
    /// for a present key, for an absent key, for a non-integer key, and for the
    /// terminating step. This context installs **no** `RuntimeHooks`, so a
    /// seam-forwarded key would answer the typed `unsupported_bif` gap: an `Ok`
    /// result here proves the real implementation. Every pinned value below comes
    /// from the oracle probe, run against the system `erl`
    /// (OTP 29.1.1 / erts-17.1) — each call wrapped in a `try` so an error is
    /// comparable with the value form:
    ///
    /// ```text
    /// -module(probe_ets_raw).
    /// -export([main/0]).
    ///
    /// safe(F) ->
    ///     try F() catch C:R -> {C, R} end.
    ///
    /// kinds() -> [set, ordered_set, bag, duplicate_bag].
    ///
    /// main() ->
    ///     io:format("otp_release=~s~n", [erlang:system_info(otp_release)]),
    ///     lists:foreach(
    ///       fun(Kind) ->
    ///           T = ets:new(probe, [Kind]),
    ///           [ets:insert(T, {K, K}) || K <- [6, 3, 4, 8, 1]],
    ///           First = ets:first(T),
    ///           RawFirst = erts_internal:ets_raw_first(T),
    ///           io:format("kind=~p first=~p raw_first=~p eq=~p~n",
    ///                     [Kind, First, RawFirst, First =:= RawFirst]),
    ///           lists:foreach(
    ///             fun(K) ->
    ///                 N = safe(fun() -> ets:next(T, K) end),
    ///                 RN = safe(fun() -> erts_internal:ets_raw_next(T, K) end),
    ///                 io:format("  kind=~p key=~p next=~p raw_next=~p eq=~p~n",
    ///                           [Kind, K, N, RN, N =:= RN])
    ///             end, [6, 3, 4, 8, 1]),
    ///           NEnd = safe(fun() -> ets:next(T, 8) end),
    ///           RNEnd = safe(fun() -> erts_internal:ets_raw_next(T, 8) end),
    ///           io:format("  kind=~p key=8 next=~p raw_next=~p eq=~p~n",
    ///                     [Kind, NEnd, RNEnd, NEnd =:= RNEnd]),
    ///           NBad = safe(fun() -> ets:next(T, 999) end),
    ///           RNBad = safe(fun() -> erts_internal:ets_raw_next(T, 999) end),
    ///           io:format("  kind=~p key=999 next=~p raw_next=~p eq=~p~n",
    ///                     [Kind, NBad, RNBad, NBad =:= RNBad]),
    ///           NBad2 = safe(fun() -> ets:next(T, foo) end),
    ///           RNBad2 = safe(fun() -> erts_internal:ets_raw_next(T, foo) end),
    ///           io:format("  kind=~p key=foo next=~p raw_next=~p eq=~p~n",
    ///                     [Kind, NBad2, RNBad2, NBad2 =:= RNBad2]),
    ///           ets:delete(T)
    ///       end, kinds()),
    ///     lists:foreach(
    ///       fun(Kind) ->
    ///           T = ets:new(probe_empty, [Kind]),
    ///           First = safe(fun() -> ets:first(T) end),
    ///           RawFirst = safe(fun() -> erts_internal:ets_raw_first(T) end),
    ///           NBad = safe(fun() -> ets:next(T, 999) end),
    ///           RNBad = safe(fun() -> erts_internal:ets_raw_next(T, 999) end),
    ///           io:format("empty kind=~p first=~p raw_first=~p eq=~p next=~p raw_next=~p eq=~p~n",
    ///                     [Kind, First, RawFirst, First =:= RawFirst,
    ///                      NBad, RNBad, NBad =:= RNBad]),
    ///           ets:delete(T)
    ///       end, kinds()),
    ///     NT = safe(fun() -> ets:first(not_a_table) end),
    ///     RNT = safe(fun() -> erts_internal:ets_raw_first(not_a_table) end),
    ///     io:format("nontable first=~p raw_first=~p eq=~p~n", [NT, RNT, NT =:= RNT]),
    ///     NT2 = safe(fun() -> ets:next(not_a_table, 1) end),
    ///     RNT2 = safe(fun() -> erts_internal:ets_raw_next(not_a_table, 1) end),
    ///     io:format("nontable next=~p raw_next=~p eq=~p~n", [NT2, RNT2, NT2 =:= RNT2]),
    ///     NT3 = safe(fun() -> ets:first(42) end),
    ///     RNT3 = safe(fun() -> erts_internal:ets_raw_first(42) end),
    ///     io:format("nontable(42) first=~p raw_first=~p eq=~p~n", [NT3, RNT3, NT3 =:= RNT3]),
    ///     ok.
    /// ```
    ///
    /// (Run as `erlc -o "$TMPDIR" probe_ets_raw.erl` then
    /// `erl -noshell -pa "$TMPDIR" -eval 'probe_ets_raw:main(),halt(0).'`; only the
    /// pinned rows are reproduced below — the full transcript is in the module's
    /// differential notes.)
    /// ```
    ///
    /// The observed transcript for the rows pinned below:
    ///
    /// ```text
    /// kind=set          first=6 raw_first=6 eq=true
    /// kind=ordered_set  first=1 raw_first=1 eq=true
    ///   kind=ordered_set key=1   next=3   raw_next=3   eq=true
    ///   kind=ordered_set key=3   next=4   raw_next=4   eq=true
    ///   kind=ordered_set key=4   next=6   raw_next=6   eq=true
    ///   kind=ordered_set key=6   next=8   raw_next=8   eq=true
    ///   kind=ordered_set key=8   next='$end_of_table' raw_next='$end_of_table' eq=true
    ///   kind=ordered_set key=999 next='$end_of_table' raw_next='$end_of_table' eq=true
    ///   kind=ordered_set key=foo next='$end_of_table' raw_next='$end_of_table' eq=true
    ///   kind=set         key=999 next={error,badarg} raw_next={error,badarg} eq=true
    ///   kind=set         key=foo next={error,badarg} raw_next={error,badarg} eq=true
    ///   kind=bag         key=999 next={error,badarg} raw_next={error,badarg} eq=true
    ///   kind=duplicate_bag key=999 next={error,badarg} raw_next={error,badarg} eq=true
    /// empty kind=set         first='$end_of_table' raw_first='$end_of_table' eq=true next={error,badarg} raw_next={error,badarg} eq=true
    /// empty kind=ordered_set first='$end_of_table' raw_first='$end_of_table' eq=true next='$end_of_table' raw_next='$end_of_table' eq=true
    /// nontable      first={error,badarg} raw_first={error,badarg} eq=true
    /// nontable(42)  first={error,badarg} raw_first={error,badarg} eq=true
    /// ```
    ///
    /// The *iteration order* of a hash kind (`set`/`bag`/`duplicate_bag`) is
    /// explicitly unspecified in OTP (and this crate's `set` walks its CHAMP
    /// interior), so only the alias equality and the walk-completeness are pinned
    /// there; the `ordered_set` order **is** pinned exactly by
    /// `raw_first_and_next_pin_the_oracle_on_an_ordered_set`.
    #[test]
    fn raw_first_and_next_are_exact_aliases_of_the_ets_navigation_bifs() {
        let bifs = crate::default_registry();
        let atoms = nav_atoms();
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let mut tables = TableRegistry::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

        for kind in ["set", "ordered_set", "bag", "duplicate_bag"] {
            let table = new_table(&bifs, &mut ctx, kind);
            insert_keys(&bifs, &mut ctx, table, &[6, 3, 4, 8, 1]);

            let plain = outcome(bifs.call("ets", "first", 1, &mut ctx, &[table]), &atoms);
            let raw = outcome(
                bifs.call("erts_internal", "ets_raw_first", 1, &mut ctx, &[table]),
                &atoms,
            );
            assert_eq!(raw, plain, "{kind}: raw_first must alias ets:first");
            assert!(
                !raw.starts_with("err("),
                "{kind}: a populated table has a first key, and with no RuntimeHooks \
                 installed a seam-forwarded key would have answered the typed gap \
                 instead (got {raw})"
            );

            // A present key steps to the same successor.
            for key in [1, 3, 4, 6, 8] {
                let key = ctx.int(key).expect("small int");
                let plain = outcome(bifs.call("ets", "next", 2, &mut ctx, &[table, key]), &atoms);
                let raw = outcome(
                    bifs.call("erts_internal", "ets_raw_next", 2, &mut ctx, &[table, key]),
                    &atoms,
                );
                assert_eq!(raw, plain, "{kind}: raw_next must alias ets:next");
                assert!(!plain.starts_with("err("), "{kind}: {plain}");
            }

            // An absent integer key and a non-integer key behave exactly alike.
            let absent = ctx.int(999).expect("small int");
            let non_integer = ctx.atom("foo").expect("seeded");
            for key in [absent, non_integer] {
                let plain = outcome(bifs.call("ets", "next", 2, &mut ctx, &[table, key]), &atoms);
                let raw = outcome(
                    bifs.call("erts_internal", "ets_raw_next", 2, &mut ctx, &[table, key]),
                    &atoms,
                );
                assert_eq!(raw, plain, "{kind}: raw_next must alias ets:next");
                // The oracle's classes: an `ordered_set` reads the key as a range
                // bound and answers the end; a hash kind demands the key be present.
                if kind == "ordered_set" {
                    assert_eq!(plain, "atom($end_of_table)", "{kind}");
                } else {
                    assert_eq!(plain, "err(BadArg)", "{kind}");
                }
            }

            // The whole-table walk visits every key exactly once and terminates on
            // the same key through either path.
            let (plain_keys, plain_end) = walk(&bifs, &mut ctx, &atoms, table, false);
            let (raw_keys, raw_end) = walk(&bifs, &mut ctx, &atoms, table, true);
            assert_eq!(raw_keys, plain_keys, "{kind}: the walks must visit alike");
            assert_eq!(raw_end, plain_end, "{kind}: the walks must end alike");
            assert_eq!(raw_end, "atom($end_of_table)", "{kind}");
            let mut sorted = plain_keys.clone();
            sorted.sort();
            assert_eq!(
                sorted,
                [
                    "int(1)".to_string(),
                    "int(3)".to_string(),
                    "int(4)".to_string(),
                    "int(6)".to_string(),
                    "int(8)".to_string()
                ],
                "{kind}: the walk visits every inserted key exactly once"
            );

            bifs.call("ets", "delete", 1, &mut ctx, &[table])
                .expect("ets:delete");
        }
    }

    /// The `ordered_set` navigation order is pinned **exactly** against the oracle
    /// (see the probe above): `first` is the least key and `next` steps the key
    /// order `1 → 3 → 4 → 6 → 8 → '$end_of_table'`, with an absent or non-integer
    /// key behaving as a range bound.
    #[test]
    fn raw_first_and_next_pin_the_oracle_on_an_ordered_set() {
        let bifs = crate::default_registry();
        let atoms = nav_atoms();
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let mut tables = TableRegistry::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

        let table = new_table(&bifs, &mut ctx, "ordered_set");
        insert_keys(&bifs, &mut ctx, table, &[6, 3, 4, 8, 1]);

        assert_eq!(
            outcome(
                bifs.call("erts_internal", "ets_raw_first", 1, &mut ctx, &[table]),
                &atoms
            ),
            "int(1)",
            "the oracle's ordered_set first key is 1"
        );

        let steps = [
            (1i64, "int(3)"),
            (3, "int(4)"),
            (4, "int(6)"),
            (6, "int(8)"),
            (8, "atom($end_of_table)"),
            (999, "atom($end_of_table)"),
        ];
        for (key, expected) in steps {
            let key = ctx.int(key).expect("small int");
            assert_eq!(
                outcome(
                    bifs.call("erts_internal", "ets_raw_next", 2, &mut ctx, &[table, key]),
                    &atoms
                ),
                expected,
                "the oracle's ordered_set step from {key:?}"
            );
        }
        // A non-integer key is a range bound past every integer, exactly as the C's
        // tree walk treats it.
        let non_integer = ctx.atom("foo").expect("seeded");
        assert_eq!(
            outcome(
                bifs.call("erts_internal", "ets_raw_next", 2, &mut ctx, &[table, non_integer]),
                &atoms
            ),
            "atom($end_of_table)"
        );
    }

    /// The empty and non-table rows of the oracle transcript: an empty table's
    /// `first` is `'$end_of_table'` for every kind, an empty hash kind's `next` is
    /// `badarg` (the key must be present) while an empty `ordered_set`'s is the
    /// end marker, and a non-table argument is `badarg` for both keys.
    #[test]
    fn raw_first_and_next_on_an_empty_table_and_a_non_table() {
        let bifs = crate::default_registry();
        let atoms = nav_atoms();
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let mut tables = TableRegistry::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

        let absent = ctx.int(999).expect("small int");
        for kind in ["set", "ordered_set", "bag", "duplicate_bag"] {
            let table = new_table(&bifs, &mut ctx, kind);

            let plain = outcome(bifs.call("ets", "first", 1, &mut ctx, &[table]), &atoms);
            let raw = outcome(
                bifs.call("erts_internal", "ets_raw_first", 1, &mut ctx, &[table]),
                &atoms,
            );
            assert_eq!(plain, "atom($end_of_table)", "an empty {kind}");
            assert_eq!(raw, plain, "an empty {kind}");

            let plain = outcome(
                bifs.call("ets", "next", 2, &mut ctx, &[table, absent]),
                &atoms,
            );
            let raw = outcome(
                bifs.call("erts_internal", "ets_raw_next", 2, &mut ctx, &[table, absent]),
                &atoms,
            );
            assert_eq!(raw, plain, "an empty {kind}");
            if kind == "ordered_set" {
                assert_eq!(plain, "atom($end_of_table)", "an empty {kind}");
            } else {
                assert_eq!(plain, "err(BadArg)", "an empty {kind}");
            }

            bifs.call("ets", "delete", 1, &mut ctx, &[table])
                .expect("ets:delete");
        }

        // A non-table argument: an unknown name atom and a term of another shape.
        let unknown_name = ctx.atom("not_a_table").expect("seeded");
        let not_a_term = ctx.int(42).expect("small int");
        for table in [unknown_name, not_a_term] {
            let plain = outcome(bifs.call("ets", "first", 1, &mut ctx, &[table]), &atoms);
            let raw = outcome(
                bifs.call("erts_internal", "ets_raw_first", 1, &mut ctx, &[table]),
                &atoms,
            );
            assert_eq!(plain, "err(BadArg)", "a non-table handle");
            assert_eq!(raw, plain, "a non-table handle");

            let plain = outcome(
                bifs.call("ets", "next", 2, &mut ctx, &[table, absent]),
                &atoms,
            );
            let raw = outcome(
                bifs.call("erts_internal", "ets_raw_next", 2, &mut ctx, &[table, absent]),
                &atoms,
            );
            assert_eq!(plain, "err(BadArg)", "a non-table handle");
            assert_eq!(raw, plain, "a non-table handle");
        }
    }
}
