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

//! The resource-type **`stop` callback invocation seam** — the C's `erts_resource_stop`
//! (`erts/emulator/beam/erl_nif.c:3032-3041`). The sibling child of `resources.rs`.
//!
//! # Ground truth
//!
//! ```c
//! void erts_resource_stop(ErtsResource* resource, ErlNifEvent e, int is_direct_call)
//! {
//!     struct enif_msg_environment_t msg_env;
//!     ASSERT(resource->type->fn.stop);
//!     pre_nif_noproc(&msg_env, resource->type->owner, NULL);
//!     resource->type->fn.stop(&msg_env.env, resource->data, e, is_direct_call);
//!     post_nif_noproc(&msg_env);
//! }
//! ```
//!
//! It has exactly two call sites, and `is_direct_call` is the whole difference between them:
//!
//! | call site | `is_direct_call` | verdict that led there |
//! |---|---|---|
//! | `erts/emulator/sys/common/erl_check_io.c:1531` (inside `enif_select`, scheduler thread) | `1` | `ERL_NIF_SELECT_STOP_CALLED` |
//! | `erl_check_io.c:2241` (the poller thread, for a deferred stop) | `0` | `ERL_NIF_SELECT_STOP_SCHEDULED` |
//!
//! # The environment
//!
//! The C builds a **nooproc message env** from the type's owner module instance, i.e. the callback
//! gets a live `ErlNifEnv*` that belongs to no process. This crate's stand-in for the owner is the
//! type's [`host`](ResourceType::host) — the host captured when the type was opened — and the env is
//! built from it exactly as [`destroy`](super::destroy) builds the destructor's env, including the
//! null-host degradation (a null env is the best-effort shape when no host is known). The C always
//! has an owner; here a type opened through an env-less call may not.
//!
//! # Refuse, never fabricate
//!
//! [`call_resource_stop`] answers `false` — and calls nothing — for
//!
//! * a **null** handle,
//! * a pointer that is **not one of ours** (not a live payload pointer this crate allocated, or one
//!   that was already freed), decided by [`Registry::object_type`] under the registry lock — the same
//!   "is it mine?" rule [`enif_get_resource`](super::enif_get_resource) and
//!   [`keep_resource`](super::keep_resource) apply, so this helper **never dereferences** a pointer it
//!   does not recognise where the C reads `DATA_TO_RESOURCE(obj)->type` unconditionally, and
//! * an object whose type was opened **without** a `stop` callback ([`resource_type_has_stop`] is the
//!   query). The C only `ASSERT`s the callback's existence (`erl_check_io.c:1279`) and that `ASSERT`
//!   is compiled out in a release build, so the oracle dereferences a NULL function pointer and takes
//!   the VM down (measured: SIGSEGV, 3/3 runs, the fixture's `probe_stop_nostop_type/0`). Here it is a
//!   silent-free `false` — a deliberate typed decision instead of a crash.
//!
//! # Reference accounting — this seam takes and releases nothing
//!
//! Neither helper touches the reference count: `stop` is a *notification*, not a lifetime event. The
//! C's release for a deferred stop happens at the poller (`erl_check_io.c:2242`) — i.e. in the drain
//! that schedules the call, [`run_pending_stops`](super::stop::run_pending_stops) — and for a
//! synchronous one at `:1532`/`:1506` (the `CALL_STOP_AND_RELEASE` arm, performed by the select path
//! that owns the interest). [`super::stop`](super::stop) and [`super::select`](super::select) therefore
//! own the accounting, and both do it per verdict.
//!
//! # Caller contract
//!
//! The caller must hold an outstanding reference on `obj` at call time — the C's own precondition
//! (`ASSERT(!erts_dbg_is_resource_dying(resource))`, `erl_check_io.c:1261`). Both of this crate's
//! callers do: the select path is reached from an `enif_select` call on an object the calling NIF
//! still references, and the drain keeps its own reference (`erl_check_io.c:1509`) for exactly as long
//! as the pending entry lives. A caller with no reference can lose the object to a concurrent destroy
//! between the registry lookup and the callback, exactly as the C loses it between its
//! `DATA_TO_RESOURCE(obj)` and its own use of the pointer.
//!
//! # These are Rust-side helpers, not `enif_*` slots
//!
//! Nothing here is `#[no_mangle]`, nothing here is part of the frozen driver/NIF surface, and
//! `ErlNifApi` stays at its **176** slots.

