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

//! End-to-end proof of **`enif_binary_to_term/5`'s consumed-byte count** (contract A.3).
//!
//! `tests/fixtures/nif_bin2term.c` is a real C NIF — compiled at test time with the system `cc`
//! against the system `erl_nif.h` — whose three entry points each run one
//! `size_t enif_binary_to_term(env, data, data_sz, &term, opts)` and answer what the C's
//! `return bp - data` (`erts/emulator/beam/erl_nif.c:1507-1545`) must produce:
//!
//! * `decode_used/1`   — `<Image>` ⇒ `{Used, Term}`;
//! * `used_truncated/1` — `<Image>` ⇒ `Used` with the last byte of the payload withheld;
//! * `used_bad_opts/1`  — `<Image>` ⇒ `Used` with `opts = 42` (outside `{0, ERL_NIF_BIN2TERM_SAFE}`).
//!
//! The count is the number of leading source bytes the **FIRST** term occupied — trailing bytes are
//! tolerated and *not* counted (`bp - data`), a truncated payload answers `0`, and an illegal `opts`
//! answers `0` **before** any decoding. The headline case is therefore an image with junk after the
//! term: `Used` is **3** (the whole `SMALL_INTEGER_EXT` term), not the 7-byte image length — which is
//! what the pre-fix `return sz` answered.
//!
//! | entry | input | oracle answer |
//! |---|---|---|
//! | `decode_used/1` | `term_to_binary(42) ++ "junk"` | `{3,42}` |
//! | `decode_used/1` | `term_to_binary(42)` | `{3,42}` |
//! | `used_truncated/1` | `term_to_binary(42)` (last byte withheld ⇒ `<<131,97>>`) | `0` |
//! | `used_truncated/1` | `term_to_binary(42) ++ "junk"` (one **trailing** byte withheld) | `3` |
//! | `used_bad_opts/1` | `term_to_binary(42)`, `opts = 42` | `0` |
//!
//! The last two rows together are what proves the `0` is *truncation* and not the count arithmetic:
//! dropping a **trailing** byte leaves term 1 intact.
//!
//! # The host seam
//!
//! `beam_abi` has no ETF decoder (no `beam_codec` dependency, by design), so the test host stands in
//! for `beam_codec::decode_term_used`: [`UsedHost`] implements the additive two-answer hook
//! [`NifHost::binary_to_term_used`] with `[131, 97, N, ..]` ⇒ `Some((make_small(N), 3))` — three
//! bytes consumed, trailing bytes tolerated. [`PlainHost`] implements **only** the legacy
//! [`NifHost::binary_to_term`], so it takes the default `binary_to_term_used` (which reports
//! `bytes.len()`); `the_default_hook_still_reports_the_slice_length` pins that fallback so nobody
//! assumes the additive hook changed an existing host's behaviour.
//!
//! The `enif_*` symbols the fixture calls are UNDEFINED in the `.so` and bind at
//! `dlopen(RTLD_NOW)` against this test binary (`build.rs` adds `-Wl,--export-dynamic`).
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system `erl_nif.h`; `compile_so` prints a loud
//! `SKIP:` banner and returns `None` when either is missing (the calling test then returns). The same
//! holds for the oracle half: it skips loudly when `erl`/`erlc` are absent. Whenever the toolchain
//! **is** present every assertion is hard.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::nif::enif::enif_make_new_binary;
use beam_abi::nif::loader::{load, NifLibrary};
use beam_abi::nif::term::ERL_NIF_TERM;
use beam_abi::{eterm_from_nif, NifEnv, NifHost};
use beam_term::atom::AtomTable;
use beam_term::prelude::{
    atom_val, is_atom, is_small, make_atom, make_small, signed_value, tuple_element, tuple_size,
    Eterm,
};

use common::{compile_so, skip};

/// The committed C fixture (see its header for what it proves).
const FIXTURE_C: &str = include_str!("fixtures/nif_bin2term.c");

