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

//! Regression + oracle cross-check for the **`ERL_NIF_UTF8` arm of `enif_get_string`** (contract
//! A.3).
//!
//! The oracle's UTF-8 path is two-stage: `enif_get_string` calls
//! `erts_unicode_list_to_buf(list, buf, len - 1, len, &written)`
//! (`erts/emulator/beam/erl_nif.c:1639`; `erts/emulator/beam/utils.c:2862-2942`) and maps its result
//! back (`erl_nif.c:1640-1653`). The **byte** budget is `len - 1` (the terminating NUL needs a slot)
//! and the **character** budget is `len`; a multibyte character that does not fully fit the byte
//! budget is dropped **whole**. A byte-at-a-time loop instead emits a *truncated* UTF-8 sequence and
//! returns a byte count the oracle never produces — the divergence this file pins.
//!
//! `tests/fixtures/nif_get_string_utf8.c` fills a 64-byte buffer with the sentinel `0x2A` (`'*'`),
//! calls the ABI, and returns `{ReturnValue, Window}` where `Window` is the first `min(len, 8)`
//! buffer bytes. Printing the whole requested window pins both the returned integer and **where the
//! NUL landed** (and that nothing past it was clobbered).
//!
//! Both halves of the comparison use the system runtime as their yardstick:
//! `the_utf8_arm_matches_the_measured_oracle` drives the *Rust* runtime through the loaded `.so` and
//! compares against the recorded oracle values, while
//! `the_same_fixture_agrees_under_the_erl_oracle` loads the *same* `.so` into the system Erlang and
//! requires its printed observations to equal those recorded values line for line.
//!
//! # Skip-vs-fail policy
//!
//! Compiling the fixture needs a C compiler and the system `erl_nif.h`; driving the oracle needs
//! `erl`/`erlc`. `compile_so` prints a loud `SKIP:` banner and returns `None` when the C toolchain is
//! missing, and `oracle_print` does the same for the Erlang side — a skip is printed, never silent.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::nif::enif::{enif_make_double, enif_make_long};
use beam_abi::nif::loader::{load, NifLibrary};
use beam_abi::nif::term::eterm_from_nif;
use beam_abi::NifEnv;
use beam_term::prelude::{self, make_small, Eterm, NIL};

use common::{compile_so, skip, TestHost};

/// The committed C fixture (see the file header for what it proves).
const FIXTURE_C: &str = include_str!("fixtures/nif_get_string_utf8.c");

/// The fixture's `ERL_NIF_INIT` name — the loader (and the oracle) require it to match the calling
/// module, so it doubles as the name of the Erlang module the oracle test generates.
const MODULE: &str = "nif_get_string_utf8";

/// The untouched-byte sentinel the fixture fills its buffer with (mirrors the C's `SENTINEL`).
const SENTINEL: i64 = 0x2A;

/// `2^60`, the smallest magnitude that is a **bignum** rather than a small integer.
const BIGNUM: i64 = 1_152_921_504_606_846_976;

// ---------------------------------------------------------------------------------------------
// The inputs, built by this runtime and written out as Erlang source for the oracle
// ---------------------------------------------------------------------------------------------

/// One `probe/2` list argument, with both a Rust builder and its Erlang source text, so the **same**
/// input reaches both runtimes.
#[derive(Clone, Copy)]
enum Input {
    /// The empty list `[]`.
    Nil,
    /// A proper list of small-integer codepoints.
    Chars(&'static [i64]),
    /// The improper list `[head|tail]`.
    Improper(i64, i64),
    /// A one-element list holding an integer too wide to be a small (a bignum).
    Big(i64),
    /// A one-element list holding the float `1.5`.
    Float,
    /// A plain atom, which is not a list at all.
    Atom,
}

impl Input {
    /// Build the term on `env`'s host heap.
    fn build(self, env: &mut NifEnv) -> Eterm {
        match self {
            Input::Nil => NIL,
            Input::Chars(codepoints) => {
                let mut acc = NIL;
                for codepoint in codepoints.iter().rev() {
                    acc = env
                        .heap()
                        .cons(make_small(*codepoint), acc)
                        .expect("the test heap has room for the input list");
                }
                acc
            }
            Input::Improper(head, tail) => env
                .heap()
                .cons(make_small(head), make_small(tail))
                .expect("the test heap has room for the improper list"),
            Input::Big(value) => {
                // SAFETY: `env` is live for the call; a one-limb bignum allocates on its heap.
                eterm_from_nif(unsafe { enif_make_long(env.as_ptr(), value) })
            }
            Input::Float => {
                // SAFETY: `env` is live for the call; `1.5` is a representable heap float.
                eterm_from_nif(unsafe { enif_make_double(env.as_ptr(), 1.5) })
            }
            Input::Atom => env
                .host()
                .intern_atom("foo")
                .expect("the test host interns every atom"),
        }
    }

