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

//! The **randomised** differential trace for the `ordered_set` interior.
//!
//! The 191 fixed cases in `tests/differential.rs` pin one shape each; the sibling
//! `tests/ets_bag.rs::randomised_multi_kind_trace_matches_the_live_oracle` drives a
//! deterministic pseudo-random *sequence* over the hash kinds, but it is
//! small-integer-only (keys `1..=4`) and exercises no navigation at all. This file
//! is the `ordered_set` counterpart: a mixed-type key pool, every operation the
//! crate exposes for the kind, and a step-by-step comparison against the live
//! system `erl` (OTP 29.1.1 / erts-17.1).
//!
//! # What is compared, and why it is byte-comparable
//!
//! The lone hard part of a mixed-type differential trace is that a float, an atom,
//! a binary, a tuple and a list each have a *textual* rendering the crate's
//! `beam_term` layer does not own. Rather than compare renderings, both sides
//! render every key the table yields as its **1-based position in a fixed key
//! pool** that is written out identically (index-for-index) on both sides
//! ([`key_pool`] in Rust, the `Keys` list in
//! [`ORDERED_TRACE_ORACLE_PROGRAM`]). An object renders `{<key index>,<value>}`,
//! a list of keys `[<index>,…]` and so on. That keeps every observable — including
//! the terms themselves, since the index is a *term identity* and not a coercion —
//! while sidestepping textual rendering divergence entirely.
//!
//! Two deliberate structural properties of the pool:
//!
//! * `1`/`1.0` **and** `2`/`2.0` are all present. They are distinct terms
//!   (`=:=`-unequal, so distinct pool entries) but *arithmetic-equal* keys, so an
//!   insert of one over the other collapses to a **single** object and the stored
//!   key **term** becomes the last-inserted one — the crate's
//!   "same-arith-key replace replaces the stored KEY term too" rule, which is
//!   therefore observed directly in `T=`.
//! * Every other pool term is `=:=`-distinct, so a rendered index is unambiguous.
//!
//! The pool mixes small integers (including `0` and negatives), floats, atoms,
//! binaries, tuples and lists — i.e. all the term *classes* the interior's
//! `cmp_arith` routing has to order — and is 73 entries wide, so the B+-tree
//! (`ORDER = 32`) really splits, and the delete churn really merges.
//!
//! # Order is a contract
//!
//! `T=` is the table's own `tab2list` order — never sorted — and `R=` is the op's
//! result in the order the operation produced it. An `ordered_set` traverses in
//! ascending arithmetic key order; the trace asserts that order holds against the
//! oracle after **every** step rather than once at the end.
//!
//! # The op driver
//!
//! Each step consumes exactly four draws on both sides, whatever the op:
//! `op`, the key index, the value, and an auxiliary key index (used by
//! `next`/`prev`). The op space is 24 `insert` slots followed by one slot for each
//! other operation, so inserts dominate enough for the table to exceed
//! `ORDER = 32` while every other op still fires.
//!
//! This test is `#[ignore]`d (run it with `--ignored`): it needs the system Erlang
//! as the oracle and a few seconds of wall clock. It is expected to **pass**; it
//! is not a soak test.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

use beam_common::BeamError;
use beam_ets::{compile, MatchSpec, OrderedInterior, Table, TableKind};
use beam_term::prelude::*;
use common::oracle;
use common::*;

/// The seeds the trace is driven with. Each is compared step by step.
const SEEDS: [u64; 3] = [1, 12_345, 20_250_601];

/// Steps per seed. Measured over these seeds the trace reaches 41–54 live keys
/// (comfortably past the B+-tree's `ORDER = 32`, so leaves split and internal
/// nodes are created) and then churns back down (so leaves merge again).
const STEPS: u64 = 600;

/// The op space: `0..=23` insert, then one slot per remaining operation.
const NUM_OPS: u64 = 40;

/// How many distinct `Value`s an inserted object may carry.
const VALUES: u64 = 6;

/// Wall-clock bound for the oracle program (it drives 1800 steps and writes a
/// few hundred kilobytes; seconds, not minutes).
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// The op driver's deterministic stream
// ---------------------------------------------------------------------------