/// The fixture's `ERL_NIF_INIT` name — the loader *and* the oracle require it to equal the calling
/// module, so it doubles as the name of the Erlang module the oracle test generates.
const MODULE: &str = "nif_bin2term";

/// `term_to_binary(42)` on the wire: `131` (VERSION_MAGIC), `97` (SMALL_INTEGER_EXT), `42`.
static SMALL_INT_IMAGE: [u8; 3] = [131, 97, 42];

/// The same image with four bytes the C never counts (`bp - data` stops at the end of term 1).
static TRAILING_BYTES_IMAGE: [u8; 7] = [131, 97, 42, b'j', b'u', b'n', b'k'];

/// The measured oracle answers (OTP 29.1.1 / erts-17.1) for [`CASES`], in order.
static ORACLE_LINES: [&str; 5] = ["{3,42}", "{3,42}", "0", "3", "0"];

// ---------------------------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------------------------

/// One call of one fixture entry: the bytes handed to it, the *same* call as an Erlang expression
/// for the oracle, and the answer both sides must give.
struct Case {
    /// The fixture entry point (`<name>/1`).
    entry: &'static str,
    /// The binary handed to it.
    bytes: &'static [u8],
    /// The oracle's expression, calling the same entry with the same bytes.
    ///
    /// The junk image is written `<<(term_to_binary(42))/binary, "junk">>` and **not**
    /// `term_to_binary(42) ++ <<"junk">>`: Erlang 29 rejects a binary on the left of `++` when the
    /// right operand's type is known to be a binary at compile time.
    expr: &'static str,
    /// The measured answer, `~w`-rendered.
    want: &'static str,
}

static CASES: [Case; 5] = [
    Case {
        entry: "decode_used",
        bytes: &TRAILING_BYTES_IMAGE,
        expr: r#"decode_used(<<(term_to_binary(42))/binary, "junk">>)"#,
        want: "{3,42}",
    },
    Case {
        entry: "decode_used",
        bytes: &SMALL_INT_IMAGE,
        expr: "decode_used(term_to_binary(42))",
        want: "{3,42}",
    },
    Case {
        entry: "used_truncated",
        bytes: &SMALL_INT_IMAGE,
        expr: "used_truncated(term_to_binary(42))",
        want: "0",
    },
    Case {
        entry: "used_truncated",
        bytes: &TRAILING_BYTES_IMAGE,
        expr: r#"used_truncated(<<(term_to_binary(42))/binary, "junk">>)"#,
        want: "3",
    },
    Case {
        entry: "used_bad_opts",
        bytes: &SMALL_INT_IMAGE,
        expr: "used_bad_opts(term_to_binary(42))",
        want: "0",
    },
];

/// Every `ErlNifFunc` the fixture declares — the oracle module must export all of them (the C's load
/// refuses a declared NIF the module does not have, `erl_nif.c:4874`).
const EXPORTS: [&str; 3] = ["decode_used/1", "used_truncated/1", "used_bad_opts/1"];

// ---------------------------------------------------------------------------------------------
// The hosts
// ---------------------------------------------------------------------------------------------

/// The tiny ETF decoder both hosts share: `SMALL_INTEGER_EXT` only.
///
/// Deliberately hand-written rather than `beam_codec`'s: this crate must not depend on the ETF
/// crate, and the test only needs the one tag the images above use.
const VERSION_MAGIC: u8 = 131;
const SMALL_INTEGER_EXT: u8 = 97;

/// Decode `[131, 97, N, ..]`, reporting the **three** bytes the term occupies (the trailing bytes are
/// tolerated). Anything else is `None`.
fn decode_small_int(bytes: &[u8]) -> Option<(Eterm, usize)> {
    match bytes {
        [VERSION_MAGIC, SMALL_INTEGER_EXT, value, ..] => Some((make_small(i64::from(*value)), 3)),
        _ => None,
    }
}

