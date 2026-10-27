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
//!   cached `hash` word (`erl_term_hashing.c:1082-1111`), which this crate
//!   computes from the definition block itself ([`compute_record_definition_hash`])
//!   because `record::write_record` can only store a caller-supplied placeholder —
//!   and
//! * an **internal reference** must hash from its **`id` word** — the M1
//!   substitution for the C's `internal_ref_numbers(t)[0]`
//!   (`erl_term_hashing.c:288-290` / `:1454-1458`), whose value is *not*
//!   oracle-comparable (ERTS's own reference numbering is per-VM), so only the
//!   structural properties are pinned: in-range, stable for one term, distinct per
//!   mint, and not the identity of the integer spelling of its id.
//!
//! The offline pins keep the record contract verifiable without `erl`; the live
//! cross-check (skipped loudly when `erlc`/`erl` are absent) compiles a real
//! native-record module with the system `erlc` and runs it.
//!
//! ## The oracle corpus is built from a COMPILED module
//!
//! Two facts drive the corpus design (both verified against OTP 29 / erts-17.1):
//!
//! 1. **A native record needs the `-record #name{...}` syntax.** The legacy
//!    `-record(name, {...})` form still compiles to a plain TUPLE
//!    `{name, V0, V1, …}`; only the `#` form is lowered to the OTP-29
//!    `create_native_record` / `RECORD_SUBTAG` opcodes. Hashing a legacy record is
//!    therefore just hashing a tuple — a different term with a different value.
//! 2. **A record's value walk follows the definition's SORTED-key order**, i.e. the
//!    runtime's **atom-index** order (`record_compare` = `erts_cmp_flatmap_keys`,
//!    which for atoms is `key_a - key_b`, `erl_utils.h:267-272`), *not* the
//!    declaration order the definition hash uses. So the record's value is only
//!    portable across VMs when the keys' atom indices agree — the C itself is
//!    **not** portable here. The corpus therefore uses keys that are defined in
//!    `erts/emulator/beam/atom.names` (`x`, `y`, `value`, `name`), which this
//!    crate's [`AtomTable::from_atom_names`] and the running system ERTS both
//!    assign from the same file in the same order. (Keys absent from `atom.names`
//!    — e.g. `a`/`b`/`c` — are inserted in *module-load* order by the oracle and in
//!    *insertion* order by a hand-built table, so their relative indices, and hence
//!    the record's hash, can differ; that is an oracle-configuration artifact, not
//!    a hashing gap.)

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use beam_term::error::TermError;
use beam_term::hashing::{compute_record_definition_hash, make_hash, make_hash2};
use beam_term::heap::Heap;
use beam_term::prelude::*;
use beam_term::record::{record_block_words, record_definition, record_values, write_record};
use beam_term::tag::REF_SUBTAG;

/// The ORACLE's atom ordering source, so `false`=0 / `true`=1 exactly as in the
/// running system ERTS (the record definition hash mixes the `is_exported` atom)
/// and — crucially — so every key used by the corpus carries the same atom index
/// the oracle assigns it.
const ATOM_NAMES: &str = include_str!("../../../../erts/emulator/beam/atom.names");

/// The record corpus: `(module, name, is_exported, fields-in-declaration-order)`.
/// `fields` are `(atom name, small value)`.
type Fields = &'static [(&'static str, i64)];

