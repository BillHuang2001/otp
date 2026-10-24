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

//! Equivalence of the three `bs_match` execution routes, and the routing pins.
//!
//! A `bs_match` command list can be executed three ways: the **generic** walk
//! (`commands::run_commands`, which decodes every command word per execution), the
//! **prepared** walk (`run_prepared`, which executes a pre-resolved `MatchCmd` list) and
//! the **straight-line** form (`run_fast`, which collapses an
//! `(ensure_at_least, integer)` run into one load and one store per field). Nothing
//! about the *result* distinguishes them — that is the whole point — so this module
//! asserts it two ways:
//!
//! * **route equivalence**: every shape below is run through all the routes it is
//!   admitted to and the outcome, the bound registers and the final context position
//!   must be identical, and
//! * **routing pins**: `route_counts()` (the `#[cfg(test)]` counters in
//!   [`super`]) proves which route actually ran, so a shape that silently drifted onto
//!   (or off) the specialised path fails here instead of only showing up as a
//!   benchmark number.
//!
//! Nothing in this module measures anything: the manager owns the timings.

use std::sync::Arc;

use super::*;

use crate::families::bitstrings::matching::read_ctx;
use crate::families::bitstrings::{exec as family_exec, raw};
use super::super::commands::run_commands;

use beam_loader::{Instr, NO_FAIL};
use beam_term::Eterm;

use crate::context::MinimalContext;
use crate::interpreter::LabelMap;

/// The `bs_match` failure label's handler position in the fixture label map.
const FAIL: Label = Label(1);

/// A non-zero module checksum: what a `.beam` load produces, and what
/// [`run_commands_prepared`] requires before it caches a program (a synthetic image's
/// all-zero checksum means "no content identity", so `(func, pc)` is not an identity).
const REAL_CHECKSUM: [u8; 16] = [0x5a; 16];

/// `[little]`, `[signed]` and `[native]` as ETF literal payloads (`LiteralArea`
/// indices `0`/`1`/`2`): the only shape [`decode_flags`] accepts for a non-empty flag
/// list, so it is how a flag-carrying `bs_match` field reaches the fixture.
const LITTLE_ETF: &[u8] = &[131, 108, 0, 0, 0, 1, 119, 6, b'l', b'i', b't', b't', b'l', b'e', 106];
const SIGNED_ETF: &[u8] = &[131, 108, 0, 0, 0, 1, 119, 6, b's', b'i', b'g', b'n', b'e', b'd', 106];
const NATIVE_ETF: &[u8] = &[131, 108, 0, 0, 0, 1, 119, 6, b'n', b'a', b't', b'i', b'v', b'e', 106];
const LITTLE: Operand = Operand::Literal(0);
const SIGNED: Operand = Operand::Literal(1);
const NATIVE: Operand = Operand::Literal(2);

/// Which route a command list is executed by.
#[derive(Clone, Copy, Debug)]
enum Route {
    /// `commands::run_commands` — the per-element decode.
    Generic,
    /// `run_prepared` — the pre-resolved `MatchCmd` list.
    Prepared,
    /// `run_fast` — the straight-line byte-read form.
    Fast,
}

/// What a route produced: the outcome, the two observable registers and the final
/// context position.
///
/// The outcome is folded into a `String` because [`beam_common::BeamError`] carries no
/// `PartialEq`; its `Debug` form is the exact typed error, which is what "the same
/// typed error" means.
struct Run {
    outcome: String,
    /// `x(2)` and `x(3)` — the two registers the cases bind.
    regs: [Option<Eterm>; 2],
    /// The context's own `sb->start` after the run.
    position: Option<usize>,
}

impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Run")
            .field("outcome", &self.outcome)
            .field(
                "regs",
                &self.regs.map(|reg| reg.map(|term| term.raw())),
            )
            .field("position", &self.position)
            .finish()
    }
}

