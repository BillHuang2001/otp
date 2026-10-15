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

//! The baseline tier's **pure-accessor BIF helper**: `erlang:byte_size/1` and
//! `erlang:length/1` evaluated natively instead of deopting to one interpreter step.
//!
//! The compiler emits both as a non-collecting `{bif,…}` (`Instr::Bif`) — pure term
//! accessors with no heap effect — so leaving them to `jit_step_one` costs a helper
//! round-trip **plus** a full interpreter dispatch and `Step` decode per call. Unlike
//! the aggregate readers (`erlang:hd/1`/`element/2`/`tuple_size/1`, lowered raw by
//! `super::emit::bif`), neither value is a plain word read: `byte_size/1` rounds
//! `bit_size` **up** to whole bytes (non-byte-aligned bitstrings included) and
//! `length/1` walks a *proper* list, rejecting an improper tail. Both are therefore
//! evaluated **natively in Rust** by [`jit_bif_value`], which reproduces the
//! `beam_bifs` ground truth word for word:
//!
//! * `byte_size/1` — `bits::bit_size` (`= beam_term::bitstring_size`) `.div_ceil(8)`
//!   (`beam_bifs/src/binconv.rs:374`), `badarg` for a non-bitstring;
//! * `length/1` — the number of cons cells in a **proper** list (`proper_list`,
//!   `beam_bifs/src/term_ops.rs:84`), `badarg` for a non-list or an improper tail.
//!
//! # The decline is a real interpreter step, never a wrong value
//!
//! The helper reads its argument through the engine's own [`read_operand`] and writes
//! its result through [`write_operand`], so it is exact by construction for *every*
//! operand kind the interpreter can address. Every shape it cannot prove — a
//! non-bitstring, a non-list or improper list, a count that would not fit a small
//! integer, an operand read or write the fast path cannot make — hands the instruction
//! to [`jit_step_one`], which runs **the** interpreter step: the oracle's exact value, or
//! its exact typed error (`badarg`, `system_limit`), never a substituted one. There is no
//! decline sentinel and no second compare: the helper's `encode_target(func, pc + 1)`
//! is the interpreter's own `Step::Next` word, so the emitted [`emit_continue_check`]
//! treats a computed value and a successful interpreter step identically (refill the
//! cached x-registers, continue in block).
//!
//! [`emit_continue_check`]: super::emit::emit_continue_check

#![allow(unsafe_code)]

use beam_loader::{ImportRef, Instr, Module, Operand};
use beam_term::{
    bitstring_size, cdr, is_bitstring, is_list, is_nil, make_small, Eterm, MAX_SMALL,
};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};

use super::encode_target;
use super::jit_step_one;
use super::runtime::JitRuntime;

/// The `(module name, function name, arity)` an import index names, resolved through the
/// running module's own load-time atom table — the same dispatch key
/// `super::emit::arith::import_names` (and so `super::emit::bif::classify`) uses.
///
/// Local rather than shared because `emit`'s helper is `pub(in emit)`; the lookup is the
/// three-line `Module` read the seam is, not a behaviour.
fn import_names(module: &Module, target: ImportRef) -> Option<(&str, &str, u32)> {
    let import = module.imports.get(target.0 as usize)?;
    let module_name = module.atom_name(import.module)?;
    let function_name = module.atom_name(import.function)?;
    Some((module_name, function_name, import.arity))
}

