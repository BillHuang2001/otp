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

//! **Test-only** structural counters for the native baseline tier.
//!
//! This module exists to *diagnose* where a baseline-tier run spends its time; it is
//! deliberately **not** part of the tier's behaviour and it is **absent from every
//! production build**: the whole module is declared behind
//! `#[cfg(test)]` (`super::mod`'s `#[cfg(test)] mod counters;`) and every call site
//! is individually `#[cfg(test)]`-gated, so `cargo build -p beam_engine` compiles
//! with the counters — and the code that would touch them — entirely gone
//! (zero-cost by construction, not by a runtime check).
//!
//! # What is counted
//!
//! * **Per-`Instr`-variant deopts** — [`record_step_one`] (from
//!   [`jit_step_one`](super::runtime::jit_step_one)) and [`record_step_ext`] (from
//!   [`jit_step_ext`](super::runtime)), each attributed to the opcode at the
//!   `(func, pc)` the helper ran by looking the instruction up in the **active**
//!   module's code (the same module the helper executes the step against).
//! * **Block entries / driver round-trips** — [`record_block_entry`]/
//!   [`record_block_entered`] from `mod::enter_compiled`, and
//!   [`record_driver_round_trip`]/[`record_driver_interp_step`] from `mod::drive`
//!   (every block-ABI word the driver had to decode, classified).
//! * **Prologue position/publish calls** — [`record_set_position`] /
//!   [`record_publish_y`] / [`record_refresh_y`] from the three prologue helpers.
//! * **`call_ext*` resolution outcomes** — [`record_ext_resolved`] (native transfer)
//!   / [`record_ext_declined`] (the host's `Ok(None)`, the once-only seam) /
//!   [`record_ext_fallback`] (the whole instruction re-run through `jit_step_one`).
//!
//! # Two-phase use (timing vs counting)
//!
//! Recording is a per-thread switch ([`set_enabled`], default **off**), so a caller
//! can take a *clean* timing pass (no `format!`, no bookkeeping — the record
//! functions return on a single `Cell` read) and then a *counting* pass with
//! recording on. The counter block itself ([`Counters`]) is thread-local, so a test
//! running in parallel with others never sees their counts.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use beam_loader::{Instr, Module};

thread_local! {
    /// Whether this thread is currently recording (default: off — see the module docs).
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    /// This thread's counter block.
    static COUNTERS: RefCell<Counters> = RefCell::new(Counters::default());
}

/// A snapshot of the baseline tier's structural counters.
///
/// One field per event class; the `*_by_variant`/`*_by_word` maps attribute the
/// per-instruction deopts and the driver round-trips.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counters {
    /// `jit_step_one` invocations (the per-instruction deopt).
    pub(crate) step_one: u64,
    /// `jit_step_ext` invocations (the `call_ext*` deopt carrying the host's answer).
    pub(crate) step_ext: u64,
    /// `jit_ext_call` invocations that resolved a callee for this machine.
    pub(crate) ext_call_resolved: u64,
    /// `jit_ext_call` invocations the host declined (`Ok(None)`).
    pub(crate) ext_call_declined: u64,
    /// `jit_ext_call` invocations that fell back to the full interpreter step.
    pub(crate) ext_call_fallback: u64,
    /// `enter_compiled` lookups (one per driver iteration that asked for native code).
    pub(crate) block_entry_lookups: u64,
    /// Of those, how many found a compiled entry (an IR block start or a sub-block).
    pub(crate) block_entry_hits: u64,
    /// Of those, how many were actually entered (the register-file guard passed).
    pub(crate) block_entry_natives: u64,
    /// Block-ABI words the driver had to decode (a block ran and returned).
    pub(crate) driver_round_trips: u64,
    /// Driver iterations with no native entry: one interpreter step each.
    pub(crate) driver_interp_steps: u64,
    /// `jit_set_position` calls (every Y-free block prologue).
    pub(crate) set_position: u64,
    /// `jit_publish_y` calls (every block prologue whose body touches a `Y` slot).
    pub(crate) publish_y: u64,
    /// `jit_refresh_y` calls (the gated in-block Y-window re-publish).
    pub(crate) refresh_y: u64,
    /// The per-opcode deopt attribution: variant name -> count.
    pub(crate) deopts_by_variant: BTreeMap<String, u64>,
    /// The driver round-trips by the word class they returned (`HALT`/`ERROR`/
    /// `ENTER`/`WAIT`/`GOTO`).
    pub(crate) round_trips_by_word: BTreeMap<String, u64>,
}

/// The thread-local counter block, or `None` when this thread is not recording.
fn with<R>(f: impl FnOnce(&mut Counters) -> R) -> Option<R> {
    if !ENABLED.with(Cell::get) {
        return None;
    }
    Some(COUNTERS.with(|c| f(&mut c.borrow_mut())))
}

/// Turn recording on or off for this thread (default off — see the module docs).
pub(crate) fn set_enabled(on: bool) {
    ENABLED.with(|e| e.set(on));
}

/// Zero this thread's counters (recording state is left as it is).
pub(crate) fn reset() {
    COUNTERS.with(|c| *c.borrow_mut() = Counters::default());
}

/// This thread's counters, cloned.
pub(crate) fn snapshot() -> Counters {
    COUNTERS.with(|c| c.borrow().clone())
}

