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

//! `enif_*` — the resource family (workspace contract **A.3**).
//!
//! Owned slots (**all eleven are populated** — none is left `None`):
//! `enif_open_resource_type`, `enif_open_resource_type_x`, `enif_init_resource_type`,
//! `enif_alloc_resource`, `enif_release_resource`, `enif_keep_resource`, `enif_make_resource`,
//! `enif_make_resource_binary`, `enif_get_resource`, `enif_sizeof_resource`,
//! `enif_dynamic_resource_call`.
//!
//! Ground truth: `erts/emulator/beam/erl_nif.c` — `open_resource_type` (2713), `enif_alloc_resource`
//! (3121), `enif_release_resource` (3164), `enif_keep_resource` (3177), `enif_make_resource` (3197),
//! `enif_make_resource_binary` (3222), `enif_get_resource` (3261), `enif_sizeof_resource` (3285),
//! `enif_dynamic_resource_call` (3297) — plus the documented contract in `erts/doc/references/
//! erl_nif.md`. The observable semantics were **re-measured against the system oracle** (OTP 29.1.1 /
//! erts-17.1) with a scratch NIF, and the measured facts are cited inline.
//!
//! # What is real here
//!
//! * **A crate-internal resource-type registry.** `enif_open_resource_type` /
//!   `_x` / `enif_init_resource_type` allocate a boxed [`ResourceType`] (name + `dtor`/`stop`/`down`/
//!   `dyncall` + the `ErlNifResourceTypeInit::members` count), return it as
//!   `*mut ErlNifResourceType`, and are keyed by name.
//! * **`*tried` reproduces `erl_nif.c` exactly** (see [`open_resource_type`]) — including the rule the
//!   documentation states as *"on success `*tried` is `ERL_NIF_RT_CREATE` or `ERL_NIF_RT_TAKEOVER`;
//!   on failure it is set to `flags`"*. Measured: a fresh name with `CREATE|TAKEOVER` yields a
//!   non-`NULL` type and `tried == CREATE`; a fresh name with `TAKEOVER` alone yields `NULL` and
//!   `tried == TAKEOVER`.
//! * **A real refcounted allocation.** `enif_alloc_resource` `malloc`s one block: a 32-byte,
//!   16-byte-aligned header (refcount + type pointer + payload size) followed by the zeroed payload.
//!   `enif_keep_resource`/`enif_release_resource` are atomic refcount operations; **an exporting call
//!   (`enif_make_resource` / `enif_make_resource_binary`) takes a reference for the term it mints**
//!   — exactly the reference the C's magic reference holds — and the last release runs the type's
//!   `dtor` with a live environment and frees the block. `enif_sizeof_resource` returns the payload
//!   size the block was allocated with (measured: `13` for `enif_alloc_resource(…, 13)`, and also `13`
//!   for a type carrying a `down` callback, whose C path returns the recorded *user* data size).
//! * **`enif_get_resource` matches the measured oracle:** it writes `*objp` and returns `1` only for a
//!   resource term of that exact type, and returns `0` (leaving `*objp` untouched) for a term of
//!   another resource type, for a plain term, or for a non-term word. Because a resource term holds a
//!   reference (point 3 below), a term that was released by its NIF still resolves — exactly as on the
//!   oracle. Resolution is by **term identity**, not by heap address: a `copy_struct` copy of a
//!   resource term (the send path, ETS) designates the same object and resolves too — as on the
//!   oracle, whose magic refs are pointer-identified through `ErtsMRefThing.mb` and therefore
//!   copy-stable. See "The registry key is the term's IDENTITY" below.
//!
//! # Documented substitutions (this crate has no `ERTS_MAGIC_REF` tag, no GC and no module identity)
//!
//! 1. **The resource term is minted here, on the host heap.** `beam_term` has no magic-reference
//!    tag, so a resource term is an ordinary **internal reference** built by this file — the same
//!    `[hdr|REF_SUBTAG][node][serial][id]` block `beam_rt`'s own `erlang:make_ref/0` builds — with a
//!    distinct id per call, recorded in the registry as *(term identity, type, object)*.
//!    `enif_get_resource` then resolves that mapping (and checks the type), which is exactly the
//!    observable contract. Measured on the oracle: two `enif_make_resource` calls for one object
//!    yield two *distinct* terms that both print as `#Ref<0.…>`, so a fresh reference per call is
//!    faithful, not an approximation. Minting it **here** rather than through the host's
//!    [`NifHost::make_ref`] hook is what makes the round trip work on a host whose hook default is
//!    `None` — the real runtime (`beam_rt`) is exactly such a host, and with the hook-based mint the
//!    whole NIF call died with `badarg` at `enif_make_resource`.
//! 2. **A resource-binary term is also a resource handle.** Measured: `enif_get_resource` on the term
//!    returned by `enif_make_resource_binary` yields `1` and the resource object. So the term built
//!    here is registered as well. (Its *bytes* are copied onto the heap; the C zero-copies the
//!    caller's buffer into a refc binary. The bytes are the same; only the aliasing differs.)
//!    **The one identity residual:** an `enif_make_resource` term is a *reference*, so its identity
//!    survives a copy — but this term is an **on-heap binary**, and a binary carries no shared
//!    identity at all (the C's magic binary is an off-heap refc binary, which is copy-shared). A
//!    *copied* resource-**binary** term therefore does not resolve here, where the oracle's does. A
//!    typed residual (`enif_get_resource` answers `0`), never a wrong result.//! 3. **Liveness: a resource TERM holds its own reference; the destructor's timing.** An exporting
//!    call (`enif_make_resource` / `enif_make_resource_binary`) takes **one extra reference** on the
//!    object for the term it returns — exactly the reference the C's magic reference holds
//!    (`erl_nif.c` `erts_bld_resource_ref` → `erts_mk_magic_ref` → `erts_bin_inc_refc`). A
//!    released-but-term-wrapped object therefore stays alive and `enif_get_resource` keeps resolving
//!    it to a live payload. Measured on the oracle: `alloc → enif_make_resource → release →
//!    enif_get_resource` reads back the `42` the NIF wrote (and the same for the
//!    `enif_make_resource_binary` path), where a runtime that frees at refcount zero would hand the
//!    NIF a dangling pointer.
//!    **Where that reference goes:** the C drops it when the term is garbage-collected. This crate has
//!    no GC, so the "the term died" signal is a **caller-owned seam** — [`release_resource_terms`]
//!    drops the reference of every term that a caller-supplied reachability predicate calls
//!    unreachable, and runs the type's `dtor` when that was the last reference. The *reachability
//!    decision is the caller's* (a wrong "unreachable" is a use-after-free, a wrong "reachable" is
//!    only a leak), exactly as for the off-heap refc binaries. With **no** caller driving that seam —
//!    a plain `beam_abi` process, or a runtime whose GC is not wired to it — the reference is never
//!    dropped: that is the bounded, deliberate leak documented here before the seam existed, and it
//!    remains the only sound *default*, because freeing on our own at refcount zero would make
//!    `enif_get_resource` hand a NIF a freed pointer, i.e. a *wrong value*, which this crate never
//!    does. An object that no term holds is released and destroyed normally.
//!    The destructor itself is **run synchronously** on the release that takes the reference count to
//!    zero — exactly once per object, never while a `keep` is outstanding, and never for a `NULL`
//!    release. The C *schedules* it instead; the full comparison is in "The destructor's timing"
//!    below.
//! 4. **The type's identity is its name.** The C keys a resource type by `(module, name)` and takes
//!    the module from the calling module instance (`env->mod_nif->mod->module`); `enif_open_resource_type`
//!    additionally *asserts* `module_str == NULL`. The env handed to a `load`/NIF call here carries no
//!    module identity, so types are keyed by **name alone** and `module_str` is accepted and ignored.
//!    Consequently `enif_dynamic_resource_call` compares only the *name* atom, not the module atom
//!    (measured: the oracle returns non-zero for a module mismatch).
//! 5. **Load-time visibility.** In the C a type created inside a `load`/`upgrade` callback is only
//!    published after the callback returns, so a second open of the same name within one `load` does
//!    not find it (measured). This crate has no `load` session boundary (the loader owns it), so a
//!    created type is visible immediately. Only `*tried` is pinned by the compatibility claim and it
//!    is unaffected: the create branch reports `CREATE` either way.
//! 6. **The `ERL_NIF_SELECT_STOP` (`stop`) callback is stored but never invoked** (it is driven by
//!    `beam_io`, which is not wired to this crate). The `down` callback **is** invoked: the registry
//!    carries the C's per-object monitor tree as one table keyed by the stored reference block
//!    ([`Registry::monitors`], owned by [`monitors`]), [`deliver_resource_down_id`] reproduces
//!    `erts_fire_nif_monitor` (`erl_nif.c:3058-3115`) and [`Registry::forget_object`] cancels a dying
//!    object's monitors silently (the C's `dtor_demonitor`, `:2940-2948`). The registry also exposes
//!    the one fact the C reads out of the object before any of that
//!    (`ASSERT(!rsrc->monitors == !rsrc->type->fn.down)`, `erl_nif.c:3966`):
//!    [`resource_type_has_down`], i.e. whether the object's type was opened WITH a `down` callback —
//!    the C's `-1` gate (`:3968-3972`), answerable from the crate's own knowledge before any runtime
//!    hook is consulted. No `rt_id`/`enif_select` registration; the type is never freed (the C frees
//!    it on module unload) — the registry is process-lifetime, exactly like the C's global
//!    `resource_type_list`.
//!
//! # The registry key is the term's IDENTITY, not its heap word
//!
//! A resource term's identity is its **reference id** — the M1 block's last word — and not the
//! address of the block. The registry is therefore keyed by [`TermKey`]: a reference thing (header
//! arity [`prelude::INTERNAL_REF_PAYLOAD_WORDS`]) keys on [`prelude::internal_ref_id`], every other
//! term shape on its raw word. One private helper, [`term_key`], derives the key for **both** the
//! binding and the lookup, so the two can never disagree.
//!
//! Why it matters: `beam_message::copy_struct` (the send path, ETS) copies a term word for word, so a
//! copy is a *different heap address carrying the same identity*. Keying on the word made such a copy
//! miss (`enif_get_resource` answered `0` where the oracle answers `1`); keying on the identity makes
//! it resolve to the same object. It is also what the C does — its magic refs are pointer-identified
//! through `ErtsMRefThing.mb` (a refc-binary header the copy shares), i.e. copy-stable.
//!
//! The `Word` arm exists for the shapes that genuinely have no shared identity: an
//! `enif_make_resource_binary` term is an **on-heap binary** here (module docs, substitution 2), so a
//! copy of it is only itself. Per-mint refcounting is unaffected: each minting call mints a fresh id
//! (or a fresh binary) and takes its own object reference, so `terms` still holds exactly one binding
//! per *live* mint, and each removed binding releases exactly one reference.
//!
//! **A reused address evicts a dead mint.** Two *live* term blocks cannot share a heap word, so when a
//! fresh mint lands on a word some existing binding still occupies, that older binding's term block
//! has necessarily been freed: its term is dead, and only its reference is still standing. Binding the
//! new term therefore evicts the old entry and gives its reference back (outside the lock, so a
//! destructor may call back in) — which is what a prune would have done, one park point earlier, and
//! it keeps `resource_terms()`'s words unique.
//!
//! **Residual (reported, not closable from this file):** this crate's reference id space is its own
//! (`next_resource_ref_id`), independent of the runtime's (`beam_process::ProcessTable::next_reference`,//! which feeds `make_ref/0`, monitors, aliases and timers). A *plain* reference whose id happens to
//! equal a live resource term's id would resolve as that resource. `beam_process` documents the same
//! hazard class ("HAZARD 2"); its closure is one shared id allocator at the host level (for instance a
//! `NifHost` mint hook that draws from the runtime's allocator), which is a `beam_rt` change.
//!
//! # The destructor's timing — the deviation the C does not let us close
//!
//! **What the C does** (`erts/emulator/beam/erl_nif.c`). `enif_release_resource` (`:3164-3175`)//! drops one reference. The release that reaches zero does **not** run the destructor: it runs
//! `nif_resource_dtor_prologue` (`:2961-2972`), which does
//! `erts_schedule_misc_aux_work(<sched_id>, run_resource_dtor, bin)` and returns `0` ("don't free"),
//! so the magic binary and its payload stay live for an **aux-work** task to pick up.
//! `run_resource_dtor` (`:2974-3030`) then runs on that other context, builds a *fresh* message
//! environment from the type's owner module instance (`pre_nif_noproc(&msg_env, type->owner, NULL)`,
//! `:3016-3021`), calls `type->fn.dtor(&msg_env.env, resource->data)`, and only then frees the magic
//! binary (`:3029`). The destructor is thus **asynchronous and on a different thread**: measured on
//! the oracle, a destructor count read immediately after a release-to-zero still reads `0` and reads
//! `1` only a moment later.
//!
//! **Why that cannot be reproduced safely here.** The deferred destructor needs a live environment,
//! and the C gets one from a **refcounted, thread-independent** object: `type->owner` is the module
//! instance, whose lifetime is pinned by the type's own reference count (`enif_alloc_resource` does
//! `erts_refc_inc(&resource->type->refc, 2)`, `:3149`), so it outlives every resource of the type on
//! whichever thread the destructor later runs. This crate has no such object: the type's environment
//! source (`ResourceType::host`) is a **borrowed** host pointer, erased from the `&mut dyn NifHost`
//! of the NIF call that opened the type. That borrow is
//!   * **not `Send`** — a raw `*mut dyn NifHost` is `!Send`, and `unsafe impl Send`-ing it would be
//!     a lie, because nothing keeps that host alive across the boundary; and
//!   * **not refcounted** — `NifHost` offers no retention, so the borrow necessarily ends when the
//!     call that produced it returns (for the destructor path, the very call that released the last
//!     reference), i.e. *before* any deferred job could run.
//! Handing a later thread that pointer would run the destructor with a freed host: that is memory
//! unsafety, not merely a wrong result. The asynchronous form is therefore **not implemented, and no
//! stand-in is faked** — `destroy` runs the destructor **synchronously**, on the releasing thread,
//! with a live environment, exactly once.
//!
//! **What is identical, and what differs.** The **count** (one run per object whose reference count
//! reaches zero), the **ordering** (release order), the **environment** the callback receives (the
//! type's, not the releasing call's) and the **payload** it is handed are all the oracle's. Only the
//! **timing** differs: deterministic and in-call here, aux-work-driven and slightly deferred there.
//! It is pinned from both sides — `tests/nif_resource.rs::resource_family_end_to_end`'s
//! `roundtrip/0` last slot is `1` here and `0` on the oracle, while the Rust-only `dtor_probe/0` +
//! `dtor_state/0` (the oracle cannot run them: its C dereferences the `NULL` keep/release and
//! segfaults) pin the count and the order exactly.
//!
//! **The sound seam, if it is ever wanted** (a `NifHost`/`beam_rt` change — reported, not added
//! here): a defaulted `NifHost` hook by which the *host* owns the deferral and runs the job later
//! with a host **it** owns (its own refcounted runtime state), with the default answering "run the
//! job inline". `destroy` would hand it a job capturing the payload, the block and the `dtor`
//! pointer (wrapped in a `Send` newtype — sound because that block is exclusively owned by the
//! pending job once its term bindings are dropped), and run the job itself when the hook declines.
//! Reconstructing the type's host pointer inside such a job is precisely what cannot be done today.
//!
//! # The one cross-crate seam added
//!
//! `enif_dynamic_resource_call` first resolves the term in the registry (the common `dyncall` case:
//! the name must match the type, and the type must have a `dyncall`; measured return codes `0` and
//! `1`). A term that is *not* one of ours falls through to the new, **defaulted**
//! [`NifHost::dynamic_resource_call`] hook — the seam for a resource type implemented by another NIF
//! module, which this crate cannot resolve. `None` from the hook is "unsupported" and yields the C's
//! failure code `1`. The hook is purely additive: its default is `None`, so no existing `NifHost`
//! implementation changes.

