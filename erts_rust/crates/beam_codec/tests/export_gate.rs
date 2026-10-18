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

//! The **active-export gate** — the `binary_to_term/2 [safe]` rule C applies to
//! `EXPORT_EXT` (`113`):
//!
//! ```c
//! if (flags & ERTS_DIST_EXT_BTT_SAFE) {
//!     if (!erts_active_export_entry(mod, name, arity)) goto error;
//! }
//! ```
//!
//! (`../../erts/emulator/beam/external.c:5032-5036`; `erts_active_export_entry` is
//! `erts_find_export_entry(m, f, a, erts_active_code_ix())`, `export.h:176-183`.)
//! The registry is the *host's*, so the codec exposes the **check site** as an
//! optional seam ([`ExportGate`]) and these tests drive it with a mock gate:
//!
//! * under `[safe]` + a gate, an `EXPORT_EXT` naming an active export decodes and a
//!   non-active one is `BadArg`;
//! * with `[safe]` off the gate is never consulted, and with no gate installed the
//!   check is skipped — the deliberate no-gate fallback that keeps every pre-seam
//!   caller byte-for-byte unchanged;
//! * the gate is asked with the exact module/function names and the
//!   **32-bit-truncated** arity (`2^32 + 7` ⇒ `7`, the C's `unsigned int` parameter);
//! * `NEW_FUN_EXT` (`112`) and every non-`safe` decode never consult it;
//! * a `COMPRESSED` (`80`) image cannot route around the check — the two gate-aware
//!   free functions install the gate on the inflated path too;
//! * one test re-checks the four accept/reject shapes against the system `erl`
//!   (`binary_to_term/2` with `[safe]`), **skipping loudly** when no oracle exists.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_codec::term::{fun_function, fun_kind_term, fun_module, fun_true_arity_term, FunKind};
use beam_codec::term::tuple_element;
use beam_codec::{
    decode_term_with_options, decode_term_with_options_and_gate,
    decode_term_with_options_and_gate_used, is_compressed_image, DecodeOptions, Decoder,
    ExportGate, COMPRESSED_EXT, EXPORT_EXT, NEW_FUN_EXT, NEW_PID_EXT, SMALL_ATOM_UTF8_EXT,
    VERSION_MAGIC,
};
use beam_common::BeamError;
use beam_term::{AtomTable, Eterm};

// ---------------------------------------------------------------------------
// The mock gate
// ---------------------------------------------------------------------------

/// A gate over a fixed `(module, function, arity)` set that records every question it
/// is asked — the counter proves *who* consults it (only a `[safe]` `EXPORT_EXT`).
#[derive(Default)]
struct MockGate {
    active: HashSet<(String, String, u32)>,
    calls: Cell<usize>,
    seen: RefCell<Vec<(String, String, u32)>>,
}

impl MockGate {
    /// A gate that reports exactly `entries` as active.
    fn new(entries: &[(&str, &str, u32)]) -> MockGate {
        MockGate {
            active: entries
                .iter()
                .map(|(m, f, a)| ((*m).to_string(), (*f).to_string(), *a))
                .collect(),
            calls: Cell::new(0),
            seen: RefCell::new(Vec::new()),
        }
    }

    /// A gate that reports nothing active.
    fn empty() -> MockGate {
        MockGate::new(&[])
    }

    fn calls(&self) -> usize {
        self.calls.get()
    }

    fn seen(&self) -> Vec<(String, String, u32)> {
        self.seen.borrow().clone()
    }
}

impl ExportGate for MockGate {
    fn is_export_active(&self, module: &str, function: &str, arity: u32) -> bool {
        self.calls.set(self.calls.get() + 1);
        self.seen
            .borrow_mut()
            .push((module.to_string(), function.to_string(), arity));
        self.active
            .contains(&(module.to_string(), function.to_string(), arity))
    }
}

// ---------------------------------------------------------------------------
// Wire images
// ---------------------------------------------------------------------------

