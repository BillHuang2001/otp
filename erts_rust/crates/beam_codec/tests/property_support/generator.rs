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

//! The deterministic term generator and the byte-identity `round_trip` helper.
//!
//! A tiny in-file splitmix64 PRNG (no external crate) drives a bounded, recursive
//! term generator that reaches every tag this codec can encode. For every generated
//! term the invariant asserted is:
//!
//! ```text
//!   encode(term)  ->  decode(fresh Heap + AtomTable)  ->  encode  ==  encode(term)
//! ```
//!
//! Byte-identical, **for every `{minor_version, N}` flavour** (`V0`/`V1`/`V2`), so the
//! frozen-point property is checked on the legacy `FLOAT_EXT`/`ATOM_EXT` wire forms too.
//!
//! ## What is generated
//!
//! Small ints at every wire boundary (`0`, `255`/`256`, `i32` edges, the 60-bit small
//! edges), bignums at every `ErtsDigit` count (the top digit's high bit is always set,
//! so a generated bignum never collapses into a small int), floats (`±0.0`, subnormals,
//! huge/tiny magnitudes, random **finite** bit patterns), UTF-8 and Latin-1 atoms
//! (incl. multibyte and the 255-character ceiling), binaries and bitstrings of 0..n bits
//! (non-byte-aligned, and above the 64-byte off-heap threshold), byte lists, proper and
//! improper lists, tuples (incl. arity 0), flat maps (≤ 32 pairs) and HAMT maps
//! (> 32 pairs), external pids/ports/refs, internal pid/port immediates (encoded with a
//! supplied local-node identity), `EXPORT_EXT` funs and `NEW_FUN_EXT` funs with a
//! non-empty environment — plus arbitrary nested mixtures.
//!
//! ### Two deliberate exclusions (documented, not weakened assertions)
//!
//! * **Atom keys in maps.** A flat map's keys are sorted by **atom-table index**
//!   (`erts_cmp_flatmap_keys`), and the index depends on the interning order of *this*
//!   VM start, so an atom-keyed flat map is *not* a byte fixed point across a fresh
//!   decode table — in ERTS either (`tests/map_key_order.rs` pins exactly this with two
//!   fresh `erl` processes). Maps are therefore keyed by integers/binaries, whose order
//!   is representation-independent; atoms still appear as map **values**, tuple/list
//!   elements and top-level terms. (`HAMT` maps are hash-keyed and *are* stable, but the
//!   generator does not need to rely on that.)
//! * **Zero-id external references.** The C decoder reads a reference's first number
//!   *unconditionally* (`external.c:4743-4796`, `max(1, len)` id words) while its encoder
//!   writes only `len` — for `len == 0` the two disagree, so ERTS itself cannot feed its
//!   own zero-id re-encoding back in. `tests/ref_ext_zero_and_arity.rs` pins that
//!   asymmetry directly; the corpus only generates refs with `1..=5` ids.

use super::render::render;
use super::{CREATION, HEAP_WORDS, MAX_DEPTH, SYSNAME};
use beam_alloc::Heap;
use beam_codec::term::{
    cons_head, cons_tail, make_big, make_bitstring, make_external_pid, make_external_port,
    make_external_ref, make_float, make_fun_external, make_fun_imported, make_map, tuple_arity,
    tuple_element, FunPid,
};
use beam_codec::{decode_term, encode_term, Encoder, MinorVersion};
use beam_term::prelude::{
    is_internal_pid, is_internal_port, is_list, is_nil, make_atom, make_internal_pid,
    make_internal_port, make_pid_data, make_port_data, make_small, Atom, AtomTable, Eterm, NIL,
};

// ---------------------------------------------------------------------------
// Deterministic PRNG (splitmix64) — no external crate
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

// ---------------------------------------------------------------------------
// Term generation
// ---------------------------------------------------------------------------

/// The atom-name pool (interned once into the shared table). Multibyte UTF-8,
/// Latin-1-representable names, and a name at the 255-character ceiling.
fn atom_pool() -> Vec<String> {
    vec![
        String::new(),
        "ok".into(),
        "error".into(),
        "hello".into(),
        "world".into(),
        "undefined".into(),
        "lists".into(),
        "reverse".into(),
        "maps".into(),
        "remove".into(),
        "erlang".into(),
        "a".into(),
        "_".into(),
        "héllo_λ_日本語".into(),
        "é".into(),
        "ñ".into(),
        "λ".into(),
        "a".repeat(255),
        "λ".repeat(130),
        "日本".repeat(20),
    ]
}

