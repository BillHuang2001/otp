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

//! Oracle cross-check for `erlang:term_to_binary/2`'s bare `deterministic` and
//! `local` options, driven through
//! [`BifRegistry::call`](beam_bifs::BifRegistry::call) (the crate's public seam).
//!
//! **This file is FAIL-LOUD**: it panics when no system `erl` oracle can be run, so
//! it can never pass vacuously. The oracle is `$ERTS_ORACLE` when that names an
//! executable, else `erl` on `$PATH` (the same resolution `tools/differential` uses).
//!
//! What it pins, each against a **live** oracle image produced in the same VM:
//!
//! * `[deterministic]` re-sorts a **flat map** (`#{zz=>1,aa=>2}`, whose ERTS storage
//!   order is atom-index order) and a **HAMT** (`maps:from_list([{N,N*10} ||
//!   N <- lists:seq(1,40)])`, whose storage order is hash-slot order) into full Erlang
//!   term order. The term is taken from the oracle's own plain image (decoded with
//!   `binary_to_term/1`), so the two sides start from byte-identical terms.
//! * `[local]` is the typed `unsupported_bif` gap, with the **evidence** recorded here:
//!   the oracle image is `[131][121][hash:4][body]` where the body is the plain body
//!   with the node atom replaced by the `NIL_EXT` marker, and the 4-byte `hash`
//!   (`local_node_hash`, seeded from the VM's monotonic clock — `external.c:136-200`)
//!   **differs between VM runs**. It is therefore not reproducible, so fabricating
//!   four bytes would produce an image the oracle rejects.
//! * `[deterministic,local]` (either order) and an unknown option are the oracle's
//!   `badarg`.
//! * `erlang:term_to_binary/3` does **not** exist in the oracle (`undef`), so this
//!   crate registers `/1`/`/2` only.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term as codec_term;
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

/// Whether `path` is an executable file (`$ERTS_ORACLE` may point at one).
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The system `erl`: `$ERTS_ORACLE` when it names an executable, else `erl` on
/// `$PATH`. **Panics** when neither exists — this file must not pass vacuously.
fn oracle() -> PathBuf {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return program;
        }
        panic!(
            "$ERTS_ORACLE='{}' is not an executable file",
            program.display()
        );
    }
    let path = std::env::var_os("PATH")
        .expect("PATH is unset, so no `erl` oracle can be found; set $ERTS_ORACLE");
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return candidate;
        }
    }
    panic!("no `erl` oracle found: $ERTS_ORACLE is unset and `erl` is not on $PATH");
}

/// Runs `erl -noshell -eval <statement>` and returns stdout. Panics on a spawn
/// failure or a non-zero exit, so a broken oracle can never look like a pass.
fn run_erl(statement: &str) -> String {
    let program = oracle();
    let output = Command::new(&program)
        .arg("-noshell")
        .arg("-eval")
        .arg(statement)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", program.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{statement}",
        program.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// The `[131,116,…]` byte list `io:format("~w", [binary_to_list(B)])` prints.
fn parse_byte_list(text: &str) -> Vec<u8> {
    let inner = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or_else(|| panic!("expected a byte list, got {text:?}"));
    if inner.is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|item| {
            item.trim()
                .parse::<u8>()
                .unwrap_or_else(|e| panic!("bad byte {:?} in {text:?}: {e}", item.trim()))
        })
        .collect()
}

/// The oracle's `(plain, [deterministic])` images of `T = <term_expr>`, from **one**
/// VM (so the two share an atom table and a map storage order).
fn oracle_deterministic_images(term_expr: &str) -> (Vec<u8>, Vec<u8>) {
    let statement = format!(
        "T = {term_expr}, \
         io:format(\"~w ~w~n\", \
                   [binary_to_list(erlang:term_to_binary(T)), \
                    binary_to_list(erlang:term_to_binary(T, [deterministic]))]), \
         halt()."
    );
    let out = run_erl(&statement);
    let mut parts = out.split_whitespace();
    let plain = parse_byte_list(parts.next().unwrap_or_else(|| panic!("no plain image in {out:?}")));
    let deterministic = parse_byte_list(
        parts
            .next()
            .unwrap_or_else(|| panic!("no deterministic image in {out:?}")),
    );
    (plain, deterministic)
}

/// The oracle's `[local]` image of `T = <term_expr>`.
fn oracle_local_image(term_expr: &str) -> Vec<u8> {
    let statement = format!(
        "T = {term_expr}, \
         io:format(\"~w~n\", [binary_to_list(erlang:term_to_binary(T, [local]))]), \
         halt()."
    );
    parse_byte_list(run_erl(&statement).trim())
}