/// Two routes run against **two fixtures**, i.e. two heaps, so a boxed term (a bignum,
/// a sub-binary) is a different address in each by construction. An immediate compares
/// by value; a boxed term compares by shape (bound vs unbound), and the cases that bind
/// one assert its content separately.
impl PartialEq for Run {
    fn eq(&self, other: &Self) -> bool {
        self.outcome == other.outcome
            && self.position == other.position
            && self.regs.iter().zip(other.regs.iter()).all(|(a, b)| match (a, b) {
                (None, None) => true,
                (Some(a), Some(b)) => {
                    !(beam_term::is_small(*a) || beam_term::is_small(*b)) || a.raw() == b.raw()
                }
                _ => false,
            })
    }
}

/// A machine, a module and a context whose `x(0)` holds `subject` and whose `x(1)`
/// holds an open match context over it.
///
/// `real` gives the module a `.beam`-shaped checksum, which is what makes
/// `(function, code offset)` an identity for the prepared-program cache.
fn fixture(subject: &[u8], bits: usize, regs: u32, real: bool) -> (Machine, Module, MinimalContext) {
    let mut machine = Machine::with_max_register(regs);
    let mut ctx = MinimalContext::new();
    let mut module = Module::default();
    for name in [
        "no_fail",
        "ensure_at_least",
        "ensure_exactly",
        "skip",
        "integer",
        "binary",
        "get_tail",
        "=:=",
        "all",
        "bogus",
        "little",
        "signed",
        "native",
    ] {
        module.atom_table.intern(name);
    }
    let literals = beam_loader::LiteralArea::decode(
        &[
            LITTLE_ETF.to_vec(),
            SIGNED_ETF.to_vec(),
            NATIVE_ETF.to_vec(),
        ],
        &mut module.atom_table,
    )
    .expect("a three-literal flag area");
    module.literals = Arc::new(literals);
    if real {
        module.checksum = REAL_CHECKSUM;
    }
    let subject = raw::make_bits(&mut machine, &mut ctx, subject, bits).expect("a bitstring");
    machine.set_x(0, subject).expect("root the subject");
    let step = family_exec(
        &mut machine,
        &module,
        &LabelMap::new(),
        &Instr::BsStartMatch3 {
            fail: NO_FAIL,
            bin: Operand::X(0),
            live: 1,
            dst: Operand::X(1),
        },
        0,
        0,
        &mut ctx,
    )
    .expect("bs_start_match3 reports no raise")
    .expect("the family claims bs_start_match3");
    assert_eq!(step, Step::Next, "a bitstring subject enters a match");
    (machine, module, ctx)
}

/// The command atom `name` of the fixture's module.
fn atom(module: &Module, name: &str) -> Operand {
    Operand::Atom(
        module
            .atom_table
            .lookup(name)
            .unwrap_or_else(|| panic!("atom {name:?} interned by the fixture")),
    )
}

/// Run one command list through `route` and report everything observable.
fn run(
    route: Route,
    subject: &[u8],
    bits: usize,
    regs: u32,
    build: &dyn Fn(&Module) -> Vec<Operand>,
) -> Run {
    let (mut machine, module, mut ctx) = fixture(subject, bits, regs, false);
    let commands = build(&module);
    let ctx_op = Operand::X(1);
    let mut labels = LabelMap::new();
    labels.insert(FAIL, (0, 9));
    let result = match route {
        Route::Generic => {
            run_commands(&mut machine, &module, &labels, FAIL, &ctx_op, &commands, &mut ctx)
        }
        Route::Prepared | Route::Fast => {
            let program = prepare(&module, &commands).expect("a pre-resolvable command list");
            match route {
                Route::Prepared => run_prepared(
                    &mut machine,
                    &module,
                    &labels,
                    FAIL,
                    &ctx_op,
                    &program.cmds,
                    &mut ctx,
                ),
                _ => run_fast(
                    &mut machine,
                    &module,
                    &labels,
                    FAIL,
                    &ctx_op,
                    program
                        .fast
                        .as_ref()
                        .expect("the shape is a straight-line fast form"),
                    &mut ctx,
                ),
            }
        }
    };
    let outcome = match &result {
        Ok(Some(step)) => format!("step {step:?}"),
        Ok(None) => "not this family".to_string(),
        Err(error) => format!("error {error:?}"),
    };
    let regs = [machine.x(2), machine.x(3)];
    let position = read_ctx(&machine, &mut ctx, &ctx_op, "test")
        .ok()
        .map(|(_, _, start, _)| start);
    Run {
        outcome,
        regs,
        position,
    }
}