/// A container width that shrinks with depth, so the recursion terminates cheaply.
fn width(rng: &mut Rng, depth: u32) -> usize {
    1 + rng.below(depth.clamp(2, 4) as u64) as usize
}

/// Generates one term, pushing the *kind* it produced onto `kinds` (coverage proof).
fn gen_term(
    h: &mut Heap,
    atoms: &AtomTable,
    pool: &[Atom],
    node: Atom,
    rng: &mut Rng,
    depth: u32,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    let top = depth == MAX_DEPTH;
    let choices = if depth == 0 { 9 } else { 23 };
    let kind = match rng.below(choices) {
        0 => {
            return Ok(leaf(
                "small_boundary",
                make_small(boundary_small(rng)),
                kinds,
            ))
        }
        1 => return Ok(leaf("small_random", make_small(random_small(rng)), kinds)),
        2 => return gen_big(h, rng, kinds),
        3 => return gen_float(h, rng, kinds),
        4 => return gen_float_bits(h, rng, kinds),
        5 => {
            let a = pool[rng.below(pool.len() as u64) as usize];
            return Ok(leaf("atom", make_atom(a), kinds));
        }
        6 => return gen_bitstring(h, rng, if top { 300 } else { 40 }, kinds),
        7 => return gen_byte_list(h, rng, kinds),
        8 => return Ok(leaf("nil", NIL, kinds)),
        k => k,
    };
    let sub = depth.saturating_sub(1);
    match kind {
        9 => {
            let n = rng.below(4) as usize;
            let mut elems = Vec::with_capacity(n);
            for _ in 0..n {
                elems.push(gen_term(h, atoms, pool, node, rng, sub, kinds)?);
            }
            kinds.push("tuple");
            h.tuple(&elems)
        }
        10 => {
            let n = width(rng, depth);
            let elems = gen_elems(h, atoms, pool, node, rng, sub, n, kinds)?;
            kinds.push("proper_list");
            build_list(h, &elems, NIL)
        }
        11 => {
            let n = width(rng, depth);
            let elems = gen_elems(h, atoms, pool, node, rng, sub, n, kinds)?;
            let tail = gen_term(h, atoms, pool, node, rng, sub, kinds)?;
            kinds.push("improper_list");
            build_list(h, &elems, tail)
        }
        12 => {
            // Only the outermost level may be a large (HAMT-sized) map: deeper levels
            // stay small so a recursively nested map cannot blow up the term size (and
            // the single bounded heap) exponentially.
            let n = if top {
                flat_map_size(rng)
            } else {
                1 + rng.below(3) as usize
            };
            gen_int_key_map(h, atoms, pool, node, rng, sub, n, kinds)
        }
        13 => {
            let n = if top {
                33 + rng.below(16) as usize
            } else {
                1 + rng.below(3) as usize
            };
            gen_int_key_map(h, atoms, pool, node, rng, sub, n, kinds)
        }
        14 => {
            let n = if top { 1 + rng.below(6) as usize } else { 1 };
            gen_bin_key_map(h, atoms, pool, node, rng, sub, n, kinds)
        }
        15 => gen_external_pid(h, node, rng, kinds),
        16 => gen_external_port(h, node, rng, kinds),
        17 => gen_external_ref(h, node, rng, kinds),
        18 => {
            let num = rng.next_u64() & ((1 << 28) - 1);
            let ser = rng.next_u64() & ((1 << 32) - 1);
            Ok(leaf(
                "internal_pid",
                make_internal_pid(make_pid_data(ser, num)),
                kinds,
            ))
        }
        19 => Ok(leaf(
            "internal_port",
            make_internal_port(make_port_data(rng.next_u64())),
            kinds,
        )),
        20 => gen_export_fun(h, pool, rng, kinds),
        21 => {
            let n = width(rng, depth);
            let env = gen_elems(h, atoms, pool, node, rng, sub, n, kinds)?;
            let uniq = rng.next_u64().to_be_bytes();
            let mut uniq16 = [0u8; 16];
            uniq16[..8].copy_from_slice(&uniq);
            uniq16[8..].copy_from_slice(&rng.next_u64().to_be_bytes());
            let creator = FunPid {
                node,
                number: rng.next_u64() as u32,
                serial: rng.next_u64() as u32,
                creation: rng.next_u64() as u32,
            };
            kinds.push("imported_fun");
            make_fun_imported(
                h,
                rng.below(3) as usize,
                pool[rng.below(pool.len() as u64) as usize],
                &uniq16,
                rng.next_u64() as u32,
                rng.next_u64() as u32,
                rng.next_u64() as u32,
                creator,
                &env,
            )
        }
        // A 2-tuple of independently generated terms (a common `.beam` literal shape).
        22 => {
            let a = gen_term(h, atoms, pool, node, rng, sub, kinds)?;
            let b = gen_term(h, atoms, pool, node, rng, sub, kinds)?;
            kinds.push("mixed_tuple");
            h.tuple(&[a, b])
        }
        _ => unreachable!("choice is bounded by `choices`"),
    }
}

