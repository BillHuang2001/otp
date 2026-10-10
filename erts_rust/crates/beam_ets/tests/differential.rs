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

//! **Differential ETS evidence suite**: one battery of ETS operations, run twice
//! — against the *system* `erl` (the OTP 29 / erts-17.1 oracle) and against this
//! crate's `beam_ets` — and compared as identical canonical renderings.
//!
//! # Why this file exists
//!
//! The crate's own unit tests assert *our* understanding of `ets:*`. This suite
//! asserts the thing that actually matters for contract A.1 (term/key/value
//! semantics) and A.8 (`ets` introspection): that a real `erl` observably agrees.
//! Every expectation below was **derived from `erl`**, not written from memory.
//!
//! # How the comparison works
//!
//! * One battery of cases (`battery.rs` on the Rust side, the embedded Erlang
//!   program in `erl_program.rs` on the oracle side) performs the same operations
//!   in the same order.
//! * Every result is reduced to a canonical textual form — small integers, atoms,
//!   tuples and proper lists only — by the *same* ~20-line renderer on each side
//!   (`render`/`t`/`l`/`srt`/`inorder`/`cap`). Table ids/handles, `memory`, `pids`
//!   and floats never appear; anything whose order is not part of the contract is
//!   rendered **sorted** (`srt`), and only `ordered_set` iteration is rendered in
//!   order (`inorder`).
//! * Each case prints/returns exactly one `case_name -> rendered_result` pair, so
//!   the two runs are compared line by line. `term_to_binary`/ETF hex is *not*
//!   used: byte-identical encoding across the two implementations is not
//!   guaranteed, whereas the textual renderer is.
//!
//! # What is asserted
//!
//! 1. [`rust_battery_matches_the_embedded_oracle_table`] — our implementation
//!    matches the `erl`-derived table. Runs **always**, so the expectations stay
//!    meaningful even on a machine with no Erlang.
//! 2. [`oracle_battery_matches_the_embedded_oracle_table`] — the live oracle
//!    still reproduces the embedded table (catches an oracle upgrade or a stale
//!    expectation).
//! 3. [`rust_battery_agrees_with_the_live_oracle`] — the real differential gate:
//!    live `erl` output vs live `beam_ets` output, case by case.
//!
//! # Skipping without Erlang
//!
//! Tests 2, 3 and 4 resolve `erl`/`erlc` from `$ERTS_ORACLE` and then `$PATH`. The
//! oracle cross-checks are **required by default** — a genuinely absent Erlang makes
//! them fail, so a silently-skipped differential can never look like a pass. Set
//! `BEAM_ETS_ALLOW_NO_ORACLE=1` to opt out explicitly (see `tests/common/oracle.rs`,
//! the crate's single gate); `BEAM_ETS_REQUIRE_ORACLE=1` still forces the requirement.
//!
//! # Battery coverage
//!
//! All four kinds (`set`, `ordered_set`, `bag`, `duplicate_bag`) × insert /
//! insert_new / duplicate-object / delete_object (matching + non-matching) /
//! tab2list+size / delete-by-key / take; `{keypos, 2}` insert+lookup+update;
//! `ordered_set` navigation (`first`/`next`/`last`/`prev`, absent anchors, empty
//! table, `'$end_of_table'`) plus the **mixed-key key-order** contract over
//! integers (incl. negative and zero), atoms, tuples, lists and binaries, and the
//! integer/float interleaving (the arithmetic key order, pinned separately),
//! plus the **replaced key term** of an arithmetically-equal-but-different-term
//! insert (`{1,a}` then `{1.0,b}`, rendered through `is_float/1`);
//! `update_counter/3,4` (scalar, `{Pos,Incr}`,
//! `{Pos,Incr,Threshold,SetValue}`, operation lists, defaults, absent keys,
//! counter-incapable kinds, and a threshold/default combination); `update_element/3`;
//! `lookup_element/3,4`; `select/2`
//! with variables/wildcards/literals/guards (comparison, type test,
//! `andalso`/`orelse`, `/`, `float/1`, `element/2`, `abs/1` over a small integer
//! and a float, the map guards `map_size/1`/`is_map_key/2`/`map_get/2`,
//! `tuple_size/1`, `byte_size/1`, `bit_size/1`) and bodies (`'$_'`, `'$$'`,
//! `'$N'`, constants, `{{…}}` construction, map construction) — incl. a whole
//! `fun2ms`-shaped spec family
//! (`select`/`select_count`/`select_replace`/`select_delete`/`match`/`match_object`/
//! `match_spec_run`); `select_delete/2`+`select_count/2`; the `match` family;
//! malformed specs (`badarg`); `ets:match_spec_run/2` (variable/whole-element/
//! all-bindings bodies, a repeated head variable, multi-clause first-match-wins,
//! an empty list, a non-list `badarg`) and the body-error `EXIT` semantics;
//! `ets:select_replace/2` (an arithmetic rewrite, an arity-1 result, the `bag`
//! refusal, a key-changing `badarg` and the identity body); the limit/continuation
//! forms (`select/3`+`select/1`, `match/3`+`match/1`, `match_object/3`+`match_object/1`
//! chained to completion, `limit 0` ⇒ `badarg`, an empty table ⇒ `'$end_of_table'`);
//! the `info` group (the registered `name` vs the
//! `named_table` flag, the renderable `ets:info/2` option fields for four option
//! combinations, the runtime-shape fields `heir`/`node`/`decentralized_counters`,
//! an unknown `ets:info/2` key, `lookup_element/3,4` on the
//! multi-object kinds, and `ets:insert/2`-over-a-list atomicity);
//! the **ownership** surface (`ets:give_away/3` success + the badarg cases, and the
//! `{heir, ...}` option of `ets:setopts/2`); the **access-control** matrix
//! (`protected`/`private`/`public` × owner/non-owner over the whole operation
//! set: the read-gated `lookup`/`tab2list`/`first`/`next`/`select`/`match_object`
//! plus the read-gated `safe_fixtable/2`, the write-gated
//! `insert`/`insert_new`/`delete`/`delete_object`/`delete_all_objects`/`take`/
//! `select_delete`, and the **ungated** `ets:info/1,2`) and the
//! `ets:setopts/2` option surface (only `{heir, ...}` is settable); the//! **reclamation-observable** semantics (repeated insert/delete churn and
//! `delete_all_objects/1` pinned only by `size`/`member`/`lookup`/`tab2list`,
//! never `memory` or `id`); and two
//! 10 001-key tables (`set` + `ordered_set`) exercising size, lookups, the
//! ordered `next` chain and a guarded `select_count`.
//!
//! # Behaviours real ETS forced (recorded here and in the case comments)
//!
//! * **`ets:size/1` does not exist in OTP 29** — `ets:info(T, size)` is the
//!   supported spelling (`erlang:function_exported(ets, size, 1)` is `false`; a
//!   call raises `undef`). The oracle program therefore uses `ets:info/2`.
//! * **A match-spec body is ANDed into one result**: only the *last* body element
//!   survives (`[{'$1','$2'},[],['$1','$2']]` returns `[2]`), which is why every
//!   case here uses a single-element body.
//! * **`{{…}}` is the tuple-construction form** — a one-element tuple whose single
//!   element is a tuple template (see `select_body_construct`).
//! * **`ets:update_element/3` accepts `pos == arity`** (and rejects `pos == keypos`
//!   and `pos == 0`) — `update_element_variants` pins the `badarg` boundary at
//!   `pos > arity`.
//! * **`ets:update_counter/4` accepts an empty operation list `[]`** (it deposits
//!   the default object and returns `[]`), while `ets:update_element(T, K, [])` is
//!   `true` and changes nothing.
//! * **`ets:match_delete/2` answers `true`**, not the number deleted — our
//!   `Table::match_delete` returns the count, so the differential renders a
//!   successful call as `true` and verifies the deletion through `size` and
//!   `tab2list` inside the same case.
//! * **`name` and `named_table` are independent.** `ets:new(my_name, [])` records
//!   `name => my_name` but reports `named_table => false`; only the `named_table`
//!   option sets the flag (and makes the name resolve to the table). Our
//!   `TableOptions::set_name` records the name only, so the two must agree without
//!   the flag being implied.
//! * **`ets:info/2` rejects an unknown key** with `badarg` (it does not return
//!   `undefined`).
//! * **`heir` and `node` are fixed**: the oracle reports `heir => none` (no heir
//!   was set) and `node => nonode@nohost` (the `-noshell` VM is not distributed).
//!   **`decentralized_counters` is the CATREE path**: `true` iff the table is an
//!   `ordered_set` with `write_concurrency` on, and `false` otherwise
//!   (`info_static_fields` pins both cases).
//! * **`ets:insert/2` over a list is atomic**: one invalid element makes the whole
//!   call `badarg` and inserts none of the elements, not even the valid ones that
//!   precede it — the oracle program pins this through `tab2list` after the
//!   failure.
//! * **`ets:lookup_element/3` answers a *list* on a multi-object kind**
//!   (`bag`/`duplicate_bag`): the `Pos`-th element of every object under the key,
//!   in stored order (a single object yields a one-element list, `duplicate_bag`
//!   duplicates are preserved). Only `set`/`ordered_set` are scalar. An absent key
//!   is `badarg` (`/3`) or the default (`/4`), `Pos == 0` is `badarg`, and one
//!   object too short for `Pos` makes the whole call `badarg`.
//! * **`ets:match_spec_run/2` needs an already-compiled spec** (`match_spec_compile/1`):
//!   a raw clause list is `badarg` there, whereas this crate's `match_spec_run`
//!   takes the raw term. The all-bindings atom `'$$'` has **two** characters but an
//!   Erlang *source literal* `'$$'` lexes to the one-character `'$'`, so the oracle
//!   program builds it with `list_to_atom([36,36])`.
//! * **A continuation is an opaque ref** — it is never rendered. The continuation
//!   cases chain the `/3` entry point and its `/1` resumptions to exhaustion and
//!   compare only the concatenated, materialized results.
//! * **The access-control matrix is `Db_READ`/`Db_WRITE`**: the owner may always
//!   read/write; a non-owner may read but not write a `protected` table, may do
//!   neither on a `private` one, and may do both on a `public` one — every denial
//!   is `badarg`. `access_control` pins all three modes × owner/non-owner over
//!   the whole operation set: the reads (`lookup`/`tab2list`/`first`/`next`/
//!   `select`/`match_object`), the read-gated `safe_fixtable/2` (a non-owner may
//!   fix a `protected` table but not a `private` one), the writes (`insert`/
//!   `insert_new`/`delete`/`delete_object`/`delete_all_objects`/`take`/
//!   `select_delete`), and the **ungated** `ets:info/1`/`info/2` — which succeed
//!   even for a non-owner of a `private` table, the discriminating fact.//! * **`ets:setopts/2` settables only `{heir, ...}`** on OTP 29:
//!   `{write_concurrency,_}` / `{read_concurrency,_}` / `{decentralized_counters,_}`
//!   and any unknown option raise `badarg` (they are `ets:new/2` options, not
//!   runtime ones), and a non-owner `setopts` raises `badarg`
//!   (`setopts_surface`).
//! * **Numbers order by value across representations** — an `ordered_set`
//!   compares keys arithmetically (`erl_db_tree.c` `cmp_key_eq`, `CMP =
//!   erts_cmp(a, b, 0, 0)`), so it interleaves integers and floats
//!   (`-3 < -2.5 < 0 < 1.5 < 5`) and `1` and `1.0` are the *same* key.
//!   `beam_ets` reproduces this (`compare::cmp_arith`), so no key-ordering case
//!   is a recorded divergence and every one is compared strictly.
//! * **A replacing insert also replaces the stored key term**: inserting
//!   `{1.0,b}` over `{1,a}` collapses to one object and makes `ets:first`/
//!   `ets:last`/`ets:next` report the *last-inserted* key term (`1.0`, and `1` in
//!   the reverse order) — `ordered_replaced_key_term` pins both orders plus a
//!   three-key (`1`/`1.0`/`2`) `next` that crosses the collapsed key.
//! * **Reclamation is invisible.** Repeated insert/delete churn and
//!   `delete_all_objects/1` leave only an observable trace (`size`, `member`,
//!   `lookup`, `tab2list`); `memory` and the table `id` are never rendered
//!   (`reclamation_observable`).
//! * **Match-spec arithmetic follows the oracle.** `erl` runs match-spec
//!   arithmetic through the ordinary BIFs, so `/` is always a float and a
//!   float/bignum operand yields a float/bignum. `DefaultArithHook` mirrors that:
//!   `+ - * /` (and unary `+`/`-`) compute in `f64` when a float is involved and
//!   `/` is always a float, while integer-only operators and a result that leaves
//!   the 60-bit small range stay the typed `unsupported_ets`. A guard's capability
//!   gap is never swallowed — only the oracle's own `badarg` skips a clause.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

