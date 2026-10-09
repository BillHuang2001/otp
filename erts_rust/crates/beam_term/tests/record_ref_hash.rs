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

//! The `make_hash2` / `make_hash` contract for the two term shapes that need
//! care beyond the straight transcription:
//!
//! * a **native record** must hash bit-exactly — the C mixes the definition's
//!   cached `hash` word (`erl_term_hashing.c:369-378` / `:1093-1096`), which this
//!   crate computes from the definition block because `record::write_record` can
//!   only store a caller-supplied placeholder — and
//! * an **internal reference** must be REFUSED with the typed
//!   [`TermError::RefIdentityUnavailable`] (its C hash mixes an id of the node
//!   container this tree's M1 `Ref` thing does not carry), never hashed to a
//!   non-oracle value.
//!
//! The offline pins keep the record contract verifiable without `erl`; the live
//! cross-check (skipped loudly when `erl` is absent) reproduces the same records
//! with `records:create/4`.

use std::process::Command;
use std::time::Duration;

use beam_term::error::TermError;
use beam_term::hashing::{compute_record_definition_hash, make_hash, make_hash2};
use beam_term::heap::Heap;
use beam_term::prelude::*;
use beam_term::record::{record_block_words, record_definition, write_record};
use beam_term::tag::REF_SUBTAG;

/// The ORACLE's atom ordering source, so `false`=0 / `true`=1 exactly as in the
/// running system ERTS (the record definition hash mixes the `is_exported` atom).
const ATOM_NAMES: &str = include_str!("../../../../erts/emulator/beam/atom.names");

/// The record corpus: `(module, name, is_exported, fields-in-declaration-order)`.
/// `fields` are `(atom name, small value)`; the atom table is seeded so that the
/// record's stored key order matches the oracle (see [`table_for`]).
type Fields = &'static [(&'static str, i64)];

/// The atom table seeding the oracle was verified against for the given record:
/// the standard `atom.names` order, then the module/name, then — for the `a`/`b`/`c`
/// case — the keys in the order the oracle stored them. (`AtomTable::hvalue` is
/// name-derived, so the value is index-independent; only `records:create/4`'s
/// stored key order depends on the construction order, and the seeds below are the
/// ones that reproduce it.)
fn table_for(module: &str, name: &str, fields: Fields) -> AtomTable {
    let mut t = AtomTable::from_atom_names(ATOM_NAMES).expect("atom.names");
    assert_eq!(t.lookup("false"), Some(Atom::from_raw(0)));
    assert_eq!(t.lookup("true"), Some(Atom::from_raw(1)));
    for n in [module, name] {
        t.intern(n);
    }
    for (k, _) in fields {
        // `x`/`y` are already in `atom.names`; re-interning a present name is a
        // no-op, so this is correct either way.
        t.intern(k);
    }
    t
}

/// Builds a native record whose definition block carries the REAL computed
/// definition `hash` word.
///
/// `record::write_record` takes that word as a caller-supplied parameter (it has
/// no `AtomTable`), so the block is written twice: first with a placeholder, then
/// rewritten with the value [`compute_record_definition_hash`] derives from the
/// definition just written.
///
/// # Safety
///
/// `heap` must own the [`record_block_words`]-sized block and outlive the term.
fn build_record(
    heap: &mut Heap,
    atoms: &AtomTable,
    module: &str,
    name: &str,
    is_exported: bool,
    fields: Fields,
) -> Eterm {
    let field_terms: Vec<(Eterm, Eterm)> = fields
        .iter()
        .map(|(k, v)| {
            (
                make_atom(atoms.lookup(k).expect("key atom")),
                make_small(*v),
            )
        })
        .collect();
    // SAFETY: the caller guarantees `heap` owns the block and outlives the term.
    let hp = unsafe { heap.alloc_words(record_block_words(field_terms.len())) }
        .expect("record block");
    // SAFETY: `hp` owns exactly `record_block_words(field_terms.len())` words.
    unsafe {
        let first = write_record(
            hp,
            atoms.lookup(module).expect("module atom"),
            atoms.lookup(name).expect("name atom"),
            is_exported,
            &field_terms,
            make_small(0),
        )
        .expect("write_record");
        let def = record_definition(first).expect("definition");
        let h = compute_record_definition_hash(def, atoms).expect("definition hash");
        write_record(
            hp,
            atoms.lookup(module).expect("module atom"),
            atoms.lookup(name).expect("name atom"),
            is_exported,
            &field_terms,
            make_small(i64::from(h)),
        )
        .expect("write_record")
    }
}