fn leaf(kind: &'static str, term: Eterm, kinds: &mut Vec<&'static str>) -> Eterm {
    kinds.push(kind);
    term
}

fn gen_elems(
    h: &mut Heap,
    atoms: &AtomTable,
    pool: &[Atom],
    node: Atom,
    rng: &mut Rng,
    depth: u32,
    n: usize,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Vec<Eterm>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(gen_term(h, atoms, pool, node, rng, depth, kinds)?);
    }
    Ok(out)
}

fn build_list(h: &mut Heap, elems: &[Eterm], tail: Eterm) -> beam_common::Result<Eterm> {
    let mut acc = tail;
    for &e in elems.iter().rev() {
        acc = h.cons(e, acc)?;
    }
    Ok(acc)
}

/// Small-int values at every wire boundary.
fn boundary_small(rng: &mut Rng) -> i64 {
    const B: &[i64] = &[
        0,
        1,
        -1,
        2,
        255,
        256,
        257,
        -255,
        -256,
        65_535,
        65_536,
        2_147_483_647,
        2_147_483_648,
        -2_147_483_648,
        -2_147_483_649,
        1 << 40,
        -(1 << 40),
        (1 << 59) - 1,
        (-(1i64 << 59)) + 1,
        (-(1i64 << 59)) + 1,
    ];
    B[rng.below(B.len() as u64) as usize]
}

/// A uniform pseudo-random 60-bit small int.
fn random_small(rng: &mut Rng) -> i64 {
    (rng.next_u64() as i64) >> 5
}

fn gen_big(
    h: &mut Heap,
    rng: &mut Rng,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    // The significant byte count is `8 * n` (the top digit's high bit is always set),
    // so `n == 31` is the `SMALL_BIG_EXT` ceiling (248 bytes) and `n == 32` the
    // `LARGE_BIG_EXT` floor (256 bytes) — both boundaries plus a few larger sizes.
    let n = match rng.below(6) {
        0 => 1 + rng.below(5) as usize,
        1 => 8,
        2 => 31,
        3 => 32,
        4 => 33 + rng.below(8) as usize,
        _ => 40,
    };
    let mut digits: Vec<u64> = (0..n).map(|_| rng.next_u64()).collect();
    // Forcing the top digit's high bit keeps the magnitude above `2^63`, so the value
    // never collapses into a 60-bit small on decode (which would change the tag).
    digits[n - 1] |= 1 << 63;
    kinds.push("bignum");
    make_big(h, rng.bool(), &digits)
}

fn gen_float(
    h: &mut Heap,
    rng: &mut Rng,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    const F: &[f64] = &[
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        3.14,
        -2.5,
        1.0e-5,
        1.0e20,
        1.0e300,
        -1.0e300,
        5.0e-324,
        1.0e-310,
        123_456_789.123_456,
        644_048_203_731.4326,
        0.1,
    ];
    kinds.push("float");
    make_float(h, F[rng.below(F.len() as u64) as usize])
}

fn gen_float_bits(
    h: &mut Heap,
    rng: &mut Rng,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    loop {
        let f = f64::from_bits(rng.next_u64());
        if f.is_finite() {
            kinds.push("float_bits");
            return make_float(h, f);
        }
    }
}

fn gen_bitstring(
    h: &mut Heap,
    rng: &mut Rng,
    max_bytes: usize,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    let nbytes = rng.below(max_bytes as u64 + 1) as usize;
    let bytes: Vec<u8> = (0..nbytes).map(|_| rng.next_u64() as u8).collect();
    let total = nbytes * 8;
    let n_bits = if total == 0 {
        0
    } else if rng.bool() {
        total
    } else {
        total - rng.below(8) as usize
    };
    // `make_bitstring` zeroes the unused trailing bits, so the term is canonical.
    kinds.push(if n_bits % 8 == 0 {
        "binary"
    } else {
        "bitstring"
    });
    make_bitstring(h, &bytes, n_bits)
}

