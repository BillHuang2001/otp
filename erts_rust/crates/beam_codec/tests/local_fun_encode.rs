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

//! Encoding a **runtime-made local fun** (`FunKind::Local`, the `make_fun3`
//! closure built by `beam_engine`) as `NEW_FUN_EXT` (`112`).
//!
//! ## Why a local fun needs a caller-supplied identity
//!
//! A local fun's term layout inlines only its engine-internal descriptor
//! (`[uniq: u64][index: u32]`, owned by `beam_term::fun`); it stores **no** module
//! atom, no 16-byte module MD5, no `old_index`/`old_uniq` and no creator pid. The
//! C encoder (`external.c:3966-3993`) resolves those through the fun's
//! `ErlFunEntry` — the running module's lambda table — which only the runtime
//! knows. `beam_codec` therefore takes them as an **optional input**,
//! [`Encoder::with_fun_identity`] ([`FunIdentity`]); with it the local fun encodes
//! exactly like an imported one, without it the codec reports the typed
//! `unsupported_local_fun` rather than emitting a wrong image.
//!
//! ## What this file checks
//!
//! 1. [`local_fun_without_identity_is_unsupported`] — the classify-don't-lie
//!    default (pure, always runs).
//! 2. [`local_fun_with_supplied_identity_round_trips_its_fields`] — the emitted
//!    `NEW_FUN_EXT` decodes back to an imported fun carrying exactly the supplied
//!    identity, and re-encodes byte-identically (pure, always runs).
//! 3. [`compiled_local_fun_encodes_byte_exactly_like_the_oracle`] — the real
//!    evidence: a compiled fixture module's local fun is built by the system
//!    `erl`/`erlc`, its `term_to_binary` and identity fields are read back, and our
//!    encoder is asserted to reproduce the oracle's bytes **exactly**. Skips
//!    **loudly** when no `erl`/`erlc` is available (never a silent pass).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;

use beam_codec::term::{
    bitstring_bytes, cons_head, cons_tail, external_pid_parts, fun_arity_term, fun_env,
    fun_index, fun_kind_term, fun_module, fun_num_free_term, fun_old_index, fun_old_uniq,
    fun_uniq_bytes, tuple_arity, tuple_element, FunKind, FunPid,
};
use beam_codec::{
    decode_term, encode_term, is_unsupported_local_fun, Encoder, FunIdentity, NEW_FUN_EXT,
    VERSION_MAGIC,
};

use beam_term::fun::{fun_words, write_fun_local};
use beam_term::prelude::{atom_val, is_nil, make_small, signed_value, AtomTable, Eterm};

/// The fixture module compiled by `erlc` for the oracle test. `mk/1` captures one
/// free variable, `mk2/2` two, `mk0/0` none — so `num_free` 1/2/0 are all covered.
const LFUN_SRC: &str = "\
-module(lfun).
-export([mk/1, mk2/2, mk0/0]).
mk(X) -> fun(Y) -> X + Y end.
mk2(A, B) -> fun() -> {A, B} end.
mk0() -> fun() -> ok end.
";

/// The oracle program: build three local funs, then print `term_to_binary/1` of a
/// list of 9-tuples, one per fun, of
/// `{term_to_binary(F), module, new_uniq, new_index, index, uniq, arity, env, pid}`.
/// The field names/meanings are exactly `ErlFunEntry`'s `module`/`uniq`/`index`/
/// `old_index`/`old_uniq` plus the fun's arity/env and the creator pid the C
/// encoder writes (`erts_init_process_id`, surfaced by `fun_info(F, pid)`).
const ORACLE_EVAL: &str = "Fs = [lfun:mk(7), lfun:mk2(1,2), lfun:mk0()], \
Ps = [{term_to_binary(F), element(2, erlang:fun_info(F, module)), \
element(2, erlang:fun_info(F, new_uniq)), element(2, erlang:fun_info(F, new_index)), \
element(2, erlang:fun_info(F, index)), element(2, erlang:fun_info(F, uniq)), \
element(2, erlang:fun_info(F, arity)), element(2, erlang:fun_info(F, env)), \
element(2, erlang:fun_info(F, pid))} || F <- Fs], \
io:format(\"~w\", [term_to_binary(Ps)])";

