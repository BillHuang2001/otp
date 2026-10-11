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

//! The **residual `Instr::Generic` census** over the installed OTP.
//!
//! `beam_loader` decodes every opcode in `../../lib/compiler/src/genop.tab` and lowers
//! each one it models to a typed [`Instr`] variant; anything it does *not* model falls
//! back — losslessly, never as a wrong result — to
//! [`Instr::Generic { op, operands, offset }`](Instr::Generic) (`src/lower/instr.rs`).
//!
//! The crate `CONTEXT.md` ("Known Issues") claims that residual set is limited to the
//! obsolete/transform-only `genop.tab` entries the standard compiler **never emits**
//! (e.g. `is_constant/2`, `make_fun2/1`). This file *proves* that claim against the
//! real thing: it walks the **whole installed OTP `lib` root** — every versioned
//! application directory and every `*.beam` found under it, recursively — the exact
//! same scope and generic, `code:lib_dir()`-based discovery as
//! `tests/real_modules.rs` (both go through the shared `tests/common/mod.rs`
//! helper [`all_otp_beams`]), and collects the exact set of opcodes that arrive as
//! `Instr::Generic`.
//!
//! [`compiler_emitted_opcodes_are_all_modelled`] is the generalisation of
//! `tests/native_record.rs` (which does the same for the eight native-record opcodes
//! 186-193) to the **whole** opcode roster: it asserts the census is exactly the
//! documented baseline below. The intended outcome is an **empty** census — no opcode
//! the installed compiler emits is left unmodelled.
//!
//! # Why a documented baseline instead of a bare `is_empty()`
//!
//! The typed variants live in `Instr` (`src/instr.rs`), which `beam_engine` matches
//! **exhaustively**. Adding one is a cross-crate change owned jointly with the engine
//! node, so this test does not add variants: where the census is non-empty it *pins*
//! the observed set here, printing each opcode (by [`Opcode::name`]) with an example
//! module that emits it, and fails **only if the set grows beyond the baseline** (or a
//! baseline entry disappears, which means the pin is stale). That keeps the gap
//! recorded and the test green while still catching any newly-emitted unmodelled
//! opcode — the exact signal a regression guard needs.
//!
//! Ground truth for opcode semantics is `../../erts/emulator/beam/emu/ops.tab` and
//! `../../lib/compiler/src/genop.tab` (read-only).
//!
//! # Skip-vs-assert policy (explicit)
//!
//! A missing OTP install (no `erl`, the `stdlib`/`kernel` `ebin` directories cannot be
//! located, or the `lib` root is unreadable) **skips LOUDLY** with a `SKIP:` line and
//! returns — it never fails in that case, matching `tests/real_modules.rs`. It never
//! silently passes without OTP.
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader
//! ```
//!
//! With `-- --nocapture` the full per-opcode census is printed.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use beam_loader::{Instr, Module, Opcode};

mod common;
use common::{all_otp_beams, skip_reason};

/// The **pinned residual `Instr::Generic` set** — the opcodes the installed OTP
/// compiler actually emits that `beam_loader` still leaves unmodelled, by
/// [`Opcode::name`].
///
/// **The intended value is empty.** Every name here is a known, cross-crate gap: the
/// typed [`Instr`] variant can only be added together with `beam_engine` (which matches
/// `Instr` exhaustively), so it is recorded rather than fixed here. The test fails if
/// the sweep observes any opcode *outside* this list, or if a listed opcode stops
/// appearing (the pin is then stale and must be tightened).
const DOCUMENTED_GENERIC: &[&str] = &[];

// ---------------------------------------------------------------------------
// The census
// ---------------------------------------------------------------------------

/// One residual-`Generic` opcode observed during the sweep: how many sites lowered to
/// it, and one module (file stem) that carries it — the evidence a handoff needs.
#[derive(Debug)]
struct Site {
    op: Opcode,
    count: usize,
    example: String,
}