use super::*;

use libc::{c_int, c_void};

use crate::nif::types::ErlNifEvent;

/// Whether the resource object `obj` belongs to a type this crate opened **with** a `stop` callback —
/// the C's `resource->type->fn.stop` presence, which `erts_resource_stop` asserts
/// (`erts/emulator/beam/erl_nif.c:3036`) before it calls it.
///
/// `false` means "no": `obj` is null, it is not a live payload pointer this crate minted (an object
/// from another NIF module, a term-shaped word, a freed block), or its type carries no `stop`. This
/// function **never** dereferences a pointer it does not recognise.
pub fn resource_type_has_stop(obj: *mut c_void) -> bool {
    if obj.is_null() {
        return false;
    }
    with_registry(|r| {
        let Some(ty) = r.object_type(obj) else {
            return false;
        };
        if ty.is_null() {
            return false;
        }
        // SAFETY: `ty` is the registry's own `Box<ResourceType>` (never freed), so reading its `stop`
        // slot is always in bounds.
        unsafe { (*ty).stop }.is_some()
    })
}

/// Run the resource object's `stop` callback — the C's `erts_resource_stop(obj, event,
/// is_direct_call)` (`erts/emulator/beam/erl_nif.c:3032-3041`).
///
/// `true` means this crate allocated `obj`, its type has a `stop` callback, and the callback ran —
/// necessarily **synchronously**, on this thread, before returning. `false` means nothing ran (module
/// docs, "Refuse, never fabricate").
///
/// `is_direct_call` is the C's own operand: `true` for the scheduler-side call an `enif_select` STOP
/// verdict makes (`erl_check_io.c:1531`) and `false` for the poller-side call a deferred stop makes
/// (`:2241`).
///
/// # Caller contract
///
/// The caller must hold an outstanding reference on `obj` (module docs, "Caller contract"). The
/// callback runs **outside** the registry lock, so it may call straight back into this crate's entry
/// points — including re-entering this one.
// A safe public function taking a raw handle on purpose: `obj` is a `void*` a `NifHost` hook (or the
// runtime) was handed, and this helper never dereferences a pointer the registry does not recognise
// (the same rule `resources_keep.rs` documents). Marking it `unsafe fn` would push a contract the
// helper fully enforces onto every caller, including the `#![deny(unsafe_code)]` runtime.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn call_resource_stop(obj: *mut c_void, event: ErlNifEvent, is_direct_call: bool) -> bool {
    if obj.is_null() {
        return false;
    }
    // Resolve the callback **and** its environment source under the lock, then release the lock
    // before running it: `stop` may call straight back into these entry points, and `destroy` (which
    // can run from a callback's own release) takes the very same lock.
    let Some((stop, host)) = with_registry(|r| {
        let ty = r.object_type(obj)?;
        if ty.is_null() {
            return None;
        }
        // SAFETY: `ty` is a registry-owned `Box<ResourceType>`, never freed.
        let stop = unsafe { (*ty).stop }?;
        // SAFETY: as above — the host pointer is `ResourceType::host`, whose contract keeps it alive
        // for as long as any resource of the type can still be released (and so stopped).
        Some((stop, unsafe { (*ty).host }))
    }) else {
        return false;
    };
    let direct = c_int::from(is_direct_call);
    if host.is_null() {
        // SAFETY: the type's own `stop`; a null env is the same best-effort shape `destroy` uses when
        // no host is known (the C always has the owner module instance to build one from).
        unsafe { stop(core::ptr::null_mut(), obj, event, direct) };
    } else {
        // SAFETY: `host` was captured from a live `&mut dyn NifHost` when the type was opened, and the
        // caller contract keeps it alive while resources of the type can still be stopped (documented
        // on `ResourceType::host`).
        let env = NifEnv::new(unsafe { &mut *host });
        // SAFETY: the type's own `stop`; `env` outlives the call.
        unsafe { stop(env.as_ptr(), obj, event, direct) };
    }
    true
}
