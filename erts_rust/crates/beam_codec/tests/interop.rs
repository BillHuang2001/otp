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

//! Ground-truth interop harness — our ETF codec vs. the system Erlang oracle.
//!
//! Unlike `tests/ground_truth.rs` (pasted byte vectors) this file **shells out to the
//! real `erl`** at test time and compares live: the oracle produces the reference ETF
//! image for each corpus term, we decode it, re-encode it, and then ask the oracle to
//! confirm that `binary_to_term/1` of both images yields `=:=` terms.
//!
//! ## Oracle resolution (mirrors `tools/differential`)
//!
//! `$ERTS_ORACLE` if it names an existing executable, otherwise `erl` found on
//! `$PATH`. When no oracle exists the oracle-dependent tests print a **loud** note to
//! stderr and `return` — a skip, never a silent pass (Rust has no test-skip). The
//! `FLOAT_EXT` and malformed-input tests below are pure and always run.
//!
//! ## Two directions, one `erl` spawn each
//!
//! Spawning `erl` costs ~120 ms of VM boot, so the corpus is batched into a **single**
//! invocation per direction:
//!
//! 1. **Oracle → us.** `erl` prints `[term_to_binary(T) || T <- corpus]` (a list of
//!    byte lists); Rust parses them, decodes each with [`decode_term`], re-encodes with
//!    [`encode_term`], decodes the result again and asserts the two decoded terms are
//!    *structurally* equal ([`terms_equal`]). We do **not** compare bytes here: map key
//!    order and the `FLOAT_EXT`→`NEW_FLOAT_EXT` rewrite are not byte-canonical, so the
//!    comparison is a term walk.
//! 2. **Us → oracle.** A second `erl` invocation evaluates
//!    `binary_to_term(B) =:= binary_to_term(B')` for every `(B, B')` pair and prints the
//!    booleans; all must be `true`. This proves the real `binary_to_term/1` accepts our
//!    encoder's output and yields an equal term — including for a compressed input,
//!    whose re-encoding is deliberately uncompressed.
//!
//! Identifiers (`self()`, `make_ref()`) are node-local, so the oracle's *own* bytes are
//! the reference and both sides are decoded by the oracle in the same process.
//!
//! ## Corpus
//!
//! Big ints (incl. `1 bsl 200`), a multi-byte UTF-8 atom, floats (`3.14`, `-0.0`,
//! `1.0e300`), binaries (short and >64 bytes), non-byte-aligned bitstrings, flat maps
//! of 1/2/40 pairs, lists/strings/tuples/`[]`, a reference, a pid, the fun/export tags,
//! and a large compressed term.
//!
//! The fun family is covered by both of its wire tags:
//!
//! * `EXPORT_EXT` (`113`) — three exported funs of different arities/modules
//!   (`fun lists:reverse/1`, `fun maps:remove/2`, `fun erlang:error/1`), plus
//!   occurrences **nested** in containers (a fun inside a list inside a tuple, a fun as
//!   a map value, a closure inside a list of tuples).
//! * `NEW_FUN_EXT` (`112`) — real closures with a **non-empty environment**. Note that
//!   the corpus is one `erl -eval` expression, so a `fun` literal in it is built by
//!   `erl_eval`: the compiler cannot constant-fold the body away, and the capture is
//!   `erl_eval`'s free-variable environment (a nested term tree of its own, itself good
//!   nesting coverage). The harness asserts below that the corpus really produces
//!   `112` images with `num_free > 0` — an export or zero-env fun is not evidence that
//!   the environment path works.
//! * a **real OTP literal pool** — `beam_lib:chunks(code:which(maps), [literals])`,
//!   whose literal list is `[{0,…}, {1,…}, {2, fun maps:remove/2}]`: the exact `.beam`
//!   shape (a fun nested inside a tuple inside a list) that motivated fun support.
//!
//! **Excluded:** records and the distribution header — the codec returns a typed
//! `unsupported` `BeamError` for those (never a wrong term), so they cannot be part of a
//! round-trip corpus.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_common::BeamError;

use beam_codec::term::{
    big_parts, bitstring_bytes, bitstring_size_bits, cons_head, cons_tail, external_pid_parts,
    external_port_parts, external_ref_parts, flatmap_parts, float_value, fun_arity_term,
    fun_creator_pid, fun_env, fun_function, fun_index, fun_kind_term, fun_module,
    fun_num_free_term, fun_old_index, fun_old_uniq, fun_uniq_bytes, make_fun_external,
    make_fun_imported, tuple_arity, tuple_element, FunKind, FunPid,
};
use beam_codec::{decode_term, encode_term, EXPORT_EXT, FLOAT_EXT, NEW_FUN_EXT, VERSION_MAGIC};

