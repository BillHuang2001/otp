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

//! The `memory` family of [`super::system_info_value`]: GC/heap policy keys plus the
//! allocator introspection keys.
//!
//! The heap/GC figures come from this runtime's own emulator heap options
//! ([`super::effective_policy`], seeded from `beam_gc`'s constants and growth table) and
//! from `beam_gc::heap_sizes` — never from the oracle's printed numbers, which belong to
//! a different process. `allocated_areas` and the `{allocator, A}` report read this
//! runtime's live tables/registry.
//!
//! | key | shape |
//! |---|---|
//! | `min_heap_size` | `{min_heap_size, N}` |
//! | `min_bin_vheap_size` | `{min_bin_vheap_size, N}` |
//! | `fullsweep_after` | `{fullsweep_after, N}` |
//! | `max_heap_size` | map `#{error_logger=>…, include_shared_binaries=>…, kill=>…, size=>…}` |
//! | `heap_sizes` | list of ints (the curated growth table) |
//! | `heap_type` | atom, `private` |
//! | `message_queue_data` | atom, `on_heap` |
//! | `garbage_collection` | `[{max_heap_size,0},{min_bin_vheap_size,…},{min_heap_size,…},{fullsweep_after,…}]` |
//! | `alloc_util_allocators` | list of the ten alloc-util allocator names |
//! | `allocated_areas` | the oracle's 19-item list |

use beam_common::Result;

use super::{effective_policy, row, usize_to_int, InfoValue};
use crate::runtime::Runtime;

/// The `system_info/1` value of a `memory`-family key, or `None` when `key` is not ours.
pub(crate) fn try_value(key: &str, rt: &Runtime) -> Option<Result<InfoValue>> {
    match key {
        "min_heap_size" => Some(Ok(heap_item(
            "min_heap_size",
            usize_to_int(effective_policy(rt).min_heap_words),
        ))),
        "min_bin_vheap_size" => Some(Ok(heap_item(
            "min_bin_vheap_size",
            usize_to_int(effective_policy(rt).min_bin_vheap_words),
        ))),
        "fullsweep_after" => Some(Ok(heap_item(
            "fullsweep_after",
            i64::from(effective_policy(rt).fullsweep_after),
        ))),
        "max_heap_size" => Some(Ok(max_heap_size(rt))),
        "heap_sizes" => Some(Ok(heap_sizes())),
        "heap_type" => Some(Ok(InfoValue::Atom("private"))),
        "message_queue_data" => Some(Ok(InfoValue::Atom("on_heap"))),
        "garbage_collection" => Some(Ok(garbage_collection(rt))),
        "alloc_util_allocators" => Some(Ok(alloc_util_allocators())),
        "allocated_areas" => Some(allocated_areas(rt)),
        _ => None,
    }
}

/// A `{Key, Value}` heap item (the `{min_heap_size, N}` / `{fullsweep_after, N}` shape).
fn heap_item(key: &'static str, value: i64) -> InfoValue {
    InfoValue::Tupled(vec![InfoValue::Atom(key), InfoValue::Int(value)])
}

/// `system_info(max_heap_size)` — the emulator's max-heap-size settings as a map.
///
/// Verified against the oracle (OTP 29.1.1 / erts-17.1):
/// `#{error_logger => true, include_shared_binaries => false, kill => true, size => 0}`.
/// `size` is `policy.max_heap_words` (`0` = unlimited, the emulator default) and the
/// three booleans are the `+hmaxk`/`+hmaxel`/`+hmaxib` flags — all from [`crate::config`],
/// never hardcoded.
fn max_heap_size(rt: &Runtime) -> InfoValue {
    let policy = effective_policy(rt);
    let flags = rt.config().heap.max_heap_flags();
    InfoValue::Map(vec![
        (InfoValue::Atom("error_logger"), InfoValue::Bool(flags.error_logger)),
        (
            InfoValue::Atom("include_shared_binaries"),
            InfoValue::Bool(flags.include_shared_binaries),
        ),
        (InfoValue::Atom("kill"), InfoValue::Bool(flags.kill)),
        (
            InfoValue::Atom("size"),
            InfoValue::Int(usize_to_int(policy.max_heap_words)),
        ),
    ])
}

/// `system_info(heap_sizes)` — the curated heap-size growth table.
///
/// Our [`beam_gc::heap_sizes`] is the C `heap_sizes` table rebuilt from the same rule
/// (`erts_init_gc`, `erl_gc.c:262-291`: Fibonacci+1 to index 23, then +20 %, capped), so
/// the values are this runtime's real table rather than the oracle's printed list.
fn heap_sizes() -> InfoValue {
    InfoValue::List(
        beam_gc::heap_sizes()
            .iter()
            .map(|&size| InfoValue::Int(usize_to_int(size)))
            .collect(),
    )
}

