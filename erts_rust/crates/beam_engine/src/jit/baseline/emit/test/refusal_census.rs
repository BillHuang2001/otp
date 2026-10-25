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

//! A **test-only refusal census**: *why* the baseline tier's `test` / `select_val` /
//! `select_tuple_arity` inliners refuse the shapes they still refuse, which BIF
//! names the `bif` / `gc_bif` / `bif0` deopt pool actually covers, and — in the
//! `Bitstring refusals` section — why each `Bs*` bitstring shape the tier *has* an
//! inline arm for was still not lowered (plus which `Bs*` opcodes have no arm at all).
//!
//! The sibling [`super::super::census`] counts *how many* refusals each variant has;
//! this attributes every refusal to the exact predicate / operand shape that caused
//! it, which is the evidence a follow-up inline-set widening is chosen from. It is
//! declared behind `#[cfg(test)]` from [`super::test`], so it is absent from every
//! production build (zero cost by construction).
//!
//! Every attribution is derived from the emitter's **own** predicate — the census
//! asks the shape function first and only classifies a genuine refusal — so the
//! report cannot drift from what the tier actually lowers.
//!
//! Run with `--nocapture`:
//! `... cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine refusal -- --nocapture`

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_loader::{Instr, Label, Module, Operand, Reg as RegRef, TestKind, NO_FAIL};
use beam_term::MAX_ARITYVAL;

use crate::interpreter::{build_label_map, LabelMap};
use crate::machine::MAX_REGISTER;

use super::super::arith::{fast_arith, import_names};
use super::super::bif::bif_shape;
use super::super::bitstr_inline;
use super::super::compare::comparison_shape;
use super::super::operands::{constant_word, x_register, Loc};
use super::{test_plan, test_src};

/// A safety bound on the installed-OTP sample (mirrors the sibling census).
const OTP_MODULE_CAP: usize = 4096;

/// The operand shape name of one operand — what the report prints.
fn shape(op: &Operand) -> String {
    match op {
        Operand::X(n) if *n < MAX_REGISTER => "X(in-bound)".to_string(),
        Operand::X(_) => "X(out-of-bound)".to_string(),
        Operand::Y(_) => "Y".to_string(),
        Operand::Typed {
            reg: RegRef::X(n),
            ..
        } if *n < MAX_REGISTER => "X(in-bound,tr)".to_string(),
        Operand::Typed {
            reg: RegRef::X(_), ..
        } => "X(out-of-bound,tr)".to_string(),
        Operand::Typed {
            reg: RegRef::Y(_), ..
        } => "Y(tr)".to_string(),
        Operand::FloatReg(_) => "FloatReg".to_string(),
        Operand::Literal(_) => "Literal".to_string(),
        Operand::Integer(_) => "Integer".to_string(),
        Operand::Character(_) => "Character".to_string(),
        Operand::Atom(_) => "Atom".to_string(),
        Operand::Nil => "Nil".to_string(),
        Operand::BigInteger(_) => "BigInteger".to_string(),
        Operand::Label(_) => "Label".to_string(),
        Operand::List(_) => "List".to_string(),
        Operand::Allocation(_) => "Allocation".to_string(),
    }
}

/// The `test` kinds the emitter serves through `test_helper.rs::jit_test` (the
/// comparison helpers) rather than by a *static* `jit_step_one` deopt: a comparison
/// that `test_plan` declines is still inlined as a helper call, so it is NOT a static
/// deopt and is excluded from this census (the sibling census agrees — no comparison
/// kind appears in its deopt table).
fn is_comparison(kind: TestKind) -> bool {
    matches!(
        kind,
        TestKind::IsEqExact
            | TestKind::IsNeExact
            | TestKind::IsEq
            | TestKind::IsNe
            | TestKind::IsLt
            | TestKind::IsGe
    )
}

/// Why `test_plan` refuses this `test` (or `None` when it inlines it): the exact
/// predicate / operand shape, classified after the plan declined.
fn test_refusal(
    module: &Module,
    labels: &LabelMap,
    kind: TestKind,
    fail: Label,
    args: &[Operand],
) -> Option<String> {
    if test_plan(module, kind, fail, args, labels).is_some() {
        return None;
    }
    if fail == NO_FAIL {
        return Some("fail=NO_FAIL".to_string());
    }
    if labels.resolve(fail).is_err() {
        return Some("unresolved fail label".to_string());
    }
    match kind {
        TestKind::IsEqExact
        | TestKind::IsNeExact
        | TestKind::IsEq
        | TestKind::IsNe
        | TestKind::IsLt
        | TestKind::IsGe => match (args.first(), args.get(1)) {
            (Some(a), Some(b)) => Some(format!("comparison operands: {} + {}", shape(a), shape(b))),
            _ => Some("comparison operand count".to_string()),
        },
        TestKind::IsBoolean => match args.first() {
            Some(op) if test_src(op).is_none() => Some(format!("operand: {}", shape(op))),
            Some(_) => Some("no true/false atom interned".to_string()),
            None => Some("missing operand".to_string()),
        },
        // Every remaining kind reads `args[0]` through `test_src`.
        _ => {
            match args.first() {
                Some(op) if test_src(op).is_none() => {
                    return Some(format!("operand: {}", shape(op)));
                }
                Some(_) => {}
                None => return Some("missing operand".to_string()),
            }
            match kind {
                TestKind::TestArity | TestKind::IsFunction2 => arity_reason(kind, args),
                TestKind::IsTaggedTuple => arity_reason(kind, args),
                _ => Some("other".to_string()),
            }
        }
    }
}