// The support modules live in `tests/differential/` so that they are *not*
// themselves integration-test targets (Cargo only builds the `.rs` files
// directly under `tests/` as targets); the explicit `#[path]` is needed because
// a crate root resolves its modules relative to its own directory.
#[path = "differential/battery/mod.rs"]
mod battery;
#[path = "differential/erl_program.rs"]
mod erl_program;

mod common;

use battery::rust_battery;
use common::oracle;

/// How long the oracle is given to compile and then to run. Generous: the two
/// 10 001-key cases build and walk a large table, and a loaded CI box can be slow.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(120);

/// The `erl`-derived reference rendering of every case.
///
/// Derived by running the program in `erl_program.rs` under the system `erl`
/// (OTP 29 / erts-17.1) — never written by hand. Kept in the oracle's `emit/2`
/// order (the map below sorts, but the *list* order is the readable record).
const EXPECTED: &[(&str, &str)] = &[
    ("set_insert_lookup", "{[{1,10}],[],true,false,3,2,false}"),
    ("set_insert_new", "{true,false,true,2,[{1,10}]}"),
    ("set_identical_object", "{1,[{1,20}]}"),
    ("set_delete_object_same", "{0,false,[]}"),
    ("set_delete_object_other", "{1,true,[{1,20}]}"),
    ("set_tab2list_count", "{5,5}"),
    ("set_delete_by_key", "{1,false,true}"),
    ("set_take", "{[{1,10}],[],1,false}"),
    (
        "ordered_set_insert_lookup",
        "{[{1,10}],[],true,false,3,2,false}",
    ),
    ("ordered_set_insert_new", "{true,false,true,2,[{1,10}]}"),
    ("ordered_set_identical_object", "{1,[{1,20}]}"),
    ("ordered_set_delete_object_same", "{0,false,[]}"),
    ("ordered_set_delete_object_other", "{1,true,[{1,20}]}"),
    ("ordered_set_tab2list_count", "{5,5}"),
    ("ordered_set_delete_by_key", "{1,false,true}"),
    ("ordered_set_take", "{[{1,10}],[],1,false}"),
    ("bag_insert_lookup", "{[{1,10}],[],true,false,3,2,false}"),
    ("bag_insert_new", "{true,false,true,2,[{1,10}]}"),
    ("bag_identical_object", "{2,[{1,10},{1,20}]}"),
    ("bag_delete_object_same", "{1,true,[{1,10}]}"),
    ("bag_delete_object_other", "{2,true,[{1,10},{1,20}]}"),
    ("bag_tab2list_count", "{5,5}"),
    ("bag_delete_by_key", "{1,false,true}"),
    ("bag_take", "{[{1,10},{1,20}],[],1,false}"),
    (
        "duplicate_bag_insert_lookup",
        "{[{1,10}],[],true,false,3,2,false}",
    ),
    ("duplicate_bag_insert_new", "{true,false,true,2,[{1,10}]}"),
    (
        "duplicate_bag_identical_object",
        "{3,[{1,10},{1,10},{1,20}]}",
    ),
    ("duplicate_bag_delete_object_same", "{1,true,[{1,10}]}"),
    (
        "duplicate_bag_delete_object_other",
        "{2,true,[{1,10},{1,20}]}",
    ),
    ("duplicate_bag_tab2list_count", "{5,5}"),
    ("duplicate_bag_delete_by_key", "{1,false,true}"),
    ("duplicate_bag_take", "{[{1,10},{1,20}],[],1,false}"),
    ("keypos2_basic", "{[{a,1}],[{z,1}],2,true,true}"),
    ("keypos2_update_counter", "{15,[{tag,1,15}]}"),
    ("keypos2_update_element", "{true,[{tag,1,99,20}]}"),
    (
        "ordered_iteration",
        "{1,3,[1,2,3],[3,2,1],[1,2,3],$end_of_table,$end_of_table}",
    ),
    (
        "ordered_absent_bounds",
        "{20,10,10,$end_of_table,$end_of_table,30}",
    ),
    (
        "ordered_empty_nav",
        "{$end_of_table,$end_of_table,$end_of_table,$end_of_table}",
    ),
    (
        "ordered_mixed_keys",
        "{[i2,i3,i1,a1,a2,t1,t2,l1,b1],i2,b1,[i2,i3,i1,a1,a2,t1,t2,l1,b1],[b1,l1,t2,t1,a2,a1,i1,i3,i2]}",
    ),
    (
        "ordered_float_key_order",
        "{[i2,f2,i3,f1,i1],i2,i1,[i2,f2,i3,f1,i1],[i1,f1,i3,f2,i2]}",
    ),
    (
        "ordered_replaced_key_term",
        "{{1,true,true,$end_of_table},{1,false,false,$end_of_table},{2,true,false,2}}",
    ),
    (
        "set_insert_new_key_exists",
        "{true,false,false,true,2,[{1,10}],[{2,20}]}",
    ),
    (
        "ordered_set_insert_new_key_exists",
        "{true,false,false,true,2,[{1,10}],[{2,20}]}",
    ),
    (
        "bag_insert_new_key_exists",
        "{true,false,false,true,2,[{1,10}],[{2,20}]}",
    ),
    (
        "duplicate_bag_insert_new_key_exists",
        "{true,false,false,true,2,[{1,10}],[{2,20}]}",
    ),
    (
        "bag_duplicate_object_multiplicity",
        "{2,[{1,10},{1,20}],[{1,10},{1,20}],[{1,10},{1,20}],2,2}",
    ),
    (
        "duplicate_bag_duplicate_object_multiplicity",
        "{3,[{1,10},{1,10},{1,20}],[{1,10},{1,10},{1,20}],[{1,10},{1,10},{1,20}],3,3}",
    ),
    (
        "bag_delete_object_all_occurrences",
        "{1,[{1,20}],[{1,20}]}",
    ),
    (
        "duplicate_bag_delete_object_all_occurrences",
        "{1,[{1,20}],[{1,20}]}",
    ),
    ("bag_size_counts_objects", "{5,5,5,5}"),
    ("duplicate_bag_size_counts_objects", "{5,5,5,5}"),
    (
        "bag_take_and_delete_key",
        "{[{1,10},{1,20},{1,30}],1,0,false,false}",
    ),
    (
        "duplicate_bag_take_and_delete_key",
        "{[{1,10},{1,20},{1,30}],1,0,false,false}",
    ),
    ("bag_update_counter_badarg", "{badarg,badarg}"),
    ("duplicate_bag_update_counter_badarg", "{badarg,badarg}"),
    ("bag_update_element_badarg", "{badarg,[{1,10,20}]}"),
    ("duplicate_bag_update_element_badarg", "{badarg,[{1,10,20}]}"),
    (
        "duplicate_bag_select_replace",
        "{ok:2,[{1,20},{1,40}]}",
    ),
    (
        "duplicate_bag_select_replace_identity",
        "{ok:2,[{1,10},{1,10}]}",
    ),
    ("multi_lookup_element_or", "{[10,20],dflt,badarg}"),
    ("multi_lookup_element", "{[10,20],badarg,badarg}"),
    ("bag_match_delete", "{ok:true,3,1,[{2,30}]}"),
    ("bag_select_delete", "{ok:2,[{2,30}],1}"),
    ("bag_keypos2", "{3,[{a,1},{b,1}],[{c,2}],true,3}"),
    ("duplicate_bag_match_delete", "{ok:true,4,1,[{2,30}]}"),
    ("duplicate_bag_select_delete", "{ok:3,[{2,30}],1}"),
    (
        "duplicate_bag_keypos2",
        "{3,[{a,1},{a,1},{b,1}],[{c,2}],true,4}",
    ),
    (
        "duplicate_bag_select_replace_changed_key",
        "{badarg,[{1,10},{1,20}]}",
    ),
    ("ordered_set_delete_object_exact_eq", "{1,0}"),
    ("ordered_set_keypos2", "{3,[b,c,a],1,3,[c]}"),
    ("ordered_set_update_element", "{15,true,[{1,15,99}]}"),
    ("ordered_set_update_counter_default", "{5,[{1,5}]}"),
    (
        "hash_nav_absent",
        "[{$end_of_table,$end_of_table,badarg,badarg,badarg,badarg},{$end_of_table,$end_of_table,badarg,badarg,badarg,badarg},{$end_of_table,$end_of_table,badarg,badarg,badarg,badarg}]",
    ),
    ("hash_key_walk", "[{[1,2],2},{[1,2],2},{[1,2],2}]"),
    (
        "hash_last_prev_alias_first_next",
        "[{true,true,true,true,true,true,[1,2,3,4,5]},{true,true,true,true,true,true,[1,2,3,4,5]},{true,true,true,true,true,true,[1,2,3,4,5]}]",
    ),
    (
        "hash_select_reverse_aliases_select",
        "[{true,[{1,20},{2,30}]},{true,[{1,10},{1,20},{2,30}]},{true,[{1,10},{1,20},{2,30}]}]",
    ),
    ("update_counter_3_variants", "{15,12,25,[13,26],[{1,13,26}]}"),
    ("update_counter_3_absent", "{badarg,badarg,0}"),
    ("update_counter_3_threshold", "[ok:100,ok:100]"),
    (
        "update_counter_4_default",
        "{5,[{1,5,0}],10,3,[{7,3,0}]}",
    ),
    (
        "update_counter_4_list",
        "{[11,21],[{1,11,21}],[16],[],[{9,0,0}]}",
    ),
    (
        "update_counter_4_threshold",
        "[{100,[{1,100}]},{12,[{1,12}]},{100,[{1,100}]},{3,[{1,3}]},{100,[{1,100}]}]",
    ),
    (
        "update_counter_threshold_default",
        "{15,[{1,15}],-1,[{1,-1}],0,[{1,0}],-5,[{2,-5}],77,[{2,77}]}",
    ),
    ("update_counter_multi_kind", "[badarg,badarg]"),
    (
        "update_counter_bignum_overflow",
        "{576460752303423488,[{1,576460752303423488}],576460752303423487,[{1,576460752303423487}]}",
    ),
    (
        "update_counter_bignum_kinds",
        "[{576460752303423488,1267650600228805862249006628864,[{1,1267650600228805862249006628864}]},{576460752303423488,1267650600228805862249006628864,[{1,1267650600228805862249006628864}]}]",
    ),
    (
        "update_counter_bignum_operand",
        "{1267650600228229401496703205386,10,[{1,10}]}",
    ),
    (
        "update_counter_bignum_element",
        "{-1267650600228229401496703205371,-1267650600228229401496703205374,[{1,-1267650600228229401496703205374}]}",
    ),
    (
        "update_counter_bignum_add",
        "{1613215145994376956305797881764370268938305349227256869814279,6277101735386680763835789423207666416102355444464034512903,[{1,6277101735386680763835789423207666416102355444464034512903}]}",
    ),
    (
        "update_counter_bignum_mixed_sign",
        "{0,-1267650600228229401496703205377,-1,[{1,-1}]}",
    ),
    (
        "update_counter_bignum_threshold",
        "[{7,[{1,7}]},{-7,[{1,-7}]},{1267650600228229401496703205376,[{1,1267650600228229401496703205376}]}]",
    ),
    (
        "update_counter_bignum_warp_value",
        "{1267650600228229401496703205376,[{1,1267650600228229401496703205376}]}",
    ),
    (
        "update_counter_bignum_list_ops",
        "{[1267650600228229401496703205376,-1267650600228229401496703205376],[{1,1267650600228229401496703205376,-1267650600228229401496703205376}]}",
    ),
    (
        "update_counter_bignum_position_badarg",
        "{badarg,ok:11,badarg,badarg,[{1,11}]}",
    ),
    (
        "update_element_variants",
        "{true,[{1,99,20,30}],true,[{1,5,2,30}],true,[{1,5,2,30}],false,badarg,badarg,badarg,[{1,5,2,30}]}",
    ),
    (
        "take_and_lookup_element",
        "{10,20,badarg,badarg,badarg,default,10,default,badarg}",
    ),
    ("select_head_vars", "[10,20]"),
    ("select_wildcard_literal", "[{1,a,10}]"),
    ("select_guard_comparison", "[2,3]"),
    ("select_guard_typetest", "[2]"),
    ("select_guard_andalso_orelse", "{[1,2],[1,3]}"),
    ("select_guard_div", "ok:[2]"),
    ("select_guard_float", "ok:[2]"),
    ("select_guard_element", "ok:[2,3]"),
    ("select_guard_abs", "{ok:[2,3],ok:[1]}"),
    ("select_guard_map_size", "ok:[1]"),
    ("select_guard_is_map_key", "ok:[1]"),
    ("select_guard_map_get", "ok:[1]"),
    ("select_guard_tuple_size", "ok:[1]"),
    ("select_guard_size_family", "ok:[1]"),
    (
        "select_guard_ceil_floor",
        "{ok:[1,2],ok:[2],ok:[1]}",
    ),
    ("select_guard_max_min", "{ok:[1],ok:[2]}"),
    ("select_guard_binary_part2", "ok:[1]"),
    ("select_guard_binary_part3", "ok:[1]"),
    ("select_guard_is_record3", "ok:[true,false,false]"),
    (
        "select_guard_is_function2",
        "{ok:[],ok:[false,false]}",
    ),
    ("select_guard_is_integer3", "ok:[2]"),
    (
        "select_body_node0",
        "ok:[nonode@nohost,nonode@nohost]",
    ),
    (
        "select_guard_xor_body",
        "{ok:[1,2],ok:[false,false]}",
    ),
    ("select_body_bnot", "ok:[-11,-21]"),
    (
        "select_head_map_nonliteral_key",
        "{badarg,badarg}",
    ),
    ("select_guard_float_arith", "ok:[2]"),
    ("select_body_whole", "[{1,a,10},{2,b,20}]"),
    ("select_body_var", "[1,2]"),
    ("select_body_const", "[ok,ok]"),
    ("select_body_construct", "[{1,a},{2,b}]"),
    ("select_body_map_construct", "ok:[10,20]"),
    ("select_delete_and_count", "{0,0,2,2,1,[1]}"),
    (
        "match_family",
        "{[{1,a,10},{2,a,20},{3,a,30}],[[1,10],[2,20],[3,30]],true,3,2,[2,3]}",
    ),
    (
        "malformed_specs",
        "[badarg,badarg,badarg,badarg,badarg,badarg,badarg,ok:[{1,10}]]",
    ),
    (
        "match_spec_run_bindings",
        "{[1,2,3],[{1,10},{2,20},{3,30}],[[1,10],[2,20],[3,30]]}",
    ),
    ("match_spec_run_repeated_var", "[1,2]"),
    ("match_spec_run_multi_clause", "[small,big,big]"),
    ("match_spec_run_empty_and_badarg", "{[],badarg}"),
    ("body_error_exit", "[EXIT,EXIT]"),
    (
        "fun2ms_spec_family",
        "{[2,3],2,[{1,1,10},{2,2,20},{3,3,30}],[[1,10],[2,20],[3,30]],[{1,1,10},{2,2,20},{3,3,30}],[[1,1,10],[2,2,20],[3,3,30]],[{1,2},{3,4}],ok:2,[{1,10},{2,40},{3,60}],2,[{1,10}]}",
    ),
    ("select_replace_small_int", "{ok:2,[{1,10},{2,40},{3,60}]}"),
    ("select_replace_arity1", "{ok:2,[{1},{2}]}"),
    ("select_replace_bag_badarg", "{badarg,[{1,10}]}"),
    (
        "select_replace_changed_key_badarg",
        "{badarg,[{1,10},{2,20}]}",
    ),
    ("select_replace_identity", "{ok:2,[{1,10},{2,20}]}"),
    ("select_limit_chain", "[1,2,3,4,5]"),
    ("match_limit_chain", "[[1,x],[2,x],[3,x],[4,x],[5,x]]"),
    (
        "match_object_limit_chain",
        "[{1,x},{2,x},{3,x},{4,x},{5,x}]",
    ),
    ("select_limit_zero_badarg", "badarg"),
    ("select_limit_empty_table", "$end_of_table"),
    (
        "info_name_named_table",
        "{anon_case_name,false,named_case_name,true}",
    ),
    (
        "info_option_fields",
        "{{diff,false,set,1,protected,false,false,false},{diff,false,set,2,protected,false,false,false},{diff,false,ordered_set,1,public,true,true,true},{diff,false,set,1,private,false,false,false}}",
    ),
    (
        "info_static_fields",
        "{none,nonode@nohost,false,none,nonode@nohost,true}",
    ),
    ("info_unknown_key", "badarg"),
    (
        "lookup_element_multi",
        "{[10,20],[2],[10,10,20],10,badarg,default,badarg,badarg}",
    ),
    (
        "insert_list_atomic",
        "{badarg,[{1,10}],badarg,[{a,1}]}",
    ),
    (
        "reclamation_observable",
        "{5,[false,true,true,false],[{4,4}],0,[],1,[{7,70}]}",
    ),
    ("ownership_give_away", "{ok:true,badarg,badarg,badarg}"),
    ("ownership_heir", "{none,ok:true,ok:true,none,badarg}"),
    (
        "access_control",
        "{{{ok:[{1,10}],ok:[{1,10},{2,20}],ok:1,ok:2,ok:[{1,10},{2,20}],ok:[{1,10},{2,20}],ok:true,ok:[compressed,decentralized_counters,heir,id,keypos,memory,name,named_table,node,owner,protection,read_concurrency,size,type,write_concurrency],ok:2,ok:true,ok:false,ok:true,ok:true,ok:true,ok:[{1,10}],ok:2},{ok:[{1,10}],ok:[{1,10},{2,20}],ok:1,ok:2,ok:[{1,10},{2,20}],ok:[{1,10},{2,20}],ok:true,ok:[compressed,decentralized_counters,heir,id,keypos,memory,name,named_table,node,owner,protection,read_concurrency,size,type,write_concurrency],ok:2,badarg,badarg,badarg,badarg,badarg,badarg,badarg}},{{ok:[{1,10}],ok:[{1,10},{2,20}],ok:1,ok:2,ok:[{1,10},{2,20}],ok:[{1,10},{2,20}],ok:true,ok:[compressed,decentralized_counters,heir,id,keypos,memory,name,named_table,node,owner,protection,read_concurrency,size,type,write_concurrency],ok:2,ok:true,ok:false,ok:true,ok:true,ok:true,ok:[{1,10}],ok:2},{badarg,badarg,badarg,badarg,badarg,badarg,badarg,ok:[compressed,decentralized_counters,heir,id,keypos,memory,name,named_table,node,owner,protection,read_concurrency,size,type,write_concurrency],ok:2,badarg,badarg,badarg,badarg,badarg,badarg,badarg}},{{ok:[{1,10}],ok:[{1,10},{2,20}],ok:1,ok:2,ok:[{1,10},{2,20}],ok:[{1,10},{2,20}],ok:true,ok:[compressed,decentralized_counters,heir,id,keypos,memory,name,named_table,node,owner,protection,read_concurrency,size,type,write_concurrency],ok:2,ok:true,ok:false,ok:true,ok:true,ok:true,ok:[{1,10}],ok:2},{ok:[{1,10}],ok:[{1,10},{2,20}],ok:1,ok:2,ok:[{1,10},{2,20}],ok:[{1,10},{2,20}],ok:true,ok:[compressed,decentralized_counters,heir,id,keypos,memory,name,named_table,node,owner,protection,read_concurrency,size,type,write_concurrency],ok:2,ok:true,ok:false,ok:true,ok:true,ok:true,ok:[{1,10}],ok:2}}}",
    ),
    ("setopts_surface", "{ok:true,ok:true,badarg,badarg,badarg,badarg,badarg}"),
    (
        "set_large",
        "{10001,[{0,0},{10000,20000},{5000,10000}],5000}",
    ),
    (
        "ordered_large",
        "{10001,0,10000,10001,5001,$end_of_table,[{0,0},{10000,20000},{5000,10000}],5000}",
    ),
];