/// The trace's LCG.
///
/// The state update is the flat recurrence the sibling randomised batteries use
/// (`x = (x*1103515245 + 12345) band 16#7FFFFFFF`); [`Lcg::draw`] projects the
/// state through the SplitMix64 finaliser ([`mix`]) before taking the residue.
///
/// The projection is not cosmetic. `a = 1103515245 ≡ 1 (mod 4)` and
/// `c = 12345 ≡ 1 (mod 4)`, so consecutive states satisfy
/// `x_{n+1} ≡ x_n + 1 (mod 4)`: the low two bits *cycle* deterministically and
/// every `x mod 4` residue is a function of the step number alone. Taking residues
/// of the raw state therefore collapses the op space — measured, only 7 of the 40
/// slots are ever reached and each step's op is one of exactly seven residues — so
/// the table never leaves a single leaf and the navigation/select family never
/// runs at all. The finaliser is a bijection on 64 bits, so the *state sequence*
/// (and hence the recurrence) is unchanged; only the projection into `0..bound` is
/// well mixed. The escript reproduces it bit-for-bit (`mix/1`).
struct Lcg(u64);

impl Lcg {
    /// Advance the state and return it.
    fn state(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345) & 0x7FFF_FFFF;
        self.0
    }

    /// The next draw in `0..bound`.
    fn draw(&mut self, bound: u64) -> u64 {
        mix(self.state()) % bound
    }
}

/// The SplitMix64 finaliser (`mix64`), reproduced bit-for-bit by the escript.
///
/// Rust's `wrapping_mul` is multiplication modulo `2^64`, exactly what the
/// escript's `(…) band 16#FFFFFFFFFFFFFFFF` computes.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ---------------------------------------------------------------------------
// The key pool (index-for-index identical to the escript's `Keys`)
// ---------------------------------------------------------------------------

/// The key pool, in the escript's order: 3 small integers, the integers `1..=44`,
/// 7 floats (two of which collapse arithmetically with an integer key), 8 atoms,
/// 5 binaries, 3 tuples and 3 lists — 73 `=:=`-distinct terms.
fn key_pool(heap: &mut Heap, atoms: &mut AtomTable) -> Vec<Eterm> {
    let mut pool = Vec::new();
    for key in [-7i64, -1, 0] {
        pool.push(make_small(key));
    }
    for key in 1..=44i64 {
        pool.push(make_small(key));
    }
    for value in [1.0f64, 2.0, -2.5, 3.5, 6.75, 0.5, -0.25] {
        pool.push(flonum(value));
    }
    for name in [
        "foo", "bar", "baz", "qux", "quux", "corge", "grault", "garply",
    ] {
        pool.push(interned_atom(atoms, name));
    }
    for bytes in [&[0u8][..], b"x", b"hello", &[0u8, 0][..], &[1u8, 2, 3][..]] {
        pool.push(binary(heap, bytes));
    }
    let a = interned_atom(atoms, "a");
    let b = interned_atom(atoms, "b");
    pool.push(tuple_of(heap, &[a, make_small(1)]));
    pool.push(tuple_of(heap, &[b, make_small(2)]));
    pool.push(tuple_of(heap, &[]));
    pool.push(proper_list(heap, &[make_small(1), make_small(2)]));
    pool.push(proper_list(heap, &[make_small(3)]));
    pool.push(NIL);
    pool
}

/// A byte-aligned binary term on `heap` (the same helper `tests/select.rs` uses).
fn binary(heap: &mut Heap, bytes: &[u8]) -> Eterm {
    let words = heap_bits_size_words(bytes.len() * 8);
    // SAFETY: `alloc_words(words)` hands back exactly the `heap_bits_size_words(n_bits)`
    // words `write_binary` writes; the heap outlives the term (it is never reset
    // here) and `bytes` is a byte-aligned payload.
    unsafe {
        let hp = heap
            .alloc_words(words)
            .expect("test heap has room for a binary");
        write_binary(hp, bytes).expect("a byte-aligned binary")
    }
}