/// Whether `build`'s command list prepares into a straight-line fast form.
fn prepares_fast(
    subject: &[u8],
    bits: usize,
    regs: u32,
    build: &dyn Fn(&Module) -> Vec<Operand>,
) -> bool {
    let (_machine, module, _ctx) = fixture(subject, bits, regs, false);
    let commands = build(&module);
    prepare(&module, &commands)
        .map(|program| program.fast.is_some())
        .unwrap_or(false)
}

/// Assert that the routes `build` is admitted to all agree, and that a `fast` shape
/// really is one (and a non-`fast` shape really is not).
fn assert_routes_agree(
    subject: &[u8],
    bits: usize,
    regs: u32,
    fast: bool,
    build: &dyn Fn(&Module) -> Vec<Operand>,
) {
    let generic = run(Route::Generic, subject, bits, regs, build);
    let prepared = run(Route::Prepared, subject, bits, regs, build);
    assert_eq!(
        generic, prepared,
        "the prepared command walk diverged from the generic walk"
    );
    assert_eq!(
        prepares_fast(subject, bits, regs, build),
        fast,
        "the fast-form admission decision changed"
    );
    if fast {
        let specialised = run(Route::Fast, subject, bits, regs, build);
        assert_eq!(
            generic, specialised,
            "the straight-line form diverged from the generic walk"
        );
    }
}

// ---------------------------------------------------------------------------
// The straight-line form: every specialised field width and position
// ---------------------------------------------------------------------------

/// The benchmark's own shape: `ensure_at_least 8 1` then `integer … 8 1 Dst`, i.e. one
/// byte per element out of a byte-aligned subject.
#[test]
fn a_byte_field_from_a_byte_aligned_subject_reads_the_same_on_every_route() {
    let subject = [0x01u8, 0x02, 0x03, 0x04];
    assert_routes_agree(&subject, 32, 8, true, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(8),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2), // Live
            Operand::Nil,        // Flags (big-endian, unsigned)
            Operand::Integer(8), // Size
            Operand::Integer(1), // Unit
            Operand::X(2),       // Dst
        ]
    });
}

/// Every whole-byte width the fast form admits: 8, 16, 32 and 64 bits.
#[test]
fn every_specialised_field_width_reads_the_same_on_every_route() {
    let subject: Vec<u8> = (1..=8u8).collect();
    for nbits in [8usize, 16, 32, 64] {
        assert_routes_agree(&subject, 64, 8, true, &|module: &Module| {
            vec![
                atom(module, "ensure_at_least"),
                Operand::Integer(nbits as i64),
                Operand::Integer(1),
                atom(module, "integer"),
                Operand::Integer(2),
                Operand::Nil,
                Operand::Integer(nbits as i64),
                Operand::Integer(1),
                Operand::X(2),
            ]
        });
    }
}

/// A 64-bit field whose top bit is set does **not** fit a small integer, so the read
/// reserves through the rooted seam and the context capture must be re-derived before
/// the commit — the one branch of the straight-line form that is not a plain load.
#[test]
fn a_field_that_builds_a_bignum_relocates_and_is_re_read_on_every_route() {
    let subject = [0xffu8; 8];
    assert_routes_agree(&subject, 64, 8, true, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(64),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(64),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });

    // Two bignum fields in one program: the second one runs after the first one's
    // relocation, so it proves the capture was re-read rather than reused.
    let wide = [0xaau8; 16];
    assert_routes_agree(&wide, 128, 8, true, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(64),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(64),
            Operand::Integer(1),
            Operand::X(2),
            atom(module, "ensure_at_least"),
            Operand::Integer(64),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(64),
            Operand::Integer(1),
            Operand::X(3),
        ]
    });
}

