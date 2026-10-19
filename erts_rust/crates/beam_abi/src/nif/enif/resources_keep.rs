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

//! The explicit **keep/unkeep seam** — hold a resource object alive, then drop that hold, with no
//! environment and no C ABI (the sibling child of `resources.rs`).
//!
//! # Why this exists
//!
//! `enif_keep_resource` / `enif_release_resource` take a bare `void*` and are therefore callable from
//! Rust — but they are `unsafe extern "C"` with the C's caller contracts (`ASSERT(erts_refc_read(…) !=
//! 0)` at `erts/emulator/beam/erl_nif.c:3173`, and no membership test at all: the C computes
//! `DATA_TO_RESOURCE(obj)` and dereferences it). A runtime that is `#![deny(unsafe_code)]` cannot call
//! them, and must not have to.
//!
//! The route that needs the pair is `enif_select`'s. The C takes **one reference per armed interest**
//! — `enif_keep_resource(resource->data)` when the event state is converted to `ERTS_EV_TYPE_NIF`
//! (`erts/emulator/sys/common/erl_check_io.c:1446`, and again for a postponed `stop_select` at
//! `:1509`) — and gives it back when the interest is dropped
//! (`enif_release_resource(resource->data)`, `:1533`, `:1834`, `:2242`, and `:913` for the postponed
//! `stop`). The host seam [`NifHost::select`](crate::nif::host::NifHost::select) forwards the bare
//! `*mut c_void` `obj` and no environment, so the runtime can neither mint a term for it
//! ([`resource_term`](super::resource_term)) nor keep it alive without a helper like this one.
//!
//! # What the answers mean
//!
//! * [`keep_resource`] `true` — a reference was taken; the object cannot be destroyed until it is
//!   dropped (through [`unkeep_resource`], an [`enif_release_resource`](super::enif_release_resource),
//!   or the term-release prune [`release_resource_terms`](super::release_resource_terms)).
//! * [`keep_resource`] `false` — **no** reference was taken.
//! * [`unkeep_resource`] `true` — one reference was dropped; if it was the last one the type's
//!   destructor has already run (see "The destructor is synchronous" below).
//! * [`unkeep_resource`] `false` — **no** reference was dropped.
//!
//! # Refuse, never fabricate
//!
//! Both helpers answer `false`, doing nothing, for:
//!
//! * a **null** handle (the C would dereference it), and
//! * a pointer that is **not one of ours** — not a live payload pointer this crate allocated. The
//!   decision is [`Registry::object_type`]'s, the same "is it mine?" rule
//!   [`enif_get_resource`](super::enif_get_resource) and
//!   [`resource_term`](super::resource_term) apply, so these helpers **never dereference** a pointer
//!   they do not recognise where the C reads `DATA_TO_RESOURCE(obj)->type` unconditionally. The set is
//!   what makes "an [`unkeep_resource`] after the object was destroyed answers `false` and runs no
//!   second destructor" true: [`destroy`] runs `Registry::forget_object`, which unregisters the
//!   payload under the registry lock **before** the block is freed.
//!
//! and, for [`keep_resource`] only:
//!
//! * an object whose reference count is already **zero**, i.e. one that is dying (its destructor is
//!   already due). The reference is taken with [`retain_object_if_alive`] — the C's
//!   `erts_refc_inc_unless(…, 0, 0)`, the guard `erts_fire_nif_monitor` uses
//!   (`erl_nif.c:3092-3096`) — so a dying object is **not resurrected** (a second destructor would
//!   then run it twice). The C states the same requirement as the
//!   `ASSERT(!erts_dbg_is_resource_dying(resource))` at the top of its select path
//!   (`erl_check_io.c:1261`).
//!
//! [`unkeep_resource`] drops its reference with [`release_object_if_alive`], a *guarded* decrement, so
//! a concurrent destroy can never underflow the count: an object already at zero is answered with
//! `false` instead. [`enif_release_resource`](super::enif_release_resource) keeps the C's own
//! behaviour (a release without a matching keep is the caller's error, `erl_nif.c:3170`); this seam
//! does not change it.
//!
//! # The destructor is synchronous
//!
//! The [`unkeep_resource`] that takes the count to zero runs the type's destructor **here and now**, on
//! this thread, with a live environment — the crate's documented deviation from the C's misc aux-work
//! deferral (`resources.rs` → "The destructor's timing": the count, the ordering, the environment and
//! the payload are the oracle's, only the timing differs, and the deferred form cannot be reproduced
//! without a host-owned environment source this crate does not have).
//!
//! # These are Rust-side helpers, not `enif_*` slots
//!
//! Nothing here is `#[no_mangle]`, nothing here is part of the frozen driver/NIF surface, and
//! `ErlNifApi` stays at its **176** slots: the C-facing `enif_keep_resource`/`enif_release_resource`
//! keep their own `unsafe extern "C"` signatures and semantics untouched. A C NIF reaches the crate
//! through those; a Rust caller (the runtime) reaches it through these.
//!
//! # Residuals (honest)
//!
//! * The liveness set is keyed by payload **address** ([`Registry::object_type`]), so a stale pointer
//!   whose address a later [`enif_alloc_resource`](super::enif_alloc_resource) reused is
//!   indistinguishable from the live object now living there — the same substitution hazard the
//!   registry already carries for every `void*` operand (a `#Ref`-shaped id is substituted likewise;
//!   see `resources.rs`'s module docs). Not closable while the C ABI's operand is a bare `void*`.
//! * A **double** [`unkeep_resource`] of a still-live object (more releases than keeps) remains the C's
//!   caller error; the guarded decrement only refuses to underflow, it does not make the extra release
//!   meaningful.
//! * A caller that never [`unkeep_resource`]s a [`keep_resource`]ed object leaks it, exactly as an
//!   unbalanced `enif_keep_resource` does in the C.