/// `[{{'_','_'},[],[true]}]` — the whole-object clause with a `true` body, the
/// spec `ets:select_count/2` is driven with.
fn all_true_spec(heap: &mut Heap, atoms: &mut AtomTable) -> MatchSpec {
    let underscore = interned_atom(atoms, "_");
    let head = tuple_of(heap, &[underscore, underscore]);
    let body = proper_list(heap, &[interned_atom(atoms, "true")]);
    let clause = tuple_of(heap, &[head, NIL, body]);
    let spec = proper_list(heap, &[clause]);
    compile(spec, atoms).expect("the count spec compiles")
}

/// The output heap the select family builds its results on.
///
/// A `['$1']` body builds a fresh one-cell list per matching object, and
/// `beam_alloc::Heap` never reallocates, so the arena is sized for a full 73-key
/// table across the ~90 select calls of the whole trace.
fn select_heap() -> beam_alloc::Heap {
    beam_alloc::Heap::new(1 << 18).expect("a select output heap is allocatable")
}

// ---------------------------------------------------------------------------
// Rendering — the one canonical form both sides must agree on
// ---------------------------------------------------------------------------

/// One trace run's read-only context plus the scratch heap the select family uses.
struct Trace<'a> {
    pool: &'a [Eterm],
    atoms: &'a AtomTable,
    key_spec: &'a MatchSpec,
    count_spec: &'a MatchSpec,
    /// The `{'_','_'}` pattern term `ets:match_object/2` is driven with.
    any_object: Eterm,
    out: beam_alloc::Heap,
}

impl Trace<'_> {
    /// A key term as its **1-based** pool index.
    ///
    /// Every term the interior can hold is one the trace inserted, so a key
    /// outside the pool is itself a divergence; rendering it as `?` keeps the step
    /// comparable line-for-line (the assertion prints the whole line) instead of
    /// losing the trace to a panic.
    fn key(&self, key: Eterm) -> String {
        match self
            .pool
            .iter()
            .position(|candidate| eq(key, *candidate, self.atoms))
        {
            Some(index) => (index + 1).to_string(),
            None => "?".to_string(),
        }
    }

    /// One stored `{Key, Value}` object as `{<key index>,<value>}`.
    fn object(&self, object: Eterm) -> String {
        // SAFETY: every object the trace inserts is a `{Key, Value}` two-word
        // tuple built by `step`; both reads address live elements of that tuple.
        let (key, value) =
            unsafe { (tuple_element(object, 0), signed_value(tuple_element(object, 1))) };
        format!("{{{},{}}}", self.key(key), value)
    }

    /// A list of objects as `[{…},…]`, in the order it is given.
    fn objects(&self, objects: &[Eterm]) -> String {
        self.list(objects.iter().map(|object| self.object(*object)))
    }

    /// A list of key terms as `[<index>,…]`, in the order it is given.
    fn keys(&self, keys: &[Eterm]) -> String {
        self.list(keys.iter().map(|key| self.key(*key)))
    }

    fn list(&self, items: impl Iterator<Item = String>) -> String {
        format!("[{}]", items.collect::<Vec<_>>().join(","))
    }

    /// `ets:first/1`, `ets:last/1`, `ets:next/2` and `ets:prev/2`: a key index, or
    /// the `'$end_of_table'` sentinel the crate models as `None`.
    fn nav(&self, key: Option<Eterm>) -> String {
        match key {
            Some(key) => self.key(key),
            None => "$end_of_table".to_string(),
        }
    }

    /// Drive one step and return the operation's rendered result.
    fn step(
        &mut self,
        table: &mut Table,
        op: u64,
        object: Eterm,
        key: Eterm,
        aux: Eterm,
    ) -> String {
        match op {
            0..=23 => match table.insert(object, self.atoms) {
                Ok(()) => "ok".to_string(),
                Err(error) => failure(op, error),
            },
            24 => match table.insert_new(object, self.atoms) {
                Ok(true) => "true".to_string(),
                Ok(false) => "false".to_string(),
                Err(error) => failure(op, error),
            },
            25 => match table.delete(key, self.atoms) {
                Ok(()) => "ok".to_string(),
                Err(error) => failure(op, error),
            },
            26 => match table.delete_object(object, self.atoms) {
                Ok(()) => "ok".to_string(),
                Err(error) => failure(op, error),
            },
            27 => match table.take(key, self.atoms) {
                Ok(taken) => self.objects(&taken),
                Err(error) => failure(op, error),
            },
            28 => {
                table.delete_all_objects();
                "ok".to_string()
            }
            29 => self.objects(&table.lookup(key, self.atoms)),
            30 => if table.member(key, self.atoms) {
                "true".to_string()
            } else {
                "false".to_string()
            },
            31 => self.nav(table.first(self.atoms)),
            32 => self.nav(table.last(self.atoms)),
            33 => match table.next(aux, self.atoms) {
                Ok(key) => self.nav(key),
                Err(error) => failure(op, error),
            },
            34 => match table.prev(aux, self.atoms) {
                Ok(key) => self.nav(key),
                Err(error) => failure(op, error),
            },
            35 => match table.select(self.key_spec, self.atoms, &mut self.out) {
                Ok(keys) => self.keys(&keys),
                Err(error) => failure(op, error),
            },
            36 => match table.select_count(self.count_spec, self.atoms, &mut self.out) {
                Ok(count) => count.to_string(),
                Err(error) => failure(op, error),
            },
            37 => match table.match_object(self.any_object, self.atoms, &mut self.out) {
                Ok(objects) => self.objects(&objects),
                Err(error) => failure(op, error),
            },
            38 => match table.select_reverse(self.key_spec, self.atoms, &mut self.out) {
                Ok(keys) => self.keys(&keys),
                Err(error) => failure(op, error),
            },
            39 => self.keys(&foldl_keys(table)),
            _ => unreachable!("`op` is drawn modulo {NUM_OPS}"),
        }
    }
}