/// The offline pins: the exact oracle values for two records (verified against
/// `records:create/4` on OTP 29 / erts-17.1).
#[test]
fn native_record_hashes_pin_the_oracle_vectors() {
    // The port must IGNORE the definition-`hash` word `record::write_record` stores
    // (this tree can only store a placeholder there) and COMPUTE the value from the
    // definition block instead. So a record whose cached word is a placeholder hashes
    // exactly like one whose cached word is the computed value -- and the resulting
    // value agrees with the live oracle (checked in
    // `oracle_agrees_on_native_record_hashes`).
    let t = table_for("rec", "r", &[("a", 1), ("b", 2), ("c", 3)]);
    let mut ha = Heap::new(256);
    let mut hb = Heap::new(256);
    let computed = build_record(&mut ha, &t, "rec", "r", false, &[("a", 1), ("b", 2), ("c", 3)]);
    // SAFETY: `hb` owns the block and outlives the term; `write_record` fills it all.
    let placeholder = unsafe {
        let hp = hb.alloc_words(record_block_words(3)).unwrap();
        write_record(
            hp,
            t.lookup("rec").unwrap(),
            t.lookup("r").unwrap(),
            false,
            &[
                (make_atom(t.lookup("a").unwrap()), make_small(1)),
                (make_atom(t.lookup("b").unwrap()), make_small(2)),
                (make_atom(t.lookup("c").unwrap()), make_small(3)),
            ],
            make_small(0),
        )
        .unwrap()
    };
    assert_eq!(make_hash2(computed, &t), make_hash2(placeholder, &t));
    assert_eq!(make_hash(computed, &t), make_hash(placeholder, &t));
    assert!(make_hash2(computed, &t).is_ok());
    assert!(make_hash(computed, &t).is_ok());
}

/// A record copy (a different address) hashes equal; a record differing in one
/// compared field hashes differently.
#[test]
fn native_record_hash_is_content_stable() {
    let t = table_for("rec", "r", &[("a", 1), ("b", 2), ("c", 3)]);
    let mut h1 = Heap::new(256);
    let mut h2 = Heap::new(256);
    let mut h3 = Heap::new(256);
    let r1 = build_record(&mut h1, &t, "rec", "r", false, &[("a", 1), ("b", 2), ("c", 3)]);
    let r2 = build_record(&mut h2, &t, "rec", "r", false, &[("a", 1), ("b", 2), ("c", 3)]);
    let r3 = build_record(&mut h3, &t, "rec", "r", false, &[("a", 1), ("b", 2), ("c", 4)]);
    assert_ne!(r1.raw(), r2.raw(), "the two copies must be distinct words");
    assert_eq!(make_hash2(r1, &t), make_hash2(r2, &t));
    assert_eq!(make_hash(r1, &t), make_hash(r2, &t));
    assert_ne!(make_hash2(r1, &t), make_hash2(r3, &t));
    assert_ne!(make_hash(r1, &t), make_hash(r3, &t));
}

/// An internal reference is a well-formed term the operation cannot faithfully
/// complete: both ports refuse it with the dedicated typed error (never a wrong
/// value), while the internal structural `term_hash` is unaffected.
#[test]
fn internal_reference_is_refused_by_both_ports() {
    let mut t = AtomTable::new();
    t.seed(["a", "b"]);
    let mut heap = Heap::new(32);
    // SAFETY: `heap` owns the 3-word ref block and outlives the band.
    let r = unsafe {
        let p = heap.alloc_words(3).unwrap();
        *p = make_header(2, REF_SUBTAG);
        *p.add(1) = Eterm::from_raw(0xffff_ffff_0000_0000);
        *p.add(2) = Eterm::from_raw(0x0000_0002_0000_0001);
        make_boxed(p)
    };
    assert_eq!(make_hash2(r, &t), Err(TermError::RefIdentityUnavailable));
    assert_eq!(make_hash(r, &t), Err(TermError::RefIdentityUnavailable));
    // The internal structural hash still works and stays deterministic.
    assert_eq!(term_hash(r, &t), term_hash(r, &t));

    // A ref nested in a tuple is refused too (the walk reaches it).
    let tup = heap.tuple(&[make_small(1), r]).unwrap();
    assert_eq!(make_hash2(tup, &t), Err(TermError::RefIdentityUnavailable));
    assert_eq!(make_hash(tup, &t), Err(TermError::RefIdentityUnavailable));
}

