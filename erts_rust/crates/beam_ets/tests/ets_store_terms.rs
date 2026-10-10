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

//! The ETS copy boundary's **newly-copyable boxed kinds**, cross-checked against
//! the live oracle.
//!
//! [`beam_ets::TermStore`] (the table-owned arena behind `ets:insert/2`) deep-copies
//! every boxed kind the oracle accepts in an ETS object. [`TermStore::copy`] is
//! unit-tested internally, but its *observable* fidelity — that a term survives the
//! round trip as a value the oracle would recognise — is the compatibility evidence
//! this file supplies. For each kind it:
//!
//! 1. builds the term with the crate's frozen-layout writers
//!    ([`beam_term::prelude`]) and inserts `{Key, Term}` into a `set` and a
//!    `duplicate_bag`;
//! 2. reads the object back with `lookup` and takes the *table-owned* value;
//! 3. **drops the source arena and any off-heap payload**, so a read-back that
//!    still works proves the copy is self-contained (nothing aliases the caller);
//! 4. hashes the read-back with the bit-exact [`make_hash2`] and compares it to the
//!    live oracle's `erlang:phash2(ReadBack, 1 bsl 32)` for the *same* term.
//!
//! `make_hash2(ReadBack) == phash2(ReadBack, 1 bsl 32)` is the oracle's own full
//! 32-bit hash, so an agreement is a byte-level statement about the copied term,
//! not just a shape test. Two kinds cannot go through the hash:
//!
//! * a **local** (`make_fun3`) fun — [`make_hash2`] returns the typed
//!   `TermError::FunIdentityUnavailable` (its inlined descriptor carries no module
//!   / `old_uniq`), so the local-fun case is cross-checked via the oracle's
//!   `fun_info(F, type)` / `arity` / `env` and the crate's fun-arity / env readers;
//! * a **pid / port / reference** prints its node as a hash, never a raw number, so
//!   those are cross-checked via `phash2` plus `node/1` (the node atom's name) —
//!   never a reconstructed `~w` rendering.
//!
//! The oracle is **required** by default (a genuinely absent Erlang fails the test);
//! the typed `beam_common::Result`/`TermResult` values are handled, never `unwrap()`ed
//! on term data.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use beam_ets::{Table, TableKind};
use beam_term::prelude::*;

use common::oracle;

// ---------------------------------------------------------------------------
// The oracle battery
// ---------------------------------------------------------------------------