/// `system_info(alloc_util_allocators)` — the *alloc-util* allocator names.
///
/// Verified against the oracle (OTP 29 / erts-17.1):
/// `erl -noshell -eval 'io:format("~p~n",[erlang:system_info(alloc_util_allocators)]).'`
/// prints `[temp_alloc,sl_alloc,std_alloc,ll_alloc,eheap_alloc,ets_alloc,fix_alloc,
/// literal_alloc,binary_alloc,driver_alloc]` — which is exactly
/// [`beam_alloc::registry::alloc_util_allocators`] (the C
/// `erts_alloc_util_allocators`, `erl_alloc.c:2789`, i.e. every [`AllocatorId`] but
/// `sys_alloc` — the raw system allocator — and `test_alloc`), in the same order.
///
/// [`AllocatorId`]: beam_alloc::types::AllocatorId
fn alloc_util_allocators() -> InfoValue {
    InfoValue::List(
        beam_alloc::registry::alloc_util_allocators()
            .into_iter()
            .map(InfoValue::Atom)
            .collect(),
    )
}

/// `system_info(garbage_collection)` — the emulator-wide GC policy knobs.
///
/// Verified against the oracle (OTP 29.1.1 / erts-17.1): `[{max_heap_size,0},
/// {min_bin_vheap_size,46422},{min_heap_size,233},{fullsweep_after,65535}]`, and an
/// invocation with `+hms 1000 +hmbs 5000` ⇒ `[{max_heap_size,0},
/// {min_bin_vheap_size,6772},{min_heap_size,1598},{fullsweep_after,65535}]`. Note that
/// `max_heap_size` is the **bare integer** here, unlike the flag *map*
/// `process_info(Pid, garbage_collection)` reports.
///
/// The values are the emulator's heap-size options ([`effective_policy`]), i.e. the same
/// figures the per-process item reports for a process with no explicit heap options — the
/// four keys [`beam_gc::GARBAGE_COLLECTION_KEYS`] names minus `minor_gcs`, which is a
/// per-process counter with no emulator-wide counterpart.
fn garbage_collection(rt: &Runtime) -> InfoValue {
    // `HeapInfo` owns the oracle's item order; the sizes are irrelevant for this key.
    let info = beam_gc::HeapInfo::new(&effective_policy(rt), 0, 0, 0);
    let items = info.items();
    InfoValue::List(
        items[..4]
            .iter()
            .map(|(key, value)| row(key, usize_to_int(*value)))
            .collect(),
    )
}

