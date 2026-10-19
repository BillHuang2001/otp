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

//! The resource **monitor tree** and the `down`-callback delivery seam — the sibling child of
//! `resources.rs`.
//!
//! # Ground truth: the monitor lives in the RESOURCE, and `down` is fired from there
//!
//! `erts/emulator/beam/erl_nif.c`:
//!
//! * `enif_monitor_process` (`:3954-4011`) inserts a monitor record into the **resource object's own
//!   monitor tree** (`erts_monitor_tree_insert(&rm->root, &mdp->origin)`, `:3983-3986`) and only
//!   *then* copies the 24-byte reference block into the caller's `ErlNifMonitor`
//!   (`erts_ref_to_driver_monitor(ref, monitor)`, `:4008-4009`).
//! * `erts_fire_nif_monitor` (`:3058-3115`) is how `down` is delivered. Under the resource's monitor
//!   lock it computes `active = !is_dying && erts_monitor_is_in_table(omon)`; when active it
//!   **deletes the monitor from the tree first** (`:3091`) and takes a protective reference on the
//!   resource (`erts_refc_inc_unless(&bin->binary.intern.refc, 0, 0)`, `:3092`), then unlocks. When
//!   `!active` there is no callback; when active but the protective reference could not be taken
//!   (`brefc == 0`, the resource is already dying) there is **no callback** either. Otherwise it
//!   builds `erts_ref_to_driver_monitor(mdp->ref, &nif_monitor)` (`:3105`) and
//!   `nif_pid.pid = omon->other.item` — the **recorded target pid** (`:3106`) — builds the env from
//!   the *type's owner* (`pre_nif_noproc`, `:3107`) and calls
//!   `resource->type->fn.down(&msg_env.env, resource->data, &nif_pid, &nif_monitor)`
//!   (`:3107-3108`), then releases the protective reference (`:3110`).
//! * `enif_demonitor_process` (`:4019-4052`) rebuilds the reference from the caller's holder, looks it
//!   up **in the resource's monitor tree**, deletes it if found, and answers `0` when it found it /
//!   `1` when it did not. Nothing outside the resource is consulted.
//! * `run_resource_dtor` (`:2974-3030`) — a dying resource **cancels** every monitor in its tree
//!   (`dtor_demonitor` → `erts_proc_sig_send_demonitor`, `:2940-2948`) and runs **no** `down`. A
//!   monitor whose resource was destroyed therefore never fires.
//!
//! # The Rust shape
//!
//! One process-wide table in the crate's registry — the same [`Registry`] mutex the resource types and
//! terms live under, **not** a second lock (a second lock would create a lock-order inversion with
//! `destroy`/`Registry::forget_object` and can deadlock) — maps [`MonitorId`] (the C's 24-byte
//! reference block) to the binding: the object, its type and the recorded target pid word.
//!
//! [`deliver_resource_down_id`] is the ABI half of `erts_fire_nif_monitor`, in the C's own order:
//! take the binding out **under the lock** (so only one caller can ever deliver a given monitor),
//! check the object is still live and take the protective `inc-unless-zero` reference, release the
//! lock, run the callback **outside** it (a re-entrant `enif_*` call from `down` must not deadlock),
//! then drop the protective reference.
//!
//! # Why a binding holds NO reference of its own
//!
//! Holding one would keep a monitored resource alive past reference-count zero — where the oracle
//! destroys it and **cancels** the monitor without firing `down` (`run_resource_dtor`, `:2974-3030`,
//! measured: arm, release-to-zero, kill the target ⇒ `down` count `0`). The protective
//! `inc-unless-zero` taken just before the callback is what keeps the block alive *across* the
//! callback instead, exactly as the C's `brefc` does.
//!
//! # The pid operand
//!
//! The `pid` handed to [`deliver_resource_down`] is the host's — it is the one that observed the
//! death, and it is what the C takes out of the monitor record (`omon->other.item`, `:3106`). The
//! word recorded at arm time is what [`resource_monitors`] reports for reconciliation.

use super::*;

use libc::c_void;

use crate::nif::enif::process::{holder_from_block, monitor_block, ref_block_of, REF_THING_BYTES};
use crate::nif::types::{ErlNifMonitor, ErlNifPid};

/// The identity of an armed process monitor: the C's 24-byte
/// `ERTS_REF_THING_SIZE * sizeof(Eterm)` reference block (`io.c:6994-7010`), **not** the `Eterm` it
/// was minted from.
///
/// The difference matters: the reference term is a heap word that dies with the NIF-call heap,
/// while the 24-byte block is what the C stores in the caller's `ErlNifMonitor` and what the
/// resource's monitor tree is keyed by (`erts_monitor_tree_lookup(rm->root, ref)`,
/// `erl_nif.c:4046`). A host that only kept the holder can still address the monitor through this
/// type ([`MonitorId::from_holder`]).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonitorId([u8; REF_THING_BYTES]);