use super::*;

use libc::{c_char, c_int, c_void, size_t};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::nif::host::NifHost;
use crate::nif::types::{
    ErlNifEnv, ErlNifResourceDtor, ErlNifResourceDown, ErlNifResourceDynCall, ErlNifResourceFlags,
    ErlNifResourceStop, ErlNifResourceType, ErlNifResourceTypeInit,
};

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_open_resource_type = Some(enif_open_resource_type);
    api.enif_open_resource_type_x = Some(enif_open_resource_type_x);
    api.enif_init_resource_type = Some(enif_init_resource_type);
    api.enif_alloc_resource = Some(enif_alloc_resource);
    api.enif_release_resource = Some(enif_release_resource);
    api.enif_keep_resource = Some(enif_keep_resource);
    api.enif_make_resource = Some(enif_make_resource);
    api.enif_make_resource_binary = Some(enif_make_resource_binary);
    api.enif_get_resource = Some(enif_get_resource);
    api.enif_sizeof_resource = Some(enif_sizeof_resource);
    api.enif_dynamic_resource_call = Some(enif_dynamic_resource_call);
    api
}

// ---------------------------------------------------------------------------------------------
// The allocated block
// ---------------------------------------------------------------------------------------------

/// The header of a resource block: the C `ErtsResource` bookkeeping minus the parts that belong to
/// subsystems this crate does not have (the magic-binary wrapper and the monitor array).
///
/// `repr(C, align(16))` pins the payload to the C's `ERTS_ALLOC_ALIGN_BYTES` (16 on the supported
/// LP64 targets) so the bytes handed to a NIF are as aligned as the oracle's; the asserts below make
/// the header size the payload offset.
#[repr(C, align(16))]
struct ResourceHeader {
    /// The reference count (`erts_refc_t`): `1` at allocation, `+1` per `enif_keep_resource`.
    refcount: AtomicUsize,
    /// The owning [`ResourceType`] (the C `resource->type`).
    ty: *mut ResourceType,
    /// The payload size the caller asked for — what `enif_sizeof_resource` reports
    /// (`offsetof(ErtsResource, data)`-relative data size, `erl_nif.c:3285-3295`).
    data_sz: usize,
}