/// Evaluate the `erlang:byte_size/1` or `erlang:length/1` at `(func, pc)` natively, or
/// run the interpreter's own step for it.
///
/// The helper has the [`StepHelper`](super::runtime::StepHelper) signature, so the
/// `erlang:byte_size/1` / `erlang:length/1` arm of `super::emit::bif` drives it through
/// the shared [`emit_helper_call`](super::emit::emit_helper_call) sequence — which
/// **spills the cached x-registers first** (so the shared [`read_operand`] reads the
/// interpreter's exact live `machine.x`) — and then the shared
/// [`emit_continue_check`](super::emit::emit_continue_check):
///
/// * [`encode_target(func, pc + 1)`](super::encode_target) — the value is in the
///   destination (written through [`write_operand`]), or the interpreter's own step ran
///   the instruction and advanced by one — the emitted code refills and continues in
///   block either way;
/// * any other word — the folded [`Step`](crate::Step) of the interpreter's own step
///   (a fail-label `Goto`, an `ENTER`, the `Wait` bit, `HALT`, or the `ERROR` of a
///   propagating raise) — leaves the block for the driver with that word in `rax`,
///   exactly as a deopt's.
///
/// Nothing is ever invented: an instruction that does not name one of the two lowered
/// BIFs, or a shape whose value the helper cannot compute, is handed to
/// [`jit_step_one`] — the interpreter's step, which produces *its* exact value or typed
/// error.
pub(crate) extern "C" fn jit_bif_value(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
    let handled = {
        // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
        // `JitRuntime` and never aliases it for the duration of the call; every pointer
        // it holds points at an object the driver owns and keeps alive for the whole
        // loop (`machine`, `module`), or at a local that outlives the loop.
        let runtime = unsafe { &mut *rt };
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs (the driver is
        // blocked in the block call that reached us).
        let machine = unsafe { &mut *runtime.machine };
        // SAFETY: `runtime.module` points at the module the driver is running, which
        // outlives the loop.
        let module = unsafe { &*runtime.module };
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed as
        // a raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };

        evaluate_bif(machine, module, ctx, func, pc)
    };
    match handled {
        Some(()) => encode_target(func, pc + 1),
        // Not this helper's instruction, or a shape it cannot prove: the interpreter's
        // own step runs it, reproducing its exact value or typed error.
        None => jit_step_one(rt, func, pc),
    }
}

/// The Rust body of [`jit_bif_value`] — separate from the `extern "C"` helper so the
/// runtime borrows are provably dead before the exit path hands the instruction back to
/// [`jit_step_one`] (the same split `super::call` uses).
///
/// `Some(())` = the value is in the destination; `None` = hand the instruction to the
/// interpreter's own step.
fn evaluate_bif(
    machine: &mut Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    func: u32,
    pc: u32,
) -> Option<()> {
    // The instruction this block emitted the helper for. An address outside the module
    // cannot be here (the block was compiled from this very code), and any other
    // instruction is not this helper's — both go to the interpreter's own step.
    let instr = module
        .functions
        .get(func as usize)
        .and_then(|function| function.code.get(pc as usize))?;
    let Instr::Bif {
        arity, bif, args, dst, ..
    } = instr
    else {
        return None;
    };
    // The same classification the interpreter's dispatch key uses: the import's module
    // name is `erlang`, its arity matches `Instr::Bif`'s, and the argument count is one.
    let (module_name, function_name, import_arity) = import_names(module, *bif)?;
    if module_name != "erlang" || import_arity != u32::from(*arity) {
        return None;
    }
    let [arg] = args.as_slice() else {
        return None;
    };
    // A non-small `Integer` operand would make `read_operand` materialize a bignum (an
    // allocation), which is not a fast path — declare it and let the interpreter handle
    // it. Every other operand kind is a pure machine read.
    if !read_is_allocation_free(arg) {
        return None;
    }
    let term = read_operand(machine, ctx, arg).ok()?;
    let value = match function_name {
        "byte_size" => byte_size_value(term)?,
        "length" => list_length(term)?,
        _ => return None,
    };
    write_operand(machine, dst, make_small(value)).ok()
}

/// Whether a [`read_operand`] of `operand` is allocation-free, i.e. a pure machine read.
///
/// The only operand kind that can allocate is a non-small `Integer` constant (the
/// `terms::make_integer_unrooted` bignum path); a `Character` is a `u32` and always a
/// small integer. Everything else — a register, a `Y` slot, a float register, a
/// literal-area term, an atom, `[]` — is a plain read.
fn read_is_allocation_free(operand: &Operand) -> bool {
    match operand {
        Operand::Integer(value) => (beam_term::MIN_SMALL..=MAX_SMALL).contains(value),
        _ => true,
    }
}

/// `byte_size(Bitstring)` — `NBYTES(bit_size)`, i.e. the bit size rounded **up** to whole
/// bytes — or `None` when `term` is not a bitstring (whose `badarg` the interpreter's own
/// step raises) or the size would not fit a small integer (whose `system_limit` the
/// interpreter's own step raises).
///
/// Byte-exact with `beam_bifs`'s `byte_size_1` (`beam_bifs/src/binconv.rs:374`):
/// `byte_size(<<1:1>>)` is `1` and `byte_size(<<1:17>>)` is `3`, never a `badarg` for a
/// non-byte-aligned bitstring — unlike the (dead) `bits::byte_size` helper.
fn byte_size_value(term: Eterm) -> Option<i64> {
    // SAFETY: `is_bitstring` dereferences a boxed header only when the word is a boxed
    // term, so a non-boxed word (an immediate, `NIL`) is a plain word compare.
    if !unsafe { is_bitstring(term) } {
        return None;
    }
    // SAFETY: `is_bitstring` just established `term` is a live heap- or sub-bitstring
    // whose backing thing is valid for its size words.
    let bits = unsafe { bitstring_size(term) };
    let bytes = i64::try_from(bits.div_ceil(8)).ok()?;
    (bytes <= MAX_SMALL).then_some(bytes)
}

/// `length(List)` — the number of cons cells in a **proper** list — or `None` when `term`
/// is not a proper list (whose `badarg` the interpreter's own step raises) or the count
/// would not fit a small integer (whose `system_limit` the interpreter's own step raises).
///
/// The exact walk `beam_bifs`'s `proper_list` (`beam_bifs/src/term_ops.rs:84`) performs,
/// without its collecting `Vec`: advance while the tail is a cons cell, then require `[]`.
/// An improper tail (or a non-list) declines, never a wrongly short count.
fn list_length(term: Eterm) -> Option<i64> {
    let mut tail = term;
    let mut count: i64 = 0;
    while is_list(tail) {
        if count == MAX_SMALL {
            // The count would need a bignum, which this rewrite's `erlang:length/1`
            // reports as `system_limit` (its `BifContext::int`): the interpreter's own
            // step is authoritative for that shape.
            return None;
        }
        // SAFETY: `is_list(tail)` just confirmed `tail` is a live non-`NIL` cons cell, so
        // its two-word cell is valid.
        tail = unsafe { cdr(tail) };
        count += 1;
    }
    is_nil(tail).then_some(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{make_atom, make_list, Atom, NIL};

    /// A cons cell built over a caller-owned `[CAR, CDR]` pair (the term layout the walk
    /// reads), returned as its list term.
    fn cons(cell: &[Eterm; 2]) -> Eterm {
        make_list(cell.as_ptr())
    }

    /// An `n_bits`-bit **on-heap** bitstring built by `beam_term`'s own writer, over a
    /// caller-owned buffer (`[thing_word, n_bits, data…]`) the returned term points into.
    fn heap_bits(n_bits: usize) -> (Vec<Eterm>, Eterm) {
        let mut buffer = vec![NIL; beam_term::heap_bits_size_words(n_bits)];
        let term = unsafe { beam_term::write_heap_bits(buffer.as_mut_ptr(), &[0xff; 8], n_bits) }
            .expect("in-range bit count");
        (buffer, term)
    }

    /// `byte_size/1` rounds the bit size **up** to whole bytes — the value the oracle
    /// reports for a non-byte-aligned bitstring (`<<1:1>>` is `1`, `<<1:17>>` is `3`).
    #[test]
    fn byte_size_rounds_the_bit_size_up() {
        let (_empty, empty) = heap_bits(0);
        let (_aligned, aligned) = heap_bits(8);
        let (_one, one_bit) = heap_bits(1);
        let (_seventeen, seventeen) = heap_bits(17);
        assert_eq!(byte_size_value(empty), Some(0));
        assert_eq!(byte_size_value(aligned), Some(1));
        assert_eq!(byte_size_value(one_bit), Some(1));
        assert_eq!(byte_size_value(seventeen), Some(3));
    }

    /// `byte_size/1` declines anything that is not a bitstring — the interpreter's own
    /// step raises `badarg` for it.
    #[test]
    fn byte_size_declines_a_non_bitstring() {
        assert_eq!(byte_size_value(make_small(42)), None);
        assert_eq!(byte_size_value(make_atom(Atom::from_raw(1))), None);
        assert_eq!(byte_size_value(NIL), None);
    }

    /// `length/1` counts a proper list's cons cells and terminates on `[]`.
    #[test]
    fn length_counts_a_proper_list() {
        assert_eq!(list_length(NIL), Some(0));
        let cell3 = [make_small(3), NIL];
        let cell2 = [make_small(2), cons(&cell3)];
        let cell1 = [make_small(1), cons(&cell2)];
        assert_eq!(list_length(cons(&cell1)), Some(3));
    }

    /// `length/1` declines a non-list and an improper tail — the interpreter's own step
    /// raises `badarg` for both.
    #[test]
    fn length_declines_a_non_proper_list() {
        assert_eq!(list_length(make_small(42)), None);
        assert_eq!(list_length(make_atom(Atom::from_raw(1))), None);
        let improper = [make_small(1), make_small(2)];
        assert_eq!(list_length(cons(&improper)), None);
    }

    /// A non-small `Integer` operand is the one non-allocation-free read; every other
    /// kind is a pure machine read.
    #[test]
    fn only_a_non_small_integer_operand_allocates() {
        assert!(read_is_allocation_free(&Operand::X(0)));
        assert!(read_is_allocation_free(&Operand::Y(0)));
        assert!(read_is_allocation_free(&Operand::Nil));
        assert!(read_is_allocation_free(&Operand::Integer(0)));
        assert!(read_is_allocation_free(&Operand::Integer(MAX_SMALL)));
        assert!(!read_is_allocation_free(&Operand::Integer(MAX_SMALL + 1)));
        assert!(!read_is_allocation_free(&Operand::Integer(
            beam_term::MIN_SMALL - 1
        )));
    }
}
