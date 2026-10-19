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

//! The **term-minting seam** — a resource term for a bare `void*` handle (the sibling child of
//! `resources.rs`).
//!
//! # Why this exists
//!
//! A bare `void*` is how the runtime hands this crate a resource object, and a **resource term** is
//! how the C names that same object in a delivered message. `enif_select` is the case that needs
//! both at once: the C's `prepare_select_msg`
//! (`erts/emulator/sys/common/erl_check_io.c:699-767`) delivers
//! `{select, ResourceTerm, Ref, EventAtom}` (`TUPLE4`, `:757`) where `ResourceTerm` is minted by
//! `erts_mk_magic_ref(&hp, &mp->hfrag.off_heap, &bin->binary)` (`:746`) over the resource whose
//! payload pointer is the `obj` the NIF registered — and that per-interest
//! `enif_keep_resource(resource->data)` (`:1446`, `:1509`) is what keeps the object alive while the
//! interest stays armed.
//!
//! [`NifHost::select`] forwards the `*mut c_void obj` verbatim, so a runtime that has no way to build
//! that term cannot deliver the C's notification at all. [`resource_term`] is that way, and it is a
//! **safe** function: the runtime (`beam_rt`, which is `#![deny(unsafe_code)]`) calls it with the
//! handle it was given and gets `Option<Eterm>`.
//!
//! # Refuse, never fabricate
//!
//! Every "no" from this helper is `None` — never a term that designates nothing:
//!
//! * a **null** handle, and
//! * a pointer that is **not one of ours**, i.e. not a live payload pointer this crate allocated.
//!   The decision is [`Registry::object_type`]'s, which answers `None` for a pointer that is not in
//!   the registry's live-object set; this helper therefore never dereferences a pointer it does not
//!   recognise, where the C reads `DATA_TO_RESOURCE(obj)->type` unconditionally. That is the same
//!   "is it mine?" rule [`enif_get_resource`](super::enif_get_resource) and
//!   [`resource_type_has_down`](super::resource_type_has_down) already apply, and
//! * a heap that cannot fit the reference block,
//! * a reference id that does not fit the block's small-integer id word (unreachable in practice —
//!   `2^59` draws — and a typed refusal, never a wrapped id; see "The id space" below), and
//! * an object whose reference count has already reached **zero** (see below).
//!
//! # It takes a reference — and refuses to resurrect
//!
//! A minted term holds **one reference** on the object, exactly as the C's magic reference does
//! (`erts_bld_resource_ref` → `erts_mk_magic_ref` → `erts_bin_inc_refc`). That reference is what
//! keeps the object alive past the allocator's own release, and it is dropped by the normal
//! term-death path — [`release_resource_terms`](super::release_resource_terms) — or by a deliberate
//! [`enif_release_resource`](super::enif_release_resource).
//!
//! Unlike [`enif_make_resource`](super::enif_make_resource) (whose caller contract already
//! guarantees a live object, so it retains unconditionally), this seam is reachable from a runtime
//! that may race a destructor, so it takes its reference with
//! [`retain_object_if_alive`] — the C's `erts_refc_inc_unless(…, 0, 0)` — and answers `None` when
//! the count was already zero. The C states the same requirement as
//! `ASSERT(!erts_dbg_is_resource_dying(resource))` at the top of its select path
//! (`erl_check_io.c:1261`): a healthy mint is the only mint it allows, and a dying object must not be
//! resurrected (its destructor is already queued, and a second one would run it twice).
//!
//! # The id space
//!
//! Like [`enif_make_resource`](super::enif_make_resource), the mint draws its id from the **one**
//! shared local reference-id space (`next_resource_ref_id` → `beam_common::next_ref_id`), the same
//! source `erlang:make_ref/0`, the NIF `make_ref`, monitors, aliases, timers, `seq_trace` references
//! and `beam_process`'s process table draw from. A resource term therefore never aliases a plain
//! reference: a reachability predicate that matches reference *identity* (the id word) sees one
//! unambiguous owner per id. The draw is narrowed to the block's small-integer slot with a
//! **checked** conversion, and an unrepresentable id is a refusal (`None`), never a wrapped one.

use super::*;

use libc::c_void;