// ---------------------------------------------------------------------------
// The oracle: resolving `erl`/`erlc`, running the battery
// ---------------------------------------------------------------------------

/// The resolved oracle executables.
struct Oracle {
    erl: PathBuf,
    erlc: PathBuf,
}

/// Resolve the oracle from `$ERTS_ORACLE`, then `$PATH`.
///
/// Returns `None` when no Erlang is installed (the caller skips), but **fails
/// loudly** when `$ERTS_ORACLE` names something that is not an executable: a
/// misconfigured override must not be mistaken for "no oracle on this machine".
fn resolve_oracle() -> Option<Oracle> {
    let erl = match env::var_os("ERTS_ORACLE") {
        Some(value) => {
            let candidate = PathBuf::from(&value);
            assert!(
                oracle::is_executable(&candidate),
                "$ERTS_ORACLE is set to '{}', which is not an executable file",
                candidate.display()
            );
            candidate
        }
        None => oracle::find_in_path("erl")?,
    };
    // `erlc` normally sits next to `erl`; fall back to `$PATH` (a wrapper script
    // may install only `erl`).
    let sibling = erl.with_file_name("erlc");
    let erlc = if oracle::is_executable(&sibling) {
        sibling
    } else {
        oracle::find_in_path("erlc")?
    };
    Some(Oracle { erl, erlc })
}
/// Compile the embedded oracle program and run its battery, returning the
/// `case_name -> rendered_result` pairs it emitted.
///
/// Panics with the captured stderr when the oracle fails to compile, crashes its
/// battery (`BATTERY_CRASH`), exits non-zero or exceeds [`ORACLE_TIMEOUT`] — a
/// broken oracle must never be mistaken for a skip.
fn run_oracle(oracle: &Oracle) -> Vec<(String, String)> {
    // The tests run concurrently in one process, so the scratch directory must be
    // unique per *call*, not just per process.
    static ATTEMPT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let attempt = ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "beam_ets_diff_oracle_{}_{attempt}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("the oracle scratch directory is creatable");
    let source = dir.join("beam_ets_diff.erl");
    fs::write(&source, erl_program::ERL_PROGRAM).expect("the oracle program is writable");

    let mut compile = Command::new(&oracle.erlc);
    compile
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .current_dir(&dir);
    let compiled = oracle::run_bounded(&mut compile, ORACLE_TIMEOUT).unwrap_or_else(|error| {
        panic!(
            "erlc ({}) could not compile the oracle program: {error}",
            oracle.erlc.display()
        )
    });
    assert!(
        compiled.status.success(),
        "erlc ({}) failed with {}:\n{}",
        oracle.erlc.display(),
        compiled.status,
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut run = Command::new(&oracle.erl);
    run.arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        // `current_dir` confines a crash dump (`erl_crash.dump`) to the scratch
        // directory instead of polluting the repository.
        .current_dir(&dir)
        .arg("-eval")
        .arg("beam_ets_diff:run()")
        .arg("-s")
        .arg("init")
        .arg("stop");
    let output = oracle::run_bounded(&mut run, ORACLE_TIMEOUT).unwrap_or_else(|error| {
        panic!(
            "erl ({}) could not run the oracle battery: {error}",
            oracle.erl.display()
        )
    });
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !stderr.contains("BATTERY_CRASH"),
        "the oracle battery crashed:\n{stderr}"
    );
    assert!(
        output.status.success(),
        "erl ({}) exited with {}:\n{stderr}",
        oracle.erl.display(),
        output.status
    );

    let emitted = parse_emitted(&String::from_utf8_lossy(&output.stdout));
    let _ = fs::remove_dir_all(&dir);
    emitted
}