/// The arity term as the C's `term_to_binary/1` writes it: `SMALL_INTEGER_EXT` below
/// 256, `INTEGER_EXT` when it fits a signed 32-bit int, else `SMALL_BIG_EXT` with the
/// significant little-endian magnitude bytes (at least one).
fn arity_term(value: u64) -> Vec<u8> {
    if value < 256 {
        return vec![97, value as u8];
    }
    if value <= 0x7FFF_FFFF {
        let mut out = vec![98]; // INTEGER_EXT
        out.extend_from_slice(&(value as u32).to_be_bytes());
        return out;
    }
    let mut magnitude = value.to_le_bytes().to_vec();
    while magnitude.len() > 1 && magnitude[magnitude.len() - 1] == 0 {
        magnitude.pop();
    }
    let mut out = vec![110, magnitude.len() as u8, 0]; // SMALL_BIG_EXT, sign 0
    out.extend_from_slice(&magnitude);
    out
}

/// `SMALL_ATOM_UTF8_EXT` for a name that fits the one-byte length field.
fn atom(name: &str) -> Vec<u8> {
    let mut out = vec![SMALL_ATOM_UTF8_EXT, name.len() as u8];
    out.extend_from_slice(name.as_bytes());
    out
}

/// `EXPORT_EXT` (`113`): `[113][module atom][function atom][arity term]`.
fn export_image(module: &str, function: &str, arity: u64) -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, EXPORT_EXT];
    image.extend_from_slice(&atom(module));
    image.extend_from_slice(&atom(function));
    image.extend_from_slice(&arity_term(arity));
    image
}

/// A valid `NEW_FUN_EXT` (`112`) with no free variables — the tag the C does **not**
/// gate.
///
/// `[112][size:4][arity:1][uniq:16][index:4][num_free:4][module atom][old_index]
/// [old_uniq][creator pid][env…]` (`external.c:5102-5176`).
fn new_fun_image() -> Vec<u8> {
    let mut image = vec![VERSION_MAGIC, NEW_FUN_EXT];
    image.extend_from_slice(&0u32.to_be_bytes()); // size (read and ignored)
    image.push(0); // arity
    image.extend_from_slice(&[0u8; 16]); // uniq (MD5)
    image.extend_from_slice(&0u32.to_be_bytes()); // index
    image.extend_from_slice(&0u32.to_be_bytes()); // num_free
    image.extend_from_slice(&atom("m"));
    image.extend_from_slice(&[97, 1]); // old_index
    image.extend_from_slice(&[97, 2]); // old_uniq
    image.push(NEW_PID_EXT); // creator pid
    image.extend_from_slice(&atom("n1"));
    image.extend_from_slice(&[0u8; 12]); // number, serial, creation
    image
}

/// The atom names a live node already has interned for these fixtures. Under `[safe]` a
/// wire atom **name** is resolved by lookup only (`erts_atom_get`), so a fixture table
/// must contain them — exactly as a running node's table does (this is the `dec_atom`
/// half of the flag, independent of the export registry).
const FIXTURE_ATOMS: [&str; 6] = ["lists", "map", "erlang", "system_info", "m", "n1"];

/// Decodes `image` with `options` **and** `gate`, returning the heap and atom table the
/// term points into (it borrows their storage).
#[allow(clippy::type_complexity)]
fn decode_gated(
    image: &[u8],
    options: &DecodeOptions,
    gate: &MockGate,
) -> Result<(Heap, AtomTable, Eterm), BeamError> {
    decode_gated_in(image, options, gate, &FIXTURE_ATOMS)
}

/// Like [`decode_gated`], but with a caller-chosen set of pre-interned atom names.
#[allow(clippy::type_complexity)]
fn decode_gated_in(
    image: &[u8],
    options: &DecodeOptions,
    gate: &MockGate,
    seeded: &[&str],
) -> Result<(Heap, AtomTable, Eterm), BeamError> {
    let mut heap = Heap::new(256).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    atoms.seed(seeded.iter().copied());
    let term = decode_term_with_options_and_gate(image, &mut heap, &mut atoms, options, gate)?;
    Ok((heap, atoms, term))
}

