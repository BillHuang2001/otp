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

//! Deterministic property/fuzz corpus for the **whole producible ETF tag set**.
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
//!
//! ## Oracle cross-check (skipped loudly without `erl`)
//!
//! Every generated term is also rendered to Erlang **source**; the renderable subset is
//! handed to the system `erl` in a few large batches, which prints `term_to_binary(T)`
//! per term. We then assert, per term:
//!
//! 1. our `encode(term)` is **byte-identical** to the oracle's image;
//! 2. our **decoder** accepts the oracle's image and re-encodes it to the same bytes;
//! 3. the two decodes are structurally equal (`terms_equal`, atoms by name);
//! 4. the oracle accepts **our** image (`term_to_binary(binary_to_term(ours))`).
//!
//! and for the `{minor_version, 0|1|2}` matrix, our `minor` image equals the oracle's
//! `term_to_binary(T, [{minor_version, N}])` byte-for-byte. Oracle resolution mirrors
//! `tools/differential`: `$ERTS_ORACLE` if executable, else `erl` on `$PATH`; when none
//! exists the test prints a loud skip note and returns — a skip, never a silent pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;

use beam_codec::term::{
    big_parts, bitstring_bytes, bitstring_size_bits, cons_head, cons_tail, external_pid_parts,
    external_port_parts, external_ref_parts, float_value, fun_arity_term, fun_function,
    fun_kind_term, fun_module, fun_true_arity_term, make_big, make_bitstring, make_external_pid,
    make_external_port, make_external_ref, make_float, make_fun_external,
    make_fun_imported, make_map, tuple_arity, tuple_element, FunKind, FunPid,
};
use beam_codec::{
    decode_term, encode_term, Encoder, MinorVersion,
    EXPORT_EXT, FLOAT_EXT, INTEGER_EXT, LARGE_BIG_EXT, MAP_EXT, NEWER_REFERENCE_EXT, NEW_FLOAT_EXT,
    NEW_FUN_EXT, NEW_PID_EXT, NIL_EXT, SMALL_ATOM_UTF8_EXT, SMALL_BIG_EXT, SMALL_INTEGER_EXT,
    STRING_EXT, V4_PORT_EXT, VERSION_MAGIC,
};
use beam_term::prelude::{
    atom_val, is_atom, is_internal_pid, is_internal_port, is_list, is_nil, is_small, make_atom,
    make_internal_pid, make_internal_port, make_pid_data, make_port_data, make_small, signed_value,
    Atom, AtomTable, Eterm, NIL,
};

// ---------------------------------------------------------------------------
// Deterministic PRNG (splitmix64) — no external crate
// ---------------------------------------------------------------------------