// ---------------------------------------------------------------------------
// Building a local fun term (the `make_fun3` block, owned by beam_term::fun)
// ---------------------------------------------------------------------------

/// Materializes a `FunKind::Local` fun term — the layout `beam_engine::terms::make_fun`
/// builds. `descriptor_uniq` is the engine-internal `u64` identity, which is **never**
/// on the wire (the wire `uniq` comes from [`FunIdentity`]).
fn build_local_fun(
    heap: &mut Heap,
    arity: usize,
    descriptor_uniq: u64,
    index: u32,
    env: &[Eterm],
) -> Eterm {
    // SAFETY: `alloc_words` returns `fun_words(FunKind::Local, env.len())`
    // exclusively-owned, writable words and `write_fun_local` initialises all of
    // them (the invariant its `# Safety` section documents).
    unsafe {
        let hp = heap
            .alloc_words(fun_words(FunKind::Local, env.len()))
            .expect("the test heap is large enough");
        write_fun_local(hp, arity, descriptor_uniq, index, env).expect("fits the layout header")
    }
}

/// Decodes a proper list term into its elements.
fn decode_list(term: Eterm) -> Vec<Eterm> {
    let mut out = Vec::new();
    let mut cur = term;
    while !is_nil(cur) {
        out.push(cons_head(cur).expect("a proper list element"));
        cur = cons_tail(cur).expect("a proper list tail");
    }
    out
}

// ---------------------------------------------------------------------------
// 1. No identity ⇒ the typed unsupported signal (classify-don't-lie).
// ---------------------------------------------------------------------------

#[test]
fn local_fun_without_identity_is_unsupported() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64).expect("a small heap");
    let local = build_local_fun(&mut heap, 1, 0xDEAD_BEEF, 3, &[make_small(7)]);

    let err = encode_term(local, &atoms).unwrap_err();
    assert!(
        is_unsupported_local_fun(&err),
        "a local fun without a fun identity must report the precise local-fun reason, got {err:?}"
    );
    assert!(!beam_codec::is_unsupported(&err));
}

// ---------------------------------------------------------------------------
// 2. With an identity the image is a faithful NEW_FUN_EXT (pure round-trip).
// ---------------------------------------------------------------------------