/// The oracle's `(plain, [local])` images of `T = <term_expr>` from **one** VM.
fn oracle_plain_and_local(term_expr: &str) -> (Vec<u8>, Vec<u8>) {
    let statement = format!(
        "T = {term_expr}, \
         io:format(\"~w ~w~n\", \
                   [binary_to_list(erlang:term_to_binary(T)), \
                    binary_to_list(erlang:term_to_binary(T, [local]))]), \
         halt()."
    );
    let out = run_erl(&statement);
    let mut parts = out.split_whitespace();
    let plain = parse_byte_list(parts.next().unwrap_or_else(|| panic!("no plain image in {out:?}")));
    let local = parse_byte_list(parts.next().unwrap_or_else(|| panic!("no local image in {out:?}")));
    (plain, local)
}

// ---------------------------------------------------------------------------
// This crate's side
// ---------------------------------------------------------------------------

/// The names every test table is seeded with. `zz` precedes `aa` so their **atom-index**
/// order is the reverse of their **name** order (what `[deterministic]` sorts by), and
/// the option atoms must be present for the option parser to recognise them. The
/// decoded terms' atoms (`zz`/`aa`) must be present too: no `AtomInterner` is installed.
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "compressed",
        "minor_version",
        "deterministic",
        "local",
        "iovec",
        "fragment",
        "bogus",
        "zz",
        "aa",
    ]);
    atoms
}

fn heap() -> Heap {
    Heap::new(64 * 1024).expect("test heap")
}

/// Calls the registered BIF `module:name/arity`.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    name: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    reg.call(module, name, arity, ctx, args)
}

/// A binary term holding `bytes`.
fn bin(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    codec_term::make_binary(ctx.heap(), bytes).expect("the image fits")
}

/// The bytes of a byte-aligned binary term.
fn bytes_of(term: Eterm) -> Vec<u8> {
    let (bytes, offset, size_bits) = codec_term::bitstring_bytes(term).expect("a binary");
    assert_eq!(offset, 0, "the result is a byte-aligned binary");
    assert_eq!(size_bits % 8, 0, "the result is a whole number of bytes");
    bytes[..size_bits / 8].to_vec()
}

/// `binary_to_term(Image)` through the registry.
fn decode(reg: &BifRegistry, ctx: &mut BifContext<'_>, image: &[u8]) -> Eterm {
    let input = bin(ctx, image);
    call(reg, ctx, "erlang", "binary_to_term", 1, &[input]).expect("the oracle image decodes")
}

/// `term_to_binary(Term, Options)` through the registry, as bytes.
fn image_with(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    term: Eterm,
    options: Eterm,
) -> Vec<u8> {
    bytes_of(call(reg, ctx, "erlang", "term_to_binary", 2, &[term, options]).expect("encodable"))
}

/// The proper option list the small integers/atoms of `names` name.
fn option_atoms(ctx: &mut BifContext<'_>, names: &[&str]) -> Eterm {
    let items: Vec<Eterm> = names
        .iter()
        .map(|name| ctx.atom(name).expect("seeded"))
        .collect();
    ctx.list(&items).expect("the option list fits")
}

// ---------------------------------------------------------------------------
// `[deterministic]`
// ---------------------------------------------------------------------------

#[test]
fn deterministic_resorts_a_flat_map_like_the_oracle() {
    // The flat map's ERTS storage order is `erts_cmp_flatmap_keys` (atom-index) order,
    // which for `zz`/`aa` is the reverse of the `[deterministic]` (atom-name) order.
    let (plain, deterministic) = oracle_deterministic_images("#{zz=>1,aa=>2}");
    assert_ne!(
        plain, deterministic,
        "the oracle's own two images must differ, else this case proves nothing"
    );

    let reg = default_registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let term = decode(&reg, &mut ctx, &plain);
    let options = option_atoms(&mut ctx, &["deterministic"]);
    let ours = image_with(&reg, &mut ctx, term, options);
    assert_eq!(
        ours, deterministic,
        "our [deterministic] flat-map image must be byte-identical to the oracle's"
    );

    // Control: without the option our image keeps the *stored* order — so the equality
    // above is the re-sort, not a coincidence of the decoder.
    let term = decode(&reg, &mut ctx, &plain);
    let plain_ours = bytes_of(call(&reg, &mut ctx, "erlang", "term_to_binary", 1, &[term]).unwrap());
    assert_ne!(
        plain_ours, deterministic,
        "the plain image is not the deterministic one (the flag really reordered)"
    );
}