/// Fixed seed: the corpus is identical on every run and every machine.
const SEED: u64 = 0x5EED_F00D_1234_5678;
/// Terms generated for the (oracle-independent) round-trip property.
const PROPERTY_TERMS: usize = 5_000;
/// Renderable terms handed to the oracle (keeps the cross-check a few spawns).
const ORACLE_LIMIT: usize = 300;
/// Recursion ceiling of the generator.
const MAX_DEPTH: u32 = 5;
/// Decode/build heap (word count) — generous for the bounded generator.
const HEAP_WORDS: usize = 1 << 18;
/// The `creation` encoded for internal id immediates.
const CREATION: u32 = 0;
/// The local-node sysname the internal-id images carry.
const SYSNAME: &str = "nonode@nohost";

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
        0 => return Ok(leaf("small_boundary", make_small(boundary_small(rng)), kinds)),
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
            let n = if top { flat_map_size(rng) } else { 1 + rng.below(3) as usize };
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
        0, 1, -1, 2, 255, 256, 257, -255, -256, 65_535, 65_536, 2_147_483_647, 2_147_483_648,
        -2_147_483_648, -2_147_483_649, 1 << 40, -(1 << 40), (1 << 59) - 1, (-(1i64 << 59)) + 1,
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
    kinds.push(if n_bits % 8 == 0 { "binary" } else { "bitstring" });
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
struct Generated {
    image: Vec<u8>,
    minor: [Vec<u8>; 3],
    source: Option<String>,
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
fn generate(seed: u64, count: usize) -> (Vec<Generated>, Vec<&'static str>) {
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
            Ok(img) => assert_eq!(minor[2], img, "the default (V2) image differs from encode_term"),
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
// Rendering a term to Erlang source (for the oracle corpus)
// ---------------------------------------------------------------------------

fn quote_atom(name: &str) -> String {
    let mut s = String::from("'");
    for c in name.chars() {
        match c {
            '\'' => s.push_str("\\'"),
            '\\' => s.push_str("\\\\"),
            _ => s.push(c),
        }
    }
    s.push('\'');
    s
}

fn render_float(f: f64) -> String {
    // `{:e}` is the shortest round-tripping decimal; Erlang needs a radix point.
    let s = format!("{f:e}");
    if s.contains('.') {
        s
    } else {
        match s.find('e') {
            Some(i) => format!("{}.0{}", &s[..i], &s[i..]),
            None => format!("{s}.0"),
        }
    }
}

fn render_big(negative: bool, digits: &[u64]) -> String {
    let mut parts = Vec::new();
    for (i, d) in digits.iter().enumerate() {
        if *d == 0 {
            continue;
        }
        if i == 0 {
            parts.push(format!("{d}"));
        } else {
            parts.push(format!("({d} bsl {})", 64 * i));
        }
    }
    let body = if parts.is_empty() {
        "0".to_string()
    } else {
        parts.join(" + ")
    };
    if negative {
        format!("-({body})")
    } else {
        format!("({body})")
    }
}

fn render_bitstring(bytes: &[u8], offset: usize, size: usize) -> Option<String> {
    let get = |start: usize, n: usize| -> u64 {
        let mut v = 0u64;
        for i in 0..n {
            let bit = (bytes[(start + i) >> 3] >> (7 - ((start + i) & 7))) & 1;
            v = (v << 1) | u64::from(bit);
        }
        v
    };
    let full = size / 8;
    let rem = size % 8;
    let mut parts = Vec::with_capacity(full + 1);
    for i in 0..full {
        parts.push(format!("{}", get(offset + i * 8, 8)));
    }
    if rem > 0 {
        parts.push(format!("{}:{}", get(offset + full * 8, rem), rem));
    }
    Some(format!("<<{}>>", parts.join(",")))
}

fn render_list(t: Eterm, atoms: &AtomTable) -> Option<String> {
    let mut elems = Vec::new();
    let mut cur = t;
    loop {
        if is_nil(cur) {
            return Some(format!("[{}]", elems.join(",")));
        }
        if !is_list(cur) {
            return Some(format!("[{} | {}]", elems.join(","), render(cur, atoms)?));
        }
        elems.push(render(cons_head(cur).ok()?, atoms)?);
        cur = cons_tail(cur).ok()?;
    }
}

/// Erlang source for `t`, or `None` for a kind with no source-level construction here
/// (internal/external identifiers, `NEW_FUN_EXT` funs, records).
fn render(t: Eterm, atoms: &AtomTable) -> Option<String> {
    if is_nil(t) {
        return Some("[]".to_string());
    }
    if is_small(t) {
        return Some(signed_value(t).to_string());
    }
    if is_atom(t) {
        return Some(quote_atom(atoms.name(atom_val(t))?));
    }
    if is_list(t) {
        return render_list(t, atoms);
    }
    if let Ok(kind) = fun_kind_term(t) {
        if kind != FunKind::External {
            return None;
        }
        let m = quote_atom(atoms.name(fun_module(t).ok()?)?);
        let f = quote_atom(atoms.name(fun_function(t).ok()?)?);
        return Some(format!("fun {m}:{f}/{}", fun_true_arity_term(t).ok()?));
    }
    if let Ok((neg, digits)) = big_parts(t) {
        return Some(render_big(neg, digits));
    }
    if let Ok(v) = float_value(t) {
        return Some(render_float(v));
    }
    if let Ok(arity) = tuple_arity(t) {
        let mut parts = Vec::with_capacity(arity);
        for i in 0..arity {
            parts.push(render(tuple_element(t, i).ok()?, atoms)?);
        }
        return Some(format!("{{{}}}", parts.join(",")));
    }
    if let Ok(pairs) = beam_term::map::map_pairs(t, atoms) {
        let mut parts = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            parts.push(format!("{} => {}", render(k, atoms)?, render(v, atoms)?));
        }
        return Some(format!("#{{{}}}", parts.join(",")));
    }
    if let Ok((bytes, offset, size)) = bitstring_bytes(t) {
        return render_bitstring(bytes, offset, size);
    }
    None
}

// ---------------------------------------------------------------------------
// Structural comparison (the renderable kinds)
// ---------------------------------------------------------------------------

fn bits_equal(ba: &[u8], oa: usize, bb: &[u8], ob: usize, nbits: usize) -> bool {
    let bit = |bytes: &[u8], i: usize| (bytes[i >> 3] >> (7 - (i & 7))) & 1;
    (0..nbits).all(|i| bit(ba, oa + i) == bit(bb, ob + i))
}

/// Structural `=:=` between two terms (atoms by name, floats by bit pattern,
/// bitstrings bit-by-bit) — the crate's `tests/interop.rs::terms_equal` contract,
/// narrowed to the renderable corpus.
fn terms_equal(a: Eterm, b: Eterm, atoms: &AtomTable) -> bool {
    if is_nil(a) || is_nil(b) {
        return is_nil(a) && is_nil(b);
    }
    if is_small(a) || is_small(b) {
        return is_small(a) && is_small(b) && signed_value(a) == signed_value(b);
    }
    if is_atom(a) || is_atom(b) {
        return is_atom(a) && is_atom(b) && atoms.name(atom_val(a)) == atoms.name(atom_val(b));
    }
    if is_list(a) || is_list(b) {
        if !(is_list(a) && is_list(b)) {
            return false;
        }
        let (Some(ha), Some(ta), Some(hb), Some(tb)) = (
            cons_head(a).ok(),
            cons_tail(a).ok(),
            cons_head(b).ok(),
            cons_tail(b).ok(),
        ) else {
            return false;
        };
        return terms_equal(ha, hb, atoms) && terms_equal(ta, tb, atoms);
    }
    if let (Ok(x), Ok(y)) = (float_value(a), float_value(b)) {
        return x.to_bits() == y.to_bits();
    }
    if let (Ok((sa, da)), Ok((sb, db))) = (big_parts(a), big_parts(b)) {
        return sa == sb && da == db;
    }
    if let (Ok(ka), Ok(kb)) = (fun_kind_term(a), fun_kind_term(b)) {
        if ka != kb || fun_arity_term(a).ok() != fun_arity_term(b).ok() {
            return false;
        }
        return match (fun_module(a).ok(), fun_module(b).ok()) {
            (Some(ma), Some(mb)) => atoms.name(ma) == atoms.name(mb),
            _ => false,
        };
    }
    if let (Ok(na), Ok(nb)) = (bitstring_size_bits(a), bitstring_size_bits(b)) {
        if na != nb {
            return false;
        }
        let (Ok((ba, oa, _)), Ok((bb, ob, _))) = (bitstring_bytes(a), bitstring_bytes(b)) else {
            return false;
        };
        return bits_equal(ba, oa, bb, ob, na);
    }
    if let (Ok(pa), Ok(pb)) = (
        beam_term::map::map_pairs(a, atoms),
        beam_term::map::map_pairs(b, atoms),
    ) {
        return pa.len() == pb.len()
            && pa.iter().zip(&pb).all(|((ka, va), (kb, vb))| {
                terms_equal(*ka, *kb, atoms) && terms_equal(*va, *vb, atoms)
            });
    }
    if let (Ok(na), Ok(nb)) = (tuple_arity(a), tuple_arity(b)) {
        return na == nb
            && (0..na).all(|i| match (tuple_element(a, i).ok(), tuple_element(b, i).ok()) {
                (Some(x), Some(y)) => terms_equal(x, y, atoms),
                _ => false,
            });
    }
    if let (Ok(pa), Ok(pb)) = (external_pid_parts(a), external_pid_parts(b)) {
        return terms_equal(pa.0, pb.0, atoms) && pa.1 == pb.1 && pa.2 == pb.2 && pa.3 == pb.3;
    }
    if let (Ok(pa), Ok(pb)) = (external_port_parts(a), external_port_parts(b)) {
        return terms_equal(pa.0, pb.0, atoms) && pa.1 == pb.1 && pa.2 == pb.2;
    }
    if let (Ok(ra), Ok(rb)) = (external_ref_parts(a), external_ref_parts(b)) {
        return terms_equal(ra.0, rb.0, atoms) && ra.1 == rb.1 && ra.2 == rb.2;
    }
    if is_internal_pid(a) || is_internal_pid(b) || is_internal_port(a) || is_internal_port(b) {
        return a.raw() == b.raw();
    }
    panic!("terms_equal: unclassified terms {a:?} / {b:?}");
}

// ---------------------------------------------------------------------------
// The round-trip property (oracle-independent)
// ---------------------------------------------------------------------------

/// `encode -> decode -> encode` is byte-identical for every generated term, under every
/// `{minor_version, N}` flavour — and the corpus really covers the producible tag set.
#[test]
fn generated_corpus_round_trips_byte_identically() {
    let (corpus, kinds) = generate(SEED, PROPERTY_TERMS);
    assert_eq!(corpus.len(), PROPERTY_TERMS);

    let mut checked = 0usize;
    let mut top_tags = std::collections::BTreeMap::<u8, usize>::new();
    for (i, g) in corpus.iter().enumerate() {
        // Default (V2) image: encode -> decode -> encode.
        round_trip(&g.image, MinorVersion::V2, &format!("term #{i} (V2)"));
        // And each minor-version flavour. Decoding an internal id yields the *external*
        // thing, so re-encoding needs no local-node identity — only the same flavour.
        for (k, m) in [
            (0usize, MinorVersion::V0),
            (1, MinorVersion::V1),
            (2, MinorVersion::V2),
        ] {
            round_trip(&g.minor[k], m, &format!("term #{i} ({m:?})"));
        }
        *top_tags.entry(g.image[1]).or_default() += 1;
        checked += 1;
    }
    assert_eq!(checked, PROPERTY_TERMS);

    // The corpus must genuinely exercise the whole producible surface.
    for kind in [
        "small_boundary",
        "small_random",
        "bignum",
        "float",
        "float_bits",
        "atom",
        "binary",
        "bitstring",
        "byte_list",
        "nil",
        "tuple",
        "proper_list",
        "improper_list",
        "flat_map",
        "hamt_map",
        "binary_key_map",
        "external_pid",
        "external_port",
        "external_ref",
        "internal_pid",
        "internal_port",
        "export_fun",
        "imported_fun",
        "mixed_tuple",
    ] {
        let n = kinds.iter().filter(|k| **k == kind).count();
        assert!(n > 0, "the generated corpus never produced a `{kind}`");
    }
    // Every integer/atom/list/map wire tag must appear at least once at the top level.
    for tag in [
        SMALL_INTEGER_EXT,
        INTEGER_EXT,
        SMALL_BIG_EXT,
        LARGE_BIG_EXT,
        NEW_FLOAT_EXT,
        SMALL_ATOM_UTF8_EXT,
        STRING_EXT,
        NIL_EXT,
        MAP_EXT,
        NEW_PID_EXT,
        V4_PORT_EXT,
        NEWER_REFERENCE_EXT,
        EXPORT_EXT,
        NEW_FUN_EXT,
    ] {
        assert!(
            top_tags.contains_key(&tag),
            "no top-level image carries tag {tag}: {:?}",
            top_tags.keys().collect::<Vec<_>>()
        );
    }
    eprintln!(
        "[beam_codec property] {} terms round-tripped byte-identically under V0/V1/V2 \
         ({} distinct top-level tags)",
        PROPERTY_TERMS,
        top_tags.len()
    );
}

fn round_trip(image: &[u8], minor: MinorVersion, what: &str) {
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

// ---------------------------------------------------------------------------
// Oracle resolution and process plumbing (mirrors `tools/differential`)
// ---------------------------------------------------------------------------

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(p) {
            Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec property] $ERTS_ORACLE='{}' is not an existing executable.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn oracle_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(program) => Some(program),
        None => {
            eprintln!();
            eprintln!("============ beam_codec property: {test} SKIPPED ============");
            eprintln!("No usable system `erl` oracle found — see the note above, if any");
            eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — install Erlang/OTP and re-run.");
            eprintln!("==============================================================");
            eprintln!();
            None
        }
    }
}

fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval (head) ---\n{}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        &eval[..eval.len().min(400)],
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// Parse the `[<<131,...>>,<<...>>]` list the oracle prints into byte vectors.
fn parse_binary_list(out: &str) -> Vec<Vec<u8>> {
    let bytes = out.as_bytes();
    let mut images = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1] == b'<' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'>' && bytes[j + 1] == b'>') {
                j += 1;
            }
            assert!(j + 1 < bytes.len(), "unterminated <<...>> in {out:?}");
            let body = &out[start..j];
            let image = if body.trim().is_empty() {
                Vec::new()
            } else {
                body.split(',')
                    .map(|t| {
                        t.trim()
                            .parse::<u8>()
                            .unwrap_or_else(|_| panic!("bad byte {t:?} in the oracle output"))
                    })
                    .collect()
            };
            images.push(image);
            i = j + 2;
        } else {
            i += 1;
        }
    }
    images
}

fn erl_bytes(bytes: &[u8]) -> String {
    let inner = bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
}

/// Index ranges whose rendered/byte `-eval` payload stays under `budget` characters.
fn chunks<T: AsRef<str>>(items: &[T], budget: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let mut end = start;
        let mut len = 0;
        while end < items.len() && (end == start || len + items[end].as_ref().len() <= budget) {
            len += items[end].as_ref().len();
            end += 1;
        }
        out.push((start, end));
        start = end;
    }
    out
}