/// Two fields in one program, each with its own guard.
#[test]
fn two_guarded_fields_in_one_program_agree_on_every_route() {
    let subject = [0x11u8, 0x22, 0x33, 0x44];
    assert_routes_agree(&subject, 32, 8, true, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(8),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(1),
            Operand::X(2),
            atom(module, "ensure_at_least"),
            Operand::Integer(16),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(16),
            Operand::Integer(1),
            Operand::X(3),
        ]
    });
}

/// A field with **no** `ensure_at_least` guard: the width check alone has to protect
/// the read.
#[test]
fn an_unguarded_field_agrees_on_every_route() {
    let subject = [0x7fu8, 0x00];
    assert_routes_agree(&subject, 16, 8, true, &|module: &Module| {
        vec![
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// `unit > 1` scales the width (`Size * Unit` bits), and the fast form must use the
/// scaled width for both the guard and the read.
#[test]
fn a_unit_scaled_field_agrees_on_every_route() {
    let subject = [0x12u8, 0x34, 0x56, 0x78];
    assert_routes_agree(&subject, 32, 8, true, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(8),
            Operand::Integer(2),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(2),
            Operand::X(2),
        ]
    });
}

/// The alignment-only guard (`Size == 0`, `Unit == 8`) tests the remaining length
/// without consuming anything, exactly as `i_bs_test_unit8` does.
#[test]
fn an_alignment_only_guard_agrees_on_every_route() {
    let subject = [0x01u8, 0x02];
    assert_routes_agree(&subject, 16, 8, true, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(0),
            Operand::Integer(8),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// A zero-width field is the fast form's boundary case: it consumes nothing and binds
/// `0`.
#[test]
fn a_zero_width_field_agrees_on_every_route() {
    let subject = [0x01u8];
    assert_routes_agree(&subject, 8, 8, true, &|module: &Module| {
        vec![
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(0),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// A guard that fails and a width that overruns both take the instruction's fail path,
/// and neither may advance the context.
#[test]
fn a_failing_guard_and_an_overrunning_width_fail_identically() {
    // The guard fails: 8 bits requested, none left.
    assert_routes_agree(&[], 0, 8, true, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(8),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });

    // No guard at all, and the field is wider than the subject.
    assert_routes_agree(&[0x01u8], 8, 8, true, &|module: &Module| {
        vec![
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(16),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// A non-byte-aligned subject: the window's bit offset is the frozen subject model's,
/// and the fast form reads it through the same `read_uint_be`.
#[test]
fn a_non_byte_aligned_subject_agrees_on_every_route() {
    let subject = [0b1010_0110u8, 0b0110_1010];
    assert_routes_agree(&subject, 12, 8, true, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(8),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

// ---------------------------------------------------------------------------
// The prepared command walk: every command tag, and every field the fast form refuses
// ---------------------------------------------------------------------------

/// A signed field: `BSF_SIGNED` keeps the prepared walk (the fast form is unsigned
/// only), and the two must agree on the sign-extended value.
#[test]
fn a_signed_field_keeps_the_prepared_walk_and_agrees() {
    let subject = [0x80u8, 0xff];
    assert_routes_agree(&subject, 16, 8, false, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(8),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2),
            SIGNED,
            Operand::Integer(8),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// A little-endian field: resolved through `field_is_little` on both routes, and
/// refused by the fast form.
#[test]
fn a_little_endian_field_keeps_the_prepared_walk_and_agrees() {
    let subject = [0x34u8, 0x12];
    assert_routes_agree(&subject, 16, 8, false, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(16),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2),
            LITTLE,
            Operand::Integer(16),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// A `native` field is admitted to the fast form **exactly** when the target's own
/// order is big-endian — the one flags decision the two must agree on per target.
#[test]
fn a_native_field_keeps_or_takes_the_fast_walk_by_target_order_and_agrees() {
    let subject = [0x01u8, 0x02];
    assert_routes_agree(
        &subject,
        16,
        8,
        !cfg!(target_endian = "little"),
        &|module: &Module| {
            vec![
                atom(module, "ensure_at_least"),
                Operand::Integer(16),
                Operand::Integer(1),
                atom(module, "integer"),
                Operand::Integer(2),
                NATIVE,
                Operand::Integer(16),
                Operand::Integer(1),
                Operand::X(2),
            ]
        },
    );
}

/// A width that is not a whole number of bytes keeps the prepared walk.
#[test]
fn a_non_byte_width_keeps_the_prepared_walk_and_agrees() {
    let subject = [0b1010_1010u8, 0b1100_0000];
    assert_routes_agree(&subject, 16, 8, false, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(12),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(12),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// A field wider than eight bytes keeps the prepared walk (the wide reader).
#[test]
fn a_field_wider_than_eight_bytes_keeps_the_prepared_walk_and_agrees() {
    let subject = [0x11u8; 12];
    assert_routes_agree(&subject, 96, 8, false, &|module: &Module| {
        vec![
            atom(module, "ensure_at_least"),
            Operand::Integer(72),
            Operand::Integer(1),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(72),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// `skip` advances the position without binding anything.
#[test]
fn a_skip_command_keeps_the_prepared_walk_and_agrees() {
    let subject = [0x01u8, 0x02, 0x03, 0x04];
    assert_routes_agree(&subject, 32, 8, false, &|module: &Module| {
        vec![
            atom(module, "skip"),
            Operand::Integer(16),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// `ensure_exactly` tests the remaining length and nothing else.
#[test]
fn an_ensure_exactly_command_keeps_the_prepared_walk_and_agrees() {
    let subject = [0x01u8, 0x02];
    assert_routes_agree(&subject, 16, 8, false, &|module: &Module| {
        vec![
            atom(module, "ensure_exactly"),
            Operand::Integer(16),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(16),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });

    // The same command on a subject of the wrong length fails.
    assert_routes_agree(&[0x01u8], 8, 8, false, &|module: &Module| {
        vec![
            atom(module, "ensure_exactly"),
            Operand::Integer(16),
            atom(module, "integer"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(16),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// `binary` with a static width builds a sub-binary; `all` takes the whole window.
#[test]
fn a_binary_command_keeps_the_prepared_walk_and_agrees() {
    let subject = [0x01u8, 0x02, 0x03, 0x04];
    assert_routes_agree(&subject, 32, 8, false, &|module: &Module| {
        vec![
            atom(module, "binary"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
    assert_routes_agree(&subject, 32, 8, false, &|module: &Module| {
        vec![
            atom(module, "binary"),
            Operand::Integer(2),
            Operand::Nil,
            atom(module, "all"),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
    // A `binary` wider than the subject fails.
    assert_routes_agree(&[0x01u8], 8, 8, false, &|module: &Module| {
        vec![
            atom(module, "binary"),
            Operand::Integer(2),
            Operand::Nil,
            Operand::Integer(16),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// `get_tail` binds the whole remaining window and leaves the position alone.
#[test]
fn a_get_tail_command_keeps_the_prepared_walk_and_agrees() {
    let subject = [0x01u8, 0x02, 0x03];
    assert_routes_agree(&subject, 24, 8, false, &|module: &Module| {
        vec![
            atom(module, "skip"),
            Operand::Integer(8),
            atom(module, "get_tail"),
            Operand::Integer(2),
            Operand::Integer(1),
            Operand::X(2),
        ]
    });
}

/// `'=:='` compares a field with a literal, and consumes it on success only.
#[test]
fn an_eq_command_keeps_the_prepared_walk_and_agrees() {
    let subject = [0xabu8, 0xcdu8];
    assert_routes_agree(&subject, 16, 8, false, &|module: &Module| {
        vec![
            atom(module, "=:="),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(0xab),
        ]
    });
    // The mismatching counterpart fails identically.
    assert_routes_agree(&subject, 16, 8, false, &|module: &Module| {
        vec![
            atom(module, "=:="),
            Operand::Nil,
            Operand::Integer(8),
            Operand::Integer(0xac),
        ]
    });
}

/// A `Value` that is not a small integer is the typed error, on the prepared walk too.
#[test]
fn a_non_integer_eq_value_is_the_same_typed_error_on_both_routes() {
    let subject = [0xabu8];
    assert_routes_agree(&subject, 8, 8, false, &|module: &Module| {
        vec![
            atom(module, "=:="),
            Operand::Nil,
            Operand::Integer(8),
            Operand::X(7), // unbound: reads as `THE_NON_VALUE`, not an integer
        ]
    });
}

// ---------------------------------------------------------------------------
// Declined shapes: `prepare` returns `None`, so the generic walk answers
// ---------------------------------------------------------------------------

/// A command list `prepare` cannot pre-resolve is not cached as a program at all: the
/// generic walk runs and reproduces its exact typed error or its exact dynamic
/// behaviour.
#[test]
fn a_declined_command_list_falls_back_to_the_generic_walk() {
    // An unknown command name.
    let (machine, module, _ctx) = fixture(&[0x01u8], 8, 8, false);
    let _ = machine;
    let unknown = vec![atom(&module, "bogus"), Operand::Integer(0)];
    assert!(
        prepare(&module, &unknown).is_none(),
        "an unknown command must decline"
    );

    // A non-atom command word.
    let not_an_atom = vec![Operand::Integer(1)];
    assert!(
        prepare(&module, &not_an_atom).is_none(),
        "a non-atom command word must decline"
    );

    // A truncated command group.
    let truncated = vec![
        atom(&module, "integer"),
        Operand::Integer(0),
        Operand::Nil,
    ];
    assert!(
        prepare(&module, &truncated).is_none(),
        "a truncated command group must decline"
    );

    // A *dynamic* size operand: only the generic walk can read a runtime width.
    let dynamic = vec![
        atom(&module, "integer"),
        Operand::Integer(2),
        Operand::Nil,
        Operand::X(3),
        Operand::Integer(1),
        Operand::X(2),
    ];
    assert!(
        prepare(&module, &dynamic).is_none(),
        "a runtime-size field must decline"
    );

    // Each of those still runs, and runs through the generic walk: the counters do not
    // move (the fixture has a synthetic, no-content-identity checksum, so it is never
    // cached either).
    for commands in [unknown, not_an_atom, truncated] {
        let (mut machine, module, mut ctx) = fixture(&[0x01u8], 8, 8, true);
        let mut labels = LabelMap::new();
        labels.insert(FAIL, (0, 9));
        let before = route_counts();
        let prepared = run_commands_prepared(
            &mut machine,
            &module,
            &labels,
            FAIL,
            &Operand::X(1),
            &commands,
            7,
            0,
            &mut ctx,
        );
        assert_eq!(
            route_counts(),
            before,
            "a declined shape must not be prepared"
        );
        let (mut generic_machine, generic_module, mut generic_ctx) = fixture(&[0x01u8], 8, 8, true);
        let generic = run_commands(
            &mut generic_machine,
            &generic_module,
            &labels,
            FAIL,
            &Operand::X(1),
            &commands,
            &mut generic_ctx,
        );
        assert_eq!(
            format!("{prepared:?}"),
            format!("{generic:?}"),
            "the declined shape must report the generic walk's own answer"
        );
        let _ = (&module, &mut ctx);
    }
}

// ---------------------------------------------------------------------------
// Routing pins: which route actually ran
// ---------------------------------------------------------------------------

/// The comprehension shape — `ensure_at_least 8 1` + `integer … 8 1 Dst` — is the fast
/// form, and a real (content-identity) module is what makes the cache answer it.
#[test]
fn the_comprehension_shape_takes_the_straight_line_form() {
    let (mut machine, module, mut ctx) = fixture(&[0x01u8, 0x02, 0x03, 0x04], 32, 8, true);
    let commands = vec![
        atom(&module, "ensure_at_least"),
        Operand::Integer(8),
        Operand::Integer(1),
        atom(&module, "integer"),
        Operand::Integer(2),
        Operand::Nil,
        Operand::Integer(8),
        Operand::Integer(1),
        Operand::X(2),
    ];
    let mut labels = LabelMap::new();
    labels.insert(FAIL, (0, 9));
    let before = route_counts();
    let step = run_commands_prepared(
        &mut machine,
        &module,
        &labels,
        FAIL,
        &Operand::X(1),
        &commands,
        1,
        0,
        &mut ctx,
    )
    .expect("bs_match reports no raise")
    .expect("the family claims bs_match");
    assert_eq!(step, Step::Next, "the command list matches");
    let after = route_counts();
    assert!(after.0 > before.0, "the straight-line form ran");
    assert_eq!(after.1, before.1, "the prepared command walk did not run");
    assert!(
        after.2 > before.2,
        "the straight-line form read the field itself"
    );
    assert_eq!(beam_term::signed_value(machine.x(2).expect("x(2) bound")), 0x01);

    // The second execution of the same instruction hits the cached program: same
    // route, same answer, one more field.
    let before = route_counts();
    run_commands_prepared(
        &mut machine,
        &module,
        &labels,
        FAIL,
        &Operand::X(1),
        &commands,
        1,
        0,
        &mut ctx,
    )
    .expect("bs_match reports no raise")
    .expect("the family claims bs_match");
    let after = route_counts();
    assert!(
        after.0 > before.0,
        "the cached program is the straight-line form"
    );
    assert!(after.2 > before.2);
}

/// A shape the fast form refuses runs the prepared command walk (not the generic one).
#[test]
fn a_non_specialised_shape_takes_the_prepared_command_walk() {
    let (mut machine, module, mut ctx) = fixture(&[0xab, 0xcd], 16, 8, true);
    let commands = vec![
        atom(&module, "=:="),
        Operand::Nil,
        Operand::Integer(8),
        Operand::Integer(0xab),
    ];
    let mut labels = LabelMap::new();
    labels.insert(FAIL, (0, 9));
    let before = route_counts();
    let step = run_commands_prepared(
        &mut machine,
        &module,
        &labels,
        FAIL,
        &Operand::X(1),
        &commands,
        2,
        0,
        &mut ctx,
    )
    .expect("bs_match reports no raise")
    .expect("the family claims bs_match");
    assert_eq!(step, Step::Next);
    let after = route_counts();
    assert!(after.1 > before.1, "the prepared walk ran");
    assert_eq!(after.0, before.0, "the straight-line form did not run");
}

/// A synthetic image (all-zero checksum) has no content identity, so it keeps the
/// generic walk even for a shape the fast form would admit.
#[test]
fn a_synthetic_image_keeps_the_generic_walk() {
    let (mut machine, module, mut ctx) = fixture(&[0x01u8, 0x02], 16, 8, false);
    assert_eq!(module.checksum(), [0u8; 16], "the fixture is synthetic");
    let commands = vec![
        atom(&module, "ensure_at_least"),
        Operand::Integer(8),
        Operand::Integer(1),
        atom(&module, "integer"),
        Operand::Integer(2),
        Operand::Nil,
        Operand::Integer(8),
        Operand::Integer(1),
        Operand::X(2),
    ];
    let mut labels = LabelMap::new();
    labels.insert(FAIL, (0, 9));
    let before = route_counts();
    let step = run_commands_prepared(
        &mut machine,
        &module,
        &labels,
        FAIL,
        &Operand::X(1),
        &commands,
        3,
        0,
        &mut ctx,
    )
    .expect("bs_match reports no raise")
    .expect("the family claims bs_match");
    assert_eq!(step, Step::Next);
    let after = route_counts();
    assert_eq!(
        (after.0, after.1),
        (before.0, before.1),
        "a synthetic image must not reach either prepared route"
    );
    assert_eq!(beam_term::signed_value(machine.x(2).expect("x(2) bound")), 0x01);
}