/// The payload offset (`offsetof(ErtsResource, data)` analogue), padded to 16-byte alignment.
const RESOURCE_HEADER_SIZE: usize = core::mem::size_of::<ResourceHeader>();

const _: () = assert!(core::mem::size_of::<ResourceHeader>() == 32);
const _: () = assert!(core::mem::align_of::<ResourceHeader>() == 16);
const _: () = assert!(RESOURCE_HEADER_SIZE % 16 == 0);

/// The payload pointer of a block (`block as u8 + RESOURCE_HEADER_SIZE`).
///
/// # Safety
///
/// `block` must point at a live [`ResourceHeader`] allocation.
#[inline]
unsafe fn payload_ptr(block: *mut ResourceHeader) -> *mut u8 {
    // SAFETY: the caller guarantees `block` is a live 32-byte block, so `+32` stays inside it.
    unsafe { (block as *mut u8).add(RESOURCE_HEADER_SIZE) }
}

/// The header of a payload (`obj as u8 - RESOURCE_HEADER_SIZE`).
///
/// # Safety
///
/// `obj` must be a payload pointer previously returned by [`enif_alloc_resource`] whose block has not
/// been freed.
#[inline]
unsafe fn header_ptr(obj: *mut c_void) -> *mut ResourceHeader {
    // SAFETY: the caller guarantees `obj` came from `enif_alloc_resource`, i.e. it is exactly
    // `RESOURCE_HEADER_SIZE` bytes past a live header allocation.
    unsafe { (obj as *mut u8).sub(RESOURCE_HEADER_SIZE) as *mut ResourceHeader }
}

// ---------------------------------------------------------------------------------------------
// The resource-type registry
// ---------------------------------------------------------------------------------------------

/// The Rust body behind the opaque `*mut ErlNifResourceType` (the C `struct enif_resource_type_t`).
///
/// `dtor`/`stop`/`down`/`dyncall` are the C `type->fn` callbacks; `members` is the
/// `ErlNifResourceTypeInit::members` count the type was opened with; `host` is the environment
/// source for the destructor.
struct ResourceType {
    /// The name the type was opened under (the C `type->name` atom; see the module docs, point 4).
    name: String,
    /// `ErlNifResourceDtor*` — run when the last reference is released.
    dtor: Option<ErlNifResourceDtor>,
    /// `ErlNifResourceStop*` — stored, never invoked (needs `enif_select`; see the module docs).
    #[allow(dead_code)]
    stop: Option<ErlNifResourceStop>,
    /// `ErlNifResourceDown*` — run by [`deliver_resource_down_id`] when a monitor armed on a resource
    /// of this type fires (`erts_fire_nif_monitor`, `erl_nif.c:3107-3108`), and observable through
    /// [`resource_type_has_down`].
    down: Option<ErlNifResourceDown>,
    /// `ErlNifResourceDynCall*` — invoked through [`enif_dynamic_resource_call`].
    dyncall: Option<ErlNifResourceDynCall>,
    /// The `ErlNifResourceTypeInit::members` count the type was opened with.
    #[allow(dead_code)]
    members: c_int,
    /// The host the type was opened against — the C uses the owner module instance to build the
    /// destructor's `msg_env`; here the host captured from the opening call stands in for it.
    ///
    /// **Contract:** the caller must keep that host alive for as long as any of the type's resources
    /// can still be released (the pointer is dereferenced to build the destructor's environment). The
    /// pointer is refreshed on every successful open of the same name, so the most recent opening
    /// host is the one used.
    host: *mut dyn NifHost,
}

/// The **identity** of a term that designates a resource object — the registry's key.
///
/// A resource term is an M1 internal reference (module docs, substitution 1), and a reference is
/// identified by its `id` word, not by the address of its block: `copy_struct` (the send path, ETS)
/// produces a *different heap address with the same identity*, and the C's magic refs are
/// pointer-identified through `ErtsMRefThing.mb`, which a copy shares. Keying the registry by the raw
/// word therefore loses every copy of a resource term; keying it by the identity keeps them all
/// (module docs, "The registry key is the term's IDENTITY").
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum TermKey {
    /// An M1 internal-reference thing whose header arity is
    /// [`prelude::INTERNAL_REF_PAYLOAD_WORDS`], keyed by its copy-stable reference id.
    Ref(u64),
    /// Every other term shape, keyed by the raw term word. This is the arm
    /// `enif_make_resource_binary` lands in: its term is an **on-heap binary**, which carries no
    /// shared identity at all, so the word is the only identity it has.
    Word(usize),
}

/// The registry key of `term` — **the one place** binding and lookup derive identity, so they can
/// never disagree.
fn term_key(term: Eterm) -> TermKey {
    let word = term.raw();
    if !is_term_word(term) {
        // Not a term at all (a bare header word / `THE_NON_VALUE`): it has no shape to inspect.
        return TermKey::Word(word);
    }
    // SAFETY: `internal_ref_id` validates before it reads — it re-checks the `REF_SUBTAG`, refuses a
    // block whose header arity is not `INTERNAL_REF_PAYLOAD_WORDS` (a C-layout `[hdr, w1, w2]` block
    // has arity 2 and no id word), and refuses an id word that is not a non-negative small integer.
    // So it only dereferences the header of a boxed term the caller holds, and it answers a typed
    // error — never a wrong id — for anything else.
    match unsafe { prelude::internal_ref_id(term) } {
        Ok(id) => TermKey::Ref(id),
        // Not an M1 reference (a small int, an atom, a binary, a tuple, a C-layout ref block, …):
        // the raw word is the only identity such a term has.
        Err(_) => TermKey::Word(word),
    }
}
/// One live term designating a resource object (the registry's value).
#[derive(Copy, Clone)]
struct ResourceRef {
    /// The object's `*mut ResourceType`.
    ty: *mut ResourceType,
    /// The resource payload pointer (what `enif_get_resource` writes to `*objp`).
    obj: *mut c_void,
    /// The term word **as minted** — the address of the block this crate built, even though the map
    /// is keyed by the term's identity ([`TermKey`]).
    ///
    /// This is what the release seam enumerates ([`resource_terms`]) and hands to the caller's
    /// reachability predicate, so a caller keeps reasoning about the term it was given (its root set
    /// holds words, and `beam_rt`'s walk answers a candidate's own word for a direct hit *or* for a
    /// copy's identity).
    word: Eterm,
}