#[test]
fn deterministic_resorts_a_hash_map_like_the_oracle() {
    // 40 distinct keys is past `MAP_SMALL_MAP_LIMIT`, so the map is a HAMT whose stored
    // order is the hash-slot walk — not the ascending key order `[deterministic]` gives.
    let (plain, deterministic) =
        oracle_deterministic_images("maps:from_list([{N, N*10} || N <- lists:seq(1, 40)])");
    assert_ne!(plain, deterministic, "the oracle's two HAMT images must differ");

    let reg = default_registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let term = decode(&reg, &mut ctx, &plain);
    let options = option_atoms(&mut ctx, &["deterministic"]);
    let ours = image_with(&reg, &mut ctx, term, options);
    assert_eq!(
        ours, deterministic,
        "our [deterministic] HAMT image must be byte-identical to the oracle's"
    );

    let term = decode(&reg, &mut ctx, &plain);
    let plain_ours = bytes_of(call(&reg, &mut ctx, "erlang", "term_to_binary", 1, &[term]).unwrap());
    assert_ne!(plain_ours, deterministic, "the HAMT re-sort really happened");
}

#[test]
fn deterministic_matches_the_oracle_for_ordinary_terms() {
    // A term with no map is unaffected by the flag — including the `compressed` and
    // `{minor_version, N}` interplay, which must stay byte-identical.
    for term_expr in ["x", "[1,2,3]", "{1,2.5,<<1,2>>}", "lists:duplicate(40, hello)"] {
        let (plain, deterministic) = oracle_deterministic_images(term_expr);
        assert_eq!(
            plain, deterministic,
            "no map ⇒ [deterministic] cannot change {term_expr}"
        );

        let reg = default_registry();
        let mut heap = heap();
        let mut atoms = atoms();
        atoms.seed(["x", "hello", "nonode@nohost"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let term = decode(&reg, &mut ctx, &plain);
        let options = option_atoms(&mut ctx, &["deterministic"]);
        let ours = image_with(&reg, &mut ctx, term, options);
        assert_eq!(ours, deterministic, "our image of {term_expr}");
    }
}

// ---------------------------------------------------------------------------
// `[local]` — the typed gap, with the evidence that makes it honest
// ---------------------------------------------------------------------------

/// Asserts the oracle's `[local]` image of an **internal** pid/port/ref is the plain
/// image behind a `LOCAL_EXT` (121) wrapper, with the node atom replaced by the
/// `NIL_EXT` (106) marker:
/// `plain_body = [prefix][SMALL_ATOM_UTF8(119)][len][node][suffix]`
/// `local_body = [prefix][NIL(106)][suffix]`.
///
/// The prefix carries the id tag — and, for `NEWER_REFERENCE_EXT` (90), its 2-byte
/// length field — so its width is tag-dependent:
/// `[88][node][number][serial][creation]` (pid), `[120][node][id:8][creation]` (port),
/// `[90][len:2][node][creation][3 × number]` (ref).
fn assert_local_wraps_the_plain_id(plain: &[u8], local: &[u8]) {
    assert_eq!(local[0], 131, "VERSION_MAGIC");
    assert_eq!(local[1], 121, "LOCAL_EXT");
    assert_eq!(plain[0], 131, "VERSION_MAGIC");
    // `NEW_PID_EXT` (88) / `V4_PORT_EXT` (120) / `NEWER_REFERENCE_EXT` (90).
    let tag = plain[1];
    let node_ix = match tag {
        88 | 120 => 2,
        90 => 4, // tag + the 2-byte `Len` field
        other => panic!("unexpected internal-id tag {other}"),
    };
    assert_eq!(plain[node_ix], 119, "SMALL_ATOM_UTF8_EXT — the node atom");
    let name_len = plain[node_ix + 1] as usize;
    assert_eq!(
        &plain[node_ix + 2..node_ix + 2 + name_len],
        b"nonode@nohost",
        "the plain node atom names the local node"
    );
    let node_len = 2 + name_len; // the tag byte + the length byte + the name
    // `local[6..]` is the *body* (the wrapper precedes it), so the plain image's own
    // `VERSION_MAGIC` (plain[0]) is skipped.
    let mut expected = plain[1..node_ix].to_vec();
    expected.push(106); // NIL_EXT — the internal-node marker
    expected.extend_from_slice(&plain[node_ix + node_len..]);
    assert_eq!(
        &local[6..],
        &expected[..],
        "the [local] body is the plain body with the node atom replaced by NIL_EXT"
    );
}
#[test]
fn local_wraps_a_pid_port_and_ref_with_a_non_reproducible_hash() {
    // A local pid, a local port and a local ref: each `[local]` image is the plain one
    // behind `[131][121][hash:4]` with the node atom swapped for `NIL_EXT`.
    for term_expr in ["self()", "hd(erlang:ports())", "make_ref()"] {
        let (plain, local) = oracle_plain_and_local(term_expr);
        assert_local_wraps_the_plain_id(&plain, &local);
    }

    // The wrapper's 4 payload bytes are `local_node_hash` — a block hash seeded from the
    // VM's monotonic clock (`external.c:136-200`) — so they differ between VM runs.
    // Four runs: if the bytes were a function of the term alone, every run would agree.
    let hashes: Vec<[u8; 4]> = (0..4)
        .map(|_| {
            let image = oracle_local_image("[1,2]");
            [image[2], image[3], image[4], image[5]]
        })
        .collect();
    assert!(
        hashes.windows(2).any(|pair| pair[0] != pair[1]),
        "the LOCAL_EXT hash must differ between VM runs, but every run gave {hashes:?}"
    );
    // …and the rest of the image is exactly the plain body (a `LIST_EXT` of `[1,2]`).
    let local = oracle_local_image("[1,2]");
    assert_eq!(local, [131, 121, local[2], local[3], local[4], local[5], 107, 0, 2, 1, 2]);
}

#[test]
fn local_is_the_typed_gap_and_never_a_fabricated_hash() {
    // The oracle answers; we must answer the typed `unsupported_bif` (the argument is
    // valid, the *capability* is missing) — never an image with invented hash bytes.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let two = ctx.list(&[ctx.int(1).unwrap(), ctx.int(2).unwrap()]).unwrap();
    let local = option_atoms(&mut ctx, &["local"]);
    let err = call(&reg, &mut ctx, "erlang", "term_to_binary", 2, &[two, local]).unwrap_err();
    assert!(
        matches!(err, BeamError::Raise(_)),
        "expected the typed unsupported_bif raise, got {err:?}"
    );

    // The same for an internal id: the option is refused before the id is reached.
    let pid = ctx.self_pid().to_eterm();
    let local = option_atoms(&mut ctx, &["local"]);
    let err = call(&reg, &mut ctx, "erlang", "term_to_binary", 2, &[pid, local]).unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)), "local + pid, got {err:?}");
}