/// The arity/tag-operand reason of an arity-shaped `test` whose `args[0]` is already
/// known loadable.
fn arity_reason(kind: TestKind, args: &[Operand]) -> Option<String> {
    let max = if kind == TestKind::IsFunction2 {
        0xFF
    } else {
        MAX_ARITYVAL as i64
    };
    match args.get(1) {
        Some(Operand::Integer(a)) => {
            if *a < 0 || *a > max {
                return Some(format!("arity operand out of range: {a}"));
            }
            if kind == TestKind::IsTaggedTuple {
                if *a == 0 {
                    return Some("is_tagged_tuple arity 0".to_string());
                }
                match args.get(2) {
                    Some(tag) if test_src(tag).is_none() => {
                        return Some(format!("tag operand: {}", shape(tag)));
                    }
                    Some(_) => return Some("other".to_string()),
                    None => return Some("missing tag operand".to_string()),
                }
            }
            Some("other".to_string())
        }
        Some(op) => Some(format!("arity operand: {}", shape(op))),
        None => Some("missing arity operand".to_string()),
    }
}

/// Why the `select_val` plan refuses this instruction, or `None` when it inlines it.
fn select_val_refusal(
    labels: &LabelMap,
    value: &Operand,
    fail: Label,
    pairs: &[(Operand, Label)],
) -> Option<String> {
    if fail == NO_FAIL {
        return Some("fail=NO_FAIL".to_string());
    }
    if labels.resolve(fail).is_err() {
        return Some("unresolved fail label".to_string());
    }
    if test_src(value).is_none() {
        return Some(format!("scrutinee: {}", shape(value)));
    }
    for (candidate, label) in pairs {
        if constant_word(candidate).is_none() {
            return Some(format!("candidate: {}", shape(candidate)));
        }
        if labels.resolve(*label).is_err() {
            return Some("unresolved case label".to_string());
        }
    }
    None
}

/// Why the `select_tuple_arity` plan refuses this instruction, or `None` when it
/// inlines it.
fn select_tuple_arity_refusal(
    labels: &LabelMap,
    tuple: &Operand,
    fail: Label,
    destinations: &[(Operand, Label)],
) -> Option<String> {
    if fail == NO_FAIL {
        return Some("fail=NO_FAIL".to_string());
    }
    if labels.resolve(fail).is_err() {
        return Some("unresolved fail label".to_string());
    }
    // The scrutinee must be an addressable `Loc` (an in-bound x-register or an
    // addressable `Y` slot) — exactly what `select_tuple_arity_plan` accepts.
    if Loc::of(tuple).is_none() {
        return Some(match x_register(tuple) {
            Some(_) => "tuple X out-of-bound".to_string(),
            None => format!("tuple: {}", shape(tuple)),
        });
    }
    for (candidate, label) in destinations {
        match candidate {
            Operand::Integer(n) if *n >= 0 => {}
            Operand::Integer(_) => return Some("negative arity candidate".to_string()),
            other => return Some(format!("arity candidate: {}", shape(other))),
        }
        if labels.resolve(*label).is_err() {
            return Some("unresolved case label".to_string());
        }
    }
    None
}

/// The `(module,function,arity)` of an import, or a placeholder.
fn bif_name(module: &Module, bif: beam_loader::ImportRef) -> String {
    match import_names(module, bif) {
        Some((m, f, a)) => format!("{m}:{f}/{a}"),
        None => "<unresolved import>".to_string(),
    }
}