/// The same through the `[used]` twin.
#[allow(clippy::type_complexity)]
fn decode_gated_used(
    image: &[u8],
    options: &DecodeOptions,
    gate: &MockGate,
) -> Result<(Heap, AtomTable, Eterm, usize), BeamError> {
    let mut heap = Heap::new(256).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    atoms.seed(FIXTURE_ATOMS.iter().copied());
    let (term, used) =
        decode_term_with_options_and_gate_used(image, &mut heap, &mut atoms, options, gate)?;
    Ok((heap, atoms, term, used))
}

/// The `[safe]` option set (the other two fields are the identity seam, unused here).
fn safe_options() -> DecodeOptions {
    DecodeOptions {
        safe: true,
        local_node: None,
        local_hash: None,
    }
}

/// The all-unset option set.
fn plain_options() -> DecodeOptions {
    DecodeOptions::default()
}

fn is_bad_arg<T>(result: Result<T, BeamError>) -> bool {
    matches!(result, Err(BeamError::BadArg))
}

/// The `{module, function, true arity}` view of a decoded `EXPORT_EXT` fun.
fn fun_view(atoms: &AtomTable, term: Eterm) -> (String, String, usize) {
    assert_eq!(
        fun_kind_term(term).expect("a fun"),
        FunKind::External,
        "an `EXPORT_EXT` decodes to an external fun"
    );
    let module = atoms
        .name(fun_module(term).expect("a module atom"))
        .expect("the module name")
        .to_string();
    let function = atoms
        .name(fun_function(term).expect("a function atom"))
        .expect("the function name")
        .to_string();
    (module, function, fun_true_arity_term(term).expect("a true arity"))
}

// ---------------------------------------------------------------------------
// `[safe]` + gate
// ---------------------------------------------------------------------------

#[test]
fn safe_with_an_active_export_decodes() {
    let gate = MockGate::new(&[("lists", "map", 2)]);
    let image = export_image("lists", "map", 2);
    let (_h, atoms, term) = decode_gated(&image, &safe_options(), &gate).expect("active export");
    assert_eq!(
        fun_view(&atoms, term),
        ("lists".to_string(), "map".to_string(), 2)
    );
    assert_eq!(gate.seen(), vec![("lists".to_string(), "map".to_string(), 2)]);
}

#[test]
fn safe_with_a_non_active_export_is_bad_arg() {
    // `lists:map/3` is not exported by `lists`, so C's `erts_active_export_entry`
    // fails and the decode is `badarg`.
    let gate = MockGate::empty();
    let image = export_image("lists", "map", 3);
    assert!(is_bad_arg(decode_gated(&image, &safe_options(), &gate)));
    assert_eq!(gate.calls(), 1, "the gate is asked exactly once");

    // An unknown module is refused the same way (the gate answers `false`).
    let gate = MockGate::new(&[("lists", "map", 2)]);
    assert!(is_bad_arg(decode_gated(
        &export_image("no_such_module_xyz", "map", 2),
        &safe_options(),
        &gate
    )));
}

#[test]
fn a_non_active_export_still_decodes_without_the_safe_flag() {
    let gate = MockGate::empty();
    let image = export_image("lists", "map", 3);
    let (_h, atoms, term) = decode_gated(&image, &plain_options(), &gate)
        .expect("without [safe] the export registry is never asked");
    assert_eq!(
        fun_view(&atoms, term),
        ("lists".to_string(), "map".to_string(), 3)
    );
    assert_eq!(gate.calls(), 0, "the C only asks under ERTS_DIST_EXT_BTT_SAFE");
}

#[test]
fn safe_without_a_gate_is_the_deliberate_no_gate_fallback() {
    // No gate installed: the `[safe]` decode keeps its pre-seam behaviour — the two name
    // atoms are still resolved by **lookup** (that half is the codec's), but the export's
    // activity is not checked, because the registry is the host's. This is a *documented*
    // fallback, not a silent pass: a host that wants the C's refusal installs a gate.
    let image = export_image("lists", "map", 3);
    let mut heap = Heap::new(256).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    atoms.seed(FIXTURE_ATOMS.iter().copied());
    let term = decode_term_with_options(&image, &mut heap, &mut atoms, &safe_options())
        .expect("no gate installed ⇒ no active-export check");
    assert_eq!(
        atoms.name(fun_module(term).expect("a module atom")),
        Some("lists")
    );
    assert_eq!(fun_true_arity_term(term).expect("a true arity"), 3);

    // The builder route behaves identically.
    let mut heap = Heap::new(256).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    atoms.seed(FIXTURE_ATOMS.iter().copied());
    let mut decoder = Decoder::new(&image).expect("well-formed").with_safe();
    let term = decoder.decode(&mut heap, &mut atoms).expect("decodes");
    assert_eq!(fun_true_arity_term(term).unwrap(), 3);
}

