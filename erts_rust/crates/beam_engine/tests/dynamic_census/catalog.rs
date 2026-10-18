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

//! **The named workload catalog** for `tests/dynamic_census.rs` — layers (b) and (c).
//!
//! Layer (b) ([`WORKLOADS`]) is a set of *program* workloads over the real installed
//! stdlib: every `module:function(args)` shape here is copied **verbatim** from an
//! existing oracle-proven suite (the module doc of `dynamic_census.rs` names the
//! suites), never invented. Each workload is one small `fn(&mut ModuleHost) -> Result<Eterm>`
//! so its terms are built through the host's one shared arena and the whole call goes
//! through [`Machine::run_export`](beam_engine::Machine::run_export) — i.e. the
//! interpreter funnel the census counts.
//!
//! Layer (c) ([`FIXTURES`]) drives the committed `.beam` fixtures under
//! `tests/fixtures/` (plus `slice.beam`, owned by the sibling `beam_loader` crate).
//! Every entry point below was read off the fixture's own `.erl` source, not guessed.
//!
//! **No `maps:*` / `ets:*` / `re:*` BIF call appears here**: the test host's BIF floor
//! (`tests/support/bifs.rs`) cannot serve those families, so a workload using them
//! would only record a typed `badarg` — that family is measured as a *blocked
//! mechanism* by the caller instead (census OFF), never as dynamic weight.

use beam_common::Result;
use beam_engine::terms;
use beam_term::{make_small, Eterm};

use crate::support::{chars, int_list, keyed_tuple, term_list, term_tuple, ModuleHost};

