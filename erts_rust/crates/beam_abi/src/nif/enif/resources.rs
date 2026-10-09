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
//!   oracle.
//!
//! # Documented substitutions (this crate has no `ERTS_MAGIC_REF` tag, no GC and no module identity)
//!
//! 1. **The resource term is minted here, on the host heap.** `beam_term` has no magic-reference
//!    tag, so a resource term is an ordinary **internal reference** built by this file — the same
//!    `[hdr|REF_SUBTAG][node][serial][id]` block `beam_rt`'s own `erlang:make_ref/0` builds — with a
//!    distinct id per call, recorded in the registry as *term word → (type, object)*.
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
//! 3. **Liveness: a resource TERM holds its own reference; the destructor's timing.** An exporting
//!    call (`enif_make_resource` / `enif_make_resource_binary`) takes **one extra reference** on the
//!    object for the term it returns — exactly the reference the C's magic reference holds
//!    (`erl_nif.c` `erts_bld_resource_ref` → `erts_mk_magic_ref` → `erts_bin_inc_refc`). A
//!    released-but-term-wrapped object therefore stays alive and `enif_get_resource` keeps resolving
//!    it to a live payload. Measured on the oracle: `alloc → enif_make_resource → release →
//!    enif_get_resource` reads back the `42` the NIF wrote (and the same for the
//!    `enif_make_resource_binary` path), where a runtime that frees at refcount zero would hand the
//!    NIF a dangling pointer.
//!    **The documented deviation from C:** the C drops that reference when the term is
//!    garbage-collected; this crate has no GC and no "term died" signal, so the term's reference is
//!    **never dropped**. An object that has ever been exposed as a resource term is therefore
//!    retained — block and payload — for the process lifetime, and its `dtor` does not run. That is a
//!    bounded, deliberate leak, and it is the *only* sound choice available: the alternative (freeing
//!    at refcount zero) makes `enif_get_resource` hand a NIF a freed pointer, i.e. a *wrong value*,
//!    which this crate never does. An object that no term holds is released and destroyed normally.
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
//! 6. **Not implemented (and therefore never faked):** monitors/`enif_monitor_process`, the
//!    `ERL_NIF_SELECT_STOP` (`stop`) and `down` *callbacks* are stored but never invoked (they are
//!    driven by `beam_io`/`beam_process`, which are not wired to this crate); no `rt_id`/`enif_select`
//!    registration; the type is never freed (the C frees it on module unload) — the registry is
//!    process-lifetime, exactly like the C's global `resource_type_list`.
//!
//! # The destructor's timing — the deviation the C does not let us close
//!
//! **What the C does** (`erts/emulator/beam/erl_nif.c`). `enif_release_resource` (`:3164-3175`)
//! drops one reference. The release that reaches zero does **not** run the destructor: it runs
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
use std::collections::HashMap;
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
    /// `ErlNifResourceDown*` — stored, never invoked (needs `enif_monitor_process`).
    #[allow(dead_code)]
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

/// One live term designating a resource object (the registry's value).
#[derive(Copy, Clone)]
struct ResourceRef {
    /// The object's `*mut ResourceType`.
    ty: *mut ResourceType,
    /// The resource payload pointer (what `enif_get_resource` writes to `*objp`).
    obj: *mut c_void,
}

/// The process-wide registry: the resource types ever opened and the live resource terms.
#[derive(Default)]
struct Registry {
    /// Every type ever opened, in open order (the C's global `resource_type_list`). A `Box` keeps the
    /// `*mut ResourceType` handed to NIFs stable across `Vec` growth.
    types: Vec<Box<ResourceType>>,
    /// `term word -> object` for every resource term this crate minted (see the module docs, point 1).
    terms: HashMap<usize, ResourceRef>,
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

    /// Record that the term word `word` designates the object `obj` of type `ty`.
    fn bind_term(&mut self, word: usize, r: ResourceRef) {
        self.terms.insert(word, r);
    }

    /// The object a resource term designates, if any.
    fn lookup_term(&self, word: usize) -> Option<ResourceRef> {
        self.terms.get(&word).copied()
    }

    /// Drop every term binding to `obj` — called as the object is destroyed, so a term that still
    /// designates it resolves to nothing instead of a dangling payload pointer.
    ///
    /// An object exported as a resource term holds its own reference (module docs, point 3), so a
    /// normal alloc/export/release sequence never reaches here with bindings outstanding; this fires
    /// for a caller-error over-release (one release more than the object has references), which the C
    /// leaves undefined. Cleaning the bindings keeps that caller error from becoming a dangling
    /// pointer in the Rust runtime.
    fn forget_object(&mut self, obj: *mut c_void) {
        self.terms.retain(|_, r| r.obj != obj);
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
    unsafe {
        block.write(ResourceHeader {
            refcount: AtomicUsize::new(1),
            ty: ty.cast::<ResourceType>(),
            data_sz: size,
        });
        let payload = payload_ptr(block);
        core::ptr::write_bytes(payload, 0, size);
        payload.cast::<c_void>()
    }
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
/// reference leaves it alive — exactly as on the oracle.
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

/// `ERL_NIF_TERM enif_make_resource(ErlNifEnv* env, void* obj);`
///
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
    with_registry(|r| {
        r.bind_term(reference.raw(), ResourceRef { ty, obj });
    });
    // The term now owns a reference — the reference the C's magic ref holds. It is never released
    // (there is no GC to collect the term); see the module docs, point 3.
    // SAFETY: `obj` is a live payload pointer with an outstanding reference (caller contract).
    unsafe { retain_object(obj) };
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
    with_registry(|r| {
        r.bind_term(term.raw(), ResourceRef { ty, obj });
    });
    // The term owns a reference, exactly as for `enif_make_resource` (module docs, point 3).
    // SAFETY: `obj` is a live payload pointer with an outstanding reference (caller contract).
    unsafe { retain_object(obj) };
    n(term)
}

/// `int enif_get_resource(ErlNifEnv* env, ERL_NIF_TERM term, ErlNifResourceType* type, void** objp);`
///
/// Returns `1` and writes `*objp` only when `term` designates a resource object of exactly `type`;
/// otherwise returns `0` and leaves `*objp` untouched. (The C writes `*objp` without a null check; a
/// null `objp` is tolerated here.)
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
    match with_registry(|r| r.lookup_term(word.raw())) {
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
/// the type and the type must carry a `dyncall`. Any other term falls through to the defaulted
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
    let Some(entry) = with_registry(|r| r.lookup_term(resource.raw())) else {
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

#[cfg(test)]
#[path = "resources_tests.rs"]
mod tests;