    /// The same input as Erlang source (what the oracle module is driven with).
    fn erl_source(self) -> String {
        match self {
            Input::Nil => "[]".to_string(),
            Input::Chars(codepoints) => format!("[{}]", join(codepoints)),
            Input::Improper(head, tail) => format!("[{head}|{tail}]"),
            Input::Big(value) => format!("[{value}]"),
            Input::Float => "[1.5]".to_string(),
            Input::Atom => "foo".to_string(),
        }
    }
}

/// `a,b,c` — the Erlang list-literal body for `values`.
fn join(values: &[i64]) -> String {
    values
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

// ---------------------------------------------------------------------------------------------
// The recorded oracle observations
// ---------------------------------------------------------------------------------------------

/// One `probe(List, Len)` observation measured on **OTP 29.1.1 / erts-17.1**.
#[derive(Clone, Copy)]
struct ProbeCase {
    /// The `List` argument.
    input: Input,
    /// The `Len` argument (the caller's buffer size).
    len: u32,
    /// The measured `{ReturnValue, Window}`.
    want: (i64, &'static [i64]),
}

/// One `len(List)` observation measured on **OTP 29.1.1 / erts-17.1**.
#[derive(Clone, Copy)]
struct LengthCase {
    /// The `List` argument.
    input: Input,
    /// The measured `{ReturnValue, OutLength}`.
    want: (i64, i64),
}

/// Every `enif_get_string(.., ERL_NIF_UTF8)` observation, with the oracle values collected from the
/// system runtime. The first three rows are the sweep that exposed the byte-at-a-time divergence
/// (`-3`/`-2` with a truncated sequence instead of `-1`/`-1`/`-2` with a bare NUL).
#[rustfmt::skip]
const PROBE_CASES: &[ProbeCase] = &[
    // -- the three rows that exposed the divergence --------------------------------------------
    // [0x20AC] needs 3 bytes but only `len - 1 == 2` are available: the whole character is dropped.
    ProbeCase { input: Input::Chars(&[0x20AC]),          len: 3, want: (-1, &[0, SENTINEL, SENTINEL]) },
    // The same for a 2-byte character against a 1-byte budget.
    ProbeCase { input: Input::Chars(&[0x7FF]),           len: 2, want: (-1, &[0, SENTINEL]) },
    // 'a' fits, then the 3-byte character does not: `written == 1`, so the answer is `-(1 + 1)`.
    ProbeCase { input: Input::Chars(&[0x61, 0x20AC]),    len: 3, want: (-2, &[0x61, 0, SENTINEL]) },

    // -- a buffer exactly big enough --------------------------------------------------------
    ProbeCase { input: Input::Chars(&[0x20AC]),          len: 4, want: (4, &[0xE2, 0x82, 0xAC, 0]) },
    ProbeCase { input: Input::Chars(&[0x61]),            len: 2, want: (2, &[0x61, 0]) },
    ProbeCase { input: Input::Chars(&[0x1F600]),         len: 5, want: (5, &[0xF0, 0x9F, 0x98, 0x80, 0]) },
    ProbeCase { input: Input::Chars(&[0x61, 0x1F600]),   len: 6, want: (6, &[0x61, 0xF0, 0x9F, 0x98, 0x80, 0]) },
    ProbeCase { input: Input::Chars(&[0x20AC, 0x20AC]),  len: 7, want: (7, &[0xE2, 0x82, 0xAC, 0xE2, 0x82, 0xAC, 0]) },

    // -- one byte short of that -------------------------------------------------------------
    ProbeCase { input: Input::Chars(&[0x20AC, 0x20AC]),  len: 4, want: (-4, &[0xE2, 0x82, 0xAC, 0]) },
    ProbeCase { input: Input::Chars(&[0x1F600]),         len: 4, want: (-1, &[0, SENTINEL, SENTINEL, SENTINEL]) },
    ProbeCase { input: Input::Chars(&[0x61, 0x1F600]),   len: 5, want: (-2, &[0x61, 0, SENTINEL, SENTINEL, SENTINEL]) },

    // -- boundary lengths -------------------------------------------------------------------
    // `len == 0` is refused before anything is written (nothing is even in the window).
    ProbeCase { input: Input::Chars(&[0x61]),            len: 0, want: (0, &[]) },
    // `len == 1` with a 1-byte character: the byte budget `len - 1 == 0` fits nothing.
    ProbeCase { input: Input::Chars(&[0x41]),            len: 1, want: (-1, &[0]) },
    ProbeCase { input: Input::Chars(&[0x61]),            len: 1, want: (-1, &[0]) },
    ProbeCase { input: Input::Chars(&[0x41, 0x42]),      len: 2, want: (-2, &[0x41, 0]) },
    // The 2-byte/3-byte encoder boundary: `0x7FF` is the last 2-byte character, `0x800` the first
    // 3-byte one.
    ProbeCase { input: Input::Chars(&[0x7FF]),           len: 3, want: (3, &[0xDF, 0xBF, 0]) },
    ProbeCase { input: Input::Chars(&[0x800]),           len: 3, want: (-1, &[0, SENTINEL, SENTINEL]) },

    // -- everything that is not a valid character list --------------------------------------
    // An out-of-range codepoint, a negative one, a surrogate, a float and a bignum are *type*
    // errors: `res = -1`, i.e. `buf[0] = '\0'` and `0` — never the `-2` truncation answer.
    ProbeCase { input: Input::Chars(&[0x110000]),        len: 8, want: (0, &[0, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL]) },
    ProbeCase { input: Input::Chars(&[-1]),              len: 8, want: (0, &[0, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL]) },
    ProbeCase { input: Input::Chars(&[0xD800]),          len: 8, want: (0, &[0, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL]) },
    ProbeCase { input: Input::Float,                     len: 8, want: (0, &[0, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL]) },
    ProbeCase { input: Input::Big(BIGNUM),               len: 8, want: (0, &[0, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL]) },
    ProbeCase { input: Input::Big(-BIGNUM),              len: 8, want: (0, &[0, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL]) },
    // An improper list is a type error too.
    ProbeCase { input: Input::Improper(1, 2),            len: 8, want: (0, &[0, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL]) },
    // ... and so is a plain atom.
    ProbeCase { input: Input::Atom,                      len: 8, want: (0, &[0, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL]) },

    // -- the empty list and a 2-byte character ----------------------------------------------
    ProbeCase { input: Input::Nil,                       len: 8, want: (1, &[0, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL]) },
    ProbeCase { input: Input::Chars(&[300]),             len: 8, want: (3, &[0xC4, 0xAC, 0, SENTINEL, SENTINEL, SENTINEL, SENTINEL, SENTINEL]) },
];

/// Every `enif_get_string_length(.., ERL_NIF_UTF8)` observation. The UTF-8 arm already agrees with
/// the oracle; these pin that agreement so a future change cannot silently break it.
#[rustfmt::skip]
const LENGTH_CASES: &[LengthCase] = &[
    LengthCase { input: Input::Chars(&[0x20AC]),          want: (1, 3) },
    LengthCase { input: Input::Nil,                       want: (1, 0) },
    LengthCase { input: Input::Chars(&[300]),             want: (1, 2) },
    LengthCase { input: Input::Chars(&[0x61, 0x20AC]),    want: (1, 4) },
    LengthCase { input: Input::Chars(&[0x1F600]),         want: (1, 4) },
    LengthCase { input: Input::Chars(&[0x20AC, 0x20AC]),  want: (1, 6) },
    LengthCase { input: Input::Chars(&[0, 0x41, 0x7F]),   want: (1, 3) },
    // Not a character list: `0`, and the out-length is left untouched (the fixture zeroes it).
    LengthCase { input: Input::Atom,                      want: (0, 0) },
    LengthCase { input: Input::Improper(1, 2),            want: (0, 0) },
    LengthCase { input: Input::Chars(&[0x110000]),        want: (0, 0) },
    LengthCase { input: Input::Chars(&[0xD800]),          want: (0, 0) },
    LengthCase { input: Input::Chars(&[-1]),              want: (0, 0) },
    LengthCase { input: Input::Float,                     want: (0, 0) },
    LengthCase { input: Input::Big(BIGNUM),               want: (0, 0) },
    LengthCase { input: Input::Big(-BIGNUM),              want: (0, 0) },
];

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The fixture compiled into a shared object, or `None` (after a loud skip) when the C toolchain is
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

/// Decode the fixture's 2-tuple answer into its two elements.
#[track_caller]
fn decode_pair(term: Eterm) -> (Eterm, Eterm) {
    // SAFETY: `term` came back from the fixture's own `enif_make_tuple_from_array`; `is_tuple` reads
    // only its header.
    assert!(
        unsafe { prelude::is_tuple(term) },
        "the fixture must return a 2-tuple, got {term:?}"
    );
    // SAFETY: `term` is a live tuple.
    let size = unsafe { prelude::tuple_size(term) };
    assert_eq!(size, 2, "the fixture must return a 2-tuple");
    // SAFETY: `term` is a live 2-tuple, so its element array has two readable words.
    let elements = unsafe { prelude::tuple_val(term).add(1) };
    // SAFETY: as above.
    unsafe { (*elements, *elements.add(1)) }
}

/// The small integer `term` carries.
#[track_caller]
fn decode_small(term: Eterm) -> i64 {
    assert!(
        prelude::is_small(term),
        "expected a small integer, got {term:?}"
    );
    prelude::signed_value(term)
}

/// A proper list of small integers (the fixture's buffer window) decoded as its values.
#[track_caller]
fn decode_byte_list(term: Eterm) -> Vec<i64> {
    let mut cur = term;
    let mut values = Vec::new();
    while prelude::is_list(cur) {
        // SAFETY: `cur` is a live cons cell.
        let (head, tail) = unsafe { (prelude::car(cur), prelude::cdr(cur)) };
        assert!(
            prelude::is_small(head),
            "every window byte must be a small integer, got {head:?}"
        );
        values.push(prelude::signed_value(head));
        cur = tail;
    }
    assert!(prelude::is_nil(cur), "the window list must be proper");
    values
}

/// Call `probe/2` for one case and require the observation to equal the recorded oracle value.
#[track_caller]
fn assert_probe_case(env: &mut NifEnv, lib: &NifLibrary, case: &ProbeCase) {
    let input = case.input.build(env);
    let len = make_small(case.len as i64);
    let got = lib
        .call(env, "probe", 2, &[input, len])
        .unwrap_or_else(|err| panic!("probe/2 must be callable: {err:?}"));
    let (ret, window) = decode_pair(got);
    assert_eq!(
        (decode_small(ret), decode_byte_list(window)),
        (case.want.0, case.want.1.to_vec()),
        "probe({}, {}) diverges from the oracle",
        case.input.erl_source(),
        case.len
    );
}

/// Call `len/1` for one case and require the observation to equal the recorded oracle value.
#[track_caller]
fn assert_length_case(env: &mut NifEnv, lib: &NifLibrary, case: &LengthCase) {
    let input = case.input.build(env);
    let got = lib
        .call(env, "len", 1, &[input])
        .unwrap_or_else(|err| panic!("len/1 must be callable: {err:?}"));
    let (ret, out) = decode_pair(got);
    assert_eq!(
        (decode_small(ret), decode_small(out)),
        case.want,
        "len({}) diverges from the oracle",
        case.input.erl_source()
    );
}

/// The oracle's `~w` rendering of a `probe/2` observation.
fn erl_probe_observation(want: (i64, &[i64])) -> String {
    format!("{{{},[{}]}}", want.0, join(want.1))
}

/// The oracle's `~w` rendering of a `len/1` observation.
fn erl_length_observation(want: (i64, i64)) -> String {
    format!("{{{},{}}}", want.0, want.1)
}

// ---------------------------------------------------------------------------------------------
// The Rust runtime
// ---------------------------------------------------------------------------------------------

#[test]
fn the_utf8_arm_matches_the_measured_oracle() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = TestHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    let mut env = NifEnv::new(&mut host);

    for case in PROBE_CASES {
        assert_probe_case(&mut env, &lib, case);
    }
    for case in LENGTH_CASES {
        assert_length_case(&mut env, &lib, case);
    }
}

// ---------------------------------------------------------------------------------------------
// The system-Erlang oracle (skipped loudly when the toolchain is absent)
// ---------------------------------------------------------------------------------------------

/// Whether `cmd` can be spawned with `args` at all (the probe for the oracle toolchain; a non-zero
/// exit is fine — e.g. `erlc -h` — only a missing binary counts as absent).
fn runnable(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok()
}

/// Load the fixture into the **system** Erlang and print every observation, one per line.
///
/// Returns `None` (after a loud skip) when `erl`/`erlc` are unavailable or the generated module could
/// not be built; a *runtime* failure of the Erlang side is a hard error, because that would mean the
/// fixture is not a valid NIF rather than that the toolchain is missing.
fn oracle_print(so: &Path) -> Option<Vec<String>> {
    if !runnable("erl", &["-noshell", "-eval", "halt(0)."]) || !runnable("erlc", &["-h"]) {
        skip("the erl/erlc oracle toolchain is not available");
        return None;
    }
    let dir = so.parent()?.to_path_buf();
    // `erlang:load_nif/2` appends the shared-object suffix itself, so hand it the path WITHOUT `.so`.
    let stem = so.file_stem()?.to_str()?;
    let nif_path = dir.join(stem);
    let nif_path = nif_path.to_str()?;

    // The oracle module: the stub bodies are replaced by the NIFs on `load_nif`. The compiler marks
    // every function `nif_start` because `init/0` calls `load_nif/2`, which is what makes the
    // replacement possible.
    let mut erl = format!("-module({MODULE}).\n");
    erl.push_str("-export([probe/2, len/1]).\n");
    erl.push_str("-on_load(init/0).\n\n");
    erl.push_str(&format!(
        "init() ->\n    erlang:load_nif(\"{nif_path}\", 0).\n\n"
    ));
    erl.push_str("probe(_, _) -> erlang:nif_error(nif_not_loaded).\n");
    erl.push_str("len(_) -> erlang:nif_error(nif_not_loaded).\n");

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
    for case in PROBE_CASES {
        expr.push_str(&format!(
            "io:format(\"~w~n\", [{MODULE}:probe({}, {})]),\n",
            case.input.erl_source(),
            case.len
        ));
    }
    for case in LENGTH_CASES {
        expr.push_str(&format!(
            "io:format(\"~w~n\", [{MODULE}:len({})]),\n",
            case.input.erl_source()
        ));
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

    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

#[test]
fn the_same_fixture_agrees_under_the_erl_oracle() {
    let Some(so) = compile_fixture() else { return };
    let Some(lines) = oracle_print(&so) else { return };

    let mut expected: Vec<String> = PROBE_CASES
        .iter()
        .map(|case| erl_probe_observation(case.want))
        .collect();
    expected.extend(
        LENGTH_CASES
            .iter()
            .map(|case| erl_length_observation(case.want)),
    );

    assert_eq!(
        lines, expected,
        "the system Erlang and this crate must agree on every recorded UTF-8 observation"
    );
}