/// A hand-written `Debug` (the derived one would print 24 decimal bytes) — the block is hex.
impl core::fmt::Debug for MonitorId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MonitorId(0x")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        f.write_str(")")
    }
}

impl MonitorId {
    /// The monitor identity of an internal-reference term (the [`NifMonitorResult::Armed`] payload).
    ///
    /// `None` when `monitor` is not an ordinary internal reference — the same refusal
    /// `enif_monitor_process` applies before it stores anything (a payload that is not a reference
    /// block could not be rebuilt by `enif_make_monitor_term`).
    pub fn from_term(monitor: Eterm) -> Option<MonitorId> {
        ref_block_of(monitor).map(MonitorId)
    }

    /// The monitor identity carried by a C `ErlNifMonitor` the caller holds.
    ///
    /// # Safety
    ///
    /// `holder` must be null or point at a readable `ErlNifMonitor`.
    pub unsafe fn from_holder(holder: *const ErlNifMonitor) -> Option<MonitorId> {
        // SAFETY: forwarded from the caller's contract.
        unsafe { monitor_block(holder) }.map(MonitorId)
    }

    /// The 24 payload bytes, for a caller that wants to compare against its own copy.
    pub fn bytes(&self) -> [u8; REF_THING_BYTES] {
        self.0
    }
}

/// One armed monitor (the registry's value; the C `ErtsMonitorData` + its tree entry).
pub(super) struct MonitorBinding {
    /// The resource payload pointer the monitor was armed on (`DATA_TO_RESOURCE(obj)`'s payload).
    pub(super) obj: *mut c_void,
    /// The owning type — the source of the `down` callback and of the environment it is built from.
    pub(super) ty: *mut ResourceType,
    /// The recorded target pid, as the raw `Eterm` word `enif_monitor_process` was handed.
    pub(super) pid: usize,
}

/// Run the resource type's `down` callback for `monitor` — the ABI half of `erts_fire_nif_monitor`
/// (`erl_nif.c:3058-3115`).
///
/// `monitor` is the internal-reference term the arming host handed over
/// ([`NifMonitorResult::Armed`]'s payload); `pid` is the target that died. Returns `true` when the
/// callback ran. Everything else (an unknown/consumed monitor, a non-reference term) answers `false`.
pub fn deliver_resource_down(monitor: Eterm, pid: Eterm) -> bool {
    match MonitorId::from_term(monitor) {
        Some(id) => deliver_resource_down_id(id, pid),
        None => false,
    }
}

/// The [`MonitorId`] form of [`deliver_resource_down`] — the route a host takes when it only kept the
/// 24-byte holder (the minted reference term lives on a NIF-call heap that has since gone).
///
/// The steps, and the `erl_nif.c` line each reproduces:
///
/// 1. **Take the binding out under the lock.** No binding ⇒ `false`: already delivered, already
///    demonitored, resource destroyed, or never armed. The removal under the lock is what makes a
///    second delivery a no-op — only one caller can ever take a given binding (`:3091`, the
///    unlink-before-call).
/// 2. **Refuse a dead object.** If the object is no longer a live registered object the block may
///    already be freed, so nothing is run. Otherwise take the protective reference with the C's
///    `erts_refc_inc_unless(&refc, 0, 0)` (`:3092`); a refusal means the resource is already dying
///    and `down` is **not** called (`brefc == 0`).
/// 3. **Release the lock, then run the callback** — a re-entrant `enif_*` call from `down` must not
///    deadlock. The operands are the object, the pid and the 24 reference bytes the arm stored
///    (`:3105-3106`), and the environment comes from the **type's** host, exactly as `destroy` builds
///    the destructor's (`pre_nif_noproc(&msg_env, type->owner, NULL)`, `:3107`).
/// 4. **Drop the protective reference** (`:3110`) — which may synchronously run the destructor; that
///    is fine and expected, the lock is already released.
pub fn deliver_resource_down_id(monitor: MonitorId, pid: Eterm) -> bool {
    // 1 + 2, under the one registry lock.
    let picked = with_registry(|r| {
        let binding = r.monitors.remove(&monitor)?;
        if !r.objects.contains(&(binding.obj as usize)) {
            return None;
        }
        // SAFETY: `obj` is still a *registered* live payload pointer under this very lock — `destroy`
        // removes the registration before it frees — so its header is readable. The reference count
        // may legitimately be zero here (a release that reached zero but whose `destroy` has not yet
        // taken the lock); that is the C's dying-resource case and `retain_object_if_alive` refuses
        // it instead of resurrecting it.
        let retained = unsafe { retain_object_if_alive(binding.obj) };
        if retained {
            Some((binding.obj, binding.ty))
        } else {
            None
        }
    });
    let Some((obj, ty)) = picked else {
        return false;
    };

    // 3, outside the lock.
    let mut ran = false;
    if !ty.is_null() {
        // SAFETY: `ty` is a live `Box<ResourceType>` the registry owns and never frees.
        let (down, host) = unsafe { ((*ty).down, (*ty).host) };
        if let Some(down) = down {
            let mut nif_pid = ErlNifPid { pid: n(pid) };
            let mut nif_monitor = holder_from_block(&monitor.bytes());
            if host.is_null() {
                // SAFETY: the type's own `down`; a null env is the same best-effort shape `destroy`
                // uses for a type whose host is unknown (the C always has the owner module instance).
                unsafe { down(core::ptr::null_mut(), obj, &mut nif_pid, &mut nif_monitor) };
            } else {
                // SAFETY: `host` was captured from a live `&mut dyn NifHost` when the type was opened,
                // and the caller contract keeps it alive while resources of the type can be released
                // (documented on `ResourceType::host`).
                let mut env = NifEnv::new(unsafe { &mut *host });
                // SAFETY: the type's own `down`; `env` outlives the call.
                unsafe { down(env.as_ptr(), obj, &mut nif_pid, &mut nif_monitor) };
            }
            ran = true;
        }
    }

    // 4: the protective reference. If it was the last one this runs the destructor synchronously —
    // outside the lock, which is exactly why the callback above runs outside it too.
    // SAFETY: step 2 took one reference on `obj` that nothing else can have dropped, so an
    // outstanding reference is guaranteed here.
    unsafe { enif_release_resource(obj) };

    ran
}