// ---------------------------------------------------------------------------
// Oracle cross-check: our image vs `term_to_binary(T)` for the rendered corpus
// ---------------------------------------------------------------------------

/// The renderable corpus is handed to `erl` in a few large batches; each image must be
/// **byte-identical** to ours, our decoder must read the oracle's exact bytes, the two
/// decodes must be structurally equal, and the oracle must accept our image.
#[test]
fn oracle_agrees_on_the_generated_corpus() {
    let Some(oracle) = oracle_or_skip("oracle_agrees_on_the_generated_corpus") else {
        return;
    };
    let (corpus, _) = generate(SEED, PROPERTY_TERMS);
    let renderable: Vec<&Generated> = corpus
        .iter()
        .filter(|g| g.source.is_some())
        .take(ORACLE_LIMIT)
        .collect();
    assert!(
        renderable.len() >= 100,
        "only {} renderable terms in {} — the generator lost its source-renderable kinds",
        renderable.len(),
        PROPERTY_TERMS
    );

    // --- Direction 1: oracle -> us ------------------------------------------------
    let sources: Vec<String> = renderable
        .iter()
        .map(|g| g.source.clone().expect("filtered on `is_some`"))
        .collect();
    let mut compared = 0usize;
    for (start, end) in chunks(&sources, 30_000) {
        let eval = format!(
            "io:format(\"~w~n\", [ [ term_to_binary(T) || T <- [{}] ] ])",
            sources[start..end].join(",")
        );
        let images = parse_binary_list(&run_erl(&oracle, &eval));
        assert_eq!(
            images.len(),
            end - start,
            "the oracle printed {} images for a {}-entry batch",
            images.len(),
            end - start
        );
        for (k, bi) in images.iter().enumerate() {
            let g = renderable[start + k];
            assert!(
                bi.first() == Some(&VERSION_MAGIC),
                "oracle image {bi:?} lacks the version byte"
            );
            assert_eq!(
                bi, &g.image,
                "source {} produced a different image\n oracle = {bi:?}\n ours   = {:?}",
                g.source.as_deref().unwrap_or(""),
                g.image
            );
            // Our decoder reads the oracle's exact bytes and reproduces them.
            let mut atoms = AtomTable::new();
            let mut h1 = Heap::new(HEAP_WORDS).expect("heap");
            let t1 = decode_term(bi, &mut h1, &mut atoms)
                .unwrap_or_else(|e| panic!("our decoder rejected the oracle image {bi:?}: {e:?}"));
            assert_eq!(
                encode_term(t1, &atoms).expect("re-encode the oracle image"),
                *bi,
                "our decoder does not reproduce the oracle image {bi:?}"
            );
            // Structural equality with our own decode of our own image.
            let mut h2 = Heap::new(HEAP_WORDS).expect("heap");
            let t2 = decode_term(&g.image, &mut h2, &mut atoms)
                .unwrap_or_else(|e| panic!("our decoder rejected our image {:?}: {e:?}", g.image));
            assert!(
                terms_equal(t1, t2, &atoms),
                "our decode of the oracle image differs structurally from our own image\n\
                 \x20 oracle = {bi:?}\n\x20 ours   = {:?}",
                g.image
            );
            compared += 1;
        }
    }

    // --- Direction 2: us -> oracle ------------------------------------------------
    let ours: Vec<String> = renderable.iter().map(|g| erl_bytes(&g.image)).collect();
    for (start, end) in chunks(&ours, 30_000) {
        let eval = format!(
            "io:format(\"~w~n\", [ [ term_to_binary(binary_to_term(list_to_binary(B))) \
             || B <- [{}] ] ])",
            ours[start..end].join(",")
        );
        let images = parse_binary_list(&run_erl(&oracle, &eval));
        assert_eq!(images.len(), end - start, "the oracle printed {} images", images.len());
        for (k, bi) in images.iter().enumerate() {
            let g = renderable[start + k];
            assert_eq!(
                bi, &g.image,
                "the oracle's decode/encode of our image is not our image\n ours = {:?}\n oracle = {bi:?}",
                g.image
            );
        }
    }
    eprintln!(
        "[beam_codec property] the oracle confirmed {compared} generated terms byte-for-byte \
         (both directions) over {} renderable terms",
        renderable.len()
    );
}