/// Parse the oracle's `name|rendered` lines.
fn parse_emitted(stdout: &str) -> Vec<(String, String)> {
    let mut cases = Vec::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let (name, rendered) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("the oracle emitted a malformed line: {line:?}"));
        cases.push((name.to_string(), rendered.to_string()));
    }
    cases
}

// ---------------------------------------------------------------------------
// Comparing
// ---------------------------------------------------------------------------

/// Index cases by name, asserting the names are unique (a duplicate would
/// otherwise silently shadow a case).
fn case_map(cases: &[(String, String)], what: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (name, rendered) in cases {
        assert!(
            map.insert(name.clone(), rendered.clone()).is_none(),
            "{what} emitted the case `{name}` twice"
        );
    }
    map
}

/// The embedded table as a map (asserting it has no duplicate case names).
fn expected_map() -> BTreeMap<String, String> {
    let cases: Vec<(String, String)> = EXPECTED
        .iter()
        .map(|(name, rendered)| (name.to_string(), rendered.to_string()))
        .collect();
    case_map(&cases, "the embedded expectation table")
}

/// Cases where `beam_ets` is **known to differ from the system `erl` today**.
///
/// Currently **empty**: the match-spec arithmetic gap is closed — `DefaultArithHook`
/// computes float `+ - * /` (and the always-float `/`) faithfully — so every battery
/// case agrees with the oracle.
///
/// A recorded divergence is excluded from the equality assertion and pinned by
/// [`known_divergences_are_still_present`], which fails — telling you to delete the
/// entry — the moment `beam_ets` matches the oracle. Keeping an entry in the battery
/// (rather than dropping it) keeps the oracle's real behaviour in `EXPECTED` and the
/// gap actionable.
const KNOWN_DIVERGENCES: &[(&str, &str)] = &[];
/// `true` when `name` is a recorded divergence.
fn is_known_divergence(name: &str) -> bool {
    KNOWN_DIVERGENCES.iter().any(|(case, _)| *case == name)
}