/// A host that implements the **new** two-answer hook, standing in for
/// `beam_codec::decode_term_used` (see the module docs).
struct UsedHost {
    heap: beam_alloc::Heap,
    atoms: AtomTable,
}

impl UsedHost {
    fn new() -> UsedHost {
        UsedHost {
            heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
        }
    }

    /// The atom table, for rendering the fixture's atoms.
    fn atoms(&self) -> &AtomTable {
        &self.atoms
    }
}

impl NifHost for UsedHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.atoms.name(atom_val(atom)).map(str::to_string)
    }

    /// The additive hook: the term **and** the bytes it occupied.
    fn binary_to_term_used(&mut self, bytes: &[u8]) -> Option<(Eterm, usize)> {
        decode_small_int(bytes)
    }
}

/// A host that implements **only** the legacy single-answer hook, so `binary_to_term_used` stays at
/// its default (`binary_to_term` plus `bytes.len()`).
struct PlainHost {
    heap: beam_alloc::Heap,
    atoms: AtomTable,
}

impl PlainHost {
    fn new() -> PlainHost {
        PlainHost {
            heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
        }
    }

    /// The atom table, for rendering the fixture's atoms.
    fn atoms(&self) -> &AtomTable {
        &self.atoms
    }
}

impl NifHost for PlainHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.atoms.name(atom_val(atom)).map(str::to_string)
    }

    /// `None` on anything that is not the `SMALL_INTEGER_EXT` image — never a fabricated term.
    fn binary_to_term(&mut self, bytes: &[u8]) -> Option<Eterm> {
        decode_small_int(bytes).map(|(term, _used)| term)
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The fixture compiled into a shared object, or `None` (after a loud skip) when the toolchain is
/// absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so(MODULE, FIXTURE_C)
}