/// The process-wide registry: the resource types ever opened and the live resource terms.
#[derive(Default)]
struct Registry {
    /// Every type ever opened, in open order (the C's global `resource_type_list`). A `Box` keeps the
    /// `*mut ResourceType` handed to NIFs stable across `Vec` growth.
    types: Vec<Box<ResourceType>>,
    /// `term identity -> object` for every resource term this crate minted (see the module docs,
    /// point 1 and "The registry key is the term's IDENTITY"). One entry per **mint**; any number of
    /// copies of a minted term share it, because they all carry the same identity.
    terms: HashMap<TermKey, ResourceRef>,

    /// The **live payload pointers** of every object this crate allocated, i.e. the set of `void*`
    /// values that really are `&ErtsResource.data` in the C's sense.
    ///
    /// It exists so a `enif_*` entry point that is handed a bare `void*` `obj` can tell "an object
    /// this crate minted" from "a pointer I must not dereference": the C reaches the owning
    /// `ErtsResource` by pointer arithmetic on `obj` (`DATA_TO_RESOURCE`) and reads `rsrc->type`
    /// unconditionally, which is only sound under the caller's "`obj` is one of mine" contract. The
    /// entries are removed the moment the block is freed (see [`Registry::forget_object`]), so a
    /// stale pointer never resolves.
    objects: HashSet<usize>,
    /// The C's per-resource monitor tree (`rsrc->monitors->root`), as **one** process-wide table keyed
    /// by the reference block an arm stored ([`MonitorId`]).
    ///
    /// It lives here — under the same lock as `types`/`terms`/`objects` — rather than behind its own
    /// mutex: the delivery path has to consult `objects` and the header refcount while holding it, and
    /// [`Registry::forget_object`] has to cancel an object's monitors, so a second lock would invert
    /// the order against `destroy` and deadlock. The rules it obeys are in [`monitors`].
    monitors: HashMap<MonitorId, MonitorBinding>,
}

// SAFETY: the registry is a process-wide singleton guarded by a `Mutex`, and the raw pointers it
// carries are only ever dereferenced to (a) compare a type identity, (b) read a type's callbacks and
// name, and (c) build a destructor environment from a host that the caller contract keeps alive for at
// least as long as the type and its resources (documented on `ResourceType::host`). It therefore
// satisfies the `Send` bound the `Mutex` needs; it never exposes those pointers to another thread.
unsafe impl Send for Registry {}

impl Registry {
    /// The type already opened under `name`, if any.
    fn find_type(&self, name: &str) -> Option<*mut ResourceType> {
        let found = self.types.iter().find(|t| t.name == name)?;
        Some(&**found as *const ResourceType as *mut ResourceType)
    }

    /// Box `ty`, keep it for the process lifetime and return its stable pointer.
    fn insert_type(&mut self, ty: ResourceType) -> *mut ResourceType {
        self.types.push(Box::new(ty));
        let last = self.types.last_mut().expect("a type was just pushed");
        &mut **last as *mut ResourceType
    }

    /// Record that the term `term` designates the object `r` binds — keyed by the term's **identity**
    /// ([`term_key`]), so a later lookup through a copy of the term (same identity, different heap
    /// address) finds this binding.
    ///
    /// Also **evicts** a binding that is provably dead because the new term reuses its heap word: two
    /// *live* term blocks cannot share an address, so an existing binding whose word this term now
    /// occupies can only be a mint whose heap is gone (module docs, "A reused address evicts a dead
    /// mint"). The evicted object is returned so the caller can drop that binding's reference
    /// **outside** the registry lock — the destructor may call straight back into these entry points.
    fn bind_term(&mut self, term: Eterm, r: ResourceRef) -> Option<*mut c_void> {
        let key = term_key(term);
        let word = term.raw();
        let dead = self
            .terms
            .iter()
            .find(|(other, entry)| **other != key && entry.word.raw() == word)
            .map(|(other, _)| *other);
        let dead = dead.and_then(|other| self.terms.remove(&other)).map(|e| e.obj);
        self.terms.insert(key, r);
        dead
    }

    /// Record that `obj` is a **live** payload pointer this crate allocated.
    fn register_object(&mut self, obj: *mut c_void) {
        self.objects.insert(obj as usize);
    }

    /// Whether `obj` is a live payload pointer this crate allocated (`None` when it is not one, so
    /// the caller must not dereference it).
    fn object_type(&self, obj: *mut c_void) -> Option<*mut ResourceType> {
        if !self.objects.contains(&(obj as usize)) {
            return None;
        }
        // SAFETY: a registered payload pointer belongs to a block this crate `malloc`ed and has not
        // freed (the entry is removed before the block is freed, under this very lock), so its
        // `RESOURCE_HEADER_SIZE`-byte header is live. The header's `ty` is a `Box<ResourceType>` the
        // registry owns and never frees.
        Some(unsafe { (*header_ptr(obj)).ty })
    }

    /// The object the resource term `key` identifies, if any.
    ///
    /// The caller passes [`term_key`]'s answer, so a copy of a minted resource term resolves to the
    /// very binding the mint created.
    fn lookup_term(&self, key: TermKey) -> Option<ResourceRef> {
        self.terms.get(&key).copied()
    }

    /// Drop every term binding to `obj` **and** its liveness registration, and cancel its armed
    /// monitors — called as the object is destroyed, so a term that still designates it resolves to
    /// nothing (instead of a dangling payload pointer) and a later [`Registry::object_type`] query
    /// answers `None` rather than reading freed memory.
    ///
    /// The retention is on the **value**, not the key, so every binding that names this object goes —
    /// whichever identity key it was filed under (a reference id, or the word of an on-heap
    /// resource-binary term), and therefore every copy of every term that designated it.
    ///
    /// Cancelling the monitors is the C's `run_resource_dtor` → `dtor_demonitor` path
    /// (`erl_nif.c:2940-2948`, `:2974-3030`): a dying resource's monitors are removed **without** a
    /// `down` callback, so a monitor that outlived its resource never fires. The retention rule is
    /// [`Registry::forget_monitors_for_object`]'s (the `monitors` child owns it).
    ///
    /// An object exported as a resource term holds its own reference (module docs, point 3), so a
    /// normal alloc/export/release sequence never reaches here with bindings outstanding; this fires
    /// for a caller-error over-release (one release more than the object has references), which the C
    /// leaves undefined. Cleaning the bindings keeps that caller error from becoming a dangling
    /// pointer in the Rust runtime.
    fn forget_object(&mut self, obj: *mut c_void) {
        self.terms.retain(|_, r| r.obj != obj);
        self.objects.remove(&(obj as usize));
        self.forget_monitors_for_object(obj);
    }
}

/// The registry singleton (initialised on first use).
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// Run `f` against the registry under its lock.
///
/// A poisoned lock means an earlier holder panicked. The guard is recovered rather than re-panicked:
/// these entry points are reached across the C ABI, where unwinding is not an option.
fn with_registry<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mut guard = match registry().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// The lifetime-erased host pointer of a live env (the same device as [`NifEnv::new`]).
///
/// # Safety
///
/// `env` must be a live environment whose host outlives everything the returned pointer is stored in
/// (the contract documented on [`ResourceType::host`]).
unsafe fn host_pointer(env: &mut NifEnv) -> *mut dyn NifHost {
    // SAFETY: only the erased trait-object lifetime is reinterpreted — `&mut dyn NifHost` and
    // `*mut dyn NifHost` are layout-identical (data word + vtable word) and `mem::transmute` between
    // them is a bit-preserving cast, exactly as in `NifEnv::new`.
    unsafe { core::mem::transmute(env.host()) }
}