/// Whether the emitter would **not** statically deopt `instr`, i.e. whether one of the
/// same shape predicates `emit_block` consults claims it.
///
/// The census must ask the emitter's own predicates (never a hardcoded BIF-name
/// allowlist) so the per-name `bif` / `gc_bif` pool self-maintains as the inline set
/// changes: a BIF that is genuinely lowered drops out of the ranking, and one that is
/// not stays in it. Only the **shape predicates** are consulted (no emitter is run), and
/// the `Instr::Bif` order mirrors `emit_block`'s `try_inline_comparison` then
/// `try_inline_bif`, the `Instr::GcBif` order its
/// `try_inline_gc_bif`/`try_inline_comparison`/`try_inline_bif`.
fn already_inlined(module: &Module, instr: &Instr) -> bool {
    match instr {
        Instr::Bif { .. } => {
            comparison_shape(module, instr).is_some() || bif_shape(module, instr).is_some()
        }
        Instr::GcBif { .. } => {
            fast_arith(module, instr).is_some()
                || comparison_shape(module, instr).is_some()
                || bif_shape(module, instr).is_some()
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Bitstring inline refusals
//
// The tier natively lowers nine `Bs*` shapes (`emit/bitstr_inline/`, `bs_create_bin`
// included); every other `Bs*` instruction goes to the family arm, and every one of the
// nine can still be *refused* by its own shape predicate. This section attributes those
// refusals.
//
// The vocabulary is closed: each reason string below is emitted from exactly one
// place, so the table cannot grow an unlisted bucket, and every `Bs*` instruction the
// corpus holds lands in exactly one of `inlined` / a reason / `no inline arm`.
// ---------------------------------------------------------------------------

/// (a) — a `bs_match` command list holding a command that builds a fresh term.
const BS_REASON_ALLOC: &str = "allocating command (binary/get_tail/=:=)";
/// (b) — a subject (or position/destination) operand that lives in the literal area.
const BS_REASON_LITERAL: &str = "literal-area subject";
/// (c) — a non-constant size/unit/stride operand (the register form).
const BS_REASON_RUNTIME: &str = "runtime size/unit/stride";
/// (d) — an `ensure_at_least` unit that is not a power of two (or is zero).
const BS_REASON_UNIT: &str = "non-power-of-two ensure_at_least unit";
/// (e) — the width rule: `nbits > MAX_SMALL`, or not `<= 16` and not `== 32`.
const BS_REASON_WIDTH: &str = "width rule (nbits > MAX_SMALL or not <=16/==32)";
/// (f) — `NO_FAIL` on a shape whose miss path has no `MatchFail::Family` arm.
const BS_REASON_NO_FAIL: &str = "NO_FAIL with no MatchFail::Family arm";
/// (g) — a real fail label that does not resolve in the module's label map.
const BS_REASON_LABEL: &str = "unresolved fail label";
/// (h) — more than one `Y` destination in one `bs_match` command list.
const BS_REASON_Y_DST: &str = ">1 Y destination";
/// (i) — a signed, little-endian or native-endian field.
const BS_REASON_FIELD: &str = "signed/little/native field";
/// The explicit catch-all: everything the nine rules above do not name (an unmodelled flag
/// atom, a truncated command group, a non-`Loc` destination that is not a literal, and
/// every `bs_create_bin` refusal `CreateBinShape::of`'s single `None` cannot discriminate).
const BS_REASON_OTHER: &str = "other";

/// What the bitstring inline set does with one instruction.
enum BsVerdict {
    /// Not a `Bs*` variant at all.
    NotBitstring,
    /// A `Bs*` variant the tier has **no** inline arm for.
    NoArm(&'static str),
    /// A `Bs*` variant an arm covers, whose shape predicate accepted it.
    Inlined(&'static str),
    /// A `Bs*` variant an arm covers, whose shape predicate refused it.
    Refused(&'static str, String),
}

/// The refusal reason for a shape whose only remaining objection is an operand that is
/// neither an in-bound `x` register nor an addressable `Y` slot: a **literal-area** operand
/// gets its own bucket, anything else the explicit catch-all.
fn operand_refusal(operands: &[&Operand]) -> String {
    if operands.iter().any(|op| matches!(op, Operand::Literal(_))) {
        BS_REASON_LITERAL.to_string()
    } else {
        BS_REASON_OTHER.to_string()
    }
}

/// (f)/(g) — the fail half every label-carrying shape shares, or `None` when the fail
/// operand is one the shape can inline. Only the shapes with **no** `MatchFail::Family` arm
/// come here: `bs_start_match3`/`4` accept `NO_FAIL` outright and classify it themselves.
fn fail_refusal(labels: &LabelMap, fail: Label) -> Option<String> {
    if fail == NO_FAIL {
        return Some(BS_REASON_NO_FAIL.to_string());
    }
    if labels.resolve(fail).is_err() {
        return Some(BS_REASON_LABEL.to_string());
    }
    None
}

/// (e) — the width rule over an already-decoded `size * unit`: `None` when the field is one
/// [`bitstr_inline::emit_bit_extract`] covers, else the refusal. An overflowing product is
/// the same refusal (no covered width can be that large).
fn width_refusal(size: usize, unit: usize) -> Option<String> {
    match size.checked_mul(unit) {
        Some(nbits) if bitstr_inline::field_width_supported(nbits) => None,
        _ => Some(BS_REASON_WIDTH.to_string()),
    }
}

/// The field-decoding refusal of a `bs_get_integer2`, in the report's documented
/// precedence:
///
/// 1. (i) the field form — `flags` is static, so a signed/little/native field is decidable
///    before the size is;
/// 2. (c) a runtime size — without a constant there is no `nbits` for (e) to test;
/// 3. (e) the width rule. (`unit` is a plain immediate here, so it can never be runtime.)
fn get_integer2_refusal(size: &Operand, unit: u32, flags: u32) -> Option<String> {
    if bitstr_inline::field_is_unsupported(flags) {
        return Some(BS_REASON_FIELD.to_string());
    }
    let Some(size) = bitstr_inline::bs_match_static_size(size) else {
        return Some(BS_REASON_RUNTIME.to_string());
    };
    width_refusal(size, unit as usize)
}

/// The field-decoding refusal of a `bs_match` `integer` command — [`get_integer2_refusal`]'s
/// precedence with one extra step, because the command's `unit` is an **operand** and can
/// therefore itself be the runtime form.
fn integer_command_refusal(size: &Operand, unit: &Operand, flags: u32) -> Option<String> {
    if bitstr_inline::field_is_unsupported(flags) {
        return Some(BS_REASON_FIELD.to_string());
    }
    let Some(size) = bitstr_inline::bs_match_static_size(size) else {
        return Some(BS_REASON_RUNTIME.to_string());
    };
    let Some(unit) = bitstr_inline::bs_match_static_size(unit) else {
        return Some(BS_REASON_RUNTIME.to_string());
    };
    width_refusal(size, unit)
}

/// (a) — the command **names** of a `bs_match` list, in program order, or `None` when the
/// list cannot be walked at all (an unknown command name or a group running past the end):
/// the family's own typed "invalid/truncated command list", which the report attributes
/// `other`.
fn match_command_names<'m>(module: &'m Module, commands: &[Operand]) -> Option<Vec<&'m str>> {
    let mut names = Vec::with_capacity(commands.len() / 2 + 1);
    let mut i = 0usize;
    while i < commands.len() {
        let Operand::Atom(atom) = &commands[i] else {
            return None;
        };
        let name = module.atom_name(*atom)?;
        let width = bitstr_inline::bs_match_group_len(name)?;
        let end = i.checked_add(width)?;
        if end > commands.len() {
            return None;
        }
        names.push(name);
        i = end;
    }
    Some(names)
}

/// (c)/(d)/(e)/(h) — the `bs_match` command walk, one group at a time, in program order, so
/// the attribution names the first group that actually declines. `None` when every group is
/// lowerable. A malformed list (an unknown command, a truncated group) is `other`.
fn match_group_refusal(module: &Module, commands: &[Operand]) -> Option<String> {
    let mut y_destinations = 0usize;
    let mut i = 0usize;
    while i < commands.len() {
        let Operand::Atom(command) = &commands[i] else {
            return Some(BS_REASON_OTHER.to_string());
        };
        let Some(name) = module.atom_name(*command) else {
            return Some(BS_REASON_OTHER.to_string());
        };
        let Some(width) = bitstr_inline::bs_match_group_len(name) else {
            return Some(BS_REASON_OTHER.to_string());
        };
        let Some(end) = i.checked_add(width) else {
            return Some(BS_REASON_OTHER.to_string());
        };
        if end > commands.len() {
            return Some(BS_REASON_OTHER.to_string());
        }
        match name {
            "ensure_at_least" => {
                let Some(size) = commands.get(i + 1) else {
                    return Some(BS_REASON_OTHER.to_string());
                };
                if bitstr_inline::bs_match_static_size(size).is_none() {
                    return Some(BS_REASON_RUNTIME.to_string());
                }
                let Some(unit) = commands.get(i + 2) else {
                    return Some(BS_REASON_OTHER.to_string());
                };
                let Some(unit) = bitstr_inline::bs_match_static_size(unit) else {
                    return Some(BS_REASON_RUNTIME.to_string());
                };
                // `remaining % unit` must lower to a mask; `unit == 0` is a division by zero.
                if !unit.is_power_of_two() {
                    return Some(BS_REASON_UNIT.to_string());
                }
            }
            "ensure_exactly" | "skip" => {
                let Some(stride) = commands.get(i + 1) else {
                    return Some(BS_REASON_OTHER.to_string());
                };
                if bitstr_inline::bs_match_static_size(stride).is_none() {
                    return Some(BS_REASON_RUNTIME.to_string());
                }
            }
            "integer" => {
                let Some(flags) = commands.get(i + 2) else {
                    return Some(BS_REASON_OTHER.to_string());
                };
                let Some(flags) = bitstr_inline::bs_match_integer_flags(module, flags) else {
                    // An unmodelled flag atom: the family decides, so the report cannot say
                    // a signed/little field caused it.
                    return Some(BS_REASON_OTHER.to_string());
                };
                let Some(size) = commands.get(i + 3) else {
                    return Some(BS_REASON_OTHER.to_string());
                };
                let Some(unit_op) = commands.get(i + 4) else {
                    return Some(BS_REASON_OTHER.to_string());
                };
                // Same precedence as `bs_get_integer2`, except a `bs_match` `integer`
                // command's unit is an operand and so can itself be runtime.
                if let Some(reason) = integer_command_refusal(size, unit_op, flags) {
                    return Some(reason);
                }
                let Some(dst) = commands.get(i + 5) else {
                    return Some(BS_REASON_OTHER.to_string());
                };
                let Some(loc) = Loc::of(dst) else {
                    return Some(operand_refusal(&[dst]));
                };
                if loc.is_y() {
                    y_destinations += 1;
                    if y_destinations > 1 {
                        return Some(BS_REASON_Y_DST.to_string());
                    }
                }
            }
            // The name pass already returned (a) for these; kept for totality.
            "binary" | "get_tail" | "=:=" => return Some(BS_REASON_ALLOC.to_string()),
            _ => return Some(BS_REASON_OTHER.to_string()),
        }
        i = end;
    }
    None
}

/// The reason a `bs_match` was refused, in the report's documented precedence: (a) the
/// allocating command anywhere in the list wins outright — it makes the **whole** list
/// unlowerable whatever else it holds — then the command walk, then (f)/(g), then (b).
fn match_refusal(
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    ctx: &Operand,
    commands: &[Operand],
) -> String {
    match match_command_names(module, commands) {
        Some(names) => {
            if names
                .iter()
                .any(|name| matches!(*name, "binary" | "get_tail" | "=:="))
            {
                return BS_REASON_ALLOC.to_string();
            }
        }
        None => return BS_REASON_OTHER.to_string(),
    }
    if let Some(reason) = match_group_refusal(module, commands) {
        return reason;
    }
    if let Some(reason) = fail_refusal(labels, fail) {
        return reason;
    }
    operand_refusal(&[ctx])
}

/// What the bitstring inline set does with one instruction: ask the emitter's own shape
/// predicate first (a refusal the emitter would not make must never be reported), and only
/// then classify the refusal.
fn bitstring_verdict(module: &Module, labels: &LabelMap, instr: &Instr) -> BsVerdict {
    match instr {
        Instr::BsGetPosition { ctx, dst, .. } => {
            const OP: &str = "bs_get_position";
            if bitstr_inline::get_position_shape(ctx, dst).is_some() {
                return BsVerdict::Inlined(OP);
            }
            BsVerdict::Refused(OP, operand_refusal(&[ctx, dst]))
        }
        Instr::BsSetPosition { ctx, pos } => {
            const OP: &str = "bs_set_position";
            if bitstr_inline::set_position_shape(ctx, pos).is_some() {
                return BsVerdict::Inlined(OP);
            }
            BsVerdict::Refused(OP, operand_refusal(&[ctx, pos]))
        }
        Instr::BsTestTail2 { fail, ctx, bits } => {
            const OP: &str = "bs_test_tail2";
            if bitstr_inline::test_tail2_shape(labels, *fail, ctx, *bits).is_some() {
                return BsVerdict::Inlined(OP);
            }
            let reason =
                fail_refusal(labels, *fail).unwrap_or_else(|| operand_refusal(&[ctx]));
            BsVerdict::Refused(OP, reason)
        }
        Instr::BsSkipBits2 {
            fail,
            ctx,
            size,
            unit,
            ..
        } => {
            const OP: &str = "bs_skip_bits2";
            if bitstr_inline::skip_bits2_shape(labels, *fail, ctx, size, *unit).is_some() {
                return BsVerdict::Inlined(OP);
            }
            let reason = runtime_size_refusal(size, *unit)
                .or_else(|| fail_refusal(labels, *fail))
                .unwrap_or_else(|| operand_refusal(&[ctx]));
            BsVerdict::Refused(OP, reason)
        }
        Instr::BsStartMatch3 { fail, bin, dst, .. } => {
            const OP: &str = "bs_start_match3";
            if bitstr_inline::start_match3_shape(labels, *fail, bin, dst).is_some() {
                return BsVerdict::Inlined(OP);
            }
            // `NO_FAIL` IS this shape's `MatchFail::Family` arm, so it is accepted — only a
            // real but unresolvable label is a label refusal.
            let reason = if *fail != NO_FAIL && labels.resolve(*fail).is_err() {
                BS_REASON_LABEL.to_string()
            } else {
                operand_refusal(&[bin, dst])
            };
            BsVerdict::Refused(OP, reason)
        }
        Instr::BsStartMatch4 {
            fail, src, dst, ..
        } => {
            const OP: &str = "bs_start_match4";
            if bitstr_inline::start_match4_shape(module, labels, fail, src, dst).is_some() {
                return BsVerdict::Inlined(OP);
            }
            let reason = match fail {
                Operand::Label(label) if *label != NO_FAIL && labels.resolve(*label).is_err() => {
                    BS_REASON_LABEL.to_string()
                }
                Operand::Label(_) => operand_refusal(&[src, dst]),
                // `no_fail` and `resume` are both accepted arms; any other atom — or an
                // atom id the module cannot name — is the family's own typed refusal.
                Operand::Atom(atom) => match module.atom_name(*atom) {
                    Some("no_fail") | Some("resume") => operand_refusal(&[src, dst]),
                    _ => BS_REASON_OTHER.to_string(),
                },
                _ => BS_REASON_OTHER.to_string(),
            };
            BsVerdict::Refused(OP, reason)
        }
        Instr::BsGetInteger2(args) => {
            const OP: &str = "bs_get_integer2";
            let inlined = bitstr_inline::get_integer2_shape(
                labels,
                args.fail,
                &args.ctx,
                &args.size,
                args.unit,
                args.flags,
                &args.dst,
            )
            .is_some();
            if inlined {
                return BsVerdict::Inlined(OP);
            }
            let reason = get_integer2_refusal(&args.size, args.unit, args.flags)
                .or_else(|| fail_refusal(labels, args.fail))
                .unwrap_or_else(|| operand_refusal(&[&args.ctx, &args.dst]));
            BsVerdict::Refused(OP, reason)
        }
        Instr::BsMatch { fail, ctx, commands } => {
            const OP: &str = "bs_match";
            if bitstr_inline::bs_match_shape(labels, module, *fail, ctx, commands).is_some() {
                return BsVerdict::Inlined(OP);
            }
            BsVerdict::Refused(OP, match_refusal(module, labels, *fail, ctx, commands))
        }
        Instr::BsCreateBin { dst, segments, .. } => {
            const OP: &str = "bs_create_bin";
            if bitstr_inline::CreateBinShape::of(module, dst, segments).is_some() {
                return BsVerdict::Inlined(OP);
            }
            BsVerdict::Refused(OP, create_bin_refusal(dst, segments))
        }
        // The remaining `Bs*` variants have no inline arm at all — reported so the corpus
        // view is complete, never silently dropped.
        Instr::BsMatchString {
            fail,
            ctx,
            bits,
            offset,
        } => {
            const OP: &str = "bs_match_string";
            if bitstr_inline::match_string_shape(labels, module, *fail, ctx, *bits, *offset)
                .is_some()
            {
                return BsVerdict::Inlined(OP);
            }
            let reason = fail_refusal(labels, *fail).unwrap_or_else(|| {
                "pattern not lowered (offset, length cap or bit width)".to_string()
            });
            BsVerdict::Refused(OP, reason)
        }
        Instr::BsGetTail { .. } => BsVerdict::NoArm("bs_get_tail"),
        Instr::BsGetFloat2(_) => BsVerdict::NoArm("bs_get_float2"),
        Instr::BsGetBinary2(_) => BsVerdict::NoArm("bs_get_binary2"),
        Instr::BsGetUtf8 { .. } => BsVerdict::NoArm("bs_get_utf8"),
        Instr::BsSkipUtf32 {
            fail, ctx, flags, ..
        } => {
            const OP: &str = "bs_skip_utf32";
            if bitstr_inline::skip_utf32_shape(labels, *fail, ctx, *flags).is_some() {
                return BsVerdict::Inlined(OP);
            }
            // The shape's two refusals, in its own precedence: the endianness flag word is
            // static, so it is decidable before the fail operand is; then the shared fail
            // half; anything else is the operand form.
            let reason = if bitstr_inline::field_is_little(*flags) {
                BS_REASON_FIELD.to_string()
            } else {
                fail_refusal(labels, *fail).unwrap_or_else(|| operand_refusal(&[ctx]))
            };
            BsVerdict::Refused(OP, reason)
        }
        Instr::BsGetUtf32 {
            fail,
            ctx,
            flags,
            dst,
            ..
        } => {
            const OP: &str = "bs_get_utf32";
            if bitstr_inline::get_utf32_shape(labels, *fail, ctx, *flags, dst).is_some() {
                return BsVerdict::Inlined(OP);
            }
            // The shape's refusals in its own precedence — the skip twin's, plus the
            // destination's: the endianness flag word is static, so it is decidable before
            // the fail operand is; then the shared fail half; anything else is the operand
            // form (`ctx` or `dst`).
            let reason = if bitstr_inline::field_is_little(*flags) {
                BS_REASON_FIELD.to_string()
            } else {
                fail_refusal(labels, *fail).unwrap_or_else(|| operand_refusal(&[ctx, dst]))
            };
            BsVerdict::Refused(OP, reason)
        }
        Instr::BsSkipUtf8 { .. } => BsVerdict::NoArm("bs_skip_utf8"),
        Instr::BsSkipUtf16 {
            fail, ctx, flags, ..
        } => {
            const OP: &str = "bs_skip_utf16";
            if bitstr_inline::skip_utf16_shape(labels, *fail, ctx, *flags).is_some() {
                return BsVerdict::Inlined(OP);
            }
            // The shape's two refusals, in its own precedence: the endianness flag word is
            // static, so it is decidable before the fail operand is; then the shared fail
            // half; anything else is the operand form.
            let reason = if bitstr_inline::field_is_little(*flags) {
                BS_REASON_FIELD.to_string()
            } else {
                fail_refusal(labels, *fail).unwrap_or_else(|| operand_refusal(&[ctx]))
            };
            BsVerdict::Refused(OP, reason)
        }
        Instr::BsGetUtf16 {
            fail,
            ctx,
            flags,
            dst,
            ..
        } => {
            const OP: &str = "bs_get_utf16";
            if bitstr_inline::get_utf16_shape(labels, *fail, ctx, *flags, dst).is_some() {
                return BsVerdict::Inlined(OP);
            }
            // The shape's refusals in its own precedence — the skip twin's, plus the
            // destination's: the endianness flag word is static, so it is decidable before
            // the fail operand is; then the shared fail half; anything else is the operand
            // form (`ctx` or `dst`).
            let reason = if bitstr_inline::field_is_little(*flags) {
                BS_REASON_FIELD.to_string()
            } else {
                fail_refusal(labels, *fail).unwrap_or_else(|| operand_refusal(&[ctx, dst]))
            };
            BsVerdict::Refused(OP, reason)
        }
        Instr::BsInitWritable => BsVerdict::NoArm("bs_init_writable"),
        _ => BsVerdict::NotBitstring,
    }
}

/// (c) for `bs_skip_bits2`, whose `unit` is a plain immediate rather than an operand: a
/// non-constant size, or a `size * unit` that cannot be an `i64` immediate (`other` — that
/// is not the width rule, it is an unaddressable stride).
fn runtime_size_refusal(size: &Operand, unit: u32) -> Option<String> {
    let Some(size) = bitstr_inline::bs_match_static_size(size) else {
        return Some(BS_REASON_RUNTIME.to_string());
    };
    match size.checked_mul(unit as usize) {
        Some(nbits) if nbits <= i64::MAX as usize => None,
        _ => Some(BS_REASON_OTHER.to_string()),
    }
}

/// (b)/(j) — the `bs_create_bin` refusal.
///
/// The predicate that answers the `None` is
/// [`CreateBinShape::of`](super::super::bitstr_inline::CreateBinShape::of) — a strict
/// **superset** of the seam's
/// [`append_plan`](crate::families::bitstrings::create::seam::append_plan), which it calls
/// first. So the refusal can be any of the plan's own declines (a plain or
/// later-position append, an append unit that does not divide 8, a
/// `binary`/`float`/`utf*`/`all` segment, a `little`/`signed`/`native` flag, a
/// partial-byte or wider-than-a-word field), or one of the shape's own, additional ones:
///
/// * an accumulator, field **value** or `dst` [`Loc::of`] cannot address — a
///   literal-area operand, an out-of-bound register index, or a `Y` index the emitted
///   `lea`'s `disp32` could not address (an x-register and an addressable `Y` slot are
///   both lowered);
/// * a `bits` width that is not a positive whole-byte multiple in `8..=64`;
/// * a field byte-offset sum that does not land on the plan's tail;
/// * its two shape caps, more than `MAX_FIELDS` fields or more than `MAX_TAIL_BYTES` tail
///   bytes.
///
/// That single `None` covers all of them, so the report can name exactly one of the closed
/// buckets: a **literal-area operand** anywhere in the instruction (a destination, an
/// accumulator or a field value), else `other` — the honest catch-all. Guessing among the
/// rest would be a fabricated attribution.
fn create_bin_refusal(dst: &Operand, segments: &[Operand]) -> String {
    let mut operands: Vec<&Operand> = Vec::with_capacity(1 + segments.len());
    operands.push(dst);
    operands.extend(segments.iter());
    operand_refusal(&operands)
}

/// Every committed `*.beam` fixture (no hardcoded names).
fn fixture_corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut paths = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("beam") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths
}

/// A bounded sample of the installed OTP `.beam`s (no hardcoded app names).
fn otp_corpus(cap: usize) -> Vec<PathBuf> {
    let erl = std::env::var("ERTS_ORACLE").unwrap_or_else(|_| "erl".to_string());
    let Ok(output) = Command::new(&erl)
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir()]),halt().",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return Vec::new();
    }
    let mut ebins: Vec<PathBuf> = Vec::new();
    let Ok(apps) = std::fs::read_dir(Path::new(&root)) else {
        return Vec::new();
    };
    for app in apps.flatten() {
        let ebin = app.path().join("ebin");
        if ebin.is_dir() {
            ebins.push(ebin);
        }
    }
    ebins.sort();
    let mut paths = Vec::new();
    'outer: for ebin in ebins {
        let mut beams: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&ebin) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("beam") {
                    beams.push(path);
                }
            }
        }
        beams.sort();
        for path in beams {
            if paths.len() >= cap {
                break 'outer;
            }
            paths.push(path);
        }
    }
    paths
}