/// The path `load` takes — the compiled `.so` **without** its suffix, which the loader appends
/// itself — or `None` (after a loud skip) when it is not valid UTF-8.
fn load_path(so: &Path) -> Option<String> {
    match so.with_extension("").to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// A binary term carrying `bytes`, built with the `Some` slot `enif_make_new_binary` on `env`'s heap
/// (the fixture inspects it with `enif_inspect_binary`).
fn binary_term(env: &NifEnv, bytes: &[u8]) -> Eterm {
    let mut term = ERL_NIF_TERM::default();
    // SAFETY: `env` is live, `term` is a live writable local, and the returned pointer heads exactly
    // `bytes.len()` writable payload bytes of the freshly written heap binary.
    let data = unsafe { enif_make_new_binary(env.as_ptr(), bytes.len(), &mut term) };
    assert!(!data.is_null(), "a non-null binary payload");
    if !bytes.is_empty() {
        // SAFETY: `data` heads `bytes.len()` writable bytes (the guard above) and `bytes` is that
        // many readable bytes, so the ranges do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len()) };
    }
    eterm_from_nif(term)
}

/// Render the fixture's answer the way `~w` does: a bare small integer, or a tuple whose elements are
/// small integers or atoms (the failure path answers the atom `undefined`).
#[track_caller]
fn render(term: Eterm, atoms: &AtomTable) -> String {
    if is_small(term) {
        return signed_value(term).to_string();
    }
    // SAFETY: the fixture's compound answer is a 2-tuple; `tuple_size`/`tuple_element` only read that
    // block and the index is bounded by `tuple_size`.
    let elements: Vec<Eterm> = unsafe {
        (0..tuple_size(term))
            .map(|i| tuple_element(term, i))
            .collect()
    };
    let parts: Vec<String> = elements
        .into_iter()
        .map(|element| {
            if is_small(element) {
                return signed_value(element).to_string();
            }
            if is_atom(element) {
                return atoms
                    .name(atom_val(element))
                    .expect("the fixture's atoms are interned in the test host")
                    .to_string();
            }
            panic!("unexpected element in the fixture's answer: {element:?}");
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

/// Run every case through the loaded library and render the answers, on `host`.
#[track_caller]
fn run_all<H: NifHost>(lib: &NifLibrary, host: &mut H, atoms: fn(&H) -> &AtomTable) -> Vec<String> {
    let mut raws = Vec::with_capacity(CASES.len());
    {
        let env = NifEnv::new(host);
        for case in CASES.iter() {
            let arg = binary_term(&env, case.bytes);
            let term = lib
                .call(&env, case.entry, 1, &[arg])
                .unwrap_or_else(|err| panic!("{}/1 must be callable: {err:?}", case.entry));
            raws.push(term);
        }
    }
    // The env's borrow ends here, so the host's atom table is readable again.
    raws.into_iter().map(|term| render(term, atoms(host))).collect()
}

// ---------------------------------------------------------------------------------------------
// The Rust runtime
// ---------------------------------------------------------------------------------------------

/// **The headline.** An image carrying junk after the term answers the term's own three bytes, not
/// the seven-byte image length — the C's `return bp - data`, not a pre-fix `return sz`.
#[test]
fn the_first_terms_bytes_are_counted_not_the_image_length() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = UsedHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    let ours = run_all(&lib, &mut host, UsedHost::atoms);

    assert_eq!(
        ours[0], "{3,42}",
        "decode_used of a 7-byte image whose first term is 3 bytes must answer the 3 consumed bytes"
    );
    assert_eq!(TRAILING_BYTES_IMAGE.len(), 7, "the image really is longer");
}

/// Whole-image call: the count is the image's own length, so the two answers coincide (the case a
/// `return sz` implementation also passes — hence the headline above).
#[test]
fn a_whole_image_answers_its_own_length() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = UsedHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil()).expect("the fixture must load");
    let ours = run_all(&lib, &mut host, UsedHost::atoms);

    assert_eq!(ours[1], "{3,42}");
    assert_eq!(SMALL_INT_IMAGE.len(), 3);
}

/// A truncated payload is `0`; withholding a **trailing** byte is not truncation and still counts the
/// term — so the `0` is the truncation, not the arithmetic.
#[test]
fn a_truncated_payload_is_zero_and_a_trailing_byte_is_not_truncation() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = UsedHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil()).expect("the fixture must load");
    let ours = run_all(&lib, &mut host, UsedHost::atoms);

    assert_eq!(
        ours[2], "0",
        "withholding the value byte leaves <<131,97>>, which decodes no term"
    );
    assert_eq!(
        ours[3], "3",
        "withholding a TRAILING byte leaves the 3-byte term intact"
    );
}

/// An `opts` outside `{0, ERL_NIF_BIN2TERM_SAFE}` answers `0` before any decoding (`opts = 42`).
#[test]
fn an_illegal_opts_value_answers_zero_without_decoding() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = UsedHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil()).expect("the fixture must load");
    let ours = run_all(&lib, &mut host, UsedHost::atoms);

    assert_eq!(ours[4], "0");
}

/// The **default** `binary_to_term_used` forwards to `binary_to_term` and reports `bytes.len()`, so a
/// host that implements only the legacy hook keeps its old behaviour — documented, not silent.
///
/// For a whole image that is identical to the C's answer; for an image with trailing bytes it reports
/// the slice length (`7`, not `3`), which is exactly why the additive hook exists. The two
/// `used_truncated` rows show the same arithmetic shift (`6` for a 6-byte argument), so the vector is
/// the fallback's honest signature rather than a copy of the C's.
#[test]
fn the_default_hook_still_reports_the_slice_length() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = PlainHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil()).expect("the fixture must load");
    let ours = run_all(&lib, &mut host, PlainHost::atoms);

    assert_eq!(ours, vec!["{7,42}", "{3,42}", "0", "6", "0"]);
}

// ---------------------------------------------------------------------------------------------
// The system-Erlang oracle (skipped loudly when the toolchain is absent)
// ---------------------------------------------------------------------------------------------