/// Render an operation failure.
///
/// `badarg` is the one failure an `ordered_set` can legitimately report (and the
/// oracle renders the same word); any other error is a capability gap, reported
/// loudly rather than folded into a `badarg` line that could hide it.
fn failure(op: u64, error: BeamError) -> String {
    match error {
        BeamError::BadArg => "badarg".to_string(),
        other => panic!("ordered-set trace op {op} failed unexpectedly: {other:?}"),
    }
}

/// `ets:foldl(fun({K, _}, Acc) -> [K | Acc] end, [], T)` over the table's own
/// iteration order.
///
/// The accumulator is rendered **as the fold builds it**, not reversed: an
/// ascending traversal with a prepending step yields the keys in *descending*
/// order, i.e. the reverse of the `select` result and the same list as the
/// `select_reverse` result. Keeping the raw accumulator makes the op a distinct
/// observable that cross-checks the two traversal paths against each other as well
/// as against the oracle. The escript runs the identical fold.
fn foldl_keys(table: &Table) -> Vec<Eterm> {
    let mut acc = Vec::new();
    for object in table.tab2list() {
        // SAFETY: every object the trace inserts is a `{Key, Value}` two-word tuple.
        acc.insert(0, unsafe { tuple_element(object, 0) });
    }
    acc
}

// ---------------------------------------------------------------------------
// The trace
// ---------------------------------------------------------------------------