/// A snapshot of every armed monitor this crate knows about: `(monitor, target pid)`, sorted by the
/// monitor bytes (deterministic; a `HashMap` order would not be).
///
/// The pid is reconstructed with [`Eterm::from_raw`] and is an **identity**, the same caveat
/// [`resource_terms`] documents: compare raw words, and only while the heap the term came from is
/// alive. There is no ordering meaning in the monitor bytes; the sort exists so two calls agree.
pub fn resource_monitors() -> Vec<(MonitorId, Eterm)> {
    let mut snapshot: Vec<(MonitorId, Eterm)> = with_registry(|r| {
        r.monitors
            .iter()
            .map(|(id, binding)| (*id, Eterm::from_raw(binding.pid)))
            .collect()
    });
    snapshot.sort_by_key(|entry| entry.0);
    snapshot
}

/// Record that `block` identifies a monitor armed on the resource object `obj` for the target `pid` —
/// the C's `erts_monitor_tree_insert(&rm->root, &mdp->origin)` + `rmon_refc_inc` (`:3983-3986`).
///
/// Answers `false` — and records nothing — when `obj` is not a live payload pointer this crate
/// allocated, or its type carries no `down` callback: a binding that could never be delivered on is
/// not worth keeping (and dereferencing a foreign `obj` would be unsound). A repeated arm of the same
/// monitor id replaces the earlier binding.
pub(crate) fn record_monitor(obj: *mut c_void, block: &[u8; REF_THING_BYTES], pid: Eterm) -> bool {
    if obj.is_null() {
        return false;
    }
    with_registry(|r| {
        if !r.objects.contains(&(obj as usize)) {
            return false;
        }
        // SAFETY: a registered payload pointer belongs to a block this crate `malloc`ed and has not
        // freed (the registration is removed before the free, under this very lock), so its header is
        // live and its `ty` is the registry's own never-freed `Box<ResourceType>`.
        let ty = unsafe { (*header_ptr(obj)).ty };
        if ty.is_null() {
            return false;
        }
        // SAFETY: `ty` is a registry-owned `Box<ResourceType>`, never freed.
        if unsafe { (*ty).down }.is_none() {
            return false;
        }
        r.monitors.insert(
            MonitorId(*block),
            MonitorBinding {
                obj,
                ty,
                pid: pid.raw(),
            },
        );
        true
    })
}

/// Remove the monitor `block` identifies, reporting whether it was there — the C's
/// `erts_monitor_tree_lookup` + `erts_monitor_tree_delete` (`:4046-4048`) for `enif_demonitor_process`.
pub(crate) fn take_monitor(block: &[u8; REF_THING_BYTES]) -> bool {
    with_registry(|r| r.monitors.remove(&MonitorId(*block)).is_some())
}

/// The retention rule [`Registry::forget_object`] applies when an object is destroyed: **every**
/// binding of that object goes, with no `down` — the C's `run_resource_dtor` → `dtor_demonitor`
/// (`erl_nif.c:2974-3030`, `:2940-2948`), which cancels a dying resource's monitors through
/// `erts_proc_sig_send_demonitor` and never fires the callback.
///
/// It is a `&mut self` method rather than a `with_registry(…)` free function because its only caller
/// already holds the registry lock (`destroy` → `forget_object`); taking it again would deadlock on
/// the non-reentrant mutex.
impl Registry {
    pub(super) fn forget_monitors_for_object(&mut self, obj: *mut c_void) {
        self.monitors.retain(|_, binding| binding.obj != obj);
    }
}