/// Mint the resource **term** that designates the object `obj` — the term the C's
/// `prepare_select_msg` (`erts/emulator/sys/common/erl_check_io.c:699-767`, `erts_mk_magic_ref` at
/// `:746`, the `{select, ResourceTerm, Ref, EventAtom}` tuple at `:757`) puts into a delivered
/// select notification.
///
/// `obj` is the payload pointer a [`NifHost`] hook was handed (the `void* obj` operand of
/// [`enif_select`](super::enif_select)); `host` is the host whose heap the reference block is built
/// on. `None` means "no term" — the object is null, is not one this crate allocated, is already
/// dying, the heap is exhausted, or the drawn reference id is not representable in the block's id
/// word (module docs, "Refuse, never fabricate").
///
/// # The returned term
///
/// It is a full **resource term**, indistinguishable from an
/// [`enif_make_resource`](super::enif_make_resource) mint: [`enif_get_resource`](super::enif_get_resource)
/// resolves it to `obj` for a `type` handle naming the object's type, and the registry keys it by the
/// term's identity, so a `copy_struct` copy of it designates the same object.
///
/// # Caller contract
///
/// * The term holds **one reference** on `obj`. The caller owns that reference and must eventually
///   drop it: through the reachability-driven prune
///   [`release_resource_terms`](super::release_resource_terms) — the sound route, where the caller's
///   root set is the predicate — or by handing the object to
///   [`enif_release_resource`](super::enif_release_resource) directly. A term left minted without ever
///   being released is a leak, never a dangling pointer (the C's magic reference behaves the same
///   way).
/// * The term lives on `host`'s heap, so it is readable only while that heap is: `beam_rt` mints it
///   at the delivery point and keeps it in the message it is building.
/// * The predicate passed to `release_resource_terms` is the **caller's soundness assertion** — see
///   that function's contract. A false "unreachable" for a term a NIF can still read is a
///   use-after-free; a false "reachable" is only a leak.
///
/// # A fresh term per call
///
/// Every call mints a new reference — the C's "fresh magic ref per call" behaviour, measured on the
/// oracle for `enif_make_resource` (two calls for one object yield two distinct terms). Two calls
/// here therefore answer two distinct terms (distinct id words) that both resolve to `obj`.
// A safe public function taking a raw handle on purpose: `obj` is a `void*` a `NifHost` hook was
// handed, and this helper never dereferences it — the registry's live-object set is the only gate
// (`Registry::object_type`), and every actual read goes through that registry or through
// `retain_object_if_alive`'s header access, which the registry has already justified. Marking it
// `unsafe fn` would push a contract the helper fully enforces onto every caller, including the
// `#![deny(unsafe_code)]` runtime.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn resource_term(host: &mut dyn NifHost, obj: *mut c_void) -> Option<Eterm> {
    if obj.is_null() {
        return None;
    }
    // The object's type is also the "is this pointer one of ours?" test: `Registry::object_type`
    // answers `None` for a pointer this crate never allocated (or already freed), and only the
    // registry may read a block's header — never this helper.
    let ty = with_registry(|r| r.object_type(obj))?;
    // SAFETY: a plain bump allocation of `RESOURCE_REF_WORDS` words from the host heap; exhaustion is
    // reported as `None`, and nothing is written before `write_resource_ref` fills the block.
    let hp = unsafe { host.heap().alloc_words(RESOURCE_REF_WORDS) }.ok()?;
    // SAFETY: `hp` addresses exactly the `RESOURCE_REF_WORDS` exclusively-owned, writable words just
    // handed out, and they stay reachable from `host`'s heap for as long as the term is. `None` (an
    // unrepresentable reference id) writes no word and takes no reference, so the reserved words are
    // simply abandoned.
    let reference = unsafe { write_resource_ref(hp) }?;
    // The term takes its reference before it is published — the reference the C's magic ref holds.
    // SAFETY: `obj` was registered as live by `object_type` above (the registry removes the entry
    // before a block is freed, under that very lock), so its block is allocated and its header is
    // readable. A count of zero means it is already dying: refuse rather than resurrect.
    if !unsafe { retain_object_if_alive(obj) } {
        return None;
    }
    let dead = with_registry(|r| {
        r.bind_term(
            reference,
            ResourceRef {
                ty,
                obj,
                word: reference,
            },
        )
    });
    // A binding whose heap word this fresh term now occupies was provably dead: give its reference
    // back outside the lock (module docs, "A reused address evicts a dead mint").
    // SAFETY: `dead` is the evicted binding's payload pointer, exactly one reference outstanding
    // (see `release_evicted`'s contract); `bind_term` is the only producer of that answer.
    unsafe { release_evicted(dead) };
    Some(reference)
}