/// The atom table seeding the oracle was verified against for the given record:
/// the standard `atom.names` order, then the module/name. All corpus *keys* are
/// drawn from `atom.names`, so re-interning them here is a no-op and both tables
/// agree on their indices (see the module docs).
fn table_for(module: &str, name: &str, fields: Fields) -> AtomTable {
    let mut t = AtomTable::from_atom_names(ATOM_NAMES).expect("atom.names");
    assert_eq!(t.lookup("false"), Some(Atom::from_raw(0)));
    assert_eq!(t.lookup("true"), Some(Atom::from_raw(1)));
    for n in [module, name] {
        t.intern(n);
    }
    for (k, _) in fields {
        // Every corpus key is already in `atom.names`; re-interning a present name
        // is a no-op, so this is correct either way.
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

/// The two oracle-verified records the fixture module below also builds.
///
/// `(module, name, fields)`. Record `#s{x, value, name}` is deliberately declared
/// in an order that is NOT the atoms' index order (`name` = 455, `value` = 756,
/// `x` = 767 in `atom.names`), so the stored value order differs from the
/// declaration order and the walk order is actually exercised.
const R2: (&str, &str, Fields) = ("recprobe", "r", &[("x", 1), ("y", 2)]);
const R3: (&str, &str, Fields) = (
    "recprobe",
    "s",
    &[("x", 1), ("value", 2), ("name", 3)],
);

/// The offline pins: the exact oracle values for the two corpus records (verified
/// on OTP 29 / erts-17.1 against the compiled fixture in `oracle_agrees_...`).
///
/// `(make_hash2, make_hash, definition_hash)`.
const R2_PIN: (u32, u32, u32) = (1_808_676_500, 3_043_701_549, 213_562_295);
const R3_PIN: (u32, u32, u32) = (1_972_919_111, 746_198_781, 975_688_124);

/// Both corpus records hash to the exact oracle integers, and the definition hash
/// they mix is the real one (not the stored placeholder).
#[test]
fn native_record_hashes_pin_the_oracle_vectors() {
    for (module, name, fields, pin) in [(R2.0, R2.1, R2.2, R2_PIN), (R3.0, R3.1, R3.2, R3_PIN)] {
        let t = table_for(module, name, fields);
        let mut heap = Heap::new(512);
        let r = build_record(&mut heap, &t, module, name, false, fields);
        // SAFETY: `r` is a live native record.
        let def = unsafe { record_definition(r).expect("definition") };
        assert_eq!(
            make_hash2(r, &t).expect("make_hash2"),
            pin.0,
            "make_hash2({module}:{name})"
        );
        assert_eq!(
            make_hash(r, &t).expect("make_hash"),
            pin.1,
            "make_hash({module}:{name})"
        );
        assert_eq!(
            compute_record_definition_hash(def, &t).expect("def hash"),
            pin.2,
            "definition hash ({module}:{name})"
        );
    }

    // The 3-field record is declared (x, value, name); its stored values follow the
    // atoms' *index* order, so the rounded-out instance is (3, 2, 1) — the exact
    // sequence the hash walk consumes.
    let t = table_for(R3.0, R3.1, R3.2);
    let mut heap = Heap::new(512);
    let r = build_record(&mut heap, &t, R3.0, R3.1, false, R3.2);
    // SAFETY: `r` is a live native record with 3 fields.
    let values = unsafe { record_values(r).expect("values") };
    assert_eq!(values, &[make_small(3), make_small(2), make_small(1)]);
}

/// The port must IGNORE the definition-`hash` word `record::write_record` stores
/// (this tree can only store a placeholder there) and COMPUTE the value from the
/// definition block instead — the same value the C's cache holds, so a record whose
/// cached word is a placeholder hashes exactly like one whose cached word is the
/// computed value.
#[test]
fn native_record_hash_ignores_the_stored_definition_hash_word() {
    let (module, name, fields) = R2;
    let t = table_for(module, name, fields);
    let mut ha = Heap::new(512);
    let mut hb = Heap::new(512);
    let computed = build_record(&mut ha, &t, module, name, false, fields);
    // SAFETY: `hb` owns the block and outlives the term; `write_record` fills it all.
    let placeholder = unsafe {
        let hp = hb.alloc_words(record_block_words(fields.len())).unwrap();
        write_record(
            hp,
            t.lookup(module).unwrap(),
            t.lookup(name).unwrap(),
            false,
            &fields
                .iter()
                .map(|(k, v)| (make_atom(t.lookup(k).unwrap()), make_small(*v)))
                .collect::<Vec<_>>(),
            make_small(0),
        )
        .unwrap()
    };
    assert_eq!(make_hash2(computed, &t), make_hash2(placeholder, &t));
    assert_eq!(make_hash(computed, &t), make_hash(placeholder, &t));
}

/// A record copy (a different address) hashes equal; a record differing in one
/// compared field hashes differently.
#[test]
fn native_record_hash_is_content_stable() {
    let (module, name, fields) = R2;
    let t = table_for(module, name, fields);
    let mut h1 = Heap::new(512);
    let mut h2 = Heap::new(512);
    let mut h3 = Heap::new(512);
    let r1 = build_record(&mut h1, &t, module, name, false, fields);
    let r2 = build_record(&mut h2, &t, module, name, false, fields);
    let r3 = build_record(&mut h3, &t, module, name, false, &[("x", 1), ("y", 9)]);
    assert_ne!(r1.raw(), r2.raw(), "the two copies must be distinct words");
    assert_eq!(make_hash2(r1, &t), make_hash2(r2, &t));
    assert_eq!(make_hash(r1, &t), make_hash(r2, &t));
    assert_ne!(make_hash2(r1, &t), make_hash2(r3, &t));
    assert_ne!(make_hash(r1, &t), make_hash(r3, &t));
}

/// An internal reference hashes from its **`id` word** — the M1 substitution for the
/// C's `internal_ref_numbers(t)[0]` (`erl_term_hashing.c:288-290` for `make_hash`,
/// `:1454-1458` for `make_hash2`).
///
/// Nothing numeric is pinned: ERTS's own reference numbering is per-VM
/// (`value = esdp->ref++`, `erl_bif_unique.h:169-178`), so even the C's exact formula
/// yields a different value in every VM. The contract asserted is structural:
///
/// * both ports answer `Ok` for a reference — alone and nested in a container the
///   walk descends into (the defect this replaces was a hard refusal);
/// * the value is **stable** for one term (a `dict`/`gb_trees` key must not wander);
/// * **distinct mints hash distinctly**, i.e. the operand really is the identity and
///   not the zeroed `node`/`serial` words (a literal C port would collapse every
///   local reference of a process onto one hash);
/// * the mix is **not the identity**: a reference does not hash like the integer
///   carrying its own id.
#[test]
fn internal_reference_hashes_by_its_id_word() {
    let mut t = AtomTable::new();
    t.seed(["a", "b"]);
    let mut heap = Heap::new(1024);
    // Eight mints from the ONE shared reference-id space (`beam_common::next_ref_id`)
    // — the same source `erlang:make_ref/0` draws from.
    let ids: [u64; 8] = core::array::from_fn(|_| beam_common::next_ref_id());
    let refs: Vec<Eterm> = ids
        .iter()
        .map(|&id| {
            // SAFETY: `heap` owns `INTERNAL_REF_WORDS` words per call, never
            // reallocates, and outlives every term below.
            let hp = unsafe { heap.alloc_words(INTERNAL_REF_WORDS) }.unwrap();
            unsafe { write_internal_ref(hp, id) }.unwrap()
        })
        .collect();
    // (1) `Ok` for both ports, and (2) stable for one term.
    let h2: Vec<u32> = refs
        .iter()
        .map(|&r| make_hash2(r, &t).expect("a reference has hashable identity"))
        .collect();
    let h1: Vec<u32> = refs
        .iter()
        .map(|&r| make_hash(r, &t).expect("a reference has hashable identity"))
        .collect();
    for (i, &r) in refs.iter().enumerate() {
        assert_eq!(make_hash2(r, &t), Ok(h2[i]));
        assert_eq!(make_hash(r, &t), Ok(h1[i]));
    }
    // (3) Distinct mints are distinct keys. The weak (and safe) form first: the hash
    // is not constant over the window — exactly the shape a literal C port produces.
    let distinct2: std::collections::BTreeSet<u32> = h2.iter().copied().collect();
    let distinct1: std::collections::BTreeSet<u32> = h1.iter().copied().collect();
    assert!(
        distinct2.len() > 1,
        "make_hash2 collapsed {} distinct references onto {distinct2:?}",
        refs.len()
    );
    assert!(
        distinct1.len() > 1,
        "make_hash collapsed {} distinct references onto {distinct1:?}",
        refs.len()
    );
    // ... and the concrete pair every `dict` insert relies on: the first two mints
    // (consecutive ids from the shared space) hash apart.
    assert_ne!(h2[0], h2[1], "two consecutive mints share a phash2 value");
    assert_ne!(h1[0], h1[1], "two consecutive mints share a phash value");
    // (4) The mix is not the identity: a reference is not its own id as an integer.
    // Id `0` is skipped: both ports start from `hash = 0` and the C's mixing steps
    // are pure `*prime + operand`, so the operand `0` maps to `0` in *every* arm —
    // probative for nothing. (`beam_common::next_ref_id`'s first draw is `0`.)
    for (i, &id) in ids.iter().enumerate() {
        if id == 0 {
            continue;
        }
        let as_int = make_small(id as i64);
        assert_ne!(
            make_hash2(refs[i], &t),
            make_hash2(as_int, &t),
            "phash2(ref {id}) equals phash2({id}) — the ref arm is the identity"
        );
        assert_ne!(
            make_hash(as_int, &t),
            Ok(h1[i]),
            "phash(ref {id}) equals phash({id}) — the ref arm is the identity"
        );
    }
    // A container the walk descends into reaches the same reference.
    let tup = heap.tuple(&[make_small(1), refs[0]]).unwrap();
    assert!(make_hash2(tup, &t).is_ok());
    assert!(make_hash(tup, &t).is_ok());
    assert_ne!(make_hash2(tup, &t), make_hash2(refs[0], &t));
    // The internal structural hash is unaffected and stays deterministic.
    assert_eq!(term_hash(refs[0], &t), term_hash(refs[0], &t));
}
/// A `REF_SUBTAG` thing of the **C layout** (`ErtsORefThing`: header arity 2, packed
/// `u32` ids) has no `id` word, so it is a *shape* failure — the typed
/// [`TermError::BadRef`], never a value mixed from a substitute — while a reference
/// the runtime actually builds is a hashable term.
///
/// The two layouts share the `REF_SUBTAG` header, so this pins that the ports go
/// through `boxed::internal_ref_id`'s arity check instead of reading word 3 blindly.
#[test]
fn c_layout_reference_is_a_shape_failure_not_a_wrong_value() {
    let mut t = AtomTable::new();
    t.seed(["a", "b"]);
    let mut heap = Heap::new(32);
    // SAFETY: `heap` owns the 3-word C-layout block and outlives the term.
    let c_layout = unsafe {
        let p = heap.alloc_words(3).unwrap();
        *p = make_header(2, REF_SUBTAG);
        *p.add(1) = Eterm::from_raw(0xffff_ffff_0000_0000);
        *p.add(2) = Eterm::from_raw(0x0000_0002_0000_0001);
        make_boxed(p)
    };
    assert_eq!(make_hash2(c_layout, &t), Err(TermError::BadRef));
    assert_eq!(make_hash(c_layout, &t), Err(TermError::BadRef));
    // Nested, the walk reaches it and propagates the same typed answer.
    let tup = heap.tuple(&[make_small(1), c_layout]).unwrap();
    assert_eq!(make_hash2(tup, &t), Err(TermError::BadRef));
    assert_eq!(make_hash(tup, &t), Err(TermError::BadRef));
}
/// An **external** reference (another node's) hashes too — its wire words are all
/// present, so both ports mix them exactly as the C does
/// (`UINT32_HASH_RET(external_ref_numbers(t)[0], …)`, `:290` / `:1460-1461`) instead
/// of refusing.
///
/// The mix is the stored `num[0]` word, so distinct id arrays hash distinctly and a
/// repeated call is stable; a **zero-length** reference (the C's `count == 0`) reads
/// the zeroed word after the count and mixes `0` rather than failing.
#[test]
fn external_reference_hashes_its_wire_words() {
    let mut t = AtomTable::new();
    t.seed(["a", "b", "other"]);
    let node = make_atom(t.lookup("other").expect("seeded"));
    let mut heap = Heap::new(256);
    let mk = |heap: &mut Heap, ids: &[u32]| {
        // SAFETY: `heap` owns `external_ref_size_words(ids.len())` words per call,
        // never reallocates, and outlives the term; `node` is a seeded atom immediate.
        let hp = unsafe { heap.alloc_words(external_ref_size_words(ids.len())) }.unwrap();
        unsafe { write_external_ref(hp, node, ids, 0x0BAD_CAFE) }
    };
    let a = mk(&mut heap, &[0x0003_FFFF, 0x1234_5678, 0xDEAD_BEEF]);
    let b = mk(&mut heap, &[0x0003_FFFE, 0x1234_5678, 0xDEAD_BEEF]);
    let empty = mk(&mut heap, &[]);
    assert!(make_hash2(a, &t).is_ok(), "an external ref must hash");
    assert!(make_hash(a, &t).is_ok(), "an external ref must hash");
    assert_eq!(make_hash2(a, &t), make_hash2(a, &t));
    assert_eq!(make_hash(a, &t), make_hash(a, &t));
    // Distinct wire ids ⇒ distinct hashes (the `num[0]` word is what both ports mix).
    assert_ne!(make_hash2(a, &t), make_hash2(b, &t));
    assert_ne!(make_hash(a, &t), make_hash(b, &t));
    // A zero-length reference mixes the zeroed word after the count (the C's `0`).
    assert!(make_hash2(empty, &t).is_ok());
    assert!(make_hash(empty, &t).is_ok());
    assert_ne!(make_hash2(empty, &t), make_hash2(a, &t));
}

// ---------------------------------------------------------------------------
// Live oracle cross-check (compiled native-record fixture).
// ---------------------------------------------------------------------------

/// The compiled-module fixture. NOTE the `-record #r{...}` syntax (see the module
/// docs): the legacy `-record(r, {...})` form would compile to a plain tuple.
///
/// `erlang:phash(T, 1 bsl 32)` is `make_hash(T) + 1` (`bif.c:5413-5452`), so the
/// fixture reports `phash - 1`.
const FIXTURE_SRC: &str = "\
-module(recprobe).
-export([r2/0, r3/0]).
-record #r{x, y}.
-record #s{x, value, name}.
r2() -> T = (#r{x=1, y=2}), {erlang:phash2(T, 1 bsl 32), erlang:phash(T, 1 bsl 32) - 1}.
r3() -> T = (#s{x=1, value=2, name=3}), {erlang:phash2(T, 1 bsl 32), erlang:phash(T, 1 bsl 32) - 1}.
";

/// `(r2_h2, r2_h, r3_h2, r3_h)` as reported by the compiled fixture.
struct OracleFixture {
    r2_h2: i64,
    r2_h: i64,
    r3_h2: i64,
    r3_h: i64,
}

/// Compiles [`FIXTURE_SRC`] with the system `erlc` and runs it with the system
/// `erl`, returning the four reported hashes — or `None` (with a loud
/// `eprintln!`) when `erlc`/`erl` are absent or the fixture cannot be built/run.
fn oracle_fixture() -> Option<OracleFixture> {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "beam_term_recprobe_fixture_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("skipping oracle check: could not create {}: {e}", dir.display());
        return None;
    }
    let src = dir.join("recprobe.erl");
    if let Err(e) = std::fs::write(&src, FIXTURE_SRC) {
        eprintln!("skipping oracle check: could not write {}: {e}", src.display());
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }

    let compiled = match Command::new("erlc")
        .args(["-o"])
        .arg(&dir)
        .arg(&src)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skipping oracle check: could not spawn `erlc`: {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
    };
    if !compiled.success() {
        eprintln!("skipping oracle check: `erlc` failed to build the fixture");
        let _ = std::fs::remove_dir_all(&dir);
        return None;
    }

    // `code:add_path/1` (not `erl -pa`) — `-pa` is unusable in this sandbox.
    let path = dir.to_str().expect("fixture path is UTF-8").to_string();
    let out = erl_stdout(&format!(
        "code:add_path(\"{path}\"), \
         io:format(\"~w|~w|~w|~w\", \
                   [element(1, recprobe:r2()), element(2, recprobe:r2()), \
                    element(1, recprobe:r3()), element(2, recprobe:r3())])"
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let out = out?;

    let mut fields = out.split('|');
    let mut next = |what: &str| -> Option<i64> {
        fields
            .next()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .or_else(|| {
                eprintln!("skipping oracle check: unparsable {what} in {out:?}");
                None
            })
    };
    Some(OracleFixture {
        r2_h2: next("r2 make_hash2")?,
        r2_h: next("r2 make_hash")?,
        r3_h2: next("r3 make_hash2")?,
        r3_h: next("r3 make_hash")?,
    })
}

/// The LIVE oracle cross-check: the same two native records, built here with
/// `record::write_record`, are built in the oracle by a **compiled module**
/// (`-record #r{...}`, the only form the compiler lowers to native records) and
/// both `make_hash2` (`phash2(T, 1 bsl 32)`) and `make_hash`
/// (`phash(T, 1 bsl 32) - 1`) must agree — with the offline pins too.
#[test]
fn oracle_agrees_on_native_record_hashes() {
    let Some(fx) = oracle_fixture() else {
        return;
    };
    let r2 = (fx.r2_h2, fx.r2_h, R2_PIN.0, R2_PIN.1);
    let r3 = (fx.r3_h2, fx.r3_h, R3_PIN.0, R3_PIN.1);
    assert_eq!(
        (r2.0, r2.1),
        (i64::from(r2.2), i64::from(r2.3)),
        "the offline pin for {}:{} went stale against the oracle",
        R2.0,
        R2.1
    );
    assert_eq!(
        (r3.0, r3.1),
        (i64::from(r3.2), i64::from(r3.3)),
        "the offline pin for {}:{} went stale against the oracle",
        R3.0,
        R3.1
    );

    for (module, name, fields, oracle) in [
        (R2.0, R2.1, R2.2, (fx.r2_h2, fx.r2_h)),
        (R3.0, R3.1, R3.2, (fx.r3_h2, fx.r3_h)),
    ] {
        let t = table_for(module, name, fields);
        let mut heap = Heap::new(512);
        let r = build_record(&mut heap, &t, module, name, false, fields);
        assert_eq!(
            i64::from(make_hash2(r, &t).expect("make_hash2")),
            oracle.0,
            "erlang:phash2(T, 1 bsl 32) must equal make_hash2(T) for {module}:{name}"
        );
        assert_eq!(
            i64::from(make_hash(r, &t).expect("make_hash")),
            oracle.1,
            "erlang:phash(T, 1 bsl 32) - 1 must equal make_hash(T) for {module}:{name}"
        );
    }
}

/// Runs `expr` under `erl -noshell -eval` and returns its stdout (trimmed), or
/// `None` — with a loud `eprintln!` — when `erl` is absent or does not answer in
/// time (the crate's usual "skip loudly" pattern). The trailing `halt(0)` is
/// appended here.
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
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                eprintln!("skipping oracle check: `erl` did not answer within 60s");
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