use beam_term::prelude::{
    atom_val, is_atom, is_list, is_nil, is_small, make_small, signed_value, AtomTable, Eterm,
};

// ---------------------------------------------------------------------------
// Oracle resolution and process plumbing
// ---------------------------------------------------------------------------

/// Whether `p` is an existing executable file.
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

/// Resolve the oracle the same way `tools/differential` does: `$ERTS_ORACLE` if it is
/// an existing executable, else `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_codec interop] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
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

/// Returns the oracle, or prints a loud skip note and returns `None`.
fn oracle_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(program) => Some(program),
        None => {
            eprintln!();
            eprintln!("==================== beam_codec interop: {test} SKIPPED ====================");
            eprintln!("No usable system `erl` oracle found — see the note above, if any");
            eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("=============================================================================");
            eprintln!();
            None
        }
    }
}

/// Run one `erl -noshell -eval <eval> -s init stop` and return its stdout.
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
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// Whether two `&[u8]` views of bitstrings are equal over their significant bits.
fn bits_equal(ba: &[u8], oa: usize, bb: &[u8], ob: usize, nbits: usize) -> bool {
    let bit = |bytes: &[u8], i: usize| (bytes[i >> 3] >> (7 - (i & 7))) & 1;
    (0..nbits).all(|i| bit(ba, oa + i) == bit(bb, ob + i))
}