fn gen_byte_list(
    h: &mut Heap,
    rng: &mut Rng,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    let n = rng.below(6) as usize;
    let elems: Vec<Eterm> = (0..n).map(|_| make_small(rng.below(256) as i64)).collect();
    kinds.push("byte_list");
    build_list(h, &elems, NIL)
}

/// A flat map size: mostly small, sometimes exactly `MAP_SMALL_MAP_LIMIT` (32).
fn flat_map_size(rng: &mut Rng) -> usize {
    match rng.below(4) {
        0 => 1 + rng.below(4) as usize,
        1 => 1 + rng.below(20) as usize,
        2 => 32,
        _ => 1 + rng.below(33) as usize,
    }
}

fn gen_int_key_map(
    h: &mut Heap,
    atoms: &AtomTable,
    pool: &[Atom],
    node: Atom,
    rng: &mut Rng,
    sub: u32,
    n: usize,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    let mut pairs = Vec::with_capacity(n);
    for i in 0..n {
        // Distinct integer keys ⇒ no duplicate-key collapse and a deterministic order.
        let k = make_small(i as i64);
        let v = gen_term(h, atoms, pool, node, rng, sub, kinds)?;
        pairs.push((k, v));
    }
    kinds.push(if n > 32 { "hamt_map" } else { "flat_map" });
    make_map(h, &pairs, atoms)
}

fn gen_bin_key_map(
    h: &mut Heap,
    atoms: &AtomTable,
    pool: &[Atom],
    node: Atom,
    rng: &mut Rng,
    sub: u32,
    n: usize,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    let mut pairs = Vec::with_capacity(n);
    for i in 0..n {
        // Distinct one-byte binaries: byte-lexicographic order is VM-independent.
        let k = make_bitstring(h, &[i as u8], 8)?;
        let v = gen_term(h, atoms, pool, node, rng, sub, kinds)?;
        pairs.push((k, v));
    }
    kinds.push("binary_key_map");
    make_map(h, &pairs, atoms)
}

fn gen_external_pid(
    h: &mut Heap,
    node: Atom,
    rng: &mut Rng,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    kinds.push("external_pid");
    make_external_pid(
        h,
        make_atom(node),
        rng.next_u64() as u32,
        rng.next_u64() as u32,
        rng.next_u64() as u32,
    )
}

fn gen_external_port(
    h: &mut Heap,
    node: Atom,
    rng: &mut Rng,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    kinds.push("external_port");
    make_external_port(h, make_atom(node), rng.next_u64(), rng.next_u64() as u32)
}

fn gen_external_ref(
    h: &mut Heap,
    node: Atom,
    rng: &mut Rng,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    // 1..=5 ids: a zero-id reference is not a fixed point of this codec (see the module
    // docs and `tests/ref_ext_zero_and_arity.rs`), so it is kept out of the corpus.
    let n = 1 + rng.below(5) as usize;
    let ids: Vec<u32> = (0..n).map(|_| rng.next_u64() as u32).collect();
    kinds.push("external_ref");
    make_external_ref(h, make_atom(node), &ids, rng.next_u64() as u32)
}

fn gen_export_fun(
    h: &mut Heap,
    pool: &[Atom],
    rng: &mut Rng,
    kinds: &mut Vec<&'static str>,
) -> beam_common::Result<Eterm> {
    kinds.push("export_fun");
    make_fun_external(
        h,
        rng.below(4) as usize,
        pool[rng.below(pool.len() as u64) as usize],
        pool[rng.below(pool.len() as u64) as usize],
    )
}

// ---------------------------------------------------------------------------
// Encoding the corpus (default + the minor-version matrix)
// ---------------------------------------------------------------------------

/// Our three `{minor_version, N}` images plus the default (V2) one.
pub struct Generated {
    pub image: Vec<u8>,
    pub minor: [Vec<u8>; 3],
    pub source: Option<String>,
}

fn encode_minor(t: Eterm, atoms: &AtomTable, node: Atom, minor: MinorVersion) -> Vec<u8> {
    let mut e = Encoder::new(atoms)
        .with_minor_version(minor)
        .with_local_node(node, CREATION);
    e.encode(t).expect("a generated term always encodes");
    e.finish()
}