#[test]
fn local_fun_with_supplied_identity_round_trips_its_fields() {
    let mut atoms = AtomTable::new();
    let module = atoms.intern("m");
    let node = atoms.intern("n1");

    let mut uniq = [0u8; 16];
    for (i, byte) in uniq.iter_mut().enumerate() {
        *byte = i as u8 + 1; // 1..=16
    }
    let creator = FunPid {
        node,
        number: 0xAAAA_BBBB,
        serial: 0xCCCC_DDDD,
        creation: 0x1122_3344,
    };
    let env = [make_small(11), make_small(22)];

    let mut heap = Heap::new(64).expect("a small heap");
    // The descriptor `uniq` is deliberately distinct from the wire `uniq` to prove it
    // never leaks into the image (a local term does not carry the module MD5).
    let local = build_local_fun(&mut heap, 2, 0xDEAD_BEEF, 0x0102_0304, &env);

    let identity = FunIdentity::new(module, uniq, 7, 3, creator);
    let mut encoder = Encoder::new(&atoms).with_fun_identity(identity);
    encoder.encode(local).unwrap();
    let bytes = encoder.as_bytes().to_vec();

    assert_eq!(bytes[0], VERSION_MAGIC);
    assert_eq!(bytes[1], NEW_FUN_EXT);

    // The image decodes to an *imported* fun (the wire has no local tag) carrying
    // exactly the identity we supplied plus the term's arity/index/num_free/env.
    let mut dheap = Heap::new(512).expect("a decode heap");
    let mut datoms = AtomTable::new();
    let decoded = decode_term(&bytes, &mut dheap, &mut datoms).expect("a valid image");
    assert_eq!(fun_kind_term(decoded).unwrap(), FunKind::Imported);
    assert_eq!(fun_arity_term(decoded).unwrap(), 2);
    assert_eq!(fun_num_free_term(decoded).unwrap(), 2);
    assert_eq!(fun_index(decoded).unwrap(), 0x0102_0304);
    assert_eq!(fun_uniq_bytes(decoded).unwrap(), uniq);
    assert_eq!(fun_old_index(decoded).unwrap(), 7);
    assert_eq!(fun_old_uniq(decoded).unwrap(), 3);
    assert_eq!(fun_env(decoded).unwrap(), &env);
    let created = beam_codec::term::fun_creator_pid(decoded).unwrap();
    assert_eq!(created.number, 0xAAAA_BBBB);
    assert_eq!(created.serial, 0xCCCC_DDDD);
    assert_eq!(created.creation, 0x1122_3344);
    // The module is the supplied one and the creator node the supplied node — compare
    // by the atom-table name, since the decode interned them in its own table.
    let module_name = datoms.name(fun_module(decoded).unwrap()).unwrap();
    assert_eq!(module_name, "m");
    let node_name = datoms.name(created.node).unwrap();
    assert_eq!(node_name, "n1");

    // A decoded NEW_FUN_EXT is self-describing, so re-encoding needs no identity and
    // must reproduce the same bytes — the image carries every supplied field.
    assert_eq!(encode_term(decoded, &datoms).unwrap(), bytes);
}

// ---------------------------------------------------------------------------
// 3. Byte-exact vs. the real oracle (skips loudly without erl/erlc).
// ---------------------------------------------------------------------------