// ---------------------------------------------------------------------------------------------
// Opening a resource type
// ---------------------------------------------------------------------------------------------

/// The callbacks an `ErlNifResourceTypeInit` contributes, honouring the C's `init_members` switch
/// (`erl_nif.c:2747-2756`): `members` counts the initialised callbacks *from the top of the struct*,
/// i.e. `>=1` reads `dtor`, `>=2` `stop`, `>=3` `down`, `>=4` `dyncall`.
fn callbacks(
    init: &ErlNifResourceTypeInit,
    members: c_int,
) -> (
    Option<ErlNifResourceDtor>,
    Option<ErlNifResourceStop>,
    Option<ErlNifResourceDown>,
    Option<ErlNifResourceDynCall>,
) {
    let mut dtor = None;
    let mut stop = None;
    let mut down = None;
    let mut dyncall = None;
    if members >= 1 {
        dtor = init.dtor;
    }
    if members >= 2 {
        stop = init.stop;
    }
    if members >= 3 {
        down = init.down;
    }
    if members >= 4 {
        dyncall = init.dyncall;
    }
    (dtor, stop, down, dyncall)
}

/// The shared body of the three open functions — the C `open_resource_type` (`erl_nif.c:2697-2772`).
///
/// `*tried` is written verbatim from the C's `op` variable: it starts as `flags`, becomes exactly
/// `ERL_NIF_RT_CREATE` when a new type is created, exactly `ERL_NIF_RT_TAKEOVER` when an existing one
/// is taken over, and is otherwise left at `flags` (the failure rule the documentation states).
///
/// # Safety
///
/// `env` must be null or a live env; `name_str` must be null or NUL-terminated; `tried` must be null
/// or valid for one `ErlNifResourceFlags` write.
unsafe fn open_resource_type(
    env: *mut ErlNifEnv,
    name_str: *const c_char,
    init: &ErlNifResourceTypeInit,
    flags: ErlNifResourceFlags,
    tried: *mut ErlNifResourceFlags,
    init_members: c_int,
) -> *mut ErlNifResourceType {
    let mut op = flags;
    // SAFETY: delegated to `cstr`; a null name is refused below rather than dereferenced.
    let Some(name_bytes) = (unsafe { cstr(name_str) }) else {
        // SAFETY: `tried` is null or writable (caller contract).
        unsafe { write_out(tried, op) };
        return core::ptr::null_mut();
    };
    let name = String::from_utf8_lossy(name_bytes).into_owned();
    // The C reads at most four members (`ERTS_ASSERT`s on anything else, `erl_nif.c:2755`); a caller
    // bug fails the call here instead of aborting the runtime.
    if !(0..=4).contains(&init_members) {
        // SAFETY: as above.
        unsafe { write_out(tried, op) };
        return core::ptr::null_mut();
    }
    // SAFETY: `env` is null or live; `host_pointer`'s contract is discharged by the caller here, which
    // keeps the host alive for the type's lifetime (module docs, "Substitution 1"/`ResourceType::host`).
    let host = match unsafe { env_mut(env) } {
        Some(e) => unsafe { host_pointer(e) },
        None => {
            // SAFETY: `tried` is null or writable.
            unsafe { write_out(tried, op) };
            return core::ptr::null_mut();
        }
    };

    let ty = match with_registry(|r| r.find_type(&name)) {
        None => {
            // Not open: `CREATE` makes a new type; without it the call fails and `op` stays `flags`.
            if flags.bits() & ErlNifResourceFlags::CREATE.bits() != 0 {
                let (dtor, stop, down, dyncall) = callbacks(init, init_members);
                let new_type = ResourceType {
                    name,
                    dtor,
                    stop,
                    down,
                    dyncall,
                    members: init_members,
                    host,
                };
                // SAFETY: `host` is the env's (erased-borrow) host pointer; see `ResourceType::host`.
                let created = with_registry(|r| r.insert_type(new_type));
                op = ErlNifResourceFlags::CREATE;
                created
            } else {
                core::ptr::null_mut()
            }
        }
        Some(existing) => {
            // Already open: `TAKEOVER` reuses it; without it the call fails and `op` stays `flags`.
            if flags.bits() & ErlNifResourceFlags::TAKEOVER.bits() != 0 {
                op = ErlNifResourceFlags::TAKEOVER;
                existing
            } else {
                core::ptr::null_mut()
            }
        }
    };

    if !ty.is_null() {
        // Refresh the host used for the destructor: the most recent opening call is the freshest live
        // host for this type (see `ResourceType::host`).
        // SAFETY: `ty` is a live boxed type owned by the registry.
        unsafe { (*ty).host = host };
    }
    // SAFETY: `tried` is null or writable (caller contract).
    unsafe { write_out(tried, op) };
    ty as *mut ErlNifResourceType
}

/// `ErlNifResourceType* enif_open_resource_type(ErlNifEnv*, const char* module_str, const char* name_str, void (*dtor)(ErlNifEnv*,void*), ErlNifResourceFlags flags, ErlNifResourceFlags* tried);`
///
/// `module_str` is ignored (the C asserts it is `NULL`); `members` is `1` — only `dtor` is read.
///
/// # Safety
///
/// `env` must be a live env (a null env fails the call); `name_str` must be null or NUL-terminated;
/// `tried` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_open_resource_type(
    env: *mut ErlNifEnv,
    module_str: *const c_char,
    name_str: *const c_char,
    dtor: Option<ErlNifResourceDtor>,
    flags: ErlNifResourceFlags,
    tried: *mut ErlNifResourceFlags,
) -> *mut ErlNifResourceType {
    let _ = module_str;
    let init = ErlNifResourceTypeInit {
        dtor,
        stop: None,
        down: None,
        members: 1,
        dyncall: None,
    };
    // SAFETY: the caller's pointer contract is forwarded unchanged.
    unsafe { open_resource_type(env, name_str, &init, flags, tried, 1) }
}

/// `ErlNifResourceType* enif_open_resource_type_x(ErlNifEnv*, const char* name_str, const ErlNifResourceTypeInit*, ErlNifResourceFlags flags, ErlNifResourceFlags* tried);`
///
/// `members` is `3`: the documentation states that only `dtor`, `down` and `stop` are read by this
/// variant and that `dyncall` requires [`enif_init_resource_type`].
///
/// # Safety
///
/// As [`enif_open_resource_type`]; `init` must be non-null and readable.
#[no_mangle]
pub unsafe extern "C" fn enif_open_resource_type_x(
    env: *mut ErlNifEnv,
    name_str: *const c_char,
    init: *const ErlNifResourceTypeInit,
    flags: ErlNifResourceFlags,
    tried: *mut ErlNifResourceFlags,
) -> *mut ErlNifResourceType {
    if init.is_null() {
        // The C would dereference `NULL`; refuse the call so `*tried` still reports `flags`.
        // SAFETY: `tried` is null or writable (caller contract).
        unsafe { write_out(tried, flags) };
        return core::ptr::null_mut();
    }
    // SAFETY: the caller declares a live `ErlNifResourceTypeInit` at `init`.
    let init = unsafe { *init };
    // SAFETY: the caller's pointer contract is forwarded unchanged.
    unsafe { open_resource_type(env, name_str, &init, flags, tried, 3) }
}

/// `ErlNifResourceType* enif_init_resource_type(ErlNifEnv*, const char* name_str, const ErlNifResourceTypeInit*, ErlNifResourceFlags flags, ErlNifResourceFlags* tried);`
///
/// `members` is taken from the `init` struct, so `dyncall` can be installed.
///
/// # Safety
///
/// As [`enif_open_resource_type_x`].
#[no_mangle]
pub unsafe extern "C" fn enif_init_resource_type(
    env: *mut ErlNifEnv,
    name_str: *const c_char,
    init: *const ErlNifResourceTypeInit,
    flags: ErlNifResourceFlags,
    tried: *mut ErlNifResourceFlags,
) -> *mut ErlNifResourceType {
    if init.is_null() {
        // SAFETY: `tried` is null or writable.
        unsafe { write_out(tried, flags) };
        return core::ptr::null_mut();
    }
    // SAFETY: the caller declares a live `ErlNifResourceTypeInit` at `init`.
    let init = unsafe { *init };
    let members = init.members;
    // SAFETY: the caller's pointer contract is forwarded unchanged.
    unsafe { open_resource_type(env, name_str, &init, flags, tried, members) }
}