/// One named program workload: a label and the body to run.
pub type Workload = (&'static str, fn(&mut ModuleHost) -> Result<Eterm>);

/// One driven export of a committed fixture.
pub type Run = (&'static str, fn(&mut ModuleHost) -> Result<Eterm>);

/// One committed `.beam` fixture and the export entry points driven on it.
pub struct Fixture {
    /// The module name the fixture is registered under (and called through).
    pub module: &'static str,
    /// The `.beam` path, when it lives outside this crate's `tests/fixtures/`
    /// (`slice.beam` is the sibling `beam_loader` crate's fixture, reused by
    /// relative path exactly as `tests/slice.rs` does — never copied).
    pub path: Option<&'static str>,
    /// The exports driven on the fixture.
    pub runs: &'static [Run],
}

/// `slice.beam`, reused by relative path from the sibling `beam_loader` crate.
pub const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// A proper list of `{K, V}` 2-tuples of small integers (the shape `gb_trees` /
/// `orddict` / `lists:keyfind` operate on).
fn int_pairs(host: &mut ModuleHost, pairs: &[(i64, i64)]) -> Result<Eterm> {
    let mut elements = Vec::with_capacity(pairs.len());
    for &(key, value) in pairs {
        elements.push(terms::make_tuple(
            host,
            &[make_small(key), make_small(value)],
        )?);
    }
    terms::make_list(host, &elements)
}

// ---------------------------------------------------------------------------
// Layer (b): program workloads over the real stdlib
// ---------------------------------------------------------------------------

/// Builds an ordered set (`ordsets:from_list/1`) from `values`.
fn ordset(host: &mut ModuleHost, values: &[i64]) -> Result<Eterm> {
    let input = int_list(host, values);
    host.call("ordsets", "from_list", &[input])
}

fn lists_reverse(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::stdlib_lists_reverse_runs_the_real_module`.
    let input = int_list(h, &[10, 20, 30]);
    h.call("lists", "reverse", &[input])
}

fn lists_seq(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::stdlib_lists_seq_runs_the_real_module`.
    h.call("lists", "seq", &[make_small(1), make_small(5)])
}

fn harness_lists_reverse(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::harness_reverse_forwards_to_stdlib_lists`.
    let input = int_list(h, &[10, 20, 30]);
    h.call("stdlists_harness", "reverse", &[input])
}

fn harness_lists_seq(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::harness_seq_forwards_to_stdlib_lists`.
    h.call("stdlists_harness", "seq", &[make_small(2), make_small(5)])
}

fn harness_lists_map_double(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::harness_map_double_applies_a_closure_over_stdlib_lists_map`
    // — the cross-module `make_fun3` + `call_fun2` + `apply_fun` case.
    let input = int_list(h, &[1, 2, 3]);
    h.call("stdlists_harness", "map_double", &[input])
}

fn harness_lists_sort(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::harness_sort_runs_the_real_lists_sort`.
    let input = int_list(h, &[3, 1, 2]);
    h.call("stdlists_harness", "sort", &[input])
}

fn harness_lists_filter_pos(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::harness_filter_pos_matches_the_predicate_result_across_modules`.
    let input = int_list(h, &[-1, 0, 2, 3]);
    h.call("stdlists_harness", "filter_pos", &[input])
}

fn ordsets_from_list(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::stdlib_ordsets_from_list_sorts_and_dedups`.
    ordset(h, &[3, 1, 2, 3])
}

fn ordsets_union(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::stdlib_ordsets_union_intersection_and_subtract`.
    let a = ordset(h, &[1, 2, 3])?;
    let b = ordset(h, &[2, 3, 4])?;
    h.call("ordsets", "union", &[a, b])
}

fn ordsets_intersection(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::stdlib_ordsets_union_intersection_and_subtract`.
    let a = ordset(h, &[1, 2, 3])?;
    let b = ordset(h, &[2, 3, 4])?;
    h.call("ordsets", "intersection", &[a, b])
}

fn ordsets_subtract(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::stdlib_ordsets_union_intersection_and_subtract`.
    let a = ordset(h, &[1, 2, 3])?;
    let b = ordset(h, &[2, 3, 4])?;
    h.call("ordsets", "subtract", &[a, b])
}

fn ordsets_is_element(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::stdlib_ordsets_is_element_returns_the_shared_true_and_false`.
    let list = ordset(h, &[1, 2, 3])?;
    h.call("ordsets", "is_element", &[make_small(2), list])
}

fn proplists_get_value_3(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::stdlib_proplists_get_value_walks_the_property_list`
    // (the default-returning arity).
    let a = keyed_tuple(h, "a", 1);
    let b = keyed_tuple(h, "b", 2);
    let props = term_list(h, &[a, b]);
    let key_c = h.intern("c");
    h.call("proplists", "get_value", &[key_c, props, make_small(9)])
}

fn proplists_get_value_2(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::stdlib_proplists_get_value_walks_the_property_list`.
    let a = keyed_tuple(h, "a", 1);
    let b = keyed_tuple(h, "b", 2);
    let props = term_list(h, &[a, b]);
    let key_b = h.intern("b");
    h.call("proplists", "get_value", &[key_b, props])
}

fn proplists_lookup_all(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_lists.rs::stdlib_proplists_lookup_all_and_get_bool`.
    let a = keyed_tuple(h, "a", 1);
    let b = keyed_tuple(h, "b", 2);
    let props = term_list(h, &[a, b]);
    let key_a = h.intern("a");
    h.call("proplists", "lookup_all", &[key_a, props])
}

fn string_join(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text.rs` (`string:join/2` → `"a-b-c"`).
    let a = chars(h, "a");
    let b = chars(h, "b");
    let c = chars(h, "c");
    let parts = term_list(h, &[a, b, c]);
    let sep = chars(h, "-");
    h.call("string", "join", &[parts, sep])
}

fn string_lowercase(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text.rs` (`string:lowercase("ABC")` → `"abc"`).
    let s = chars(h, "ABC");
    h.call("string", "lowercase", &[s])
}

fn string_titlecase(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text.rs` (`string:titlecase("hello world")` → `"Hello world"`).
    let s = chars(h, "hello world");
    h.call("string", "titlecase", &[s])
}

fn string_tokens(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text.rs` (`string:tokens("a b c", " ")` → `["a","b","c"]`).
    let s = chars(h, "a b c");
    let seps = chars(h, " ");
    h.call("string", "tokens", &[s, seps])
}

fn string_slice_3(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text.rs` (`string:slice("hello", 1, 3)` → `"ell"`).
    let s = chars(h, "hello");
    h.call("string", "slice", &[s, make_small(1), make_small(3)])
}

fn string_trim_2(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text.rs` (`string:trim("  hi  ", both)` → `"hi"`).
    let s = chars(h, "  hi  ");
    let both = h.intern("both");
    h.call("string", "trim", &[s, both])
}

fn base64_decode(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text.rs` (`base64:decode(<<"aGVsbG8=">>)` → `<<"hello">>`).
    let input = terms::make_heap_binary(h, b"aGVsbG8=")?;
    h.call("base64", "decode", &[input])
}

fn base64_encode(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text.rs` (`base64:encode(<<"hi">>)` → `<<"aGk=">>`).
    let input = terms::make_heap_binary(h, b"hi")?;
    h.call("base64", "encode", &[input])
}

fn io_lib_write(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text.rs` (`io_lib:write(42)` → `"42"`).
    h.call("io_lib", "write", &[make_small(42)])
}

fn calendar_date_to_gregorian_days(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections.rs` (`calendar:date_to_gregorian_days({2000,1,1})`
    // → `730485`).
    let day = term_tuple(h, &[make_small(2000), make_small(1), make_small(1)]);
    h.call("calendar", "date_to_gregorian_days", &[day])
}

fn calendar_iso_week_number(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections.rs` (`calendar:iso_week_number({2000,1,1})` → `{1999,52}`).
    let day = term_tuple(h, &[make_small(2000), make_small(1), make_small(1)]);
    h.call("calendar", "iso_week_number", &[day])
}

fn calendar_seconds_to_time(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections.rs` (`calendar:seconds_to_time(3723)` → `{1,2,3}`).
    h.call("calendar", "seconds_to_time", &[make_small(3723)])
}

fn filename_join(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections.rs` (`filename:join("a","b")` → `"a/b"`).
    let a = chars(h, "a");
    let b = chars(h, "b");
    h.call("filename", "join", &[a, b])
}

fn filename_split(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections.rs` (`filename:split("/a/b/c.txt")`).
    let path = chars(h, "/a/b/c.txt");
    h.call("filename", "split", &[path])
}

fn array_set_roundtrip(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections.rs` (`array:from_list/1` + `array:set/3` +
    // `array:get/2` + `array:to_list/1` → `[10,99,30]`).
    let list = int_list(h, &[10, 20, 30]);
    let array = h.call("array", "from_list", &[list])?;
    let updated = h.call("array", "set", &[make_small(1), make_small(99), array])?;
    h.call("array", "to_list", &[updated])
}

fn gb_trees_from_orddict(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections_trees.rs` (`gb_trees:from_orddict/1` + `to_list/1`).
    let pairs = int_pairs(h, &[(1, 10), (2, 20)])?;
    let tree = h.call("gb_trees", "from_orddict", &[pairs])?;
    h.call("gb_trees", "to_list", &[tree])
}

fn gb_trees_iterator_next(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections_trees.rs` (`gb_trees:iterator/1` + `gb_trees:next/1`).
    let pairs = int_pairs(h, &[(1, 10), (2, 20)])?;
    let tree = h.call("gb_trees", "from_orddict", &[pairs])?;
    let iter = h.call("gb_trees", "iterator", &[tree])?;
    let first = h.call("gb_trees", "next", &[iter])?;
    h.call("gb_trees", "next", &[iter]).map(|_| first)
}

fn gb_sets_union(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections_trees.rs` (`gb_sets:union/2` over two `from_list/1`s).
    let a = int_list(h, &[1, 2, 3]);
    let set = h.call("gb_sets", "from_list", &[a])?;
    let b = int_list(h, &[2, 3, 4]);
    let other = h.call("gb_sets", "from_list", &[b])?;
    let union = h.call("gb_sets", "union", &[set, other])?;
    h.call("gb_sets", "to_list", &[union])
}

fn queue_from_list_to_list(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections_queue_maps.rs` (`queue:from_list/1` + `to_list/1`).
    let list = int_list(h, &[1, 2, 3]);
    let queue = h.call("queue", "from_list", &[list])?;
    h.call("queue", "to_list", &[queue])
}

fn queue_split(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections_queue_maps.rs` (`queue:split/2`).
    let list = int_list(h, &[1, 2, 3]);
    let queue = h.call("queue", "from_list", &[list])?;
    h.call("queue", "split", &[make_small(2), queue])
}

fn harness_lists_foldr_sub(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text_more.rs` (`stdlists`/text harness: `lists:foldr/3` with a fun).
    let list = int_list(h, &[1, 2, 3]);
    h.call("stdlib_text_harness", "lists_foldr_sub", &[list])
}

fn harness_lists_search_gt(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text_more.rs` (`lists:search/2` with a fun).
    let list = int_list(h, &[1, 2, 3]);
    h.call("stdlib_text_harness", "lists_search_gt", &[list])
}

fn harness_ordsets_filter_gt(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_text_more.rs` (`ordsets:filter/2` with a fun).
    let set = ordset(h, &[1, 2, 3])?;
    h.call("stdlib_text_harness", "ordsets_filter_gt", &[set])
}

fn harness_lists_zipwith_add(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_more.rs` (`lists:zipwith/3` with a two-argument fun).
    let a = int_list(h, &[1, 2, 3]);
    let b = int_list(h, &[10, 20, 30]);
    h.call("stdlib_more_harness", "lists_zipwith_add", &[a, b])
}

fn harness_orddict_map(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_more.rs` (`orddict:map/2` with a fun over a real orddict).
    let pairs = int_pairs(h, &[(1, 10), (2, 20)])?;
    let dict = h.call("orddict", "from_list", &[pairs])?;
    h.call("stdlib_more_harness", "orddict_map", &[dict])
}

fn harness_ordsets_map_double(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_exports_more.rs` (`ordsets:map/2` with a fun).
    let set = ordset(h, &[1, 2, 3])?;
    h.call("stdlib_exports_harness", "ordsets_map_double", &[set])
}

// --- deliberate clause failures + guard kinds ------------------------------
//
// These are the shapes that reach `func_info`: a call whose **every head clause
// fails** jumps to the function's `func_label` (the label immediately *before*
// `func_info`), and the `FuncInfo` arm then raises class `error` with the bare
// atom `function_clause` — C's `i_func_info_IaaI`
// (`../../erts/emulator/beam/emu/beam_emu.c`). Each of them therefore executes
// `FuncInfo` exactly once **and** executes the failing guard's `Test(<Kind>)`.
// Ground truth: `erl -noshell -eval 'catch dict:update_counter(a, x, d())'` →
// `{'EXIT',{function_clause,…}}` (a real `function_clause`, not `badarg`).

/// `dict:update_counter/3` with a **number** increment — the guard matches.
fn dict_update_counter_hit(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/stdlib_collections.rs` drives `dict` (a hash dictionary); here the
    // third argument is never inspected, only the `is_number(Incr)` guard is.
    let key = h.intern("a");
    h.call("dict", "update_counter", &[key, make_small(1), make_small(0)])
}

/// `dict:update_counter/3` with a **non-number** increment — `is_number` fails,
/// every clause is exhausted, `func_info` raises `function_clause`.
fn dict_update_counter_guard_fails(h: &mut ModuleHost) -> Result<Eterm> {
    // `dict.erl:554` — `update_counter(Key, Incr, D0) when is_number(Incr)` is the
    // function's only clause, so the failed guard lands on `func_info`.
    let key = h.intern("a");
    let not_a_number = h.intern("not_a_number");
    h.call("dict", "update_counter", &[key, not_a_number, make_small(0)])
}

/// `gen:name/1` with an argument matching no clause but the last, whose guard is
/// `is_pid(Pid)` — so `Test(IsPid)` runs and then `func_info` raises.
fn gen_name_pid_guard_fails(h: &mut ModuleHost) -> Result<Eterm> {
    // `gen.erl:644-647` — `name(Pid) when is_pid(Pid) -> Pid.` is the last clause;
    // the three tuple clauses above it fail on their tag tests.
    h.call("gen", "name", &[make_small(42)])
}

/// **Layer (b)** — the program workloads. Every shape is copied from an existing
/// oracle-proven suite (named in each workload's comment).
pub const WORKLOADS: &[Workload] = &[
    ("lists_reverse", lists_reverse),
    ("lists_seq", lists_seq),
    ("harness_lists_reverse", harness_lists_reverse),
    ("harness_lists_seq", harness_lists_seq),
    ("harness_lists_map_double", harness_lists_map_double),
    ("harness_lists_sort", harness_lists_sort),
    ("harness_lists_filter_pos", harness_lists_filter_pos),
    ("ordsets_from_list", ordsets_from_list),
    ("ordsets_union", ordsets_union),
    ("ordsets_intersection", ordsets_intersection),
    ("ordsets_subtract", ordsets_subtract),
    ("ordsets_is_element", ordsets_is_element),
    ("proplists_get_value_3", proplists_get_value_3),
    ("proplists_get_value_2", proplists_get_value_2),
    ("proplists_lookup_all", proplists_lookup_all),
    ("string_join", string_join),
    ("string_lowercase", string_lowercase),
    ("string_titlecase", string_titlecase),
    ("string_tokens", string_tokens),
    ("string_slice_3", string_slice_3),
    ("string_trim_2", string_trim_2),
    ("base64_decode", base64_decode),
    ("base64_encode", base64_encode),
    ("io_lib_write", io_lib_write),
    ("calendar_date_to_gregorian_days", calendar_date_to_gregorian_days),
    ("calendar_iso_week_number", calendar_iso_week_number),
    ("calendar_seconds_to_time", calendar_seconds_to_time),
    ("filename_join", filename_join),
    ("filename_split", filename_split),
    ("array_set_roundtrip", array_set_roundtrip),
    ("gb_trees_from_orddict", gb_trees_from_orddict),
    ("gb_trees_iterator_next", gb_trees_iterator_next),
    ("gb_sets_union", gb_sets_union),
    ("queue_from_list_to_list", queue_from_list_to_list),
    ("queue_split", queue_split),
    ("harness_lists_foldr_sub", harness_lists_foldr_sub),
    ("harness_lists_search_gt", harness_lists_search_gt),
    ("harness_ordsets_filter_gt", harness_ordsets_filter_gt),
    ("harness_lists_zipwith_add", harness_lists_zipwith_add),
    ("harness_orddict_map", harness_orddict_map),
    ("harness_ordsets_map_double", harness_ordsets_map_double),
    ("dict_update_counter_hit", dict_update_counter_hit),
    ("dict_update_counter_guard_fails", dict_update_counter_guard_fails),
    ("gen_name_pid_guard_fails", gen_name_pid_guard_fails),
];

// ---------------------------------------------------------------------------
// Layer (c): the committed `.beam` fixtures
// ---------------------------------------------------------------------------

// --- slice (the sibling `beam_loader` fixture) ------------------------------

fn slice_add(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/slice.rs`: `slice:add/2` → the vertical slice's entry point.
    h.call("slice", "add", &[make_small(2), make_small(3)])
}

fn slice_fib(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/slice.rs`: `slice:fib/1` → 55 at n=10.
    h.call("slice", "fib", &[make_small(10)])
}

fn slice_pick(h: &mut ModuleHost) -> Result<Eterm> {
    // `tests/slice.rs`: `pick(a)` → 1 (a literal-head clause selection).
    let a = h.intern("a");
    h.call("slice", "pick", &[a])
}

// --- tuples ----------------------------------------------------------------

fn tuples_mk(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/tuples.erl`: `mk(A, B) -> {A, B}` (`put_tuple2`).
    let a = h.intern("a");
    let b = h.intern("b");
    h.call("tuples", "mk", &[a, b])
}

fn tuples_first(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/tuples.erl`: `first/1` reads element 0 of a 2-tuple.
    let a = h.intern("a");
    let b = h.intern("b");
    let tuple = term_tuple(h, &[a, b]);
    h.call("tuples", "first", &[tuple])
}

fn tuples_classify(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/tuples.erl`: `classify/1` (`select_tuple_arity`).
    let a = h.intern("a");
    let b = h.intern("b");
    let tuple = term_tuple(h, &[a, b]);
    h.call("tuples", "classify", &[tuple])
}

// --- records ---------------------------------------------------------------

/// The record tuple `#r{A, B, C}` = `{r, A, B, C}` (`fixtures/records.erl`).
fn record(h: &mut ModuleHost) -> Result<Eterm> {
    let tag = h.intern("r");
    Ok(term_tuple(
        h,
        &[tag, make_small(1), make_small(2), make_small(3)],
    ))
}

fn records_update_a(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/records.erl`: `update_a/1` (`update_record`, reuse hint).
    let source = record(h)?;
    h.call("records", "update_a", &[source])
}

fn records_chain(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/records.erl`: `chain/1` builds the record locally and updates it.
    h.call("records", "chain", &[make_small(5)])
}

fn records_keep(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/records.erl`: `keep/1` returns `{Updated, Original}` (a fresh tuple).
    let source = record(h)?;
    h.call("records", "keep", &[source])
}

// --- floats ----------------------------------------------------------------

fn floats_add(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/floats.erl`: `add/2` (`fadd`).
    let a = terms::make_float(h, 1.5)?;
    let b = terms::make_float(h, 2.25)?;
    h.call("floats", "add", &[a, b])
}

fn floats_temps(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/floats.erl`: `temps/4` (four FR temporaries).
    let a = terms::make_float(h, 1.5)?;
    let b = terms::make_float(h, 2.0)?;
    let c = terms::make_float(h, 0.5)?;
    let d = terms::make_float(h, 4.0)?;
    h.call("floats", "temps", &[a, b, c, d])
}

fn floats_conv(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/floats.erl`: `conv/1` (`fconv` from an untyped argument).
    h.call("floats", "conv", &[make_small(3)])
}

// --- bitstrings / bscreate / bsmatch / bsstring ---------------------------

fn bitstrings_mk(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bitstrings.erl`: `mk/2` (`bs_create_bin`, integer segments).
    h.call("bitstrings", "mk", &[make_small(1), make_small(2)])
}

fn bitstrings_pair(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bitstrings.erl`: `pair/2`.
    h.call("bitstrings", "pair", &[make_small(1), make_small(2)])
}

fn bitstrings_first8(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bitstrings.erl`: `first8/1` (`bs_start_match3` + `bs_get_integer2`).
    let bin = terms::make_heap_binary(h, &[7, 8, 9])?;
    h.call("bitstrings", "first8", &[bin])
}

fn bitstrings_take2(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bitstrings.erl`: `take2/1` (two integer reads + tail).
    let bin = terms::make_heap_binary(h, &[7, 8, 9])?;
    h.call("bitstrings", "take2", &[bin])
}

fn bitstrings_tail1(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bitstrings.erl`: `tail1/1` (`bs_get_tail`).
    let bin = terms::make_heap_binary(h, &[7, 8, 9])?;
    h.call("bitstrings", "tail1", &[bin])
}

fn bscreate_f32(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bscreate.erl`: `f32/1` (a 32-bit float segment).
    let value = terms::make_float(h, 1.5)?;
    h.call("bscreate", "f32", &[value])
}

fn bscreate_u8(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bscreate.erl`: `u8/1` (a `utf8` segment).
    h.call("bscreate", "u8", &[make_small(65)])
}

fn bsmatch_str(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bsmatch.erl`: `str/1` (a literal string folded into `bs_match`).
    let bin = terms::make_heap_binary(h, b"abXYZ")?;
    h.call("bsmatch", "str", &[bin])
}

fn bsmatch_f32(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bsmatch.erl`: `f32/1` (`bs_get_float2`, 32 bits big-endian).
    let bin = terms::make_heap_binary(h, &[0x3F, 0x80, 0x00, 0x00])?;
    h.call("bsmatch", "f32", &[bin])
}

fn bsmatch_u8(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bsmatch.erl`: `u8/1` (`bs_get_utf8`).
    let bin = terms::make_heap_binary(h, b"A")?;
    h.call("bsmatch", "u8", &[bin])
}

fn bsstring_prefix(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bsstring.erl`: `prefix/1` (`<<"ab", X/binary>>`).
    let bin = terms::make_heap_binary(h, b"XY")?;
    h.call("bsstring", "prefix", &[bin])
}

fn bsstring_two(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bsstring.erl`: `two/1` (two string constants around a value).
    h.call("bsstring", "two", &[make_small(65)])
}

fn bsstring_combo(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bsstring.erl`: `combo/2` (a string between two dynamic segments).
    let bin = terms::make_heap_binary(h, b"PQ")?;
    h.call("bsstring", "combo", &[make_small(65), bin])
}

// --- bignum / exceptions / catchclasses -----------------------------------

fn bignum_literal(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bignum.erl`: `lit_2pow59/0` — a bignum **instruction operand** the
    // engine materialises through `terms::make_integer`.
    h.call("bignum", "lit_2pow59", &[])
}

fn bignum_add(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bignum.erl`: `add/2` over two small ints (the `gc_bif '+'` route).
    h.call("bignum", "add", &[make_small(1), make_small(2)])
}

fn exceptions_case_clause(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/exceptions.erl`: `case_clause(2)` → `caught_case_clause`.
    h.call("exceptions", "case_clause", &[make_small(2)])
}

fn exceptions_badmatch(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/exceptions.erl`: `badmatch(ok)` → `matched`.
    let ok = h.intern("ok");
    h.call("exceptions", "badmatch", &[ok])
}

fn exceptions_nested(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/exceptions.erl`: `nested(2)` → `outer_case_clause`.
    h.call("exceptions", "nested", &[make_small(2)])
}

fn exceptions_oldcatch_match(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/exceptions.erl`: `oldcatch_match(2)` → `caught_exit`.
    h.call("exceptions", "oldcatch_match", &[make_small(2)])
}

fn catchclasses_plain(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/catchclasses.erl`: `plain/0` (no exception, the value passes through).
    h.call("catchclasses", "plain", &[])
}

fn catchclasses_thrown(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/catchclasses.erl`: `thrown/0` — `catch erlang:raise(throw, ball, [])`.
    h.call("catchclasses", "thrown", &[])
}

fn catchclasses_exited(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/catchclasses.erl`: `exited/0` — class `exit` through `erlang:raise/3`.
    h.call("catchclasses", "exited", &[])
}

fn catchclasses_errored(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/catchclasses.erl`: `errored/0` — the `badmatch` opcode's raise, caught.
    h.call("catchclasses", "errored", &[])
}

// --- mapfx / tailcall / funs ----------------------------------------------

fn mapfx_get(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/mapfx.erl`: `get/1` (`get_map_elements` over a flat map).
    let a = h.intern("a");
    let b = h.intern("b");
    let map = terms::make_flatmap(h, &[(a, make_small(1)), (b, make_small(2))])?;
    h.call("mapfx", "get", &[map])
}

fn mapfx_has(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/mapfx.erl`: `has/1` (`has_map_fields`).
    let a = h.intern("a");
    let b = h.intern("b");
    let map = terms::make_flatmap(h, &[(a, make_small(1)), (b, make_small(2))])?;
    h.call("mapfx", "has", &[map])
}

fn mapfx_assoc(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/mapfx.erl`: `assoc/1` (`put_map_assoc`).
    let a = h.intern("a");
    let map = terms::make_flatmap(h, &[(a, make_small(1))])?;
    h.call("mapfx", "assoc", &[map])
}

fn tailcall_loop(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/tailcall.erl`: `loop(5, [])` → `[1,2,3,4,5]` (`call_only` chain).
    h.call("tailcall", "loop", &[make_small(5), beam_term::NIL])
}

fn tailcall_even(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/tailcall.erl`: `even(10)` → `true` (a mutual tail-call chain).
    h.call("tailcall", "even", &[make_small(10)])
}

fn tailcall_tailrev(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/tailcall.erl`: `tailrev([a])` → `[[a]]` (`call_ext_last` over an import).
    let value = h.intern("a");
    h.call("tailcall", "tailrev", &[value])
}

fn funs_test(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/funs.erl`: `test/0` (`make_fun3` + `call_fun2`) → 6.
    h.call("funs", "test", &[])
}

fn funs_capture(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/funs.erl`: `capture/0` (a closure with a captured free variable) → 15.
    h.call("funs", "capture", &[])
}

fn funs_dynamic(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/funs.erl`: `dynamic(erlang, '+', 2, 3)` → `{ok, 5}` (`apply 2`).
    let module = h.intern("erlang");
    let function = h.intern("+");
    h.call(
        "funs",
        "dynamic",
        &[module, function, make_small(2), make_small(3)],
    )
}

/// **Layer (c)** — the committed fixtures and the exports driven on each. Every
/// entry point is the one the fixture's own `.erl` source (and the suite that owns
/// it) drives.
pub const FIXTURES: &[Fixture] = &[
    Fixture {
        module: "slice",
        path: Some(SLICE_BEAM),
        runs: &[
            ("slice:add/2", slice_add),
            ("slice:fib/1", slice_fib),
            ("slice:pick/1", slice_pick),
        ],
    },
    Fixture {
        module: "tuples",
        path: None,
        runs: &[
            ("tuples:mk/2", tuples_mk),
            ("tuples:first/1", tuples_first),
            ("tuples:classify/1", tuples_classify),
        ],
    },
    Fixture {
        module: "records",
        path: None,
        runs: &[
            ("records:update_a/1", records_update_a),
            ("records:chain/1", records_chain),
            ("records:keep/1", records_keep),
        ],
    },
    Fixture {
        module: "floats",
        path: None,
        runs: &[
            ("floats:add/2", floats_add),
            ("floats:temps/4", floats_temps),
            ("floats:conv/1", floats_conv),
        ],
    },
    Fixture {
        module: "bitstrings",
        path: None,
        runs: &[
            ("bitstrings:mk/2", bitstrings_mk),
            ("bitstrings:pair/2", bitstrings_pair),
            ("bitstrings:first8/1", bitstrings_first8),
            ("bitstrings:take2/1", bitstrings_take2),
            ("bitstrings:tail1/1", bitstrings_tail1),
        ],
    },
    Fixture {
        module: "bscreate",
        path: None,
        runs: &[
            ("bscreate:f32/1", bscreate_f32),
            ("bscreate:u8/1", bscreate_u8),
        ],
    },
    Fixture {
        module: "bsmatch",
        path: None,
        runs: &[
            ("bsmatch:str/1", bsmatch_str),
            ("bsmatch:f32/1", bsmatch_f32),
            ("bsmatch:u8/1", bsmatch_u8),
        ],
    },
    Fixture {
        module: "bsstring",
        path: None,
        runs: &[
            ("bsstring:prefix/1", bsstring_prefix),
            ("bsstring:two/1", bsstring_two),
            ("bsstring:combo/2", bsstring_combo),
        ],
    },
    Fixture {
        module: "bignum",
        path: None,
        runs: &[
            ("bignum:lit_2pow59/0", bignum_literal),
            ("bignum:add/2", bignum_add),
        ],
    },
    Fixture {
        module: "exceptions",
        path: None,
        runs: &[
            ("exceptions:case_clause/1", exceptions_case_clause),
            ("exceptions:badmatch/1", exceptions_badmatch),
            ("exceptions:nested/1", exceptions_nested),
            ("exceptions:oldcatch_match/1", exceptions_oldcatch_match),
        ],
    },
    Fixture {
        module: "catchclasses",
        path: None,
        runs: &[
            ("catchclasses:plain/0", catchclasses_plain),
            ("catchclasses:thrown/0", catchclasses_thrown),
            ("catchclasses:exited/0", catchclasses_exited),
            ("catchclasses:errored/0", catchclasses_errored),
        ],
    },
    Fixture {
        module: "mapfx",
        path: None,
        runs: &[
            ("mapfx:get/1", mapfx_get),
            ("mapfx:has/1", mapfx_has),
            ("mapfx:assoc/1", mapfx_assoc),
        ],
    },
    Fixture {
        module: "tailcall",
        path: None,
        runs: &[
            ("tailcall:loop/2", tailcall_loop),
            ("tailcall:even/1", tailcall_even),
            ("tailcall:tailrev/1", tailcall_tailrev),
        ],
    },
    Fixture {
        module: "funs",
        path: None,
        runs: &[
            ("funs:test/0", funs_test),
            ("funs:capture/0", funs_capture),
            ("funs:dynamic/4", funs_dynamic),
        ],
    },
    // --- the widened export sets -------------------------------------------
    //
    // A fixture is registered once per module file; these entries drive the
    // **additional** exports of modules the roster above already registers (the
    // `register`/`load` pair is memoized by module name, so a second entry costs
    // nothing). Every one of them is an export an existing suite already drives,
    // and each was chosen because it is the only shape in the fixture that emits
    // the variant named in its comment.
    Fixture {
        module: "floats",
        path: None,
        runs: &[
            // `fixtures/floats.erl`: `divide/2` → `fdiv` (the only `fdiv` in the corpus
            // of committed fixtures), `negate/1` → `fnegate`.
            ("floats:divide/2", floats_divide),
            ("floats:negate/1", floats_negate),
        ],
    },
    Fixture {
        module: "mapfx",
        path: None,
        runs: &[
            // `fixtures/mapfx.erl`: `exact/1` and `assoc_exact/1` are the `Map#{K := V}`
            // **exact** update shapes → `put_map_exact`.
            ("mapfx:exact/1", mapfx_exact),
            ("mapfx:assoc_exact/1", mapfx_assoc_exact),
        ],
    },
    Fixture {
        module: "bsmatch",
        path: None,
        runs: &[
            // `fixtures/bsmatch.erl`: `strlong/1` is the only export with a
            // `{test,bs_match_string,…}` instruction → `BsMatchString`; `sk16/1` and
            // `sk32/1` are the `bs_skip_utf16`/`bs_skip_utf32` shapes, `u16/1`/`u32/1`
            // the `bs_get_utf16`/`bs_get_utf32` ones.
            ("bsmatch:strlong/1", bsmatch_strlong),
            ("bsmatch:sk16/1", bsmatch_sk16),
            ("bsmatch:sk32/1", bsmatch_sk32),
            ("bsmatch:u16/1", bsmatch_u16),
            ("bsmatch:u32/1", bsmatch_u32),
        ],
    },
    Fixture {
        module: "exceptions",
        path: None,
        runs: &[
            // `fixtures/exceptions.erl`: the `if`/`try`-of/guard-`try` clause failures,
            // every one of them **caught inside the module** (so they count as
            // executed, not as an error — the census semantics).
            ("exceptions:if_clause/1", exceptions_if_clause),
            ("exceptions:try_clause/1", exceptions_try_clause),
            ("exceptions:guard_badarith/1", exceptions_guard_badarith),
            ("exceptions:deep/1", exceptions_deep),
        ],
    },
    Fixture {
        // `fixtures/excpaths.erl`: the whole `try … catch` path matrix. `fc/1` fails
        // every clause of its own function (`func_info` → `function_clause`), and the
        // `catch_*` entries swallow the same shapes in a live `try`.
        module: "excpaths",
        path: None,
        runs: &[
            ("excpaths:fc/1", excpaths_fc),
            ("excpaths:catch_fc/1", excpaths_catch_fc),
            ("excpaths:catch_bm/1", excpaths_catch_bm),
        ],
    },
    Fixture {
        // `fixtures/catchtrace.erl`: the `try … catch C:R:S` shapes that cook the
        // raise-time snapshot → `build_stacktrace`.
        module: "catchtrace",
        path: None,
        runs: &[
            ("catchtrace:run/0", catchtrace_run),
            ("catchtrace:run2/0", catchtrace_run2),
        ],
    },
    Fixture {
        // `fixtures/stacktrace_frames.erl`: a raise re-raised through nested frames.
        module: "stacktrace_frames",
        path: None,
        runs: &[
            ("stacktrace_frames:t0/0", stacktrace_frames_t0),
            ("stacktrace_frames:t3/0", stacktrace_frames_t3),
        ],
    },
    Fixture {
        // `fixtures/atomapply.erl` → `fixtures/atomtarget.erl`: a fully dynamic
        // `apply/3` on runtime **atom** targets (the `atom_name` seam route).
        module: "atomapply",
        path: None,
        runs: &[("atomapply:run/3", atomapply_run)],
    },
];

// The layer-(c) fns appended above (Rust does not care about declaration order).

fn floats_divide(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/floats.erl`: `divide/2` → `fdiv`; oracle: `floats:divide(6.0,2.0)` → 3.0.
    let a = terms::make_float(h, 6.0)?;
    let b = terms::make_float(h, 2.0)?;
    h.call("floats", "divide", &[a, b])
}

fn floats_negate(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/floats.erl`: `negate/1` → `fnegate`; oracle: `floats:negate(2.5)` → -2.5.
    let value = terms::make_float(h, 2.5)?;
    h.call("floats", "negate", &[value])
}

fn mapfx_exact(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/mapfx.erl`: `exact/1` — `Map#{a := _}` (`put_map_exact`).
    let a = h.intern("a");
    let map = terms::make_flatmap(h, &[(a, make_small(1))])?;
    h.call("mapfx", "exact", &[map])
}

fn mapfx_assoc_exact(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/mapfx.erl`: `assoc_exact/1` — a mixed `=>`/`:=` update.
    let a = h.intern("a");
    let map = terms::make_flatmap(h, &[(a, make_small(1))])?;
    h.call("mapfx", "assoc_exact", &[map])
}

fn bsmatch_strlong(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bsmatch.erl`: `strlong/1` (`{test,bs_match_string,…}`) — needs the
    // full 10-byte literal to succeed.
    let bin = terms::make_heap_binary(h, b"abcdefghij")?;
    h.call("bsmatch", "strlong", &[bin])
}

fn bsmatch_sk16(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bsmatch.erl`: `sk16/1` (`bs_skip_utf16`) over `<<0,65,66>>`.
    let bin = terms::make_heap_binary(h, &[0x00, 0x41, 0x42])?;
    h.call("bsmatch", "sk16", &[bin])
}

fn bsmatch_sk32(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bsmatch.erl`: `sk32/1` (`bs_skip_utf32`) over `<<0,0,0,65>>`.
    let bin = terms::make_heap_binary(h, &[0x00, 0x00, 0x00, 0x41, 0x42])?;
    h.call("bsmatch", "sk32", &[bin])
}

fn bsmatch_u16(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bsmatch.erl`: `u16/1` (`bs_get_utf16`) over `<<0,65>>`.
    let bin = terms::make_heap_binary(h, &[0x00, 0x41])?;
    h.call("bsmatch", "u16", &[bin])
}

fn bsmatch_u32(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/bsmatch.erl`: `u32/1` (`bs_get_utf32`) over `<<0,0,0,65>>`.
    let bin = terms::make_heap_binary(h, &[0x00, 0x00, 0x00, 0x41])?;
    h.call("bsmatch", "u32", &[bin])
}

fn exceptions_if_clause(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/exceptions.erl`: `if_clause(2)` → the `if` has no true branch.
    h.call("exceptions", "if_clause", &[make_small(2)])
}

fn exceptions_try_clause(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/exceptions.erl`: `try_clause(2)` → `try … of` has no matching clause.
    h.call("exceptions", "try_clause", &[make_small(2)])
}

fn exceptions_guard_badarith(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/exceptions.erl`: `guard_badarith(0)` → a guard's `1/X`.
    h.call("exceptions", "guard_badarith", &[make_small(0)])
}

fn exceptions_deep(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/exceptions.erl`: `deep(2)` → a raise four frames down, caught above.
    h.call("exceptions", "deep", &[make_small(2)])
}

fn excpaths_fc(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/excpaths.erl`: `fc(b)` — every clause of `fc/1` fails, so `func_info`
    // raises `function_clause` (declared raise; the oracle agrees).
    let b = h.intern("b");
    h.call("excpaths", "fc", &[b])
}

fn excpaths_catch_fc(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/excpaths.erl`: `catch_fc(b)` — the same failure, caught in-module.
    let b = h.intern("b");
    h.call("excpaths", "catch_fc", &[b])
}

fn excpaths_catch_bm(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/excpaths.erl`: `catch_bm(b)` — a `badmatch` caught in-module.
    let b = h.intern("b");
    h.call("excpaths", "catch_bm", &[b])
}

fn catchtrace_run(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/catchtrace.erl`: `run/0` — `try … catch C:R:S` (`build_stacktrace`).
    h.call("catchtrace", "run", &[])
}

fn catchtrace_run2(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/catchtrace.erl`: `run2/0` — the second stacktrace-cooking shape.
    h.call("catchtrace", "run2", &[])
}

fn stacktrace_frames_t0(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/stacktrace_frames.erl`: `t0/0`.
    h.call("stacktrace_frames", "t0", &[])
}

fn stacktrace_frames_t3(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/stacktrace_frames.erl`: `t3/0` — three nested frames.
    h.call("stacktrace_frames", "t3", &[])
}

fn atomapply_run(h: &mut ModuleHost) -> Result<Eterm> {
    // `fixtures/atomapply.erl`: `run/3` → `apply(M, F, Args)` on runtime atoms.
    let module = h.intern("atomtarget");
    let function = h.intern("target_fun");
    let empty = terms::make_list(h, &[])?;
    h.call("atomapply", "run", &[module, function, empty])
}

// ---------------------------------------------------------------------------
// Layer (c), the mailbox-seam fixture: `recv.beam`
// ---------------------------------------------------------------------------

/// One layer-(c) run that needs the engine's `Mailbox` seam.
///
/// [`crate::CensusHost`] is the host that owns a mailbox (it mirrors the
/// `ProbeMailbox`/`TestMailbox` shape the receive suites use), so these bodies
/// take it rather than the plain [`ModuleHost`].
pub type MailboxRun = (&'static str, fn(&mut crate::CensusHost) -> Result<Eterm>);

/// One mailbox-seam fixture and the exports driven on it.
pub struct MailboxFixture {
    /// The module name the fixture is registered under.
    pub module: &'static str,
    /// The exports driven on the fixture.
    pub runs: &'static [MailboxRun],
}

/// `recv:wait/0` with one queued message: `loop_rec` + `remove_message`.
fn recv_wait_queued(h: &mut crate::CensusHost) -> Result<Eterm> {
    let hello = h.intern("hello");
    h.push_message(hello);
    h.call("recv", "wait", &[])
}

/// `recv:wait/0` on an **empty** mailbox: `loop_rec` then `wait`, which suspends —
/// the non-blocking entry folds that into the engine's typed `blocked` error.
fn recv_wait_empty(h: &mut crate::CensusHost) -> Result<Eterm> {
    h.call("recv", "wait", &[])
}

/// `recv:sel/0` with a non-atom message: the catch-all clause binds it.
fn recv_sel(h: &mut crate::CensusHost) -> Result<Eterm> {
    h.push_message(make_small(99));
    h.call("recv", "sel", &[])
}

/// `recv:scan/0` with `42` ahead of `{bar,7}`: `42` fails both clauses and reaches
/// `loop_rec_end`, then `{bar,7}` matches.
fn recv_scan(h: &mut crate::CensusHost) -> Result<Eterm> {
    let bar = h.intern("bar");
    let pair = terms::make_tuple(h, &[bar, make_small(7)])?;
    h.push_message(make_small(42));
    h.push_message(pair);
    h.call("recv", "scan", &[])
}

/// `recv:afterzero/0` on an empty mailbox: `loop_rec` then `timeout`.
fn recv_afterzero(h: &mut crate::CensusHost) -> Result<Eterm> {
    h.call("recv", "afterzero", &[])
}

/// `recv:aftervar(0)` on an empty mailbox: a **runtime** zero reaches
/// `wait_timeout`, which falls through to `timeout`.
fn recv_aftervar_zero(h: &mut crate::CensusHost) -> Result<Eterm> {
    h.call("recv", "aftervar", &[make_small(0)])
}

/// `recv:aftervar(5)` on an empty mailbox: `wait_timeout` suspends (declared raise).
fn recv_aftervar_blocks(h: &mut crate::CensusHost) -> Result<Eterm> {
    h.call("recv", "aftervar", &[make_small(5)])
}

/// `recv:marked/0`: the whole `recv_marker_*` prologue (`reserve`/`bind`/`use`/
/// `clear`) around a `self() ! {Ref, 7}`.
fn recv_marked(h: &mut crate::CensusHost) -> Result<Eterm> {
    h.call("recv", "marked", &[])
}

/// `recv:bomb/1`: the `send` opcode (`Pid ! 42`).
fn recv_bomb(h: &mut crate::CensusHost) -> Result<Eterm> {
    let destination = h.self_term();
    h.call("recv", "bomb", &[destination])
}

/// **Layer (c), the mailbox-seam fixture.** `fixtures/recv.erl` is the one fixture
/// whose exports need the `Mailbox` seam, so it is driven on the census host.
pub const MAILBOX_FIXTURES: &[MailboxFixture] = &[MailboxFixture {
    module: "recv",
    runs: &[
        ("recv:wait/0 (queued)", recv_wait_queued),
        ("recv:wait/0 (empty)", recv_wait_empty),
        ("recv:sel/0", recv_sel),
        ("recv:scan/0", recv_scan),
        ("recv:afterzero/0", recv_afterzero),
        ("recv:aftervar/0", recv_aftervar_zero),
        ("recv:aftervar/5", recv_aftervar_blocks),
        ("recv:marked/0", recv_marked),
        ("recv:bomb/1", recv_bomb),
    ],
}];

/// The index `tests/receive.rs` seeds for each of the entries above is 0- or
/// 1-based in the same way: a receive on an empty mailbox is the only shape that
/// reaches `wait`/`wait_timeout` (a queued message matches at `loop_rec`).
const _SEEDING_NOTE: () = ();