/// The opcode name a `(func, pc)` names in `module`, or a placeholder when the
/// address is out of range (which cannot happen for a real deopt — the helper only
/// runs an instruction the driver located in that module).
fn variant_at(module: &Module, func: u32, pc: u32) -> String {
    match module
        .functions
        .get(func as usize)
        .and_then(|function| function.code.get(pc as usize))
    {
        Some(instr) => variant_name(instr),
        None => format!("<out-of-range {func}:{pc}>"),
    }
}

/// The variant name of an `Instr`: the leading identifier of its `Debug` rendering
/// (`Move { .. }` -> `Move`, `Send` -> `Send`). Test-only, so the allocation and the
/// `Debug` formatting are acceptable.
fn variant_name(instr: &Instr) -> String {
    let rendered = format!("{instr:?}");
    let end = rendered
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rendered.len());
    rendered[..end].to_string()
}

/// Attribute a `jit_step_one` deopt to the opcode at `(func, pc)`.
pub(crate) fn record_step_one(module: &Module, func: u32, pc: u32) {
    with(|c| {
        c.step_one += 1;
        let name = variant_at(module, func, pc);
        *c.deopts_by_variant.entry(name).or_insert(0) += 1;
    });
}

/// Attribute a `jit_step_ext` (`call_ext*` deopt) to the opcode at `(func, pc)`.
pub(crate) fn record_step_ext(module: &Module, func: u32, pc: u32) {
    with(|c| {
        c.step_ext += 1;
        let name = variant_at(module, func, pc);
        *c.deopts_by_variant.entry(name).or_insert(0) += 1;
    });
}

/// A `jit_ext_call` that resolved a callee for this machine.
pub(crate) fn record_ext_resolved() {
    with(|c| c.ext_call_resolved += 1);
}

/// A `jit_ext_call` the host declined (`Ok(None)`).
pub(crate) fn record_ext_declined() {
    with(|c| c.ext_call_declined += 1);
}

/// A `jit_ext_call` that fell back to the full interpreter step.
pub(crate) fn record_ext_fallback() {
    with(|c| c.ext_call_fallback += 1);
}

/// An `enter_compiled` lookup; `hit` is whether a compiled entry was found.
pub(crate) fn record_block_entry(hit: bool) {
    with(|c| {
        c.block_entry_lookups += 1;
        if hit {
            c.block_entry_hits += 1;
        }
    });
}

/// An `enter_compiled` lookup that was actually entered (the register-file guard passed).
pub(crate) fn record_block_entered() {
    with(|c| c.block_entry_natives += 1);
}

/// One block-ABI word the driver decoded, classified by the word class.
pub(crate) fn record_driver_round_trip(word: i64) {
    with(|c| {
        c.driver_round_trips += 1;
        *c.round_trips_by_word.entry(word_class(word).to_string()).or_insert(0) += 1;
    });
}

/// One driver iteration with no native entry (one interpreter step).
pub(crate) fn record_driver_interp_step() {
    with(|c| c.driver_interp_steps += 1);
}

/// A `jit_set_position` prologue call.
pub(crate) fn record_set_position() {
    with(|c| c.set_position += 1);
}

/// A `jit_publish_y` prologue call (position store + Y-window publish).
pub(crate) fn record_publish_y() {
    with(|c| c.publish_y += 1);
}

/// A `jit_refresh_y` call (the gated in-block Y-window refresh).
pub(crate) fn record_refresh_y() {
    with(|c| c.refresh_y += 1);
}

/// The word class of a block-ABI word (`HALT`/`ERROR`/`ENTER`/`WAIT`/`GOTO`).
fn word_class(word: i64) -> &'static str {
    if word == super::HALT {
        "HALT"
    } else if word == super::ERROR {
        "ERROR"
    } else if word == super::ENTER {
        "ENTER"
    } else if super::decode_wait(word).is_some() {
        "WAIT"
    } else {
        "GOTO"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variant name is the leading identifier of the `Debug` rendering.
    #[test]
    fn variant_names_are_the_opcode_identifier() {
        assert_eq!(variant_name(&Instr::Send), "Send");
        assert_eq!(
            variant_name(&Instr::Move {
                src: beam_loader::Operand::X(0),
                dst: beam_loader::Operand::X(1),
            }),
            "Move"
        );
    }

    /// The word classes distinguish the four sentinels plus a plain continuation.
    #[test]
    fn word_classes_distinguish_the_sentinels() {
        assert_eq!(word_class(super::super::HALT), "HALT");
        assert_eq!(word_class(super::super::ERROR), "ERROR");
        assert_eq!(word_class(super::super::ENTER), "ENTER");
        assert_eq!(word_class(super::super::encode_wait(1, 2)), "WAIT");
        assert_eq!(word_class(super::super::encode_target(1, 2)), "GOTO");
    }

    /// Recording is off by default, and `set_enabled(true)` turns it on.
    #[test]
    fn recording_defaults_off_and_round_trips() {
        set_enabled(false);
        reset();
        record_set_position();
        assert_eq!(snapshot().set_position, 0, "recording is off by default");

        set_enabled(true);
        record_set_position();
        record_step_one(&Module::default(), 0, 0);
        let snap = snapshot();
        assert_eq!(snap.set_position, 1);
        assert_eq!(snap.step_one, 1);

        set_enabled(false);
        reset();
        assert_eq!(snapshot(), Counters::default());
    }
}