#[test]
fn the_builder_route_installs_the_gate() {
    // `Decoder::with_export_gate` — the borrowed-decoder form a caller uses when no
    // `COMPRESSED` wrapper is involved.
    let image = export_image("lists", "map", 2);

    let mut heap = Heap::new(256).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    atoms.seed(FIXTURE_ATOMS.iter().copied());
    let gate = MockGate::new(&[("lists", "map", 2)]);
    let mut decoder = Decoder::new(&image)
        .expect("well-formed")
        .with_safe()
        .with_export_gate(&gate);
    assert!(decoder.decode(&mut heap, &mut atoms).is_ok());
    assert_eq!(gate.calls(), 1);

    let mut heap = Heap::new(256).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    atoms.seed(FIXTURE_ATOMS.iter().copied());
    let gate = MockGate::empty();
    let mut decoder = Decoder::new(&image)
        .expect("well-formed")
        .with_safe()
        .with_export_gate(&gate);
    assert!(matches!(
        decoder.decode(&mut heap, &mut atoms),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// The question the gate is asked
// ---------------------------------------------------------------------------

#[test]
fn the_gate_is_asked_with_the_32_bit_truncated_arity() {
    // The C passes the arity to `erts_active_export_entry(Eterm, Eterm, unsigned)`, so
    // only the low 32 bits survive (`external.c:5028-5036`). A wire arity of `2^32 + 7`
    // is therefore `lists:map/7` — verified against the oracle in
    // `the_oracle_agrees_on_the_safe_active_export_rule`.
    let gate = MockGate::new(&[("lists", "map", 7)]);
    let image = export_image("lists", "map", (1 << 32) + 7);
    let (_h, atoms, term) = decode_gated(&image, &safe_options(), &gate).expect("map/7 is active");
    assert_eq!(gate.seen(), vec![("lists".to_string(), "map".to_string(), 7)]);
    assert_eq!(
        fun_view(&atoms, term),
        ("lists".to_string(), "map".to_string(), 7)
    );

    // `2^32 + 2` truncates to 2: active iff `lists:map/2` is.
    let gate = MockGate::new(&[("lists", "map", 2)]);
    assert!(decode_gated(&export_image("lists", "map", (1 << 32) + 2), &safe_options(), &gate)
        .is_ok());
    assert_eq!(gate.seen(), vec![("lists".to_string(), "map".to_string(), 2)]);

    // ... and a wire arity of 2 alone is the very same question.
    let gate = MockGate::new(&[("lists", "map", 2)]);
    let image = export_image("lists", "map", 2);
    let (_h, _a, _t) = decode_gated(&image, &safe_options(), &gate).expect("active");
    assert_eq!(
        gate.seen(),
        vec![("lists".to_string(), "map".to_string(), 2)],
        "the truncated arity is the only arity the gate ever sees"
    );
}

#[test]
fn new_fun_ext_never_consults_the_gate() {
    // `NEW_FUN_EXT` has no active-export rule in the C (`external.c:5102-5176`), even
    // under `[safe]` — only its module atom is looked up (the `dec_atom`/`safe` half).
    let gate = MockGate::empty();
    let image = new_fun_image();
    let options = safe_options();
    let mut heap = Heap::new(256).expect("tiny heap allocation");
    // The `[safe]` atom lookup requires the names to be interned already.
    let mut atoms = AtomTable::new();
    atoms.seed(["m", "n1"]);
    let term = decode_term_with_options_and_gate(&image, &mut heap, &mut atoms, &options, &gate)
        .expect("a NEW_FUN_EXT decodes under [safe]");
    assert_eq!(fun_kind_term(term).expect("a fun"), FunKind::Imported);
    assert_eq!(gate.calls(), 0);

    // And a `NEW_FUN_EXT` with an uninterned module name is refused by the *atom*
    // lookup — never by the gate.
    let gate = MockGate::empty();
    let mut heap = Heap::new(256).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    assert!(matches!(
        decode_term_with_options_and_gate(&image, &mut heap, &mut atoms, &options, &gate),
        Err(BeamError::BadArg)
    ));
    assert_eq!(gate.calls(), 0);
}

#[test]
fn a_non_safe_decode_of_any_term_never_consults_the_gate() {
    let gate = MockGate::empty();
    let tuple_of_funs = {
        let mut image = vec![VERSION_MAGIC, 104, 2]; // SMALL_TUPLE_EXT, 2 elements
        for _ in 0..2 {
            image.extend_from_slice(&export_image("lists", "map", 3)[1..]);
        }
        image
    };
    let (_h, atoms, term) = decode_gated(&tuple_of_funs, &plain_options(), &gate)
        .expect("both funs decode without [safe]");
    for index in 0..2 {
        let fun = tuple_element(term, index).expect("an element");
        assert_eq!(
            fun_view(&atoms, fun),
            ("lists".to_string(), "map".to_string(), 3)
        );
    }
    assert_eq!(gate.calls(), 0);
}

// ---------------------------------------------------------------------------
// The `COMPRESSED` wrapper cannot route around the gate
// ---------------------------------------------------------------------------

#[test]
fn a_compressed_image_still_enforces_the_gate() {
    // An explicit `[VERSION_MAGIC][COMPRESSED_EXT][inner size:4][zlib body]` wrapper
    // around one `EXPORT_EXT` body: the gate must be installed on the *inflated* path
    // too, not only on the plain one.
    let body = {
        let mut body = export_image("lists", "map", 2);
        body.remove(0); // the inner body is version-less
        body
    };
    let mut image = vec![VERSION_MAGIC, COMPRESSED_EXT];
    image.extend_from_slice(&(body.len() as u32).to_be_bytes());
    image.extend_from_slice(
        &beam_codec::compress::compress(&body, 6).expect("zlib deflates the body"),
    );
    assert!(is_compressed_image(&image), "the fixture must be a COMPRESSED image");

    let gate = MockGate::empty();
    assert!(
        is_bad_arg(decode_gated(&image, &safe_options(), &gate)),
        "a compressed non-active export is badarg"
    );
    assert_eq!(gate.calls(), 1);
    assert!(is_bad_arg(decode_gated_used(&image, &safe_options(), &gate)));

    let gate = MockGate::new(&[("lists", "map", 2)]);
    let (_h, atoms, term, used) =
        decode_gated_used(&image, &safe_options(), &gate).expect("an active export decodes");
    assert_eq!(
        fun_view(&atoms, term),
        ("lists".to_string(), "map".to_string(), 2)
    );
    assert_eq!(gate.seen(), vec![("lists".to_string(), "map".to_string(), 2)]);
    assert_eq!(used, image.len(), "the whole compressed image is consumed");

    // Without `[safe]` the compressed image decodes and the gate is never asked.
    let gate = MockGate::empty();
    assert!(decode_gated(&image, &plain_options(), &gate).is_ok());
    assert_eq!(gate.calls(), 0);
}

#[test]
fn the_used_count_is_the_plain_positions_after_the_term() {
    // A trailing byte after the term: the `[used]` twin tolerates it and reports the
    // term's own length (`decode_term_with_options`'s `finish()` would refuse it).
    let mut image = export_image("lists", "map", 2);
    let term_len = image.len();
    image.push(0xFF); // trailing junk
    let gate = MockGate::new(&[("lists", "map", 2)]);
    let (_h, _a, _t, used) =
        decode_gated_used(&image, &safe_options(), &gate).expect("trailing bytes are tolerated");
    assert_eq!(used, term_len);
    // The `finish()`-ing twin refuses the same image.
    let mut heap = Heap::new(256).expect("tiny heap allocation");
    let mut atoms = AtomTable::new();
    let gate = MockGate::new(&[("lists", "map", 2)]);
    atoms.seed(FIXTURE_ATOMS.iter().copied());
    assert!(is_bad_arg(decode_term_with_options_and_gate(
        &image,
        &mut heap,
        &mut atoms,
        &safe_options(),
        &gate
    )));
}

// ---------------------------------------------------------------------------
// Oracle cross-check
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
            "[beam_codec export_gate] $ERTS_ORACLE='{}' is not an existing executable — \
             treating it as no oracle.",
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
            eprintln!("============ beam_codec export_gate: {test} SKIPPED ============");
            eprintln!("No usable system `erl` oracle found.");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP)");
            eprintln!("and re-run.");
            eprintln!("==================================================================");
            eprintln!();
            None
        }
    }
}