/// Wall-clock bound for the oracle program.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Every probed fact, printed as `key -> value`.
///
/// For each newly-copyable kind the program inserts the term into an ETS table,
/// reads it back and prints `phash2(ReadBack, 1 bsl 32)` — the oracle's full 32-bit
/// `phash2` — once per table kind (`set`, `duplicate_bag`), so the Rust side can
/// compare the store's copy against the oracle's own term for the *same* table kind.
/// The local-fun and node facts are printed separately: a local fun is an identity
/// gap for [`make_hash2`], and a pid/port/reference's node must be read as an atom
/// name (`node/1`), never reconstructed.
///
/// The `pid` / `port` / `reference` are built with `binary_to_term` of a known ETF
/// blob, so the Rust side can build the byte-identical term with the frozen-layout
/// writers:
///
/// * pid `<<131,88,119,1,$a, 1:32, 2:32, 3:32>>`  — node `'a'`, number 1, serial 2, creation 3;
/// * port `<<131,89,119,1,$a, 48:32, 3:32>>`        — node `'a'`, id 48, creation 3;
/// * ref `<<131,90,3:16,119,1,$a, 3:32, 16#11:32, 16#22:32, 16#33:32>>`
///   — node `'a'`, ids `[0x11,0x22,0x33]`, creation 3.
///
/// The local fun's single free variable is a *runtime* value (`phash2(<<"seed">>)`),
/// not a literal, so the compiler cannot fold it away and `num_free` is genuinely 1.
const ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~w~n", [K, V]) end,
    Z = erlang:phash2(<<"seed">>) + 7,
    PID = binary_to_term(<<131,88,119,1,97,1:32,2:32,3:32>>),
    PORT = binary_to_term(<<131,89,119,1,97,48:32,3:32>>),
    REF = binary_to_term(<<131,90,3:16,119,1,97,3:32,16#11:32,16#22:32,16#33:32>>),
    Terms = [
        {"map", maps:from_list([{I, I+100} || I <- lists:seq(0,39)])},
        {"extfun", fun lists:reverse/2},
        {"pid", PID},
        {"port", PORT},
        {"ref", REF},
        {"bits", <<16#146B:13>>},
        {"binary", list_to_binary(lists:seq(0,199))}
    ],
    lists:foreach(
      fun(Kind) ->
        T = ets:new(t, [Kind, public]),
        F = fun(S) -> lists:flatten(io_lib:format("~p.~s", [Kind, S])) end,
        lists:foldl(
          fun({Name, Term}, Key) ->
            true = ets:insert(T, {Key, Term}),
            [{_, Read}] = ets:lookup(T, Key),
            P(F(Name), erlang:phash2(Read, 1 bsl 32)),
            Key + 1
          end, 1, Terms),
        ets:delete(T)
      end, [set, duplicate_bag]),
    LF = fun(X, Y) -> X + Y + Z end,
    P("localfun.type", element(2, erlang:fun_info(LF, type))),
    P("localfun.arity", element(2, erlang:fun_info(LF, arity))),
    P("localfun.envcount", length(element(2, erlang:fun_info(LF, env)))),
    P("extfun.type", element(2, erlang:fun_info(fun lists:reverse/2, type))),
    P("pid.node", node(PID)),
    P("port.node", node(PORT)),
    P("ref.node", node(REF)),
    ok.
"#;

/// Run the oracle battery once and return its `key -> value` facts.
///
/// Returns `None` (after a **loud** skip) only under the explicit oracle opt-out
/// (`BEAM_ETS_ALLOW_NO_ORACLE=1`); a genuinely absent Erlang otherwise **fails** the
/// calling test, since the oracle is required by default.
fn oracle_facts(test: &str) -> Option<BTreeMap<String, String>> {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            test,
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return None;
    };
    let output = oracle::run_escript(&escript, ORACLE_PROGRAM, &[], test, ORACLE_TIMEOUT);
    let text = String::from_utf8_lossy(&output.stdout);
    let facts = common::parse_lines(&text);
    // The battery prints one line per fact; a short read means the program died half
    // way through and every missing key would otherwise be one confusing assertion.
    assert!(
        facts.len() >= 21,
        "the oracle program printed only {} facts (expected the whole battery):\n{text}",
        facts.len()
    );
    Some(facts)
}

/// One fact of the battery (panics — loudly — on a missing key).
fn fact<'a>(facts: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    facts
        .get(key)
        .unwrap_or_else(|| panic!("the oracle printed no `{key} -> ...` line"))
        .as_str()
}

// ---------------------------------------------------------------------------
// The Rust-side terms (frozen-layout writers)
// ---------------------------------------------------------------------------

/// A word-aligned backing payload of `bytes` (zero-padded to whole `Eterm` words)
/// — the aligned storage an off-heap bitstring's base pointer needs
/// (`write_refc_binary`'s alignment contract; a `Box<[u8]>` would not be
/// `Eterm`-aligned).
fn word_aligned_payload(bytes: &[u8]) -> Box<[Eterm]> {
    let word_bytes = core::mem::size_of::<Eterm>();
    let mut payload: Box<[Eterm]> =
        vec![Eterm::from_raw(0); bytes.len().div_ceil(word_bytes).max(1)].into_boxed_slice();
    // SAFETY: `payload` owns at least `bytes.len()` writable bytes (rounded up to
    // whole words, at least one), so the copy stays inside the box.
    unsafe {
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            payload.as_mut_ptr().cast::<u8>(),
            bytes.len(),
        );
    }
    payload
}

