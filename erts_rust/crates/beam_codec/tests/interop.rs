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
//! of 1/2/40 pairs, lists/strings/tuples/`[]`, a reference, a pid, and a large
//! compressed term. **Excluded:** records and the distribution header — the codec
//! returns a typed `unsupported` `BeamError` for those (never a wrong term), so they
//! cannot be part of a round-trip corpus. Funs and exports are supported by the codec
//! (`NEW_FUN_EXT`/`EXPORT_EXT`); their interop corpus entries land in a later pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_common::BeamError;

use beam_codec::term::{
    big_parts, bitstring_bytes, bitstring_size_bits, cons_head, cons_tail, external_pid_parts,
    external_port_parts, external_ref_parts, flatmap_parts, float_value, tuple_arity, tuple_element,
};
use beam_codec::{decode_term, encode_term, FLOAT_EXT, VERSION_MAGIC};

use beam_term::prelude::{
    atom_val, is_atom, is_list, is_nil, is_small, signed_value, AtomTable, Eterm,
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
                       , term_to_binary(lists:duplicate(500, hello), [compressed]) ]"#;

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
    assert!(
        images.len() >= 21,
        "the oracle printed too few images ({}): {images:?}",
        images.len()
    );
    assert!(
        images.iter().all(|i| i.first() == Some(&VERSION_MAGIC)),
        "every oracle image must start with the version byte {VERSION_MAGIC}: {images:?}"
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
    for (i, bi) in images.iter().enumerate() {
        // One AtomTable for both decodes so atom names resolve on both sides.
        let mut atoms = AtomTable::new();
        let mut h1 = heap();
        let term1 = decode_term(bi, &mut h1, &mut atoms)
            .unwrap_or_else(|e| panic!("corpus #{i}: our decoder rejected the oracle image {bi:?}: {e:?}"));
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
    eprintln!(
        "[beam_codec interop] direction 1: {} images decoded, re-encoded and re-decoded \
         ({byte_identical} of them byte-identical to the oracle image)",
        pairs.len()
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