// ---------------------------------------------------------------------------------------------
// Allocating, keeping and releasing a resource object
// ---------------------------------------------------------------------------------------------

/// `void* enif_alloc_resource(ErlNifResourceType* type, size_t size);`
///
/// One `malloc`'d block: the 32-byte aligned [`ResourceHeader`] followed by `size` **zeroed** payload
/// bytes (the C hands out whatever the allocator returned; zeroing is strictly safer and observable
/// only through bytes the NIF never wrote). The reference count starts at `1`.
///
/// # Safety
///
/// `type` must be a type returned by one of the open functions (or null, which fails the call). The
/// returned pointer must eventually be `enif_release_resource`d.
#[no_mangle]
pub unsafe extern "C" fn enif_alloc_resource(
    ty: *mut ErlNifResourceType,
    size: size_t,
) -> *mut c_void {
    if ty.is_null() {
        return core::ptr::null_mut();
    }
    let Some(total) = RESOURCE_HEADER_SIZE.checked_add(size) else {
        return core::ptr::null_mut();
    };
    // SAFETY: a plain `malloc` of the computed block size; a null result is handled below.
    let block = unsafe { libc::malloc(total) }.cast::<ResourceHeader>();
    if block.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: `block` owns `total = RESOURCE_HEADER_SIZE + size` bytes, so both the header and the
    // payload are in bounds; the payload is zeroed and returned.
    let payload = unsafe {
        block.write(ResourceHeader {
            refcount: AtomicUsize::new(1),
            ty: ty.cast::<ResourceType>(),
            data_sz: size,
        });
        let payload = payload_ptr(block);
        core::ptr::write_bytes(payload, 0, size);
        payload.cast::<c_void>()
    };
    // Publish the payload as a live object of this crate, so a later `void*` operand (for instance
    // `enif_monitor_process`'s `obj`) can be recognised as one of ours before it is dereferenced.
    with_registry(|r| r.register_object(payload));
    payload
}

/// Take one extra reference on a live resource object — the `erts_bin_inc_refc` increment that
/// `enif_keep_resource` and every exporting call (`enif_make_resource` /
/// `enif_make_resource_binary`, via the C's `erts_bld_resource_ref`) perform.
///
/// # Safety
///
/// `obj` must be a live payload pointer from [`enif_alloc_resource`] with a non-zero reference count.
#[inline]
unsafe fn retain_object(obj: *mut c_void) {
    // SAFETY: `obj` is a payload pointer, so its header is the `RESOURCE_HEADER_SIZE` bytes before it
    // and lies in the same live block (caller contract).
    let header = unsafe { header_ptr(obj) };
    // SAFETY: `header` points at a live block whose count is non-zero (caller contract).
    unsafe { (*header).refcount.fetch_add(1, Ordering::Relaxed) };
}

/// Take one extra reference on a live resource object **only if it is still alive** — the C's
/// `erts_refc_inc_unless(&bin->binary.intern.refc, 0, 0)` that `erts_fire_nif_monitor` takes before
/// it runs a `down` callback (`erl_nif.c:3092`).
///
/// `true` means the count was non-zero and went up (the reference is now held by the caller, who must
/// drop it through [`enif_release_resource`]). `false` means it was already **zero**: the resource is
/// dying, so the C does **not** call `down` (`brefc == 0`, `:3093-3096`) — and neither may we, because
/// a resource whose count reached zero is already queued for destruction, and resurrecting it would
/// run its destructor twice.
///
/// # Safety
///
/// `obj` must be a payload pointer from [`enif_alloc_resource`] whose block is still allocated and not
/// freed — in practice an object still registered as live ([`Registry::object_type`] answers `Some`),
/// which is the only way `destroy` cannot have freed it.
#[inline]
unsafe fn retain_object_if_alive(obj: *mut c_void) -> bool {
    // SAFETY: `obj` is a payload pointer, so its header is the `RESOURCE_HEADER_SIZE` bytes before it
    // and lies in the same live block (caller contract).
    let header = unsafe { header_ptr(obj) };
    // SAFETY: `header` points at a live block; `fetch_update` reads and conditionally writes the count
    // atomically, so a concurrent release cannot interleave between the test and the increment.
    unsafe {
        (*header)
            .refcount
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count != 0).then_some(count + 1)
            })
    }
    .is_ok()
}

/// `void enif_keep_resource(void* obj);`
///
/// # Safety
///
/// `obj` must be a live payload pointer from [`enif_alloc_resource`] (or null, which is ignored —
/// the C would dereference it).
#[no_mangle]
pub unsafe extern "C" fn enif_keep_resource(obj: *mut c_void) {
    if obj.is_null() {
        return;
    }
    // SAFETY: `obj` is a live payload pointer with an outstanding reference (caller contract); null
    // was handled above.
    unsafe { retain_object(obj) };
}

/// `void enif_release_resource(void* obj);`
///
/// Drops one reference; the release that reaches zero runs the type's `dtor` (with a live environment
/// built from the type's host) and frees the block. An object that has been exported as a resource
/// term holds one more reference (see the module docs, point 3), so releasing the allocator's
/// reference leaves it alive — exactly as on the oracle. That extra reference is dropped through this
/// same function by [`release_resource_terms`], so both routes share one decrement/destructor path and
/// cannot diverge.
///
/// The destructor is run **synchronously** on this thread, where the C schedules it as misc aux work;
/// see the module docs, "The destructor's timing", for why the asynchronous form is not reproduced.
///
/// # Safety
///
/// `obj` must be a payload pointer from [`enif_alloc_resource`] with an outstanding reference (or
/// null, which is ignored — the C would dereference it). A release without a matching alloc/keep —
/// in particular a double release of the same pointer — is a caller error and is **not** defended
/// against, exactly as in the C (`erl_nif.c:3170` asserts the count is non-zero).
#[no_mangle]
pub unsafe extern "C" fn enif_release_resource(obj: *mut c_void) {
    if obj.is_null() {
        return;
    }
    // SAFETY: `obj` is a payload pointer, so its header is the `RESOURCE_HEADER_SIZE` bytes before it.
    let header = unsafe { header_ptr(obj) };
    // SAFETY: `header` points at a live block with a non-zero count (caller contract).
    let previous = unsafe { (*header).refcount.fetch_sub(1, Ordering::AcqRel) };
    if previous == 1 {
        // SAFETY: this call took the count from 1 to 0, so it is the only owner left.
        unsafe { destroy(header) };
    }
}

/// Run the destructor and free the block of a resource whose count just reached zero.
///
/// The destructor runs **here and now**, on the releasing thread, with a live environment built from
/// the type's host — where the C decouples the object and runs it later on a misc aux-work thread.
/// The deviation, its reason and the sound seam that would close it are documented in the module
/// header ("The destructor's timing").
///
/// # Safety
///
/// `header` must point at a live block whose reference count is exactly zero and which is not freed
/// twice.
unsafe fn destroy(header: *mut ResourceHeader) {
    // SAFETY: `header` is a live block (caller contract).
    let ty = unsafe { (*header).ty };
    // SAFETY: as above, and the payload is inside the block.
    let payload = unsafe { payload_ptr(header) }.cast::<c_void>();
    // Invalidate every term designating this object *before* the memory goes away, so a stale resource
    // term resolves to nothing rather than to a dangling pointer. The lock is released before the
    // destructor runs, which may itself call back into these entry points.
    with_registry(|r| r.forget_object(payload));
    if !ty.is_null() {
        // SAFETY: `ty` is a live boxed type owned by the registry (never freed).
        let (dtor, host) = unsafe { ((*ty).dtor, (*ty).host) };
        if let Some(dtor) = dtor {
            if host.is_null() {
                // SAFETY: the type's own destructor; a null env is the best-effort shape when no host
                // is known (the C always has the owner module instance to build one from).
                unsafe { dtor(core::ptr::null_mut(), payload) };
            } else {
                // SAFETY: `host` was captured from a live `&mut dyn NifHost` when the type was opened,
                // and the caller contract keeps it alive while resources can still be released
                // (documented on `ResourceType::host`).
                let mut env = NifEnv::new(unsafe { &mut *host });
                // SAFETY: the type's own destructor; `env` outlives the call.
                unsafe { dtor(env.as_ptr(), payload) };
            }
        }
    }
    // SAFETY: the block came from `libc::malloc` and is freed exactly once, here.
    unsafe { libc::free(header.cast::<c_void>()) };
}