/// The `i`-th oracle term of [`ORACLE_PROGRAM`], built with the crate's writers.
///
/// Returns the source term plus the key it is stored under. `payloads` receives any
/// off-heap backing box so the caller can keep it alive (and then drop it, to prove
/// the store's copy is self-contained).
fn build_sources(
    heap: &mut Heap,
    atoms: &AtomTable,
    node: Atom,
    lists: Atom,
    reverse: Atom,
    payloads: &mut Vec<Box<[Eterm]>>,
) -> Vec<(&'static str, i64, Eterm)> {
    let mut out: Vec<(&'static str, i64, Eterm)> = Vec::new();

    // (1) a hash map (>32 pairs) — the HAMT interior.
    let pairs: Vec<(Eterm, Eterm)> = (0..40i64)
        .map(|i| (make_small(i), make_small(i + 100)))
        .collect();
    // SAFETY: `alloc_words(map_words_capacity(40))` reserves the upper bound
    // `write_map` documents for 40 input pairs, and the heap outlives the term.
    let map = unsafe {
        let hp = heap
            .alloc_words(map_words_capacity(pairs.len()))
            .expect("heap room for the hash map");
        write_map(hp, &pairs, atoms).expect("a hash map is writable")
    };
    out.push(("map", 1, map));

    // (2) an external (`EXPORT_EXT`) fun — `fun lists:reverse/2`.
    // SAFETY: `alloc_words(fun_words(External, 0))` reserves exactly the 4-word
    // block `write_fun_external` fills.
    let extfun = unsafe {
        let hp = heap
            .alloc_words(fun_words(FunKind::External, 0))
            .expect("heap room for the external fun");
        write_fun_external(hp, 2, lists, reverse).expect("an external fun is writable")
    };
    out.push(("extfun", 2, extfun));

    // (3) an external pid — node `'a'`, number 1, serial 2, creation 3.
    // SAFETY: `alloc_words(external_pid_size_words())` reserves the 4-word block
    // `write_external_pid` fills.
    let pid = unsafe {
        let hp = heap
            .alloc_words(external_pid_size_words())
            .expect("heap room for the external pid");
        write_external_pid(hp, make_atom(node), 1, 2, 3)
    };
    out.push(("pid", 3, pid));

    // (4) an external port — node `'a'`, id 48, creation 3.
    // SAFETY: `alloc_words(external_port_size_words())` reserves the 4-word block
    // `write_external_port` fills.
    let port = unsafe {
        let hp = heap
            .alloc_words(external_port_size_words())
            .expect("heap room for the external port");
        write_external_port(hp, make_atom(node), 48, 3)
    };
    out.push(("port", 4, port));

    // (5) an external reference — node `'a'`, ids [0x11,0x22,0x33], creation 3.
    let ids = [0x11u32, 0x22, 0x33];
    // SAFETY: `alloc_words(external_ref_size_words(3))` reserves the block
    // `write_external_ref` fills.
    let ext_ref = unsafe {
        let hp = heap
            .alloc_words(external_ref_size_words(ids.len()))
            .expect("heap room for the external reference");
        write_external_ref(hp, make_atom(node), &ids, 3)
    };
    out.push(("ref", 5, ext_ref));

    // (6) a non-byte-aligned on-heap bitstring — the 13 bits `<<16#146B:13>>`.
    // The writer clears the unused low bits of the last byte, so `0x5C`'s top five
    // bits (`0b01011`) complete `0xA3`'s eight (`0b1010_0011`).
    // SAFETY: `alloc_words(heap_bits_size_words(13))` reserves exactly the block
    // `write_heap_bits` fills.
    let bits = unsafe {
        let hp = heap
            .alloc_words(heap_bits_size_words(13))
            .expect("heap room for the bitstring");
        write_heap_bits(hp, &[0xA3, 0x5C], 13).expect("a bitstring is writable")
    };
    out.push(("bits", 6, bits));

    // (7) a large binary — an off-heap refc binary re-materialised on-heap by the
    // store. The payload box is handed back so the caller keeps it alive until the
    // copy is taken.
    let bytes: Vec<u8> = (0..200u16).map(|i| i as u8).collect();
    let payload = word_aligned_payload(&bytes);
    let base = payload.as_ptr().cast::<u8>();
    // SAFETY: the heap has room for the 8-word pair; `base` names the `Eterm`-aligned
    // payload's first byte and covers `bytes.len() * 8` bits, and the payload box
    // outlives the term (it is pushed into `payloads`).
    let refc = unsafe {
        let hp = heap
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("heap room for the refc binary");
        write_refc_binary(
            hp,
            payload.as_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            base,
            0,
            bytes.len() * 8,
        )
    };
    // Keep the payload alive across the insert: the writer stored a raw pointer into
    // it, and the store gathers the bytes while copying.
    payloads.push(payload);
    out.push(("binary", 7, refc));

    // (8) a local (`make_fun3`) fun — arity 2, one boxed free variable. Its identity
    // is a `make_hash2` gap, so it is cross-checked by kind/arity/env, not by hash.
    // SAFETY: `alloc_words(fun_words(Local, 1))` reserves exactly the block
    // `write_fun_local` fills.
    let localfun = unsafe {
        let env = [make_small(7)];
        let hp = heap
            .alloc_words(fun_words(FunKind::Local, env.len()))
            .expect("heap room for the local fun");
        write_fun_local(hp, 2, 0xDEAD_BEEF_0000_0001, 3, &env).expect("a local fun is writable")
    };
    out.push(("localfun", 8, localfun));

    out
}

/// The read-back value stored under `name` (panics on a missing name).
fn read_named(reads: &[(&'static str, Eterm)], name: &str) -> Eterm {
    reads
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no read-back named `{name}`"))
        .1
}

/// The value element of the single stored object under `key` in `table`.
///
/// A fresh table holds exactly one object per key for the kinds this file uses, so a
/// length other than one is a genuine failure, not a case to paper over.
fn sole_value(table: &Table, key: i64, atoms: &AtomTable) -> Eterm {
    let objects = table.lookup(make_small(key), atoms);
    assert_eq!(objects.len(), 1, "key {key} holds exactly one object");
    // SAFETY: the only objects inserted are two-word `{Key, Value}` tuples, so
    // element 1 of any returned object is a live, initialised term word.
    unsafe { tuple_element(objects[0], 1) }
}

// ---------------------------------------------------------------------------
// The cross-check
// ---------------------------------------------------------------------------

/// Every newly-copyable boxed kind survives `ets:insert/2`'s deep copy as the exact
/// term the oracle independently builds.
#[test]
fn every_copyable_kind_round_trips_with_the_oracle_hash() {
    let Some(facts) = oracle_facts("every_copyable_kind_round_trips_with_the_oracle_hash") else {
        return;
    };

    let mut atoms = AtomTable::new();
    // Intern the node/fun atoms up front so the source terms can be built from a
    // shared, immutable `&AtomTable`.
    let node = atoms.intern("a");
    let lists = atoms.intern("lists");
    let reverse = atoms.intern("reverse");

    let mut heap = Heap::new(2048);
    let mut payloads: Vec<Box<[Eterm]>> = Vec::new();
    let sources = build_sources(&mut heap, &atoms, node, lists, reverse, &mut payloads);

    // Insert each source into a `set` and a `duplicate_bag`, read the object back.
    let mut set_table = common::new_table(TableKind::Set);
    let mut bag_table = common::new_table(TableKind::DuplicateBag);
    for (name, key, term) in &sources {
        let object = heap
            .tuple(&[make_small(*key), *term])
            .expect("heap room for the object tuple");
        set_table
            .insert(object, &atoms)
            .unwrap_or_else(|error| panic!("{name}: a `set` insert must succeed: {error:?}"));
        bag_table
            .insert(object, &atoms)
            .unwrap_or_else(|error| panic!("{name}: a `duplicate_bag` insert must succeed: {error:?}"));
    }

    let set_reads: Vec<(&'static str, Eterm)> = sources
        .iter()
        .map(|(name, key, _)| (*name, sole_value(&set_table, *key, &atoms)))
        .collect();
    let bag_reads: Vec<(&'static str, Eterm)> = sources
        .iter()
        .map(|(name, key, _)| (*name, sole_value(&bag_table, *key, &atoms)))
        .collect();

    // Drop the source arena and the off-heap payload: every read-back below is now
    // provably the *store's* self-contained copy, not a pointer into the source.
    drop(heap);
    drop(payloads);

    // --- the hashable kinds: hash equal to the oracle's own full 32-bit phash2 ---
    for (table_kind, reads) in [("set", &set_reads), ("duplicate_bag", &bag_reads)] {
        for (name, read) in reads {
            if *name == "localfun" {
                // `make_hash2` cannot hash a `make_fun3` fun (identity gap) — see the
                // kind-specific checks below.
                continue;
            }
            let expected: u32 = fact(&facts, &format!("{table_kind}.{name}"))
                .parse()
                .unwrap_or_else(|error| panic!("{table_kind}.{name}: the oracle printed a non-u32 hash: {error}"));
            let actual = make_hash2(*read, &atoms).unwrap_or_else(|error| {
                panic!("{table_kind}.{name}: the store's copy must be hashable: {error:?}")
            });
            assert_eq!(
                actual, expected,
                "{table_kind}.{name}: the store's deep copy must hash like the oracle's term"
            );
        }
    }

    // --- the external fun: kind is `external` (the hash above already agrees) ---
    let extfun = read_named(&set_reads, "extfun");
    assert_eq!(fact(&facts, "extfun.type"), "external");
    // SAFETY: `extfun` is a live store-owned external fun.
    let extfun_hdr = unsafe { *boxed_val(extfun) };
    assert_eq!(
        fun_kind_of_header(extfun_hdr),
        FunKind::External,
        "the copy is an external fun"
    );

    // --- the local fun: `make_hash2` gap, so cross-check kind / arity / env ---
    let localfun = read_named(&set_reads, "localfun");
    // SAFETY: `localfun` is a live store-owned local fun.
    let local_hdr = unsafe { *boxed_val(localfun) };
    assert_eq!(fact(&facts, "localfun.type"), "local");
    assert_eq!(
        fun_kind_of_header(local_hdr),
        FunKind::Local,
        "the copy is a local fun"
    );
    assert_eq!(
        fun_arity(local_hdr).to_string(),
        fact(&facts, "localfun.arity"),
        "the arity survives the copy"
    );
    assert_eq!(
        fun_num_free(local_hdr).to_string(),
        fact(&facts, "localfun.envcount"),
        "the free-variable count matches the oracle's `fun_info(F, env)` length"
    );
    // SAFETY: `localfun` is a live local fun, so its environment is a live slice.
    let env = unsafe { fun_env(localfun) }.expect("a live local fun carries its environment");
    assert_eq!(env.len(), 1, "one free variable");
    assert_eq!(
        signed_value(env[0]),
        7,
        "the deep-copied environment still holds its value"
    );

    // --- the external pid / port / reference: phash (above) plus `node/1` ---
    // The node is an atom, so its *name* is the oracle's `node/1` rendering; a raw
    // number is never reconstructed (the oracle prints node as a hash).
    let (pid_node, _, _, _) = unsafe { external_pid_parts(read_named(&set_reads, "pid")) };
    assert_eq!(
        atoms.name(atom_val(pid_node)),
        Some(fact(&facts, "pid.node")),
        "the pid's node atom survives the copy"
    );
    let (port_node, _, _) = unsafe { external_port_parts(read_named(&set_reads, "port")) };
    assert_eq!(
        atoms.name(atom_val(port_node)),
        Some(fact(&facts, "port.node")),
        "the port's node atom survives the copy"
    );
    let (ref_node, _, _) = unsafe { external_ref_parts(read_named(&set_reads, "ref")) };
    assert_eq!(
        atoms.name(atom_val(ref_node)),
        Some(fact(&facts, "ref.node")),
        "the reference's node atom survives the copy"
    );
}