/// Adds every residual [`Instr::Generic`] opcode among `instructions` into `observed`,
/// keyed by the wire opcode byte (so the report is in opcode order and independent of
/// the non-`Ord` [`Opcode`]).
///
/// `module_name` is recorded as the `example` for a first sighting only, so the report
/// names one concrete module that emits each opcode. Kept instruction-level so the
/// detector can be exercised directly by
/// [`census_detects_a_residual_generic_opcode`], independently of a loadable `.beam`.
fn census_instrs<'a>(
    instructions: impl Iterator<Item = &'a Instr>,
    module_name: &str,
    observed: &mut BTreeMap<u8, Site>,
) {
    for instr in instructions {
        if let Instr::Generic { op, .. } = instr {
            let site = observed.entry(op.as_u8()).or_insert_with(|| Site {
                op: *op,
                count: 0,
                example: module_name.to_string(),
            });
            site.count += 1;
        }
    }
}

/// Adds `module`'s residual [`Instr::Generic`] opcodes into `observed`.
fn census(module: &Module, module_name: &str, observed: &mut BTreeMap<u8, Site>) {
    for function in &module.functions {
        census_instrs(function.code.iter(), module_name, observed);
    }
}

/// The `opcode name` for each observed site, `(byte, name)` — name-sorted.
fn observed_names(observed: &BTreeMap<u8, Site>) -> Vec<String> {
    let mut names: Vec<String> = observed.values().map(|site| site.op.name().to_string()).collect();
    names.sort();
    names.dedup();
    names
}