/// The refusal census over the fixture + OTP corpus.
#[test]
fn the_refusal_attribution_census_is_printed() {
    let mut paths = fixture_corpus();
    paths.extend(otp_corpus(OTP_MODULE_CAP));

    // kind -> reason -> count
    let mut tests: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut selects: BTreeMap<String, u64> = BTreeMap::new();
    let mut tuple_arities: BTreeMap<String, u64> = BTreeMap::new();
    let mut bifs: BTreeMap<String, u64> = BTreeMap::new();
    let mut gcbifs: BTreeMap<String, u64> = BTreeMap::new();
    let mut bif0s: BTreeMap<String, u64> = BTreeMap::new();
    // opcode -> reason -> count, and the two non-refusal buckets (see `BsVerdict`).
    let mut bs_refusals: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut bs_inlined: BTreeMap<String, u64> = BTreeMap::new();
    let mut bs_no_arm: BTreeMap<String, u64> = BTreeMap::new();
    // Every `Bs*` instruction the walk met — the "never silently dropped" pin.
    let mut bs_seen = 0u64;
    let mut modules_loaded = 0u64;

    for path in &paths {
        let Ok(module) = beam_loader::load_file(path) else {
            continue;
        };
        modules_loaded += 1;
        let labels = build_label_map(&module);
        for function in &module.functions {
            for instr in &function.code {
                match instr {
                    Instr::Test { kind, fail, args } => {
                        if !is_comparison(*kind) {
                        if let Some(reason) =
                            test_refusal(&module, &labels, *kind, *fail, args)
                        {
                            *tests
                                .entry(format!("Test({kind:?})"))
                                .or_default()
                                .entry(reason)
                                .or_insert(0) += 1;
                        }
                        }
                    }
                    Instr::SelectVal { value, fail, pairs } => {
                        if let Some(reason) = select_val_refusal(&labels, value, *fail, pairs) {
                            *selects.entry(reason).or_insert(0) += 1;
                        }
                    }
                    Instr::SelectTupleArity {
                        tuple,
                        fail,
                        destinations,
                    } => {
                        if let Some(reason) =
                            select_tuple_arity_refusal(&labels, tuple, *fail, destinations)
                        {
                            *tuple_arities.entry(reason).or_insert(0) += 1;
                        }
                    }
                    Instr::Bif { bif, .. } => {
                        if !already_inlined(&module, instr) {
                            *bifs.entry(bif_name(&module, *bif)).or_insert(0) += 1;
                        }
                    }
                    Instr::GcBif { bif, .. } => {
                        if !already_inlined(&module, instr) {
                            *gcbifs.entry(bif_name(&module, *bif)).or_insert(0) += 1;
                        }
                    }
                    Instr::Bif0 { bif, .. } => {
                        *bif0s.entry(bif_name(&module, *bif)).or_insert(0) += 1;
                    }
                    _ => match bitstring_verdict(&module, &labels, instr) {
                        BsVerdict::NotBitstring => {}
                        BsVerdict::NoArm(op) => {
                            bs_seen += 1;
                            *bs_no_arm.entry(op.to_string()).or_insert(0) += 1;
                        }
                        BsVerdict::Inlined(op) => {
                            bs_seen += 1;
                            *bs_inlined.entry(op.to_string()).or_insert(0) += 1;
                        }
                        BsVerdict::Refused(op, reason) => {
                            bs_seen += 1;
                            *bs_refusals
                                .entry(op.to_string())
                                .or_default()
                                .entry(reason)
                                .or_insert(0) += 1;
                        }
                    },
                }
            }
        }
    }

    assert!(modules_loaded > 0, "the corpus must load at least one module");
    println!("=== baseline-JIT refusal-attribution census ===");
    println!("corpus: paths={} modules_loaded={modules_loaded}", paths.len());

    println!();
    println!("-- `test` refusals: kind | reason | count --");
    let mut test_rows: Vec<(String, String, u64)> = Vec::new();
    for (kind, reasons) in &tests {
        for (reason, count) in reasons {
            test_rows.push((kind.clone(), reason.clone(), *count));
        }
    }
    test_rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    let test_total: u64 = test_rows.iter().map(|r| r.2).sum();
    println!("total test refusals = {test_total}");
    for (kind, reason, count) in &test_rows {
        println!("{count:>7}  {kind:<24} {reason}");
    }

    println!();
    println!("-- `select_val` refusals: reason | count --");
    let mut sel_rows: Vec<(String, u64)> = selects.into_iter().collect();
    sel_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (reason, count) in &sel_rows {
        println!("{count:>7}  {reason}");
    }

    println!();
    println!("-- `select_tuple_arity` refusals: reason | count --");
    let mut sta_rows: Vec<(String, u64)> = tuple_arities.into_iter().collect();
    sta_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (reason, count) in &sta_rows {
        println!("{count:>7}  {reason}");
    }

    println!();
    println!("-- `bif` (non-gc) deopt pool by BIF name --");
    let mut bif_rows: Vec<(String, u64)> = bifs.into_iter().collect();
    bif_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let bif_total: u64 = bif_rows.iter().map(|r| r.1).sum();
    println!("total = {bif_total}");
    for (name, count) in bif_rows.iter().take(40) {
        println!("{count:>7}  {name}");
    }

    println!();
    println!("-- `gc_bif` deopt pool by BIF name --");
    let mut gc_rows: Vec<(String, u64)> = gcbifs.into_iter().collect();
    gc_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let gc_total: u64 = gc_rows.iter().map(|r| r.1).sum();
    println!("total = {gc_total}");
    for (name, count) in gc_rows.iter().take(40) {
        println!("{count:>7}  {name}");
    }

    println!();
    println!("-- `bif0` deopt pool by BIF name (no arm at all) --");
    let mut b0_rows: Vec<(String, u64)> = bif0s.into_iter().collect();
    b0_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (name, count) in b0_rows.iter().take(40) {
        println!("{count:>7}  {name}");
    }

    // ---- bitstring inline refusals (see `BsVerdict`) -----------------------
    println!();
    println!("-- bitstring inline refusals: opcode | reason | count --");
    let mut bs_rows: Vec<(String, String, u64)> = Vec::new();
    for (opcode, reasons) in &bs_refusals {
        for (reason, count) in reasons {
            bs_rows.push((opcode.clone(), reason.clone(), *count));
        }
    }
    bs_rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    let bs_refused_total: u64 = bs_rows.iter().map(|r| r.2).sum();
    let bs_inlined_total: u64 = bs_inlined.values().sum();
    let bs_no_arm_total: u64 = bs_no_arm.values().sum();
    println!(
        "totals: bs_instructions={bs_seen} inlined={bs_inlined_total} \
         refused={bs_refused_total} no_arm={bs_no_arm_total}"
    );
    for (opcode, reason, count) in &bs_rows {
        println!("{count:>7}  {opcode:<20} {reason}");
    }

    // The `inlined` half, per opcode, so a refusal count reads against its denominator.
    println!();
    println!("-- bitstring inline accounting: inlined | refused | opcode --");
    let mut bs_ops: Vec<String> = bs_inlined.keys().chain(bs_refusals.keys()).cloned().collect();
    bs_ops.sort();
    bs_ops.dedup();
    for opcode in &bs_ops {
        let inlined = bs_inlined.get(opcode).copied().unwrap_or(0);
        let refused = bs_refusals.get(opcode).map(|r| r.values().sum::<u64>()).unwrap_or(0);
        println!("{inlined:>7} {refused:>8}  {opcode}");
    }

    // Every other `Bs*` opcode: no arm at all (never silently dropped from the view).
    println!();
    println!("-- bitstring opcodes with no inline arm: count | opcode --");
    let mut na_rows: Vec<(String, u64)> = bs_no_arm.into_iter().collect();
    na_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (opcode, count) in &na_rows {
        println!("{count:>7}  {opcode}");
    }

    // The section's own pins: the corpus really does hold bitstring instructions, and every
    // one of them landed in exactly one bucket (`seen` == inlined + refused + no-arm).
    assert!(
        bs_seen > 0,
        "the corpus must contain at least one bitstring instruction"
    );
    assert_eq!(
        bs_seen,
        bs_inlined_total + bs_refused_total + bs_no_arm_total,
        "every `Bs*` instruction must land in exactly one bitstring bucket"
    );
}