/// `system_info(allocated_areas)` — the runtime's own allocation-area footprints, in the
/// oracle's 19-item order and arity.
///
/// Verified against the oracle (OTP 29 / erts-17.1), which emits exactly these names in
/// this order (the C `erts_allocated_areas`, `erl_alloc.c:2580-2699`):
/// `sys_misc`, `static`, `atom_space` (a **three**-element tuple), `atom_table`,
/// `module_table`, `export_table`, `export_list`, `register_table`, `fun_table`,
/// `fun_list`, `module_refs`, `loaded_code`, `dist_table`, `node_table`, `bif_timer`,
/// `process_table`, `port_table`, `ets_misc`, `external_alloc`.
///
/// The oracle's *values* are run-dependent byte counts of a completely different
/// emulator, so none of them can be mirrored. Every value here is instead **derived from
/// state this runtime actually has** — never a hardcoded oracle figure, which would be a
/// lie about a different process:
///
/// | item | value |
/// |---|---|
/// | `atom_space` | `{atom_space, B, B}`, `B` = the atom table's index slots |
/// | `atom_table` | `B` |
/// | `module_table`, `loaded_code` | one slot per **loaded** code module |
/// | `register_table` | one slot per locally **registered** name |
/// | `process_table` | one slot per **live** process |
/// | `port_table` | one slot per **live** port |
/// | `ets_misc` | one slot per **live** ETS table |
/// | every other item | `0` |
///
/// A "slot" is [`std::mem::size_of::<usize>`] bytes — the word the runtime's own tables
/// spend per entry — so the figures are a real (if coarse) lower bound on this runtime's
/// footprint, in the byte unit the oracle uses. The `0` items are the ones this runtime
/// has **no counterpart** for (no `export`/BIF-timer/`fun` table, no distribution or node
/// table, no external port-driver allocator), and a `0` is honest where an invented
/// figure would not be.
fn allocated_areas(rt: &Runtime) -> Result<InfoValue> {
    let slot = std::mem::size_of::<usize>() as i64;
    let atoms = slot * rt.code_server.atoms().len() as i64;
    let modules = slot * rt.loaded_modules().len() as i64;
    let registered = slot * rt.processes().registered().count() as i64;
    let processes = slot * rt.processes().len() as i64;
    let ports = slot * super::live_ports(rt)?;
    let tables = slot * super::live_tables(rt)?;
    Ok(InfoValue::List(vec![
        row("sys_misc", 0),
        row("static", 0),
        InfoValue::Tupled(vec![
            InfoValue::Atom("atom_space"),
            InfoValue::Int(atoms),
            InfoValue::Int(atoms),
        ]),
        row("atom_table", atoms),
        row("module_table", modules),
        row("export_table", 0),
        row("export_list", 0),
        row("register_table", registered),
        row("fun_table", 0),
        row("fun_list", 0),
        row("module_refs", 0),
        row("loaded_code", modules),
        row("dist_table", 0),
        row("node_table", 0),
        row("bif_timer", 0),
        row("process_table", processes),
        row("port_table", ports),
        row("ets_misc", tables),
        row("external_alloc", 0),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info::test_support::runtime;

    #[test]
    fn heap_policy_keys_have_the_oracle_shape() {
        let rt = runtime();
        assert_eq!(
            try_value("min_heap_size", &rt).unwrap().unwrap(),
            InfoValue::Tupled(vec![InfoValue::Atom("min_heap_size"), InfoValue::Int(233)])
        );
        assert_eq!(
            try_value("min_bin_vheap_size", &rt).unwrap().unwrap(),
            InfoValue::Tupled(vec![
                InfoValue::Atom("min_bin_vheap_size"),
                InfoValue::Int(46_422)
            ])
        );
        assert_eq!(
            try_value("fullsweep_after", &rt).unwrap().unwrap(),
            InfoValue::Tupled(vec![
                InfoValue::Atom("fullsweep_after"),
                InfoValue::Int(65_535)
            ])
        );
        assert_eq!(
            try_value("heap_type", &rt).unwrap().unwrap(),
            InfoValue::Atom("private")
        );
        assert_eq!(
            try_value("message_queue_data", &rt).unwrap().unwrap(),
            InfoValue::Atom("on_heap")
        );
        assert!(try_value("nonsense", &rt).is_none());
    }

    #[test]
    fn max_heap_size_is_the_oracle_map_shape() {
        // The oracle's default: #{error_logger => true, include_shared_binaries => false,
        //  kill => true, size => 0}.
        let rt = runtime();
        let value = try_value("max_heap_size", &rt).unwrap().unwrap();
        let pairs = match value {
            InfoValue::Map(pairs) => pairs,
            other => panic!("max_heap_size must be a map, got {other:?}"),
        };
        assert_eq!(pairs.len(), 4);
        assert_eq!(pairs[0], (InfoValue::Atom("error_logger"), InfoValue::Bool(true)));
        assert_eq!(
            pairs[1],
            (
                InfoValue::Atom("include_shared_binaries"),
                InfoValue::Bool(false)
            )
        );
        assert_eq!(pairs[2], (InfoValue::Atom("kill"), InfoValue::Bool(true)));
        assert_eq!(pairs[3], (InfoValue::Atom("size"), InfoValue::Int(0)));

        // `+hmax 1000000` sets the size; the flags stay the defaults.
        let argv: Vec<String> = ["+hmax", "1000000"].iter().map(|s| s.to_string()).collect();
        let config = crate::cli::parse(&argv).expect("+hmax 1000000 parses");
        let rt = Runtime::new(config).expect("a runtime with +hmax");
        let InfoValue::Map(pairs) = try_value("max_heap_size", &rt).unwrap().unwrap() else {
            panic!("a map");
        };
        assert_eq!(
            pairs[3],
            (InfoValue::Atom("size"), InfoValue::Int(1_000_000))
        );
    }

    #[test]
    fn heap_sizes_is_the_growth_table() {
        let rt = runtime();
        let value = try_value("heap_sizes", &rt).unwrap().unwrap();
        let InfoValue::List(sizes) = value else {
            panic!("heap_sizes must be a list");
        };
        assert!(sizes.len() > 20);
        // The table is non-decreasing and starts at the C's 12.
        assert_eq!(sizes[0], InfoValue::Int(12));
        let numbers: Vec<i64> = sizes
            .iter()
            .map(|element| match element {
                InfoValue::Int(value) => *value,
                other => panic!("a size must be an integer, got {other:?}"),
            })
            .collect();
        assert!(numbers.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn alloc_util_allocators_matches_the_oracle_list() {
        // Pinned against the oracle (OTP 29 / erts-17.1):
        // `erl -noshell -eval 'io:format("~p~n",[erlang:system_info(alloc_util_allocators)]).'`
        // — set **and** order.
        let rt = runtime();
        let value = try_value("alloc_util_allocators", &rt).unwrap().unwrap();
        let names: Vec<&str> = match &value {
            InfoValue::List(elements) => elements
                .iter()
                .map(|element| match element {
                    InfoValue::Atom(name) => *name,
                    other => panic!("an allocator name must be an atom, got {other:?}"),
                })
                .collect(),
            other => panic!("alloc_util_allocators must be a list, got {other:?}"),
        };
        assert_eq!(
            names,
            [
                "temp_alloc",
                "sl_alloc",
                "std_alloc",
                "ll_alloc",
                "eheap_alloc",
                "ets_alloc",
                "fix_alloc",
                "literal_alloc",
                "binary_alloc",
                "driver_alloc",
            ]
        );
    }

    #[test]
    fn allocated_areas_has_the_oracle_names_arities_and_derivations() {
        // The oracle's 19 items, in order (`erts_allocated_areas`,
        // `erl_alloc.c:2580-2699`), with `atom_space` as its three-element tuple.
        let rt = runtime();
        let value = try_value("allocated_areas", &rt).unwrap().unwrap();
        let rows = match &value {
            InfoValue::List(rows) => rows,
            other => panic!("allocated_areas must be a list, got {other:?}"),
        };
        let expected = [
            "sys_misc",
            "static",
            "atom_space",
            "atom_table",
            "module_table",
            "export_table",
            "export_list",
            "register_table",
            "fun_table",
            "fun_list",
            "module_refs",
            "loaded_code",
            "dist_table",
            "node_table",
            "bif_timer",
            "process_table",
            "port_table",
            "ets_misc",
            "external_alloc",
        ];
        let pairs: Vec<(String, Vec<i64>)> = rows
            .iter()
            .map(|row| match row {
                InfoValue::Tupled(fields) => {
                    let name = match &fields[0] {
                        InfoValue::Atom(name) => (*name).to_string(),
                        other => panic!("a row's first field must be an atom, got {other:?}"),
                    };
                    let values = fields[1..]
                        .iter()
                        .map(|field| match field {
                            InfoValue::Int(value) => *value,
                            other => panic!("{name}'s value must be an integer, got {other:?}"),
                        })
                        .collect();
                    (name, values)
                }
                other => panic!("a row must be a tuple, got {other:?}"),
            })
            .collect();
        let names: Vec<&str> = pairs.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, expected);
        assert_eq!(pairs[2].1.len(), 2, "atom_space carries a value and a peak");

        // The measured items reflect *this* runtime's own state (never a hardcoded oracle
        // figure): the atom table always holds the boot seed and the runtime always has
        // its root process, while the items with no counterpart are honestly `0`.
        let value_of = |name: &str| -> i64 {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .unwrap_or_else(|| panic!("{name} must be present"))
                .1[0]
        };
        assert!(value_of("atom_table") > 0, "the atom table holds the core seed");
        assert!(value_of("process_table") > 0, "the runtime has its root process");
        for absent in [
            "bif_timer",
            "dist_table",
            "node_table",
            "external_alloc",
            "export_table",
            "fun_table",
        ] {
            assert_eq!(value_of(absent), 0, "{absent} has no counterpart here");
        }
    }

    #[test]
    fn garbage_collection_matches_the_oracle_list() {
        // Pinned against the oracle (OTP 29 / erts-17.1):
        // `erl -noshell -eval 'io:format("~p~n",[erlang:system_info(garbage_collection)]).'`
        // prints `[{max_heap_size,0},{min_bin_vheap_size,46422},{min_heap_size,233},
        // {fullsweep_after,65535}]` — `max_heap_size` is the **bare integer** here, and
        // none of the values is hardcoded: they come from `beam_gc`'s growth table.
        let rt = runtime();
        let value = try_value("garbage_collection", &rt).unwrap().unwrap();
        let expected = [
            ("max_heap_size", 0_i64),
            ("min_bin_vheap_size", 46_422),
            ("min_heap_size", 233),
            ("fullsweep_after", 65_535),
        ];
        let rows = match &value {
            InfoValue::List(rows) => rows,
            other => panic!("garbage_collection must be a list, got {other:?}"),
        };
        assert_eq!(rows.len(), expected.len());
        for (row, (key, expected_value)) in rows.iter().zip(expected) {
            assert_eq!(
                *row,
                InfoValue::Tupled(vec![InfoValue::Atom(key), InfoValue::Int(expected_value)])
            );
        }
    }

    /// The emulator-wide GC knobs follow the configured heap options, pinned against the
    /// oracle (OTP 29.1.1 / erts-17.1):
    ///
    /// ```text
    /// erl +hms 1000 +hmbs 5000 -noshell \
    ///     -eval 'io:format("~p~n",[erlang:system_info(garbage_collection)]), halt().'
    ///   → [{max_heap_size,0},{min_bin_vheap_size,6772},{min_heap_size,1598},
    ///      {fullsweep_after,65535}]
    /// ```
    ///
    /// The *values* are `beam_gc`'s growth-table figures, not literals from the oracle.
    #[test]
    fn garbage_collection_follows_the_configured_heap_options() {
        let argv: Vec<String> = ["+hms", "1000", "+hmbs", "5000"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();
        let config = crate::cli::parse(&argv).expect("+hms 1000 +hmbs 5000 parses");
        let rt = Runtime::new(config).expect("a runtime with the configured heap options");
        let value = try_value("garbage_collection", &rt).unwrap().unwrap();
        let expected = [
            ("max_heap_size", 0_i64),
            ("min_bin_vheap_size", 6772),
            ("min_heap_size", 1598),
            ("fullsweep_after", 65_535),
        ];
        let rows = match &value {
            InfoValue::List(rows) => rows,
            other => panic!("garbage_collection must be a list, got {other:?}"),
        };
        assert_eq!(rows.len(), expected.len());
        for (row, (key, expected_value)) in rows.iter().zip(expected) {
            assert_eq!(
                *row,
                InfoValue::Tupled(vec![InfoValue::Atom(key), InfoValue::Int(expected_value)]),
                "{key}"
            );
        }
        // The heap-policy tuples follow the same options.
        assert_eq!(
            try_value("min_heap_size", &rt).unwrap().unwrap(),
            InfoValue::Tupled(vec![InfoValue::Atom("min_heap_size"), InfoValue::Int(1598)])
        );
        assert_eq!(
            try_value("min_bin_vheap_size", &rt).unwrap().unwrap(),
            InfoValue::Tupled(vec![
                InfoValue::Atom("min_bin_vheap_size"),
                InfoValue::Int(6772)
            ])
        );
    }

    #[test]
    fn port_count_is_the_live_port_count() {
        use beam_engine::{terms, ExecutionContext};

        // A fresh runtime has opened no port (the oracle's own boot opens a
        // `standard_io` port that this runtime has no counterpart for).
        let mut rt = runtime();
        assert_eq!(
            crate::info::system_info_value("port_count", &rt).expect("port_count"),
            InfoValue::Int(0)
        );

        // Open a real `{spawn, "true"}` port through `erlang:open_port/2`: the count rises
        // while it is live and drops once it is closed, so the answer tracks the live port
        // set exactly.
        let port = {
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut rt, &mut sink);
            let spawn = context.atom("spawn").expect("the `spawn` atom");
            let command = terms::make_list(
                &mut context,
                &"true"
                    .bytes()
                    .map(|b| beam_term::make_small(i64::from(b)))
                    .collect::<Vec<_>>(),
            )
            .expect("the command string");
            let name = terms::make_tuple(&mut context, &[spawn, command]).expect("a {spawn, Cmd}");
            let options = terms::make_list(&mut context, &[]).expect("[]");
            context
                .call_bif("erlang", "open_port", 2, &[name, options])
                .expect("erlang:open_port/2")
        };
        assert_eq!(
            crate::info::system_info_value("port_count", &rt).expect("port_count"),
            InfoValue::Int(1),
            "the opened port is counted"
        );
        // `erlang:ports/0` reports exactly the same one port.
        let ports = {
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut rt, &mut sink);
            context
                .call_bif("erlang", "ports", 0, &[])
                .expect("erlang:ports/0")
        };
        assert_eq!(terms::list_elements(ports).expect("a list"), vec![port]);

        {
            let mut sink = beam_bifs::NullSink;
            let mut context = crate::context::RuntimeContext::with_output(&mut rt, &mut sink);
            context
                .call_bif("erlang", "port_close", 1, &[port])
                .expect("erlang:port_close/1");
        }
        assert_eq!(
            crate::info::system_info_value("port_count", &rt).expect("port_count"),
            InfoValue::Int(0),
            "a closed port is no longer counted"
        );
    }
}
