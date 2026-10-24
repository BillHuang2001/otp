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
//! ([`Machine::with_bitstr_programs`]) and its arms, plus the run-boundary **retention
//! rule** ([`Machine::bind_entry_programs`]) that lets the entry table outlive a
//! [`Machine::reset`].
//!
//! A compiled binary comprehension re-enters the seam once per element, so the hand-over
//! is two struct moves and nothing else: the context id is captured **once**, so the
//! file-back lands in the very slot the table came from. These tests pin the properties a
//! wrong implementation breaks: the table a closure sees is the one the context already
//! had (a prepared program is therefore a cache **hit** on the next execution, which is
//! how a missed file-back shows up as a re-prepare rather than as a wrong answer), the
//! file-back happens on every ordinary exit, a nested run inside the closure cannot
//! redirect it, an unregistered context gets a throwaway table that is dropped instead of
//! filed back, and the closure really holds the machine.
//!
//! The retention half pins [`Machine::bind_entry_programs`]'s witness
//! ([`ProgramWitness::describes`]): the entry table survives a run boundary only when the
//! run re-enters **the very image it was filled from** — same address, same content digest,
//! same atom namespace — and never for a synthetic (all-zero checksum) image. Each arm is
//! pinned by observable behaviour: a retained table answers the next execution (the prepare
//! closure does not run), a discarded one re-prepares, so a table that should have been
//! dropped but was kept shows up as a **missed** preparation and vice versa.
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

/// An image with a **content identity** — `Module::default()` plus a load-time checksum
/// digest, the shape a real `.beam` has ([`Module::checksum`]).
///
/// This is the smallest image [`Machine::bind_entry_programs`] may **retain** a table
/// for: an all-zero checksum (the bare [`Module::default`], see [`image`]) is the marker
/// of a hand-built module whose `(function, code offset)` numbers are not an identity.
fn identified_image(name: &str, digest: [u8; 16]) -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);
    module.checksum = digest;
    module
}

/// [`identified_image`] behind the `Arc` handle a module context is registered with.
#[allow(clippy::arc_with_non_send_sync)]
fn identified_handle(name: &str, digest: [u8; 16]) -> Arc<Module> {
    Arc::new(identified_image(name, digest))
}

/// Take the table, prepare `key` once and report whether the closure ran.
fn prepare_key(machine: &mut Machine, module: &Module, key: u64, prepares: &mut usize) {
    machine.with_bitstr_programs(|_machine, programs| {
        let _ = programs.get_or_prepare(key, module, || {
            *prepares += 1;
            Program::Generic
        });
    });
}