/// `size_t enif_sizeof_resource(void* obj);`
///
/// The payload size (`erl_nif.c:3285-3295` returns the recorded user data size on both branches — with
/// and without a monitor array; measured `13` for both).
///
/// # Safety
///
/// `obj` must be a live payload pointer from [`enif_alloc_resource`] (null yields `0`).
#[no_mangle]
pub unsafe extern "C" fn enif_sizeof_resource(obj: *mut c_void) -> size_t {
    if obj.is_null() {
        return 0;
    }
    // SAFETY: `obj` is a payload pointer, so its header is in the same live block.
    unsafe { (*header_ptr(obj)).data_sz }
}

/// Whether the resource object `obj` belongs to a type this crate opened **with** a `down` callback
/// — the C's `rsrc->type->fn.down` presence, the fact `enif_monitor_process` gates its `-1` on
/// (`erl_nif.c:3968-3972`, whose own `ASSERT(!rsrc->monitors == !rsrc->type->fn.down)` at `:3966`
/// states the equivalence).
///
/// `None` means "no answer": `obj` is null, or it is not a live payload pointer this crate minted
/// (an object from another NIF module, a term-shaped word, a freed block). The caller must then fall
/// back to its own behaviour — this function **never** dereferences a pointer it does not recognise,
/// where the C would read `DATA_TO_RESOURCE(obj)->type` unconditionally.
///
/// Measured on the oracle (OTP 29.1.1 / erts-17.1, `tests/fixtures`-style NIF, both resource types
/// opened from the `load` callback — the only place the C allows it, `erl_nif.c:2708`):
/// `enif_open_resource_type` (`members = 1`, no `down`) ⇒ `enif_monitor_process` answers `-1` for
/// both a live and an undefined target; `enif_init_resource_type` with `members = 3` and a `down` ⇒
/// `0` for a live target and `1` for an undefined one.
pub fn resource_type_has_down(obj: *mut c_void) -> Option<bool> {
    if obj.is_null() {
        return None;
    }
    with_registry(|r| {
        let ty = r.object_type(obj)?;
        if ty.is_null() {
            return None;
        }
        // SAFETY: `ty` is the registry's own `Box<ResourceType>` (never freed), so reading its
        // `down` slot is always in bounds.
        Some(unsafe { (*ty).down }.is_some())
    })
}

// ---------------------------------------------------------------------------------------------
// Turning an object into a term (and back)
// ---------------------------------------------------------------------------------------------

/// The words of the internal-reference block [`mint_resource_ref`] writes: the header plus three
/// payload words (`node`, `serial`, `id`) — the same shape `beam_rt`'s `erlang:make_ref/0` and the
/// monitor references use, so the term prints as the oracle's `#Ref<0.…>`.
const RESOURCE_REF_PAYLOAD_WORDS: usize = 3;

/// The next resource-reference id.
///
/// A process-wide monotonic counter, exactly like the C's reference id space: two
/// `enif_make_resource` calls never mint the same id, so the two terms stay distinct (`#Ref<…>`
/// answers differ), which is what the oracle does (measured).
fn next_resource_ref_id() -> i64 {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed) as i64
}

/// Build the on-heap internal reference that designates a resource object (the C's magic ref).
///
/// The block is `[make_header(3, REF_SUBTAG), node, serial, id]` on the host heap, with `node` and
/// `serial` zero (Milestone 1 has one node and one id space) and a fresh monotonic `id`. Returns
/// `None` only when the host heap cannot fit the four words.
fn mint_resource_ref(e: &mut NifEnv) -> Option<Eterm> {
    // SAFETY: `alloc` hands out exactly `RESOURCE_REF_PAYLOAD_WORDS + 1` exclusively-owned,
    // writable words from the host heap; every one is written below, before the pointer escapes.
    let p = unsafe { alloc(e, RESOURCE_REF_PAYLOAD_WORDS + 1) }?;
    // SAFETY: `p` addresses the four words just handed out (see above).
    unsafe {
        *p = prelude::make_header(
            RESOURCE_REF_PAYLOAD_WORDS,
            beam_term::REF_SUBTAG,
        );
        *p.add(1) = prelude::make_small(0); // node: the local node
        *p.add(2) = prelude::make_small(0); // serial: one id space in M1
        *p.add(3) = prelude::make_small(next_resource_ref_id());
    }
    Some(prelude::make_boxed(p as *const Eterm))
}

/// Drop the reference an **evicted** (provably dead) binding held — outside the registry lock, so the
/// destructor it may run can call straight back into these entry points.
///
/// # Safety
///
/// `dead` must be null or a payload pointer that carried exactly one outstanding reference: the one
/// the evicted binding took when it was minted. [`Registry::bind_term`] only ever answers the object
/// of a binding whose heap word a *live* term has just re-occupied, which is exactly that case.
unsafe fn release_evicted(dead: Option<*mut c_void>) {
    if let Some(stale) = dead {
        // SAFETY: forwarded from the caller's contract; the registry lock is already released.
        unsafe { enif_release_resource(stale) };
    }
}

/// `ERL_NIF_TERM enif_make_resource(ErlNifEnv* env, void* obj);`///
/// Mints the resource term **here** — a fresh internal reference built on the host heap by
/// [`mint_resource_ref`] — and records it in the registry (module docs, substitution 1). The C
/// returns a fresh magic ref per call (measured: two calls for one object yield two distinct terms),
/// so minting a fresh reference is faithful. The host's `make_ref` hook is deliberately **not** used:
/// it is `None` by default, and the real runtime leaves it at that default, which made this whole
/// call raise `badarg` there.
///
/// The minted term takes **its own reference** on `obj` (the C's `erts_bld_resource_ref` →
/// `erts_mk_magic_ref`), so a later `enif_release_resource` of the allocator's reference does not free
/// the block while the term is observable (module docs, point 3).
///
/// # Safety
///
/// `env` must be a live env; `obj` must be a live payload pointer from [`enif_alloc_resource`].
#[no_mangle]
pub unsafe extern "C" fn enif_make_resource(env: *mut ErlNifEnv, obj: *mut c_void) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    if obj.is_null() {
        return bare_badarg();
    }
    let Some(reference) = mint_resource_ref(e) else {
        // The host heap cannot fit a four-word reference term; fail loudly rather than minting a
        // term that would not designate anything. (No reference is taken, so the caller's single
        // release still frees the block — the term was never created.)
        return bare_badarg();
    };
    // SAFETY: `obj` is a payload pointer, so its header is in the same live block.
    let ty = unsafe { (*header_ptr(obj)).ty };
    // The term takes its reference **before** it is published — the reference the C's magic ref
    // holds. It is dropped by `release_resource_terms` once the caller's reachability analysis says
    // the term is dead, and never otherwise (there is no GC to collect the term); see the module
    // docs, point 3.
    // SAFETY: `obj` is a live payload pointer with an outstanding reference (caller contract).
    unsafe { retain_object(obj) };
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
    // back (module docs, "A reused address evicts a dead mint"). It is never `obj` itself — the
    // caller's contract keeps `obj` live.
    // SAFETY: `dead` is the evicted binding's payload pointer, exactly one reference outstanding
    // (see `release_evicted`'s contract); `bind_term` is the only producer of that answer.
    unsafe { release_evicted(dead) };
    n(reference)
}