#[test]
fn compiled_local_fun_encodes_byte_exactly_like_the_oracle() {
    let Some(oracle) = oracle_or_skip("compiled_local_fun_encodes_byte_exactly_like_the_oracle")
    else {
        return;
    };
    let Some(dir) = compile_fixture(&oracle) else {
        return;
    };

    let out = run_erl(&oracle, &dir, ORACLE_EVAL);
    let images = parse_binary_list(&out);
    assert_eq!(
        images.len(),
        1,
        "the oracle should print exactly one binary image, got {}: {out:?}",
        images.len()
    );

    let mut heap = Heap::new(8192).expect("a decode heap");
    let mut atoms = AtomTable::new();
    let payload = decode_term(&images[0], &mut heap, &mut atoms).expect("a valid payload");
    let entries = decode_list(payload);
    assert_eq!(entries.len(), 3, "one entry per fixture fun");

    for (case, entry) in entries.iter().enumerate() {
        assert_eq!(tuple_arity(*entry).unwrap(), 9);

        // (0) the oracle's own `term_to_binary(F)`.
        let image = tuple_element(*entry, 0).unwrap();
        let (img_bytes, img_off, img_bits) = bitstring_bytes(image).unwrap();
        assert_eq!(img_off, 0);
        let oracle_bytes = &img_bytes[..img_bits / 8];
        assert_eq!(oracle_bytes[0], VERSION_MAGIC, "case {case}");
        assert_eq!(
            oracle_bytes[1], NEW_FUN_EXT,
            "case {case}: a compiled local fun must encode as NEW_FUN_EXT"
        );

        // The identity fields the wire needs (all surfaced by `fun_info/2`).
        let module = atom_val(tuple_element(*entry, 1).unwrap());
        let (uniq_bytes, uniq_off, uniq_bits) =
            bitstring_bytes(tuple_element(*entry, 2).unwrap()).unwrap();
        assert_eq!((uniq_off, uniq_bits), (0, 128), "the MD5 is 16 bytes");
        let mut uniq = [0u8; 16];
        uniq.copy_from_slice(&uniq_bytes[..16]);
        let new_index = signed_value(tuple_element(*entry, 3).unwrap()) as u32;
        let old_index = signed_value(tuple_element(*entry, 4).unwrap()) as u32;
        let old_uniq = signed_value(tuple_element(*entry, 5).unwrap()) as u32;
        let arity = signed_value(tuple_element(*entry, 6).unwrap()) as usize;
        let env = decode_list(tuple_element(*entry, 7).unwrap());
        let (node_term, number, serial, creation) =
            external_pid_parts(tuple_element(*entry, 8).unwrap()).unwrap();
        let creator = FunPid {
            node: atom_val(node_term),
            number,
            serial,
            creation,
        };

        // Build our local fun with the **same** arity/index/env (the term's own
        // contributions) and the oracle's identity, then encode.
        let mut lheap = Heap::new(256).expect("a local-fun heap");
        let local = build_local_fun(&mut lheap, arity, 0xDEAD_BEEF, new_index, &env);
        let identity = FunIdentity::new(module, uniq, old_index, old_uniq, creator);
        let mut encoder = Encoder::new(&atoms).with_fun_identity(identity);
        encoder.encode(local).unwrap();

        assert_eq!(
            encoder.as_bytes(),
            oracle_bytes,
            "case {case}: our NEW_FUN_EXT image must be byte-identical to the oracle's \
             (arity={arity}, index={new_index}, num_free={})",
            env.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Oracle resolution, fixture compilation and process plumbing
// (mirrors tests/interop.rs / tests/map_key_order.rs)
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
            "[beam_codec local_fun_encode] $ERTS_ORACLE='{}' is not an existing executable — treating it as no oracle.",
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

/// Returns the oracle, or prints a loud skip note and returns `None` (never a silent pass).
fn oracle_or_skip(test: &str) -> Option<PathBuf> {
    match resolve_oracle() {
        Some(program) => Some(program),
        None => {
            eprintln!();
            eprintln!("=============== beam_codec local_fun_encode: {test} SKIPPED ===============");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH). This test is NOT a pass —");
            eprintln!("set $ERTS_ORACLE (or install Erlang/OTP) and re-run.");
            eprintln!("============================================================================");
            eprintln!();
            None
        }
    }
}

/// Resolve `erlc`: the oracle's sibling `erlc`, else `erlc` on `$PATH`.
fn resolve_erlc(oracle: &Path) -> Option<PathBuf> {
    let sibling = oracle.with_file_name("erlc");
    if is_executable(&sibling) {
        return Some(sibling);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erlc");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Compiles [`LFUN_SRC`] into `$CARGO_TARGET_TMPDIR` and returns the output dir, or
/// prints a loud skip note and returns `None` when no usable `erlc` exists.
fn compile_fixture(oracle: &Path) -> Option<PathBuf> {
    let Some(erlc) = resolve_erlc(oracle) else {
        eprintln!();
        eprintln!("=============== beam_codec local_fun_encode SKIPPED ===============");
        eprintln!("No usable system `erlc` found (no sibling of the oracle, none on");
        eprintln!("$PATH). The fixture module cannot be compiled. This is NOT a pass.");
        eprintln!("===================================================================");
        eprintln!();
        return None;
    };

    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("beam_codec_local_fun_encode");
    std::fs::create_dir_all(&dir).expect("the test temp dir is writable");
    let src = dir.join("lfun.erl");
    std::fs::write(&src, LFUN_SRC).expect("the fixture source is writable");

    let output = Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&src)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn erlc '{}': {e}", erlc.display()));
    assert!(
        output.status.success(),
        "erlc '{}' failed with {:?}\n--- stderr ---\n{}",
        erlc.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    Some(dir)
}

/// Run one `erl -noshell -pa <dir> -eval <eval> -s init stop` and return its stdout.
fn run_erl(oracle: &Path, code_dir: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-pa")
        .arg(code_dir)
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

/// Parse the `<<131,…>>` binary image(s) the oracle prints (mirrors
/// `tests/map_key_order.rs::parse_binary_list`).
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