/// Does `t` contain a **runtime-internal** pid/port immediate anywhere?
///
/// Such an immediate carries no node identity, so the default `encode_term` (no
/// `with_local_node`) must reject it with the typed `unsupported_external_term`
/// (crate `CONTEXT.md`, §Internal ids); every other term kind encodes identically
/// with or without the local-node identity.
fn has_internal_id(t: Eterm, atoms: &AtomTable) -> bool {
    if is_internal_pid(t) || is_internal_port(t) {
        return true;
    }
    // A fun's environment is a term vector reached only through the fun interior.
    if let Ok(env) = beam_codec::term::fun_env(t) {
        return env.iter().any(|e| has_internal_id(*e, atoms));
    }
    if let Ok(arity) = tuple_arity(t) {
        return (0..arity)
            .filter_map(|i| tuple_element(t, i).ok())
            .any(|e| has_internal_id(e, atoms));
    }
    if is_list(t) {
        let mut cur = t;
        loop {
            if is_nil(cur) {
                return false;
            }
            if !is_list(cur) {
                // An improper list's final tail is itself a term.
                return has_internal_id(cur, atoms);
            }
            if let Ok(head) = cons_head(cur) {
                if has_internal_id(head, atoms) {
                    return true;
                }
            }
            match cons_tail(cur) {
                Ok(tail) => cur = tail,
                Err(_) => return false,
            }
        }
    }
    if let Ok(pairs) = beam_codec::term::map_pairs_storage_order(t) {
        return pairs
            .into_iter()
            .any(|(k, v)| has_internal_id(k, atoms) || has_internal_id(v, atoms));
    }
    false
}

/// Generates `count` terms and, for each, its image set and Erlang source (if any).
pub fn generate(seed: u64, count: usize) -> (Vec<Generated>, Vec<&'static str>) {
    let mut atoms = AtomTable::new();
    let pool: Vec<Atom> = atom_pool().iter().map(|n| atoms.intern(n)).collect();
    let node = atoms.intern(SYSNAME);
    let mut rng = Rng::new(seed);
    let mut kinds = Vec::new();
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let mut h = Heap::new(HEAP_WORDS).expect("the carrier allocator serves this heap");
        let t = gen_term(&mut h, &atoms, &pool, node, &mut rng, MAX_DEPTH, &mut kinds)
            .expect("generation stays inside the heap");
        let minor = [
            encode_minor(t, &atoms, node, MinorVersion::V0),
            encode_minor(t, &atoms, node, MinorVersion::V1),
            encode_minor(t, &atoms, node, MinorVersion::V2),
        ];
        let source = render(t, &atoms);
        // The `V2` flavour is the default image whenever the term carries no runtime-internal
        // id (internal ids additionally need the local-node identity the default lacks).
        match encode_term(t, &atoms) {
            Ok(img) => assert_eq!(
                minor[2], img,
                "the default (V2) image differs from encode_term"
            ),
            Err(e) => assert!(
                has_internal_id(t, &atoms) && beam_codec::error::is_unsupported(&e),
                "the default image failed with {e:?} on a term with no internal id; \
                 source = {:?}",
                render(t, &atoms)
            ),
        }
        out.push(Generated {
            image: minor[2].clone(),
            minor,
            source,
        });
    }
    (out, kinds)
}

// ---------------------------------------------------------------------------
// The round-trip property helper (oracle-independent)
// ---------------------------------------------------------------------------

/// `encode -> decode -> encode` must be byte-identical to `image` for the flavour it
/// was produced in.
pub fn round_trip(image: &[u8], minor: MinorVersion, what: &str) {
    let mut h = Heap::new(HEAP_WORDS).expect("the carrier allocator serves this heap");
    let mut atoms = AtomTable::new();
    let t = decode_term(image, &mut h, &mut atoms)
        .unwrap_or_else(|e| panic!("{what}: our decoder rejected img {image:?}: {e:?}"));
    // Re-encode in the *same* flavour the image was produced in; a decoded internal id
    // has become an external thing, so no local-node identity is needed here.
    let mut e = Encoder::new(&atoms).with_minor_version(minor);
    e.encode(t)
        .unwrap_or_else(|e| panic!("{what}: our encoder rejected the decoded term: {e:?}"));
    let re = e.finish();
    assert_eq!(
        re, image,
        "{what}: encode -> decode -> encode is not byte-identical\n image = {image:?}\n again = {re:?}"
    );
}