/// Drive the trace on the crate's `ordered_set` and collect
/// `"<seed>.<step>" -> "S=<size> T=<objects> R=<result>"`.
fn trace() -> BTreeMap<String, String> {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 18);
    let pool = key_pool(&mut heap, &mut atoms);
    let pool_len = pool.len() as u64;
    let key_spec = key_spec(&mut heap, &mut atoms);
    let count_spec = all_true_spec(&mut heap, &mut atoms);
    let underscore = interned_atom(&mut atoms, "_");
    let any_object = tuple_of(&mut heap, &[underscore, underscore]);

    let mut trace = Trace {
        pool: &pool,
        atoms: &atoms,
        key_spec: &key_spec,
        count_spec: &count_spec,
        any_object,
        out: select_heap(),
    };

    let mut facts = BTreeMap::new();
    for seed in SEEDS {
        // The default `ordered_set` interior is the K-ary B+-tree; assert it so a
        // future change of the default cannot silently retarget this trace.
        let mut table = new_table(TableKind::OrderedSet);
        assert_eq!(
            table.ordered_interior(),
            OrderedInterior::BPlusTree,
            "the randomised ordered-set trace must drive the default B+-tree interior"
        );

        let mut lcg = Lcg(seed);
        for step in 1..=STEPS {
            // Exactly four draws per step, whatever the op, so the crate and the
            // escript stay in lockstep.
            let op = lcg.draw(NUM_OPS);
            let key = pool[lcg.draw(pool_len) as usize];
            let value = lcg.draw(VALUES) as i64;
            let aux = pool[lcg.draw(pool_len) as usize];
            let object = tuple_of(&mut heap, &[key, make_small(value)]);

            let result = trace.step(&mut table, op, object, key, aux);
            facts.insert(
                format!("{seed}.{step}"),
                format!(
                    "S={} T={} R={}",
                    table.size(),
                    trace.objects(&table.tab2list()),
                    result
                ),
            );
        }
    }
    facts
}

// ---------------------------------------------------------------------------
// The live oracle
// ---------------------------------------------------------------------------

/// The identical trace, as an `escript`.
///
/// It writes its trace to the file named by `argv[1]` and prints only the step
/// count: the crate's `run_bounded` reads the child's pipes only *after* it exits,
/// so a trace this size written to `stdout` would fill the pipe buffer and
/// deadlock the child.
///
/// `?LB`/`?RB` are the character codes of `{`/`}`: an Erlang list integer is a
/// byte, and spelling them as `$`-literals would put a `$` immediately before a
/// brace.
pub const ORDERED_TRACE_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
-define(LB, 123).
-define(RB, 125).
main([OutPath]) ->
    Keys = [-7, -1, 0] ++ lists:seq(1, 44) ++
           [1.0, 2.0, -2.5, 3.5, 6.75, 0.5, -0.25] ++
           ['foo', 'bar', 'baz', 'qux', 'quux', 'corge', 'grault', 'garply'] ++
           [<<0>>, <<"x">>, <<"hello">>, <<0,0>>, <<1,2,3>>] ++
           [{a,1}, {b,2}, {}] ++
           [[1,2], [3], []],
    PLen = length(Keys),
    NumOps = 40,
    Steps = 600,
    Seeds = [1, 12345, 20250601],
    {ok, F} = file:open(OutPath, [write, raw, binary]),
    RObj = fun({K, V}) -> [?LB, idx(K, Keys, 1), $,, integer_to_list(V), ?RB] end,
    RObjs = fun(L) -> [$[, lists:join(",", [RObj(O) || O <- L]), $]] end,
    RKeys = fun(L) -> [$[, lists:join(",", [idx(K, Keys, 1) || K <- L]), $]] end,
    REnd = fun('$end_of_table') -> "$end_of_table"; (K) -> idx(K, Keys, 1) end,
    lists:foreach(
      fun(Seed) ->
        T = ets:new(ot, [ordered_set, public]),
        lists:foldl(
          fun(Step, X0) ->
            X1 = next_state(X0), Op = mix(X1) rem NumOps,
            X2 = next_state(X1), Ki = mix(X2) rem PLen,
            X3 = next_state(X2), Vi = mix(X3) rem 6,
            X4 = next_state(X3), Ai = mix(X4) rem PLen,
            K = lists:nth(Ki + 1, Keys),
            A = lists:nth(Ai + 1, Keys),
            R = try
                    case Op of
                      _ when Op < 24 -> ets:insert(T, {K, Vi}), "ok";
                      24 -> case ets:insert_new(T, {K, Vi}) of
                              true -> "true";
                              false -> "false"
                            end;
                      25 -> ets:delete(T, K), "ok";
                      26 -> ets:delete_object(T, {K, Vi}), "ok";
                      27 -> RObjs(ets:take(T, K));
                      28 -> ets:delete_all_objects(T), "ok";
                      29 -> RObjs(ets:lookup(T, K));
                      30 -> case ets:member(T, K) of
                              true -> "true";
                              false -> "false"
                            end;
                      31 -> REnd(ets:first(T));
                      32 -> REnd(ets:last(T));
                      33 -> REnd(ets:next(T, A));
                      34 -> REnd(ets:prev(T, A));
                      35 -> RKeys(ets:select(T, [{{'$1','_'},[],['$1']}]));
                      36 -> integer_to_list(ets:select_count(T, [{{'_','_'},[],[true]}]));
                      37 -> RObjs(ets:match_object(T, {'_','_'}));
                      38 -> RKeys(ets:select_reverse(T, [{{'$1','_'},[],['$1']}]));
                      39 -> RKeys(ets:foldl(fun({Kk, _}, Acc) -> [Kk|Acc] end, [], T))
                    end
                catch error:badarg -> "badarg"
                end,
            ok = file:write(F, [integer_to_list(Seed), $., integer_to_list(Step),
                                " -> S=", integer_to_list(ets:info(T, size)),
                                " T=", RObjs(ets:tab2list(T)),
                                " R=", R, $\n]),
            X4
          end, Seed, lists:seq(1, Steps)),
        ets:delete(T)
      end, Seeds),
    file:close(F),
    io:format("steps ~b~n", [Steps * length(Seeds)]),
    ok.