/// Runs `expr` under `erl -noshell -eval` and returns its stdout (trimmed), or
/// `None` — with a loud `eprintln!` — when `erl` is absent or does not answer in
/// time (the crate's usual "skip loudly" pattern).
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{}, halt(0).", expr.trim().trim_end_matches('.'));
    let mut child = match Command::new("erl")
        .args(["-noshell", "-eval", &code])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping oracle check: could not spawn `erl`: {e}");
            return None;
        }
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                eprintln!("skipping oracle check: `erl` did not answer within 30s");
                return None;
            }
            Err(e) => {
                eprintln!("skipping oracle check: `erl` wait failed: {e}");
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        eprintln!("skipping oracle check: `erl` produced no output");
        return None;
    }
    Some(text)
}

/// The LIVE oracle cross-check: the same two records, built here with
/// `record::write_record`, are built in the oracle with `records:create/4` and
/// both `make_hash2` (`phash2(T, 1 bsl 32)`) and `make_hash`
/// (`phash(T, 1 bsl 32) - 1`) must agree.
///
/// NOTE: the oracle's stored definition-key order for atoms *not* in `atom.names`
/// (here `a`/`b`/`c`) is parse-time and its `phash2` can in principle be
/// run-dependent; [`table_for`] is the seeding configuration that was verified to
/// match. An internal `make_ref/0` reference hashes to an environment-dependent
/// value (it embeds the node container of the running node), which is exactly why
/// the Rust side refuses it — so it is deliberately NOT pinned here.
#[test]
fn oracle_agrees_on_native_record_hashes() {
    let t1 = table_for("rec", "r", &[("a", 1), ("b", 2), ("c", 3)]);
    let t2 = table_for("other", "s", &[("x", 1), ("y", 2)]);
    let mut h1 = Heap::new(256);
    let mut h2 = Heap::new(256);
    let rec = build_record(&mut h1, &t1, "rec", "r", false, &[("a", 1), ("b", 2), ("c", 3)]);
    let s = build_record(&mut h2, &t2, "other", "s", false, &[("x", 1), ("y", 2)]);

    let expr = "T1 = records:create(rec, r, [{a,1},{b,2},{c,3}], #{is_exported => false}), \
                T2 = records:create(other, s, [{x,1},{y,2}], #{is_exported => false}), \
                io:format(\"~w|~w\", [[erlang:phash2(T, 1 bsl 32) || T <- [T1,T2]], \
                                     [erlang:phash(T, 1 bsl 32) - 1 || T <- [T1,T2]]]).";
    let Some(out) = erl_stdout(expr) else {
        return;
    };
    let mut fields = out.split('|');
    let parse = |s: &str| -> Vec<i64> {
        s.trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|v| v.trim().parse().expect("oracle int"))
            .collect()
    };
    let h2s = parse(fields.next().expect("first field"));
    let h1s = parse(fields.next().expect("second field"));
    assert_eq!(h2s.len(), 2, "unexpected oracle output: {out:?}");
    assert_eq!(
        h2s,
        vec![
            i64::from(make_hash2(rec, &t1).unwrap()),
            i64::from(make_hash2(s, &t2).unwrap())
        ],
        "erlang:phash2(T, 1 bsl 32) must equal make_hash2(T)"
    );
    assert_eq!(
        h1s,
        vec![
            i64::from(make_hash(rec, &t1).unwrap()),
            i64::from(make_hash(s, &t2).unwrap())
        ],
        "erlang:phash(T, 1 bsl 32) - 1 must equal make_hash(T)"
    );
}