/// Renders the per-opcode census, one `  <byte> <name>  (<count>x in <module>)` line
/// per observed opcode — the exact evidence a cross-crate handoff must carry.
fn render(observed: &BTreeMap<u8, Site>) -> String {
    let mut report = String::new();
    if observed.is_empty() {
        let _ = writeln!(report, "  (none — every compiler-emitted opcode is modelled)");
        return report;
    }
    for (byte, site) in observed {
        let _ = writeln!(
            report,
            "  {byte:>3} {:<28} {:>6}x  e.g. {}",
            site.op.name(),
            site.count,
            site.example
        );
    }
    report
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The residual-`Instr::Generic` census across the whole installed OTP: **no opcode
/// the installed compiler emits may be left unmodelled beyond [`DOCUMENTED_GENERIC`]**.
///
/// The sweep loads every `.beam` of the whole installed OTP `lib` root — every
/// versioned application (all 34 apps, ≈1366 modules) — through the public pipeline
/// ([`beam_loader::load_file`]), using the same generic discovery as
/// `tests/real_modules.rs` (shared [`all_otp_beams`]), and collects the exact opcode
/// set that arrives as [`Instr::Generic`]. The census is printed in full, so a
/// non-empty result is immediately actionable.
///
/// It fails when:
/// * an observed opcode is **not** in [`DOCUMENTED_GENERIC`] — a new gap the loader
///   must model (or the baseline must grow, with a cross-crate handoff), or
/// * a [`DOCUMENTED_GENERIC`] entry is **no longer observed** — the pin is stale.
///
/// A missing OTP install skips loudly (see the module docs).
#[test]
fn compiler_emitted_opcodes_are_all_modelled() {
    if let Some(reason) = skip_reason("the residual-generic census") {
        eprintln!("{reason}");
        return;
    }

    let Some(sweep) = all_otp_beams() else {
        eprintln!(
            "SKIP: could not resolve/read the OTP `lib` root (need `erl` on PATH or $ERTS_LIB_DIR) \
             — the residual-generic census requires the installed OTP `.beam` files"
        );
        return;
    };

    let mut observed: BTreeMap<u8, Site> = BTreeMap::new();
    let mut module_count = 0usize;
    let mut instruction_count = 0usize;
    for path in &sweep.beams {
        let module_name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let module = beam_loader::load_file(path)
            .unwrap_or_else(|error| panic!("{} must load: {error:?}", path.display()));
        instruction_count += module
            .functions
            .iter()
            .map(|function| function.code.len())
            .sum::<usize>();
        census(&module, &module_name, &mut observed);
        module_count += 1;
    }

    let names = observed_names(&observed);
    println!(
        "CENSUS: {} module(s), {instruction_count} instruction(s); {} distinct residual \
         Instr::Generic opcode(s)",
        module_count,
        names.len()
    );
    print!("CENSUS DETAIL (opcode order):\n{}", render(&observed));

    // Non-vacuity: the sweep must actually be broad and non-empty, or an empty census
    // would prove nothing about "a broken walk" vs "no gaps". The floor is well below
    // the real count (≈1366 modules across ≈34 apps) so an OTP point release
    // adding/removing a few does not break it, but a broken walk does.
    assert!(
        module_count >= 1000,
        "the census swept only {module_count} module(s) — expected the full OTP `lib` root (≈1366)"
    );
    assert!(
        instruction_count > 0,
        "the census saw no instructions at all — the sweep is vacuous"
    );

    // Every observed opcode must be documented; every documented opcode must still be
    // observed (so the pin cannot silently rot into a rubber-stamp).
    let undocumented: Vec<String> = names
        .iter()
        .filter(|name| !DOCUMENTED_GENERIC.contains(&name.as_str()))
        .cloned()
        .collect();
    if !undocumented.is_empty() {
        let mut detail = String::new();
        for name in &undocumented {
            if let Some(site) = observed.values().find(|site| site.op.name() == name) {
                let _ = writeln!(
                    detail,
                    "  - {name} (opcode {}): {}x, e.g. {}",
                    site.op.as_u8(),
                    site.count,
                    site.example
                );
            }
        }
        panic!(
            "the installed OTP compiler emits {} opcode(s) that beam_loader leaves unmodelled \
             (Instr::Generic) and that are not in DOCUMENTED_GENERIC:\n{detail}\
             Either model them as typed Instr variants (a cross-crate change with beam_engine, \
             which matches Instr exhaustively) or add them to DOCUMENTED_GENERIC with a handoff \
             note. Full census:\n{}",
            undocumented.len(),
            render(&observed)
        );
    }

    let unobserved: Vec<&str> = DOCUMENTED_GENERIC
        .iter()
        .copied()
        .filter(|name| !names.iter().any(|observed| observed == name))
        .collect();
    assert!(
        unobserved.is_empty(),
        "DOCUMENTED_GENERIC lists opcode(s) the installed OTP compiler no longer emits: \
         {unobserved:?} — the pin is stale; tighten it to:\n{names:?}"
    );
}

/// **Positive control for the census detector.** An empty census only means something if
/// the detector can *see* a residual `Instr::Generic` at all — otherwise a broken
/// extraction would pass vacuously.
///
/// This hands the collector a synthetic instruction stream mixing residual `Generic`
/// instructions (`is_constant`, `make_fun2` — both `genop.tab` entries the standard
/// compiler never emits, confirmed unmodelled in `src/lower/instr.rs`) with a typed one,
/// and asserts it reports **exactly** the generic opcodes, with the site count and the
/// example module tracked. It needs no OTP install.
#[test]
fn census_detects_a_residual_generic_opcode() {
    let instructions = [
        Instr::Generic {
            op: Opcode::is_constant,
            operands: Vec::new(),
            offset: 0,
        },
        Instr::IntCodeEnd,
        Instr::Generic {
            op: Opcode::is_constant,
            operands: Vec::new(),
            offset: 1,
        },
        Instr::Generic {
            op: Opcode::make_fun2,
            operands: Vec::new(),
            offset: 2,
        },
        Instr::Return,
    ];

    let mut observed = BTreeMap::new();
    census_instrs(instructions.iter(), "synthetic", &mut observed);

    assert_eq!(
        observed_names(&observed),
        vec!["is_constant", "make_fun2"],
        "the census must report exactly the residual Generic opcodes it is handed"
    );
    let is_constant = observed
        .get(&Opcode::is_constant.as_u8())
        .expect("is_constant must be censused");
    assert_eq!(is_constant.count, 2, "both is_constant sites count");
    assert_eq!(is_constant.example, "synthetic");
}