/// Whether `cmd` can be spawned with `args` at all (the probe for the oracle toolchain; a non-zero
/// exit is fine — e.g. `erlc -h` — only a missing binary counts as absent).
fn runnable(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok()
}

/// Load the fixture into the **system** Erlang and print `exprs` (each an Erlang expression calling
/// the fixture), one `~w` line per expression.
///
/// `exports` lists every NIF the fixture declares as `name/arity`; the generated module exports and
/// stubs all of them, because `erlang:load_nif/2` refuses a declared NIF the module does not have.
///
/// Returns `None` (after a loud skip) when `erl`/`erlc` are unavailable or the generated module could
/// not be built; a *runtime* failure of the Erlang side is a hard error, because that would mean the
/// fixture is not a valid NIF rather than that the toolchain is missing.
fn oracle_print(so: &Path, exprs: &[String], exports: &[&str]) -> Option<Vec<String>> {
    if !runnable("erl", &["-noshell", "-eval", "halt(0)."]) || !runnable("erlc", &["-h"]) {
        skip("the erl/erlc oracle toolchain is not available");
        return None;
    }
    let dir = so.parent()?.to_path_buf();
    // `erlang:load_nif/2` appends the shared-object suffix itself, so hand it the path WITHOUT `.so`.
    let stem = so.file_stem()?.to_str()?;
    let nif_path = dir.join(stem);
    let nif_path = nif_path.to_str()?;

    let mut erl = format!("-module({MODULE}).\n");
    erl.push_str(&format!("-export([{}]).\n", exports.join(", ")));
    erl.push_str("-on_load(init/0).\n\n");
    erl.push_str(&format!(
        "init() ->\n    erlang:load_nif(\"{nif_path}\", 0).\n\n"
    ));
    for export in exports {
        let (name, arity) = export
            .split_once('/')
            .expect("every export is spelled name/arity");
        let params: Vec<String> = (1..=arity.parse::<usize>().expect("a numeric arity"))
            .map(|index| format!("_A{index}"))
            .collect();
        erl.push_str(&format!(
            "{name}({}) -> erlang:nif_error(nif_not_loaded).\n",
            params.join(", ")
        ));
    }

    let erl_file = dir.join(format!("{MODULE}.erl"));
    std::fs::write(&erl_file, erl).expect("the oracle module is writable");

    let compiled = Command::new("erlc")
        .arg("-o")
        .arg(&dir)
        .arg(&erl_file)
        .output()
        .expect("erlc is runnable (probed above)");
    assert!(
        compiled.status.success(),
        "erlc failed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut expr = String::new();
    for e in exprs {
        expr.push_str(&format!("io:format(\"~w~n\", [{MODULE}:{e}]),\n"));
    }
    expr.push_str("halt(0).\n");

    let out = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-eval")
        .arg(&expr)
        .output()
        .expect("erl is runnable (probed above)");
    assert!(
        out.status.success(),
        "the oracle run failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    println!("oracle: {}", lines.join(" | "));
    Some(lines)
}

/// The same `.so`, driven by the same calls, under the **system** Erlang: the answers must equal this
/// crate's and the measured oracle constants — so an accidentally identical divergence still fails.
#[test]
fn the_same_fixture_agrees_under_the_erl_oracle() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = UsedHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil()).expect("the fixture must load");
    let ours = run_all(&lib, &mut host, UsedHost::atoms);

    let exprs: Vec<String> = CASES.iter().map(|case| case.expr.to_string()).collect();
    let Some(lines) = oracle_print(&so, &exprs, &EXPORTS) else {
        return;
    };
    assert_eq!(
        lines, ours,
        "the system Erlang and this crate must agree on every enif_binary_to_term count"
    );
    assert_eq!(
        lines,
        ORACLE_LINES
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>(),
        "the measured oracle answers"
    );
    assert_eq!(
        ours,
        CASES.iter().map(|case| case.want.to_string()).collect::<Vec<_>>(),
        "and the crate's answers are the documented constants"
    );
}