// ---------------------------------------------------------------------------
// Oracle cross-check: the `{minor_version, N}` matrix
// ---------------------------------------------------------------------------

/// For the renderable corpus, our `encode_term_with_minor_version` must equal the
/// oracle's `term_to_binary(T, [{minor_version, N}])` byte-for-byte for `N` 0/1/2.
/// (The only documented exclusion — a non-finite float under V0 — cannot occur: the
/// generator never emits a non-finite float, and such a `NEW_FLOAT_EXT`/`FLOAT_EXT`
/// image is rejected on decode anyway.)
#[test]
fn oracle_agrees_on_the_minor_version_matrix() {
    let Some(oracle) = oracle_or_skip("oracle_agrees_on_the_minor_version_matrix") else {
        return;
    };
    let (corpus, _) = generate(SEED, PROPERTY_TERMS);
    let renderable: Vec<&Generated> = corpus
        .iter()
        .filter(|g| g.source.is_some())
        .take(ORACLE_LIMIT)
        .collect();
    let sources: Vec<String> = renderable
        .iter()
        .map(|g| g.source.clone().expect("filtered on `is_some`"))
        .collect();

    for (k, opt) in [
        (0usize, "{minor_version,0}"),
        (1, "{minor_version,1}"),
        (2, "{minor_version,2}"),
    ] {
        let mut exact = 0usize;
        for (start, end) in chunks(&sources, 30_000) {
            let eval = format!(
                "io:format(\"~w~n\", [ [ term_to_binary(T,[{opt}]) || T <- [{}] ] ])",
                sources[start..end].join(",")
            );
            let images = parse_binary_list(&run_erl(&oracle, &eval));
            assert_eq!(images.len(), end - start, "the oracle printed {} images", images.len());
            for (j, bi) in images.iter().enumerate() {
                let g = renderable[start + j];
                assert_eq!(
                    bi, &g.minor[k],
                    "{opt}: our minor-version image differs from the oracle\n source = {}\n \
                     oracle = {bi:?}\n ours   = {:?}",
                    g.source.as_deref().unwrap_or(""),
                    g.minor[k]
                );
                exact += 1;
            }
        }
        eprintln!("[beam_codec property] minor_version {k}: {exact} images byte-identical to the oracle");
    }
    // `V0` is the only flavour that may emit the legacy 32-bit `INTEGER_EXT`/`FLOAT_EXT`
    // wire forms differently; assert the corpus actually reached `FLOAT_EXT` at least
    // once so the legacy path is covered.
    assert!(
        renderable.iter().any(|g| g.minor[0].contains(&FLOAT_EXT)),
        "the V0 corpus never emitted a FLOAT_EXT — the legacy float path is uncovered"
    );
}