/// An Erlang binary literal for `image`.
fn erl_binary(image: &[u8]) -> String {
    let bytes = image.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    format!("<<{bytes}>>")
}

/// Runs `erl -noshell -eval <eval> -s init stop` and returns its stdout.
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

#[test]
fn the_oracle_agrees_on_the_safe_active_export_rule() {
    let Some(oracle) = oracle_or_skip("the_oracle_agrees_on_the_safe_active_export_rule") else {
        return;
    };

    // The four shapes from the C rule, as the oracle classifies them: `lists:map/2` and
    // `erlang:system_info/1` are exported by modules loaded at boot (accepted), while
    // `lists:map/3` is not (rejected) — with `lists`/`map`/`erlang`/`system_info` all
    // interned, so the `[safe]` *atom* lookup is not what rejects it. The `2^32 + 7`
    // image pins the arity truncation: it asks about `lists:map/7`.
    let images = vec![
        export_image("lists", "map", 2),
        export_image("lists", "map", 3),
        export_image("erlang", "system_info", 1),
        export_image("lists", "map", (1 << 32) + 7),
        export_image("lists", "map", (1 << 32) + 2),
    ];
    let corpus = images
        .iter()
        .map(|image| erl_binary(image))
        .collect::<Vec<_>>()
        .join(",\n    ");
    let eval = format!(
        "Images = [\n    {corpus}\n],\n\
         io:format(\"~p~n\", [[case catch binary_to_term(I, [safe]) of\n\
         \x20   {{'EXIT', _}} -> badarg;\n\
         \x20   _ -> ok\n\
         end || I <- Images]]),\n\
         halt(0)."
    );
    let stdout = run_erl(&oracle, &eval);
    let line = stdout.trim();
    let oracle_verdicts: Vec<&str> = line
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(str::trim)
        .collect();
    assert_eq!(
        oracle_verdicts.len(),
        images.len(),
        "unparsable oracle output: {stdout:?}"
    );

    // Our verdicts through the gate, with the mock reporting exactly the exports the
    // oracle has loaded and the shapes name: `lists:map/2` and `erlang:system_info/1`
    // are active, `lists:map/3` and `lists:map/7` are not.
    let gate = MockGate::new(&[("lists", "map", 2), ("erlang", "system_info", 1)]);
    for (image, expected) in images.iter().zip(&oracle_verdicts) {
        let ours = match decode_gated(image, &safe_options(), &gate) {
            Ok(_) => "ok",
            Err(BeamError::BadArg) => "badarg",
            Err(other) => panic!("unexpected error {other:?} for {image:?}"),
        };
        assert_eq!(
            ours, *expected,
            "our verdict for {image:?} must match the oracle's ({line})"
        );
    }

    // The last two images are the same question, so the gate saw the same arity twice.
    let seen = gate.seen();
    assert_eq!(
        seen,
        vec![
            ("lists".to_string(), "map".to_string(), 2),
            ("lists".to_string(), "map".to_string(), 3),
            ("erlang".to_string(), "system_info".to_string(), 1),
            ("lists".to_string(), "map".to_string(), 7),
            ("lists".to_string(), "map".to_string(), 2),
        ]
    );
}