// ---------------------------------------------------------------------------
// The C's rejections, and the absent `/3`
// ---------------------------------------------------------------------------

#[test]
fn local_with_deterministic_and_unknown_options_are_badarg() {
    let reg = default_registry();
    let mut heap = heap();
    let atoms = atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // The oracle: `term_to_binary(T, [deterministic,local])` and `[local,deterministic]`
    // are both `badarg` (`external.c:1543-1545`) — order is irrelevant.
    for statement in [
        "erlang:term_to_binary([1,2], [deterministic,local])",
        "erlang:term_to_binary([1,2], [local,deterministic])",
        "erlang:term_to_binary([1,2], [bogus])",
        "erlang:term_to_binary([1,2], [iovec])",
        "erlang:term_to_binary([1,2], [fragment])",
    ] {
        let out = run_erl(&format!(
            "io:format(\"~w~n\", [catch {statement}]), halt()."
        ));
        assert!(
            out.contains("badarg"),
            "the oracle must reject {statement}, got {out:?}"
        );
    }

    let two = ctx.list(&[ctx.int(1).unwrap(), ctx.int(2).unwrap()]).unwrap();
    for names in [
        &["deterministic", "local"][..],
        &["local", "deterministic"][..],
        &["bogus"][..],
        &["iovec"][..],
        &["fragment"][..],
    ] {
        let options = option_atoms(&mut ctx, names);
        let err = call(&reg, &mut ctx, "erlang", "term_to_binary", 2, &[two, options])
            .unwrap_err();
        assert!(
            matches!(err, BeamError::BadArg),
            "expected badarg for {names:?}, got {err:?}"
        );
    }
}

#[test]
fn term_to_binary_3_does_not_exist() {
    // ORACLE: `erlang:term_to_binary(x, [], 1)` is `undef` — the oracle's error tuple
    // names `{erlang,term_to_binary,[x,[],1],[]}`, which is exactly what an undefined
    // function produces. So `term_to_binary/2` is the whole surface, and this crate
    // registers `/1`/`/2` only (never inventing an arity the oracle answers `undef`).
    let out = run_erl("io:format(\"~w~n\", [catch erlang:term_to_binary(x, [], 1)]), halt().");
    assert!(
        out.contains("undef"),
        "the oracle must report `undef` for term_to_binary/3, got {out:?}"
    );
    assert!(
        out.contains("term_to_binary") && out.contains("undef"),
        "the undef error must name the function, got {out:?}"
    );

    let reg = default_registry();
    assert!(
        !reg.contains("erlang", "term_to_binary", 3),
        "term_to_binary/3 must not be registered"
    );
    assert!(reg.contains("erlang", "term_to_binary", 1));
    assert!(reg.contains("erlang", "term_to_binary", 2));
}