use super::*;

use libc::c_void;

/// Hold the resource object `obj` alive — one reference, held until the matching
/// [`unkeep_resource`].
///
/// `true` means a reference was taken. `false` means none was: `obj` is null, it is not a live payload
/// pointer this crate allocated, or the object is already dying (module docs, "Refuse, never
/// fabricate"). `false` is **not** an error — a caller that treats it as "the interest could not be
/// armed" and skips the matching [`unkeep_resource`] is behaving correctly.
///
/// # Caller contract
///
/// * The reference must be dropped exactly once, by [`unkeep_resource`] (or by an
///   [`enif_release_resource`](super::enif_release_resource)/term-release prune that consumes it — the
///   count does not know which route released it). A reference never dropped is a leak, never a
///   dangling pointer.
/// * `obj` must stay a valid handle for the object for as long as the hold lasts; that is the whole
///   point of the hold, and the C's per-interest keep is used the same way.
// A safe public function taking a raw handle on purpose: `obj` is a `void*` a `NifHost` hook was
// handed, and this helper never dereferences it — the registry's live-object set is the only gate
// (`Registry::object_type`), and the one header access goes through `retain_object_if_alive`, which
// the registry has already justified. Marking it `unsafe fn` would push a contract the helper fully
// enforces onto every caller, including the `#![deny(unsafe_code)]` runtime.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn keep_resource(obj: *mut c_void) -> bool {
    if obj.is_null() {
        return false;
    }
    // The object's type is also the "is this pointer one of ours?" test: `Registry::object_type`
    // answers `None` for a pointer this crate never allocated (or already freed), and only the
    // registry may read a block's header — never this helper.
    if with_registry(|r| r.object_type(obj)).is_none() {
        return false;
    }
    // SAFETY: `obj` was registered as live by `object_type` above (the registry removes the entry
    // before a block is freed, under that very lock), so its block is allocated and its header is
    // readable. A count of zero means it is already dying: refuse rather than resurrect.
    unsafe { retain_object_if_alive(obj) }
}

/// Drop the reference [`keep_resource`] took — and, when it was the object's last one, run the type's
/// destructor (synchronously) and free the block.
///
/// `true` means one reference was dropped. `false` means none was: `obj` is null, it is not a live
/// payload pointer this crate allocated (in particular the object was already destroyed, so **no**
/// second destructor runs), or its count had already reached zero.
///
/// # Caller contract
///
/// `unkeep_resource` must not be called more often than [`keep_resource`] succeeded for the same
/// object — the C's own rule for `enif_release_resource` (`erl_nif.c:3170`). The guarded decrement
/// keeps such an over-release from underflowing the count, but it cannot give the extra release a
/// meaning: the release that legitimately follows may then destroy the object early.
// A safe public function taking a raw handle on purpose — see `keep_resource`'s rationale: the
// registry is the only gate, no unrecognised pointer is ever dereferenced, and the decrement goes
// through `release_object_if_alive`'s own guard.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn unkeep_resource(obj: *mut c_void) -> bool {
    if obj.is_null() {
        return false;
    }
    // The same membership gate as `keep_resource` — and here it is also what answers `false` for an
    // object that was already destroyed, because `destroy` unregisters the payload before freeing the
    // block.
    if with_registry(|r| r.object_type(obj)).is_none() {
        return false;
    }
    // SAFETY: `obj` is a live payload pointer (the registry said so under its lock), and the decrement
    // is guarded, so a concurrent destroy cannot drive the count below zero.
    unsafe { release_object_if_alive(obj) }
}