/// Structural `=:=` between two terms decoded into the *same* [`AtomTable`].
///
/// Neither `Eterm::raw() ==` nor `beam_term::compare::eq` works across two separate
/// heap allocations for maps and external identifiers (their documented fallback
/// compares the raw term word, i.e. the heap address), so this walks the structure
/// directly through the safe `beam_codec::term` accessors. Atoms are compared by name,
/// floats by value bits (never `==`), bitstrings bit-by-bit. An unclassifiable term is
/// a harness bug and panics loudly rather than silently reporting inequality.
fn terms_equal(a: Eterm, b: Eterm, atoms: &AtomTable) -> bool {
    // Immediates first: they need no heap dereference.
    if is_nil(a) || is_nil(b) {
        return is_nil(a) && is_nil(b);
    }
    if is_small(a) || is_small(b) {
        return is_small(a) && is_small(b) && signed_value(a) == signed_value(b);
    }
    if is_atom(a) || is_atom(b) {
        return is_atom(a)
            && is_atom(b)
            && atoms.name(atom_val(a)) == atoms.name(atom_val(b));
    }
    if is_list(a) || is_list(b) {
        if !(is_list(a) && is_list(b)) {
            return false;
        }
        let (ha, ta) = (
            cons_head(a).expect("the head of a cons cell"),
            cons_tail(a).expect("the tail of a cons cell"),
        );
        let (hb, tb) = (
            cons_head(b).expect("the head of a cons cell"),
            cons_tail(b).expect("the tail of a cons cell"),
        );
        return terms_equal(ha, hb, atoms) && terms_equal(ta, tb, atoms);
    }

    // Boxed: probe the type-specific accessors in turn (each returns `Err` for a term
    // of the wrong kind, so the guards are exhaustive).

    // Funs come first in the boxed-family chain: a fun header carries a distinct
    // subtag, so this probe cannot be confused with any of the probes below. Funs are
    // **not** byte-canonical across the two encoders (`NEW_FUN_EXT`'s `size` field is
    // written differently by the C and is ignored on both sides), so they are compared
    // field by field — and the environment element-wise, recursively.
    let kind_a = fun_kind_term(a);
    let kind_b = fun_kind_term(b);
    if kind_a.is_ok() || kind_b.is_ok() {
        let (Ok(kind_a), Ok(kind_b)) = (kind_a, kind_b) else {
            // Exactly one side is a fun: a genuine inequality, not a harness bug.
            return false;
        };
        if kind_a != kind_b
            || fun_arity_term(a).expect("a fun's arity")
                != fun_arity_term(b).expect("a fun's arity")
        {
            return false;
        }
        let module_eq = |x: Eterm, y: Eterm| {
            atoms.name(fun_module(x).expect("the module of a fun"))
                == atoms.name(fun_module(y).expect("the module of a fun"))
        };
        match kind_a {
            // `EXPORT_EXT`: module + function (+ the arity compared above) is the
            // whole identity of the fun.
            FunKind::External => {
                return module_eq(a, b)
                    && atoms.name(fun_function(a).expect("an external fun's function"))
                        == atoms.name(fun_function(b).expect("an external fun's function"));
            }
            // `NEW_FUN_EXT`: the descriptor — module, 16-byte `uniq`, `index`,
            // `old_index`, `old_uniq`, creator pid — **and** the environment, whose
            // element `k` is the wire element `k` (stream order; never permuted here).
            FunKind::Imported => {
                let (pa, pb) = (
                    fun_creator_pid(a).expect("an imported fun's creator pid"),
                    fun_creator_pid(b).expect("an imported fun's creator pid"),
                );
                let creator_eq = atoms.name(pa.node) == atoms.name(pb.node)
                    && pa.number == pb.number
                    && pa.serial == pb.serial
                    && pa.creation == pb.creation;
                if !(module_eq(a, b)
                    && fun_uniq_bytes(a).expect("an imported fun's uniq")
                        == fun_uniq_bytes(b).expect("an imported fun's uniq")
                    && fun_index(a).expect("an imported fun's index")
                        == fun_index(b).expect("an imported fun's index")
                    && fun_old_index(a).expect("an imported fun's old_index")
                        == fun_old_index(b).expect("an imported fun's old_index")
                    && fun_old_uniq(a).expect("an imported fun's old_uniq")
                        == fun_old_uniq(b).expect("an imported fun's old_uniq")
                    && creator_eq)
                {
                    return false;
                }
                let (ea, eb) = (
                    fun_env(a).expect("an imported fun's env"),
                    fun_env(b).expect("an imported fun's env"),
                );
                return ea.len() == eb.len()
                    && ea.iter().zip(eb).all(|(x, y)| terms_equal(*x, *y, atoms));
            }
            // Unreachable from the wire: our decoder never materializes a local
            // (`make_fun3`) fun and cannot encode one, so neither side can be one.
            // Compared structurally anyway (index + environment) for totality.
            FunKind::Local => {
                return fun_index(a).expect("a local fun's index")
                    == fun_index(b).expect("a local fun's index")
                    && fun_num_free_term(a).expect("a local fun's num_free")
                        == fun_num_free_term(b).expect("a local fun's num_free")
                    && {
                        let (ea, eb) = (
                            fun_env(a).expect("a local fun's env"),
                            fun_env(b).expect("a local fun's env"),
                        );
                        ea.len() == eb.len()
                            && ea.iter().zip(eb).all(|(x, y)| terms_equal(*x, *y, atoms))
                    };
            }
        }
    }

    if let (Ok(x), Ok(y)) = (float_value(a), float_value(b)) {
        return x.to_bits() == y.to_bits();
    }
    if let (Ok((sa, da)), Ok((sb, db))) = (big_parts(a), big_parts(b)) {
        return sa == sb && da == db;
    }
    if let (Ok(na), Ok(nb)) = (bitstring_size_bits(a), bitstring_size_bits(b)) {
        if na != nb {
            return false;
        }
        let (ba, oa, _) = bitstring_bytes(a).expect("a bitstring's byte view");
        let (bb, ob, _) = bitstring_bytes(b).expect("a bitstring's byte view");
        return bits_equal(ba, oa, bb, ob, na);
    }
    if let (Ok((ka, va)), Ok((kb, vb))) = (flatmap_parts(a), flatmap_parts(b)) {
        return ka.len() == kb.len()
            && va.len() == vb.len()
            && ka.iter().zip(kb).all(|(x, y)| terms_equal(*x, *y, atoms))
            && va.iter().zip(vb).all(|(x, y)| terms_equal(*x, *y, atoms));
    }
    // A **hash map** (> 32 pairs) is a HAMT, which `flatmap_parts` rejects. Compare through
    // `map_pairs`, which sorts both sides into the same (term) key order, so the
    // comparison is independent of the flat/HAMT representation.
    if let (Ok(pa), Ok(pb)) = (
        beam_term::map::map_pairs(a, atoms),
        beam_term::map::map_pairs(b, atoms),
    ) {
        return pa.len() == pb.len()
            && pa.iter().zip(&pb).all(|((ka, va), (kb, vb))| {
                terms_equal(*ka, *kb, atoms) && terms_equal(*va, *vb, atoms)
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
    if let (Ok(na), Ok(nb)) = (tuple_arity(a), tuple_arity(b)) {
        return na == nb
            && (0..na).all(|i| {
                terms_equal(
                    tuple_element(a, i).expect("an in-range tuple element"),
                    tuple_element(b, i).expect("an in-range tuple element"),
                    atoms,
                )
            });
    }
    panic!("terms_equal: could not classify the term {a:?} / {b:?}");
}

/// A fresh decode heap (word-count generous enough for the whole corpus).
const HEAP_WORDS: usize = 1 << 16;

/// A fresh heap for decoding one corpus image.
fn heap() -> Heap {
    Heap::new(HEAP_WORDS).expect("the carrier allocator never fails for this size")
}

/// The corpus: **a single Erlang expression** evaluating to the list of reference ETF
/// images, one per term. Every element but the last is `term_to_binary(T)`; the last
/// is already an image — the compressed wrapper `term_to_binary(lists:duplicate(500,
/// hello), [compressed])` — so the `COMPRESSED` payload is exercised directly rather
/// than being wrapped in yet another `BINARY_EXT` image.
const CORPUS: &str = r#"[ term_to_binary(12345678901234567890123)
                       , term_to_binary(-12345678901234567890123)
                       , term_to_binary(1 bsl 200)
                       , term_to_binary('héllo_λ_日本語')
                       , term_to_binary(3.14)
                       , term_to_binary(-0.0)
                       , term_to_binary(1.0e300)
                       , term_to_binary(<<"hello world">>)
                       , term_to_binary(binary:copy(<<1,2,3>>, 40))
                       , term_to_binary(<<1:3>>)
                       , term_to_binary(<<5:12>>)
                       , term_to_binary(<<1:1>>)
                       , term_to_binary(#{a=>1})
                       , term_to_binary(#{a=>1, b=>2})
                       , term_to_binary(maps:from_list([{I,I} || I <- lists:seq(1,40)]))
                       , term_to_binary([1,2,3])
                       , term_to_binary("abc")
                       , term_to_binary({ok,17})
                       , term_to_binary([])
                       , term_to_binary(make_ref())
                       , term_to_binary(self())
                       , term_to_binary(fun lists:reverse/1)
                       , term_to_binary(fun maps:remove/2)
                       , term_to_binary(fun erlang:error/1)
                       , term_to_binary(begin Aa=1,Bb=2,Cc=3, fun() -> {Aa,Bb,Cc} end end)
                       , term_to_binary(begin Xx=hd(lists:seq(7,7)), fun(Zz) -> Xx + Zz end end)
                       , term_to_binary({fun lists:reverse/1, [1, 2, fun maps:remove/2]})
                       , term_to_binary(#{f => fun lists:reverse/1, g => 2})
                       , term_to_binary([{maps, fun maps:remove/2}, begin Yy=hd(lists:seq(9,9)), fun() -> [Yy] end end])
                       , begin {ok,{_,[{literals,L}]}} = beam_lib:chunks(code:which(maps),[literals]), term_to_binary(L) end
                       , term_to_binary(lists:duplicate(500, hello), [compressed]) ]"#;

/// How many entries [`CORPUS`] evaluates to — i.e. how many reference images the
/// oracle must print in direction 1.
const CORPUS_LEN: usize = 31;

/// Parse the `[<<131,104,...>>,<<...>>]` list the oracle prints into byte vectors.
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
                            .unwrap_or_else(|_| panic!("bad byte {t:?} in the oracle output {out:?}"))
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

/// Render a byte vector as an Erlang list literal, e.g. `[131,104,2]`.
fn erl_bytes(bytes: &[u8]) -> String {
    let inner = bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
}

// ---------------------------------------------------------------------------
// The differential comparison
// ---------------------------------------------------------------------------

#[test]
fn oracle_agrees_on_the_corpus_in_both_directions() {
    let Some(oracle) = oracle_or_skip("oracle_agrees_on_the_corpus_in_both_directions") else {
        return;
    };

    // --- Direction 1: oracle → us ------------------------------------------------
    // ONE erl spawn: print the list of reference images.
    let get_images = ["io:format(\"~w~n\", [ ", CORPUS, " ])"].concat();
    let images = parse_binary_list(&run_erl(&oracle, &get_images));
    assert_eq!(
        images.len(),
        CORPUS_LEN,
        "the oracle printed {} images for a {CORPUS_LEN}-entry corpus: {images:?}",
        images.len()
    );
    assert!(
        images.iter().all(|i| i.first() == Some(&VERSION_MAGIC)),
        "every oracle image must start with the version byte {VERSION_MAGIC}: {images:?}"
    );
    // The fun coverage must be real, not incidental: the corpus carries top-level
    // `EXPORT_EXT` and `NEW_FUN_EXT` images. (Nested funs — in tuples/lists/maps and
    // in the literal pool — are covered by the structural walk below.)
    let tagged = |tag: u8| images.iter().filter(|i| i.get(1) == Some(&tag)).count();
    assert!(
        tagged(EXPORT_EXT) >= 3 && tagged(NEW_FUN_EXT) >= 2,
        "the corpus must carry real EXPORT_EXT ({EXPORT_EXT}) and NEW_FUN_EXT \
         ({NEW_FUN_EXT}) images: {:?}",
        images.iter().map(|i| i.get(1).copied()).collect::<Vec<_>>()
    );
    // Exactly one corpus entry is the compressed wrapper.
    assert_eq!(
        images
            .iter()
            .filter(|i| i.get(1) == Some(&beam_codec::COMPRESSED_EXT))
            .count(),
        1,
        "the corpus must exercise exactly one COMPRESSED image"
    );

    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut byte_identical = 0usize;
    // How many images decode to a `NEW_FUN_EXT` fun with a *non-empty* environment.
    let mut funs_with_env = 0usize;
    for (i, bi) in images.iter().enumerate() {
        // One AtomTable for both decodes so atom names resolve on both sides.
        let mut atoms = AtomTable::new();
        let mut h1 = heap();
        let term1 = decode_term(bi, &mut h1, &mut atoms)
            .unwrap_or_else(|e| panic!("corpus #{i}: our decoder rejected the oracle image {bi:?}: {e:?}"));
        if matches!(fun_kind_term(term1), Ok(FunKind::Imported))
            && fun_num_free_term(term1).expect("an imported fun's num_free") > 0
        {
            funs_with_env += 1;
        }
        let bip = encode_term(term1, &atoms)
            .unwrap_or_else(|e| panic!("corpus #{i}: our encoder rejected the decoded term: {e:?}"));
        if &bip == bi {
            byte_identical += 1;
        }
        let mut h2 = heap();
        let term2 = decode_term(&bip, &mut h2, &mut atoms).unwrap_or_else(|e| {
            panic!("corpus #{i}: our own image {bip:?} did not decode back: {e:?}")
        });
        assert!(
            terms_equal(term1, term2, &atoms),
            "corpus #{i}: the re-decoded term is not structurally equal to the first decode\n\
             \x20 oracle image = {bi:?}\n\
             \x20 our image    = {bip:?}"
        );
        pairs.push((bi.clone(), bip));
    }
    assert!(
        funs_with_env >= 2,
        "only {funs_with_env} corpus image(s) decoded to a NEW_FUN_EXT fun with a \
         non-empty environment — an export or zero-env fun is not evidence that the \
         env path round-trips"
    );
    eprintln!(
        "[beam_codec interop] direction 1: {} images decoded, re-encoded and re-decoded \
         ({} of them byte-identical to the oracle image; \
         {} EXPORT_EXT / {} NEW_FUN_EXT top-level images, {} with a non-empty env)",
        pairs.len(),
        byte_identical,
        tagged(EXPORT_EXT),
        tagged(NEW_FUN_EXT),
        funs_with_env,
    );

    // --- Direction 2: us → oracle ------------------------------------------------
    // ONE erl spawn: ask the oracle whether binary_to_term/1 agrees on every pair.
    let assertions = pairs
        .iter()
        .map(|(bi, bip)| {
            format!(
                "binary_to_term(list_to_binary({})) =:= binary_to_term(list_to_binary({}))",
                erl_bytes(bi),
                erl_bytes(bip)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let check = format!("io:format(\"~w~n\", [ [ {assertions} ] ])");
    let printed = run_erl(&oracle, &check);

    let bools = parse_bool_list(printed.trim());
    assert_eq!(
        bools.len(),
        pairs.len(),
        "the oracle returned {} booleans for {} pairs: {printed:?}",
        bools.len(),
        pairs.len()
    );
    for (i, ok) in bools.iter().enumerate() {
        assert!(
            *ok,
            "corpus #{i}: the oracle reports binary_to_term(oracle) =/= binary_to_term(ours)\n\
             \x20 oracle image = {:?}\n\
             \x20 our image    = {:?}",
            pairs[i].0, pairs[i].1
        );
    }
    eprintln!(
        "[beam_codec interop] direction 2: the oracle confirmed binary_to_term/1 agreement \
         on all {} pairs",
        bools.len()
    );
}

/// Parse the `[true,true,false]` list the oracle prints into a `Vec<bool>`.
fn parse_bool_list(out: &str) -> Vec<bool> {
    let inner = out
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or_else(|| panic!("expected a `[true,...]` list, got {out:?}"));
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|t| match t.trim() {
            "true" => true,
            "false" => false,
            other => panic!("expected a boolean, got {other:?} in {out:?}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Oracle-independent checks
// ---------------------------------------------------------------------------

/// [`terms_equal`]'s fun branch must really discriminate: two funs equal in every
/// identity field are equal, any differing field (kind, module/function, arity, the
/// `NEW_FUN_EXT` descriptor, or the environment *by slot*) is not, and a fun is never
/// equal to a non-fun. The corpus above only ever compares *equal* funs, so this pins
/// the comparator itself. Oracle-independent: the funs are built with this crate's own
/// writers, so the run never depends on a system `erl`.
#[test]
fn terms_equal_discriminates_funs() {
    let mut atoms = AtomTable::new();
    let node = atoms.intern("nonode@nohost");
    let (lists, reverse, maps, remove, erlang, error) = (
        atoms.intern("lists"),
        atoms.intern("reverse"),
        atoms.intern("maps"),
        atoms.intern("remove"),
        atoms.intern("erlang"),
        atoms.intern("error"),
    );

    let mut build = heap();
    let rev1 = make_fun_external(&mut build, 1, lists, reverse).expect("an export fun");
    let rev2 = make_fun_external(&mut build, 1, lists, reverse).expect("an export fun");
    let remove2 = make_fun_external(&mut build, 2, maps, remove).expect("an export fun");
    // Same arity as `rev1`, but a different module *and* function.
    let err1 = make_fun_external(&mut build, 1, erlang, error).expect("an export fun");

    // The same MFA materialized twice is equal (two distinct heap blocks); a differing
    // arity, module or function is not.
    assert!(terms_equal(rev1, rev2, &atoms));
    assert!(!terms_equal(rev1, remove2, &atoms));
    assert!(!terms_equal(rev1, err1, &atoms));

    // A fun is never equal to a non-fun, from either side.
    let small = make_small(1);
    assert!(!terms_equal(rev1, small, &atoms));
    assert!(!terms_equal(small, rev1, &atoms));

    // `NEW_FUN_EXT`: identical descriptors, so the *only* difference is the environment
    // — compared element-wise, in wire order, never as an unordered set.
    let uniq = [7u8; 16];
    let creator = FunPid {
        node,
        number: 0,
        serial: 0,
        creation: 0,
    };
    let imported = |build: &mut Heap, env: &[Eterm]| {
        make_fun_imported(build, 1, lists, &uniq, 3, 4, 5, creator, env).expect("an imported fun")
    };
    let env12 = [make_small(1), make_small(2)];
    let env12b = [make_small(1), make_small(2)];
    let env21 = [make_small(2), make_small(1)];
    let env13 = [make_small(1), make_small(3)];
    let env1 = [make_small(1)];
    let f12 = imported(&mut build, &env12);
    let f12b = imported(&mut build, &env12b);
    let f21 = imported(&mut build, &env21);
    let f13 = imported(&mut build, &env13);
    let f1 = imported(&mut build, &env1);
    assert!(terms_equal(f12, f12b, &atoms));
    assert!(!terms_equal(f12, f21, &atoms));
    assert!(!terms_equal(f12, f13, &atoms));
    assert!(!terms_equal(f12, f1, &atoms));

    // Different kind, same module/function/arity: still unequal.
    assert!(!terms_equal(f12, rev1, &atoms));
    assert!(!terms_equal(rev1, f12, &atoms));
}

/// A `FLOAT_EXT` (`99`) is the legacy `%.20e`, NUL-terminated, 31-byte form; modern
/// `term_to_binary/1` never emits it, so it is pinned here by hand and decoded only.
#[test]
fn legacy_float_ext_is_decoded() {
    // `3.14` as the oracle's `%.20e` rendering, NUL-terminated and padded to 31 bytes.
    let text = b"3.14000000000000012434e+00";
    let mut image = vec![VERSION_MAGIC, FLOAT_EXT];
    image.extend_from_slice(text);
    image.push(0);
    image.resize(2 + 31, 0);

    let mut h = heap();
    let mut a = AtomTable::new();
    let term = decode_term(&image, &mut h, &mut a).expect("a well-formed FLOAT_EXT must decode");
    assert_eq!(
        float_value(term).expect("a float").to_bits(),
        3.14f64.to_bits()
    );

    // The canonical re-encoding is the modern `NEW_FLOAT_EXT` (`70`), not `FLOAT_EXT`.
    assert_eq!(encode_term(term, &a).expect("encodes")[1], beam_codec::NEW_FLOAT_EXT);

    // A non-numeric, NUL-padded payload is malformed, not a silent zero.
    let mut bad = vec![VERSION_MAGIC, FLOAT_EXT];
    bad.extend_from_slice(b"not a number");
    bad.resize(2 + 31, 0);
    let mut h = heap();
    let mut a = AtomTable::new();
    assert!(matches!(
        decode_term(&bad, &mut h, &mut a),
        Err(BeamError::BadArg)
    ));
}

/// Asserts malformed input is a typed `BeamError::BadArg` — never a panic, never a
/// silently accepted/mis-decoded term.
fn assert_bad_arg(image: &[u8]) {
    let mut h = heap();
    let mut a = AtomTable::new();
    match decode_term(image, &mut h, &mut a) {
        Err(BeamError::BadArg) => {}
        other => panic!("expected BadArg for {image:?}, got {other:?}"),
    }
}

/// Asserts malformed input is *some* typed [`BeamError`] (never a panic, never `Ok`).
/// Some malformed lengths are, correctly, a `SystemLimit` rather than a `BadArg`.
fn assert_typed_error(image: &[u8]) {
    let mut h = heap();
    let mut a = AtomTable::new();
    match decode_term(image, &mut h, &mut a) {
        Err(_) => {}
        Ok(term) => panic!("expected a typed BeamError for {image:?}, got Ok({term:?})"),
    }
}

#[test]
fn malformed_images_are_typed_errors_never_panics() {
    // A truncated tag: `SMALL_TUPLE_EXT` with no arity byte follows.
    assert_bad_arg(&[VERSION_MAGIC, 104]);

    // A lying length: `STRING_EXT` declares 5 bytes but only 2 are present.
    assert_bad_arg(&[VERSION_MAGIC, 107, 0, 5, 1, 2]);

    // An oversized length: `LIST_EXT` head arity far past the available bytes.
    assert_bad_arg(&[VERSION_MAGIC, 108, 0xFF, 0xFF, 0xFF, 0xFF]);

    // `BINARY_EXT` that declares 5 bytes but supplies 1.
    assert_bad_arg(&[VERSION_MAGIC, 109, 0, 0, 0, 5, b'h']);

    // Big integers whose byte count exceeds the payload (a `SMALL_BIG` short of bytes
    // is `BadArg`; a `LARGE_BIG` whose count exceeds `BIG_ARITY_MAX` is a `SystemLimit`).
    assert_bad_arg(&[VERSION_MAGIC, 110, 5, 0, 1, 2]);
    assert_typed_error(&[VERSION_MAGIC, 111, 0xFF, 0xFF, 0xFF, 0xFF]);

    // An invalid-UTF-8 atom (`ATOM_UTF8_EXT`, 2 bytes `0xFF 0xFE`).
    assert_bad_arg(&[VERSION_MAGIC, 118, 0, 2, 0xFF, 0xFE]);
    // The short variant, with the same invalid payload.
    assert_bad_arg(&[VERSION_MAGIC, 119, 2, 0xFF, 0xFE]);

    // An empty input, and a bare version byte with no term.
    assert_bad_arg(&[]);
    assert_bad_arg(&[VERSION_MAGIC]);
}

// ---------------------------------------------------------------------------
// Oracle acceptance of our `NEW_FUN_EXT` / `RECORD_EXT` images
// ---------------------------------------------------------------------------

/// A hand-written `RECORD_EXT` (`67`) image: module `m`, name `r`, not exported, with
/// the two fields declared `[(b, 22), (a, 11)]` (the wire keys are `b`, `a`). Ordinary
/// Erlang records encode as tuples, so `term_to_binary/1` never emits `67`; the oracle
/// is asked below to `binary_to_term/1` this one and confirm it accepts it.
const RECORD_IMAGE: &[u8] = &[
    131, 67, 0, 0, 0, 2, 0, 119, 1, b'm', 119, 1, b'r', 119, 1, b'b', 119, 1, b'a', 97, 22, 97, 11,
];

/// Oracle-gated acceptance check for the two tags whose images are pinned offline
/// (`tests/roundtrip.rs::oracle_eval_new_fun_image_round_trips` and
/// `tests/record_roundtrip.rs::record_ext_declaration_order_round_trips_byte_exact`):
///
/// * the oracle's own `term_to_binary(fun() -> ok end)` (`NEW_FUN_EXT`) is decoded by
///   **our** decoder and re-encoded; the oracle must read our re-encoding as the same
///   fun;
/// * our re-encoded `RECORD_EXT` image must be accepted by the oracle's
///   `binary_to_term/1` without raising.
///
/// Skipped **loudly** when no `erl` oracle is available, exactly like the corpus test —
/// never a silent pass. The two offline pins stay independent of this test.
#[test]
fn oracle_accepts_our_new_fun_and_record_images() {
    let Some(oracle) = oracle_or_skip("oracle_accepts_our_new_fun_and_record_images") else {
        return;
    };

    // ONE erl spawn: the oracle's own `NEW_FUN_EXT` image for the `erl_eval` lambda.
    let images = parse_binary_list(&run_erl(
        &oracle,
        "io:format(\"~w~n\", [ term_to_binary(fun() -> ok end) ])",
    ));
    assert_eq!(images.len(), 1, "expected exactly one image: {images:?}");
    let oracle_fun = &images[0];
    assert_eq!(oracle_fun.first(), Some(&VERSION_MAGIC));
    assert_eq!(
        oracle_fun.get(1),
        Some(&NEW_FUN_EXT),
        "the oracle's `fun() -> ok end` image must be NEW_FUN_EXT ({NEW_FUN_EXT})"
    );

    // Our decoder accepts the oracle's fun image…
    let mut h = heap();
    let mut atoms = AtomTable::new();
    let fun = decode_term(oracle_fun, &mut h, &mut atoms)
        .unwrap_or_else(|e| panic!("our decoder rejected the oracle NEW_FUN_EXT image: {e:?}"));
    assert_eq!(fun_kind_term(fun).expect("a fun"), FunKind::Imported);

    // …and our re-encoding of it is the image the oracle must read back.
    let our_fun = encode_term(fun, &atoms).expect("re-encode the decoded fun");

    // Our own `RECORD_EXT` image (the decoder and encoder are inverse on the wire, so
    // re-encoding it here reproduces the same bytes — see the offline record pin).
    let mut rec_atoms = AtomTable::new();
    rec_atoms.seed(["false", "true"]);
    rec_atoms.seed(["m", "r", "a", "b"]);
    let mut rec_heap = heap();
    let rec = decode_term(RECORD_IMAGE, &mut rec_heap, &mut rec_atoms)
        .expect("our decoder accepts the hand-written RECORD_EXT image");
    let our_record = encode_term(rec, &rec_atoms).expect("re-encode the decoded record");

    let check = format!(
        "io:format(\"~w~n\", [ [ \
            (fun() -> try binary_to_term(list_to_binary({record})), true \
                      catch _:_ -> false end end)(), \
            binary_to_term(list_to_binary({ours})) =:= binary_to_term(list_to_binary({theirs})) \
         ] ])",
        record = erl_bytes(&our_record),
        ours = erl_bytes(&our_fun),
        theirs = erl_bytes(oracle_fun),
    );
    let printed = run_erl(&oracle, &check);
    let bools = parse_bool_list(printed.trim());
    assert_eq!(
        bools.len(),
        2,
        "the oracle returned {} booleans for 2 checks: {printed:?}",
        bools.len()
    );
    assert!(
        bools[0],
        "the oracle's binary_to_term/1 rejected our RECORD_EXT image {our_record:?}"
    );
    assert!(
        bools[1],
        "the oracle read our NEW_FUN_EXT image {our_fun:?} as a different fun than its \
         own {oracle_fun:?}"
    );
    eprintln!(
        "[beam_codec interop] the oracle accepted our re-encoded NEW_FUN_EXT ({} bytes) and \
         RECORD_EXT ({} bytes) images, and read our fun as =:= its own",
        our_fun.len(),
        our_record.len()
    );
}