/// Take the table, prepare `(0, 0)` once and report whether the closure ran.
fn prepare_once(machine: &mut Machine, module: &Module, prepares: &mut usize) {
    prepare_key(machine, module, program_key(0, 0), prepares);
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

// ---------------------------------------------------------------------------
// The run-boundary retention rule (`Machine::bind_entry_programs`)
// ---------------------------------------------------------------------------

/// **Re-entering the very image the table was filled from retains it.**
///
/// This is the whole point of the rule: a host that runs one module many times (the
/// runtime's `run` route, a benchmark loop, a batch harness recycling the machine) would
/// otherwise allocate, fill and drop the whole table once per run entry. Both halves are
/// asserted — the table is still the filled one right after the entry (no re-allocation),
/// and the next execution of the same instruction is a **hit** (the prepare closure does
/// not run), which is the observable a discarded table could not produce.
#[test]
fn re_entering_the_same_image_retains_the_entry_table() {
    let module = identified_image("retained_program_image", [3u8; 16]);
    assert_ne!(module.checksum(), [0u8; 16], "a real image has a digest");
    let mut machine = Machine::new();
    machine.enter_run(&module, 0);

    let mut prepares = 0usize;
    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(prepares, 1, "the empty table prepares");
    let filled = machine.entry_programs.allocated_slots();
    assert!(filled > 0, "the prepared table is filed back");

    // A fresh run of the same image.
    machine.reset();
    assert_eq!(
        machine.entry_programs.allocated_slots(),
        filled,
        "`reset` no longer drops the entry table: the entry module is not known yet"
    );
    machine.enter_run(&module, 0);
    assert_eq!(
        machine.entry_programs.allocated_slots(),
        filled,
        "re-entering the same image retains the filled table"
    );

    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(
        prepares, 1,
        "the retained table answers the same instruction — no re-prepare"
    );
}

/// **A different image is not retained**, and the stale entry cannot answer for it.
///
/// The second image is a different allocation with different content, so all three witness
/// fields differ. The table is discarded at the run entry (before its first execution) and
/// the same `(function, code offset)` key prepares afresh — a hit here would be a program
/// derived from the *first* image's descriptor list, i.e. a wrong answer rather than a slow
/// one.
#[test]
fn a_different_image_discards_the_entry_table() {
    let first = identified_image("first_program_image", [4u8; 16]);
    let mut machine = Machine::new();
    machine.enter_run(&first, 0);
    let mut prepares = 0usize;
    prepare_once(&mut machine, &first, &mut prepares);
    assert_eq!(prepares, 1);
    assert!(machine.entry_programs.allocated_slots() > 0);

    let second = identified_image("second_program_image", [5u8; 16]);
    machine.reset();
    machine.enter_run(&second, 0);
    assert_eq!(
        machine.entry_programs.allocated_slots(),
        0,
        "a different image's entry drops the table before its first use"
    );

    prepare_once(&mut machine, &second, &mut prepares);
    assert_eq!(
        prepares, 2,
        "the stale entry cannot answer: the new image prepares its own program"
    );
    prepare_once(&mut machine, &second, &mut prepares);
    assert_eq!(prepares, 2, "and that table is retained from then on");
}

/// **The witness's content-digest arm, isolated:** the *same allocation*, re-entered with
/// different content, is not retained.
///
/// Only the digest can catch this one — the address still matches (it is the same
/// `Module`) and the atom namespace is untouched — so a witness that compared the owner
/// word alone (the per-lookup check `BitstrPrograms::get_or_prepare` makes) would answer
/// the stale program for the new content. `Module::checksum` is the digest over exactly
/// the chunks a prepared program is derived from (`Code` descriptors, `AtU8` type atoms,
/// `StrT` string bytes, `LitT` widths).
#[test]
fn the_same_address_with_different_content_is_not_retained() {
    let mut module = identified_image("mutated_program_image", [6u8; 16]);
    let mut machine = Machine::new();
    machine.enter_run(&module, 0);
    let mut prepares = 0usize;
    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(prepares, 1);

    // Same address, different content: `(function, code offset)` may now name another
    // instruction's descriptor list.
    module.checksum = [7u8; 16];
    machine.reset();
    machine.enter_run(&module, 0);
    assert_eq!(
        machine.entry_programs.allocated_slots(),
        0,
        "a changed digest is a changed image, whatever the address says"
    );
    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(
        prepares, 2,
        "the previous content's program must not answer for the new content"
    );
}

/// **The witness's atom-namespace arm:** the same allocation with the same digest but a
/// namespace that moved is not retained either.
///
/// A prepared program's type/command words resolve through the module's atom table, so the
/// witness keys on the table's snapshot exactly as the label-map cache does. The check is
/// deliberately an equality — an atom appended after the fill discards the table even
/// though an append preserves every earlier index — because the cost of a discard is one
/// re-decode while the cost of a wrong namespace is a wrong answer.
#[test]
fn a_changed_atom_namespace_is_not_retained() {
    let mut module = identified_image("namespace_program_image", [8u8; 16]);
    let mut machine = Machine::new();
    machine.enter_run(&module, 0);
    let mut prepares = 0usize;
    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(prepares, 1);

    let _ = module.atom_table.intern("an_atom_interned_after_the_fill");
    machine.reset();
    machine.enter_run(&module, 0);
    assert_eq!(
        machine.entry_programs.allocated_slots(),
        0,
        "a moved atom namespace discards the table"
    );
    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(
        prepares, 2,
        "the program is re-derived in the new namespace"
    );
}

/// **An all-zero-checksum (hand-built / synthetic) image is never retained**, not even
/// when the run re-enters the very same allocation.
///
/// A synthetic image freely re-uses `(0, 0)` for many different descriptor lists, so its
/// key is not an identity and a retained entry could answer for a different instruction.
/// The address alone matches here, which is why this pins the witness's "no content
/// identity ⇒ never retained" rule rather than the address arm.
#[test]
fn a_synthetic_image_is_never_retained() {
    let module = image();
    assert_eq!(
        module.checksum(),
        [0u8; 16],
        "`Module::default` carries no content identity"
    );
    let mut machine = Machine::new();
    machine.enter_run(&module, 0);
    let mut prepares = 0usize;
    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(prepares, 1);
    assert!(machine.entry_programs.allocated_slots() > 0);

    machine.reset();
    machine.enter_run(&module, 0);
    assert_eq!(
        machine.entry_programs.allocated_slots(),
        0,
        "no content identity, so nothing is retained across the run boundary"
    );
    prepare_once(&mut machine, &module, &mut prepares);
    assert_eq!(
        prepares, 2,
        "a synthetic image re-prepares on every run entry"
    );
}

/// **A nested run inside the closure cannot redirect the file-back.**
///
/// The seam captures the context id **once**, before the closure runs. The closure here
/// enters another registered context, runs a bitstring instruction there and returns with
/// the machine *still* in that inner context — so an implementation that re-resolved
/// `self.context` on the way out would file the outer table into the inner context's slot
/// and leave the outer one holding the placeholder. The two contexts prepare **different
/// keys**, so both halves of that mistake are observable: the outer context would miss on
/// its own key, and the inner one would answer the outer image's program.
#[test]
fn a_nested_run_cannot_redirect_the_file_back() {
    let outer = identified_handle("outer_program_image", [9u8; 16]);
    let inner = identified_handle("inner_program_image", [10u8; 16]);
    let mut machine = Machine::new();
    let outer_id = machine.register_context(&outer);
    let inner_id = machine.register_context(&inner);
    machine
        .switch_context(outer_id)
        .expect("a registered context");

    let outer_key = program_key(1, 1);
    let inner_key = program_key(2, 2);
    let mut outer_prepares = 0usize;
    let mut inner_prepares = 0usize;
    machine.with_bitstr_programs(|machine, programs| {
        let _ = programs.get_or_prepare(outer_key, &outer, || {
            outer_prepares += 1;
            Program::Generic
        });
        // A nested run: another module context executes a bitstring instruction of its own
        // while the outer table is out. The closure returns **without** switching back.
        machine
            .switch_context(inner_id)
            .expect("a registered context");
        machine.with_bitstr_programs(|_machine, programs| {
            let _ = programs.get_or_prepare(inner_key, &inner, || {
                inner_prepares += 1;
                Program::Generic
            });
        });
    });
    assert_eq!(outer_prepares, 1);
    assert_eq!(inner_prepares, 1);
    assert_eq!(
        machine.context, inner_id,
        "the closure left the machine here"
    );

    // The outer table went back to the OUTER context's slot.
    machine
        .switch_context(outer_id)
        .expect("a registered context");
    prepare_key(&mut machine, &outer, outer_key, &mut outer_prepares);
    assert_eq!(
        outer_prepares, 1,
        "the outer context's own slot holds the outer program"
    );

    // …and the inner context's slot was not overwritten by it: the inner table answers the
    // inner key, and the outer key is a miss there (the inner image never prepared it).
    machine
        .switch_context(inner_id)
        .expect("a registered context");
    prepare_key(&mut machine, &inner, inner_key, &mut inner_prepares);
    assert_eq!(inner_prepares, 1, "the inner context kept its own table");
    prepare_key(&mut machine, &inner, outer_key, &mut inner_prepares);
    assert_eq!(
        inner_prepares, 2,
        "the inner context never answers the outer image's instruction"
    );
}

/// **A registered context's table is filed back on an early exit too**, not only on a
/// fall-through — the shape a family takes when the instruction turns out not to be its
/// own. The entry-context twin is
/// `the_entry_table_is_filed_back_on_every_ordinary_exit`; this is the parallel-`Vec` arm,
/// where a missed file-back would leave the registry holding the placeholder.
#[test]
fn a_registered_contexts_table_is_filed_back_on_an_early_exit() {
    let module = image();
    let mut machine = Machine::new();
    let id = machine.register_context(&module);
    machine.switch_context(id).expect("a registered context");

    let mut prepares = 0usize;
    let answer = machine.with_bitstr_programs(|_machine, programs| {
        let _ = programs.get_or_prepare(program_key(5, 5), &module, || {
            prepares += 1;
            Program::Generic
        });
        return "not-my-instruction";
    });
    assert_eq!(answer, "not-my-instruction");
    assert_eq!(prepares, 1);

    prepare_key(&mut machine, &module, program_key(5, 5), &mut prepares);
    assert_eq!(
        prepares, 1,
        "an early closure return still files the context's table back"
    );
}

/// **An unwind through the seam leaves the registry's slot present and re-usable.**
///
/// `take_programs` leaves an empty placeholder behind, so a panic inside the closure (a
/// family bug) loses the prepared programs — the documented "an unwind is the one
/// non-restore" — but must not corrupt the registry: the slot still exists for the context
/// and the next instruction re-prepares and files back. A hole there would make the
/// context permanently un-cacheable, and a *retained* table from the unwound instruction
/// would answer for whatever that instruction left half-prepared.
#[test]
fn a_panic_through_the_seam_leaves_the_contexts_slot_present_and_reusable() {
    let module = identified_handle("panicking_program_image", [11u8; 16]);
    let mut machine = Machine::new();
    let id = machine.register_context(&module);
    machine.switch_context(id).expect("a registered context");

    let key = program_key(4, 4);
    let mut prepares = 0usize;
    prepare_key(&mut machine, &module, key, &mut prepares);
    assert_eq!(prepares, 1);
    prepare_key(&mut machine, &module, key, &mut prepares);
    assert_eq!(prepares, 1, "the prepared table answers before the panic");

    let caught: std::result::Result<(), _> =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            machine.with_bitstr_programs(|_machine, _programs| {
                panic!("a bitstring family panics mid-instruction");
            })
        }));
    assert!(caught.is_err(), "the closure's panic leaves the seam");

    assert_eq!(machine.context, id, "the machine is still in the context");
    assert!(
        machine.contexts.take_programs(id).is_some(),
        "the context keeps its slot (an empty placeholder), so the registry is intact"
    );

    prepare_key(&mut machine, &module, key, &mut prepares);
    assert_eq!(
        prepares, 2,
        "the unwound table is dropped, so the program is re-prepared"
    );
    prepare_key(&mut machine, &module, key, &mut prepares);
    assert_eq!(prepares, 2, "and the re-prepared table is filed back");
}