/// A human-readable report of every case the two sides disagree about, or `None`
/// when they agree completely. Recorded divergences are skipped.
fn differences(
    expected: &BTreeMap<String, String>,
    actual: &BTreeMap<String, String>,
) -> Option<String> {
    let mut report = String::new();
    for (name, wanted) in expected {
        if is_known_divergence(name) {
            continue;
        }
        match actual.get(name) {
            None => report.push_str(&format!("  {name}: MISSING (expected {wanted})\n")),
            Some(got) if got != wanted => {
                report.push_str(&format!("  {name}:\n    expected: {wanted}\n    actual:   {got}\n"));
            }
            Some(_) => {}
        }
    }
    for name in actual.keys() {
        if !expected.contains_key(name) && !is_known_divergence(name) {
            report.push_str(&format!("  {name}: UNEXPECTED\n"));
        }
    }
    if report.is_empty() {
        None
    } else {
        Some(report)
    }
}

/// Assert two case maps agree, with a full per-case report on failure.
fn assert_agree(expected: &BTreeMap<String, String>, actual: &BTreeMap<String, String>, what: &str) {
    if let Some(report) = differences(expected, actual) {
        panic!("{what} differs from the oracle reference:\n{report}");
    }
}

/// Resolve the oracle, or skip the calling test with a note.
///
/// `None` means "skip": this environment explicitly accepted a missing oracle
/// (`BEAM_ETS_ALLOW_NO_ORACLE=1`). Because the oracle is **required by default**, a
/// genuinely absent Erlang **fails** the test instead — see `oracle::skip`.
fn oracle_or_skip(test: &str) -> Option<Oracle> {
    match resolve_oracle() {
        Some(oracle) => Some(oracle),
        None => {
            oracle::skip(
                test,
                "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl` on $PATH); the \
                 embedded, erl-derived expectation table is still checked by \
                 `rust_battery_matches_the_embedded_oracle_table`",
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Our implementation against the `erl`-derived table. Always runs, so the
/// differential evidence keeps its meaning on a machine without Erlang.
#[test]
fn rust_battery_matches_the_embedded_oracle_table() {
    let expected = expected_map();
    let rust = case_map(&rust_battery(), "the beam_ets battery");
    assert_agree(&expected, &rust, "the beam_ets battery");
}

/// The live oracle against the embedded table: catches an oracle upgrade (a
/// changed ETS semantic) or a stale expectation.
#[test]
fn oracle_battery_matches_the_embedded_oracle_table() {
    let Some(oracle) = oracle_or_skip("oracle_battery_matches_the_embedded_oracle_table") else {
        return;
    };
    let emitted = run_oracle(&oracle);
    eprintln!(
        "[differential] oracle: {} — {} cases emitted",
        oracle.erl.display(),
        emitted.len()
    );
    assert_eq!(
        emitted.len(),
        EXPECTED.len(),
        "the oracle emitted {} cases but the embedded table has {}",
        emitted.len(),
        EXPECTED.len()
    );
    let expected = expected_map();
    let oracle_cases = case_map(&emitted, "the oracle battery");
    assert_agree(&expected, &oracle_cases, "the live oracle battery");
}

/// **The compatibility gate**: the live oracle and the live `beam_ets` must
/// produce the identical rendering for every case (except the recorded
/// divergences in [`KNOWN_DIVERGENCES`]).
#[test]
fn rust_battery_agrees_with_the_live_oracle() {
    let Some(oracle) = oracle_or_skip("rust_battery_agrees_with_the_live_oracle") else {
        return;
    };
    let oracle_cases = case_map(&run_oracle(&oracle), "the oracle battery");
    let rust_cases = case_map(&rust_battery(), "the beam_ets battery");
    eprintln!(
        "[differential] oracle: {} — comparing {} cases against beam_ets",
        oracle.erl.display(),
        oracle_cases.len()
    );
    assert_agree(&oracle_cases, &rust_cases, "the beam_ets battery");
}

/// Every recorded divergence must still be *present on both sides* and must
/// still *differ* — so the record cannot silently rot. When `beam_ets` is fixed
/// this test fails with an instruction to delete the entry (which re-enables the
/// strict comparison of that case).
#[test]
fn known_divergences_are_still_present() {
    let Some(oracle) = oracle_or_skip("known_divergences_are_still_present") else {
        return;
    };
    let oracle_cases = case_map(&run_oracle(&oracle), "the oracle battery");
    let rust_cases = case_map(&rust_battery(), "the beam_ets battery");
    for (name, explanation) in KNOWN_DIVERGENCES {
        let oracle_value = oracle_cases
            .get(*name)
            .unwrap_or_else(|| panic!("{name}: the oracle no longer emits this case"));
        let rust_value = rust_cases
            .get(*name)
            .unwrap_or_else(|| panic!("{name}: the beam_ets battery no longer emits this case"));
        assert_ne!(
            oracle_value, rust_value,
            "{name} now AGREES between erl and beam_ets — the divergence was fixed: delete it \
             from KNOWN_DIVERGENCES so the case is compared strictly again.\nRecorded reason: \
             {explanation}"
        );
        eprintln!(
            "[differential] KNOWN DIVERGENCE {name}:\n  erl:       {oracle_value}\n  beam_ets:  {rust_value}\n  reason:    {explanation}"
        );
    }
}
