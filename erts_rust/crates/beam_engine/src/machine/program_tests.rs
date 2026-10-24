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

//! The **prepared-bitstring-program hand-over seam**
//! ([`Machine::with_bitstr_programs`]) and its three arms.
//!
//! A compiled binary comprehension re-enters the seam once per element, so it resolves
//! the executing module context's cell **once** — for the take and the matching
//! restore together — instead of twice. These tests pin what that fusion must not
//! change: the table a closure sees is the one the context already had (a prepared
//! program is therefore a cache **hit** on the next execution, which is how a missed
//! restore shows up as a re-prepare rather than as a wrong answer), the file-back
//! happens on every exit, an unregistered context gets a throwaway table that is
//! dropped instead of filed back, and the closure really holds the machine.
use std::sync::Arc;

use beam_loader::Module;
use beam_term::make_small;

use crate::families::bitstrings::programs::{program_key, Program};

use super::Machine;

/// A synthetic module image (all-zero checksum — only the owner witness is exercised
/// here, never the `bs_match` content-identity rule) plus an owned handle, so a real
/// module context can be registered.
///
/// `Module` is deliberately neither `Send` nor `Sync` (a module image is owned by the
/// host and executed by one machine), so the `Arc` here is a plain single-threaded
/// handle — the same shape `machine::tests` builds its callee contexts with.
#[allow(clippy::arc_with_non_send_sync)]
fn image() -> Arc<Module> {
    let mut module = Module::default();
    module.name = module.atom_table.intern("bitstr_program_seam_image");
    Arc::new(module)
}

/// Take the table, prepare `(func, pc)` once and report whether the closure ran.
fn prepare_once(machine: &mut Machine, module: &Module, prepares: &mut usize) {
    machine.with_bitstr_programs(|_machine, programs| {
        let _ = programs.get_or_prepare(program_key(0, 0), module, || {
            *prepares += 1;
            Program::Generic
        });
    });
}

/// **Context `0` (the run's entry module, whose table the machine owns).** The table
/// is handed over, the closure's preparation lands in it, and it is filed back — so a
/// second execution of the same instruction is a hit (no re-prepare) rather than a
/// fresh empty table.
#[test]
fn the_entry_table_is_filed_back_so_the_next_execution_is_a_cache_hit() {
    let module = image();
    let mut machine = Machine::new();
    assert_eq!(
        machine.entry_programs.allocated_slots(),
        0,
        "lazily allocated"
    );

    let mut prepares = 0usize;
    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(prepares, 1, "an empty table prepares");
    assert!(
        machine.entry_programs.allocated_slots() > 0,
        "the prepared table is filed back into the machine"
    );

    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(
        prepares, 1,
        "the filed-back table answers from its own slot"
    );
}

/// **The file-back is not conditional on what the closure returns.** A closure that
/// returns early — the shape a family takes when the instruction turns out not to be its
/// own — still leaves the machine's prepared table in place, so the next execution hits
/// rather than re-decodes. (An unwind is the one documented non-restore: the table is a
/// cache, so a lost one costs a re-decode and nothing else.)
#[test]
fn the_entry_table_is_filed_back_on_every_ordinary_exit() {
    let module = image();
    let mut machine = Machine::new();

    let mut prepares = 0usize;
    let answer = machine.with_bitstr_programs(|_machine, programs| {
        let _ = programs.get_or_prepare(program_key(3, 7), &module, || {
            prepares += 1;
            Program::Generic
        });
        // An early `return` out of the closure — the shape a family takes when the shape
        // it found is not this instruction's.
        if prepares == 1 {
            return "not-my-instruction";
        }
        "unreachable"
    });
    assert_eq!(
        answer, "not-my-instruction",
        "the seam returns the closure's value"
    );
    assert_eq!(prepares, 1);
    assert!(
        machine.entry_programs.allocated_slots() > 0,
        "an early closure return still files the table back"
    );
}

/// **A registered module context resolves its cell once and files the table into the
/// very cell it came from**, so the prepared program survives for the next execution in
/// that context.
#[test]
fn a_registered_contexts_table_is_filed_back_into_its_own_cell() {
    let module = image();
    let mut machine = Machine::new();
    let id = machine.register_context(&module);
    assert!(id >= 1);
    machine.switch_context(id).expect("a registered context");

    let mut prepares = 0usize;
    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(prepares, 1);
    // The entry table is untouched by anything that happens in another context, which is
    // only true if the table went back to *this* context's cell.
    assert_eq!(machine.entry_programs.allocated_slots(), 0);

    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(
        prepares, 1,
        "the context's own cell answers on the second execution"
    );
}

/// **An unregistered context has no module image and therefore no programs:** it gets a
/// throwaway empty table, which is dropped rather than filed back anywhere — the next
/// execution prepares again, and the entry table keeps exactly what it had.
#[test]
fn an_unregistered_context_gets_a_throwaway_table_that_is_never_filed_back() {
    let module = image();
    let mut machine = Machine::new();
    // An id that was never registered (`Contexts` ids are `>= 1` and dense).
    machine.context = 4242;

    let mut prepares = 0usize;
    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(prepares, 1);
    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(prepares, 2, "the throwaway table is dropped, not cached");
    assert_eq!(
        machine.entry_programs.allocated_slots(),
        0,
        "nothing is filed back into the entry table from another context"
    );
}

/// **The closure holds the machine**, which is why the seam hands it over: the family
/// writes registers and allocates while the table is out, and those writes are visible
/// afterwards.
#[test]
fn the_closure_receives_the_machine_it_is_executing_in() {
    let module = image();
    let mut machine = Machine::new();
    machine
        .set_x(0, make_small(1))
        .expect("an in-range register");

    machine.with_bitstr_programs(|machine, programs| {
        // A program is prepared through the very same borrow the writers use.
        let _ = programs.get_or_prepare(program_key(0, 0), &module, || Program::Generic);
        machine
            .set_x(0, make_small(42))
            .expect("an in-range register");
    });

    assert_eq!(machine.x(0), Some(make_small(42)));
    assert!(machine.entry_programs.allocated_slots() > 0);
}