/// `ERL_NIF_TERM enif_make_resource_binary(ErlNifEnv* env, void* obj, const void* data, size_t size);`
///
/// Builds an **on-heap** binary holding a copy of `data[0..size]` and records the term as a resource
/// handle of `obj` — the oracle returns `1` from `enif_get_resource` on such a term (measured). The
/// documented C contract that `data` must stay readable until the destructor runs is therefore
/// *relaxed* here (the bytes are copied); the bytes a NIF observes are identical. Like
/// [`enif_make_resource`], the minted term takes its own reference on `obj` (the C's magic binary
/// does; measured on the oracle: `alloc → enif_make_resource_binary → release → enif_get_resource`
/// still resolves, with no destructor run).
///
/// # Safety
///
/// `env` must be a live env; `obj` must be a live payload pointer; `data` must be null or point at
/// `size` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn enif_make_resource_binary(
    env: *mut ErlNifEnv,
    obj: *mut c_void,
    data: *const c_void,
    size: size_t,
) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    if obj.is_null() {
        return bare_badarg();
    }
    let Some(bits) = size.checked_mul(8) else {
        return bare_badarg();
    };
    let bytes: &[u8] = if data.is_null() || size == 0 {
        &[]
    } else {
        // SAFETY: the caller declares `size` readable bytes at `data`.
        unsafe { core::slice::from_raw_parts(data.cast::<u8>(), size) }
    };
    // SAFETY: `heap_bits_size_words` sizes the `[hdr][size][payload]` block.
    let Some(hp) = (unsafe { alloc(e, prelude::heap_bits_size_words(bits)) }) else {
        return bare_badarg();
    };
    // SAFETY: `hp` heads the reserved block and `bytes` supplies exactly the `bits` payload bits.
    let term = match unsafe { prelude::write_binary(hp, bytes) } {
        Ok(term) => term,
        Err(_) => return bare_badarg(),
    };
    // SAFETY: `obj` is a payload pointer, so its header is in the same live block.
    let ty = unsafe { (*header_ptr(obj)).ty };
    // The term takes its reference before it is published, exactly as for `enif_make_resource`
    // (module docs, point 3).
    // SAFETY: `obj` is a live payload pointer with an outstanding reference (caller contract).
    unsafe { retain_object(obj) };
    let dead = with_registry(|r| {
        r.bind_term(
            term,
            ResourceRef {
                ty,
                obj,
                word: term,
            },
        )
    });
    // As in `enif_make_resource`: a dead binding whose word this term reuses gives its reference back.
    // SAFETY: `dead` is the evicted binding's payload pointer, exactly one reference outstanding
    // (see `release_evicted`'s contract); `bind_term` is the only producer of that answer.
    unsafe { release_evicted(dead) };
    n(term)
}

/// `int enif_get_resource(ErlNifEnv* env, ERL_NIF_TERM term, ErlNifResourceType* type, void** objp);`
///
/// Returns `1` and writes `*objp` only when `term` designates a resource object of exactly `type`;
/// otherwise returns `0` and leaves `*objp` untouched. (The C writes `*objp` without a null check; a
/// null `objp` is tolerated here.)
///
/// Resolution is by the term's **identity** ([`term_key`]), not by its heap address, so a
/// `copy_struct` copy of a resource term — a different heap address with the same reference identity —
/// resolves to the same object, exactly as the C's copy-stable magic refs do.
///
/// # Safety
///
/// `env` must be a live env (it is unused, as in the C); `type` must be a type from the open family or
/// null; `objp` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_resource(
    env: *mut ErlNifEnv,
    term: ERL_NIF_TERM,
    ty: *mut ErlNifResourceType,
    objp: *mut *mut c_void,
) -> c_int {
    let _ = env;
    if ty.is_null() {
        return 0;
    }
    let target = ty.cast::<ResourceType>();
    let word = t(term);
    if !is_term_word(word) {
        return 0;
    }
    match with_registry(|r| r.lookup_term(term_key(word))) {
        Some(entry) if entry.ty == target => {
            if !objp.is_null() {
                // SAFETY: `objp` is non-null and writable (checked; the caller declares it).
                unsafe { objp.write(entry.obj) };
            }
            1
        }
        _ => 0,
    }
}

/// `int enif_dynamic_resource_call(ErlNifEnv*, ERL_NIF_TERM mod, ERL_NIF_TERM name, ERL_NIF_TERM rsrc, void* call_data);`
///
/// Returns `0` after calling the identified type's `dyncall`, and non-zero when no call was made
/// (`erl_nif.c:3297-3323`). A term this crate minted is resolved directly: the `name` atom must match
/// the type and the type must carry a `dyncall`. Resolution is by the term's identity
/// ([`term_key`]), so a `copy_struct` copy of the minted term reaches the `dyncall` too. Any other
/// term falls through to the defaulted
/// [`NifHost::dynamic_resource_call`] hook (a resource type from another NIF module); `None` from the
/// hook is unsupported and yields the C's failure code `1`.
///
/// # Safety
///
/// `env` must be a live env; `call_data` is passed through to the callback verbatim.
#[no_mangle]
pub unsafe extern "C" fn enif_dynamic_resource_call(
    env: *mut ErlNifEnv,
    module: ERL_NIF_TERM,
    name: ERL_NIF_TERM,
    rsrc: ERL_NIF_TERM,
    call_data: *mut c_void,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        // The C dereferences `caller_env`; refusing the call is the honest failure return.
        return 1;
    };
    let env_ptr = e.as_ptr();
    let resource = t(rsrc);
    if !is_term_word(resource) {
        // Not a term at all — never a resource handle (the C's `is_internal_magic_ref` is false).
        return 1;
    }
    let Some(entry) = with_registry(|r| r.lookup_term(term_key(resource))) else {
        // Not a resource object this crate minted: let the host resolve it (another NIF module).
        return match e
            .host()
            .dynamic_resource_call(env_ptr, t(module), t(name), resource, call_data)
        {
            Some(code) => code,
            None => 1,
        };
    };
    // SAFETY: `entry.ty` is a live boxed type owned by the registry.
    let (ty_name, dyncall) = unsafe { ((*entry.ty).name.clone(), (*entry.ty).dyncall) };
    // The oracle also compares the type's *module* atom; the env carries no module identity here, so
    // only the name is compared (module docs, substitution 4).
    let name_matches = e
        .host()
        .intern_atom(&ty_name)
        .map(|atom| n(atom) == name)
        .unwrap_or(false);
    if !name_matches {
        return 1;
    }
    match dyncall {
        Some(dyncall) => {
            // SAFETY: the type's own `dyncall` callback; `env_ptr` designates the live caller env,
            // which outlives the call, and `entry.obj` is the object the term designates.
            unsafe { dyncall(env_ptr, entry.obj, call_data) };
            0
        }
        None => 1,
    }
}

// ---------------------------------------------------------------------------------------------
// The term-release seam — the caller-owned "the term died" signal
// ---------------------------------------------------------------------------------------------

// The seam lives in its own file (this one is at the ~1000-line target) and is re-exported here, so
// `nif::enif::resources::{resource_terms, release_resource_terms}` is the public path.
#[path = "resources_release.rs"]
mod release;

pub use self::release::{release_resource_terms, resource_terms};

// ---------------------------------------------------------------------------------------------
// The monitor tree — the `down`-callback delivery seam
// ---------------------------------------------------------------------------------------------

// The C's per-resource monitor tree and the `down` delivery path live in their own file (this one is
// at the ~1000-line target); the registry field they own is `Registry::monitors` above.
#[path = "resources_monitors.rs"]
mod monitors;

// The binding type the registry's monitor table is keyed to.
use self::monitors::MonitorBinding;

pub use self::monitors::{
    deliver_resource_down, deliver_resource_down_id, resource_monitors, MonitorId,
};

// The two crate-internal hooks `super::process` drives the arm/demonitor entry points through.
pub(super) use self::monitors::{record_monitor, take_monitor};

#[cfg(test)]
#[path = "resources_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "resources_monitors_tests.rs"]
mod monitor_tests;