next_state(X) -> (X * 1103515245 + 12345) band 16#7FFFFFFF.

mix(Z0) ->
    M = 16#FFFFFFFFFFFFFFFF,
    Z1 = (Z0 + 16#9E3779B97F4A7C15) band M,
    Z2 = ((Z1 bxor (Z1 bsr 30)) * 16#BF58476D1CE4E5B9) band M,
    Z3 = ((Z2 bxor (Z2 bsr 27)) * 16#94D049BB133111EB) band M,
    Z3 bxor (Z3 bsr 31).

idx(K, [K | _], I) -> integer_to_list(I);
idx(K, [_ | T], I) -> idx(K, T, I + 1).
"#;

/// The crate's trace equals the live oracle's, step by step.
#[test]
#[ignore = "live-oracle differential trace; run with --ignored"]
fn randomised_ordered_set_trace_matches_the_live_oracle() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "ordered-set trace",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };

    // The trace is far larger than a pipe buffer, so the escript writes it to a
    // file and receives that file's absolute path as `argv[1]`.
    let scratch = oracle::Scratch::new("ordered_trace");
    let trace_path = scratch.path().join("trace.txt");
    let args = vec![trace_path.to_string_lossy().into_owned()];
    let output = oracle::run_escript(
        &escript,
        ORDERED_TRACE_ORACLE_PROGRAM,
        &args,
        "ordered_trace",
        ORACLE_TIMEOUT,
    );

    let expected_steps = SEEDS.len() * STEPS as usize;
    let announced: usize = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("steps ")?.trim().parse().ok())
        .expect("the oracle announces how many steps it wrote");
    assert_eq!(
        announced, expected_steps,
        "the oracle ran a different number of steps than the trace is defined with"
    );

    let text = fs::read_to_string(&trace_path).unwrap_or_else(|error| {
        panic!(
            "cannot read the oracle trace {}: {error}",
            trace_path.display()
        )
    });
    let oracle_facts = parse_lines(&text);
    assert_eq!(
        oracle_facts.len(),
        announced,
        "the oracle's trace file holds {} steps but it announced {announced}",
        oracle_facts.len()
    );

    let ours = trace();
    for (name, want) in &oracle_facts {
        let got = ours
            .get(name)
            .unwrap_or_else(|| panic!("the crate's trace has no step `{name}`"));
        assert_eq!(
            got, want,
            "step `{name}`: the crate and the live oracle disagree"
        );
    }
    assert_eq!(
        ours.len(),
        oracle_facts.len(),
        "the crate and the oracle ran different numbers of steps"
    );

    println!(
        "[ordered trace] compared {} steps across {} seeds x {} steps",
        ours.len(),
        SEEDS.len(),
        STEPS
    );
}
