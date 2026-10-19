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

//! `enif_*` — poll-select registration: `enif_select` and `enif_select_x`.
//!
//! Both slots are populated. The registration itself is the runtime's ([`NifHost::select`], backed by
//! the poller in `beam_io`); this file reproduces the C's *argument handling and return codes* and
//! hands the request over.
//!
//! # Ground truth (`erts/emulator/sys/common/erl_check_io.c`)
//!
//! ## The mode is the complete documented flag set, forwarded verbatim
//!
//! [`ErlNifSelectFlags`] (crate root, `src/common.rs`) models **every** documented member of the C's
//! `enum ErlNifSelectFlags`, byte for byte against `erts/emulator/beam/erl_drv_nif.h:56-61`:
//! `ERL_NIF_SELECT_READ` `1<<0`, `WRITE` `1<<1`, `STOP` `1<<2`, `CANCEL` `1<<3`, `CUSTOM_MSG` `1<<4`,
//! `ERROR` `1<<5`. The mask reaches [`NifHost::select`] **verbatim** (the same `c_int`, neither
//! normalised nor reordered), so a host can decode it with
//! [`ErlNifSelectFlags::intersects`](crate::common::ErlNifSelectFlags::intersects) — the C's
//! `mode & FLAG` — and map it onto its poller's interests:
//!
//! | flag | what the C does with it |
//! |---|---|
//! | `READ` / `WRITE` | arms the `ready_input` / `ready_output` notification (`:1447-1456`) |
//! | `ERROR` | arms `ready_error`, or refuses with `INT_MIN \| ERL_NIF_SELECT_NOTSUP` when the platform's poller cannot report errors (`:1300-1307`, `:1460-1463`) |
//! | `STOP` | replaces the mode with `ERL_DRV_READ \| ERL_DRV_WRITE \| ERL_DRV_USE` and deletes the fd from the pollset (`:1272-1284`) — i.e. "tell me when it is safe to stop" |
//! | `CANCEL` | clears the interest instead of arming it (`on = !(mode & ERL_NIF_SELECT_CANCEL)`, `:1292`) |
//! | `CUSTOM_MSG` | makes the notification the caller's own message instead of `{select, Resource, Ref, EventAtom}` (`:716-731`) |
//!
//! `ERL_DRV_READ` / `ERL_DRV_WRITE` / `ERL_DRV_USE` are `#define`s of `READ` / `WRITE` / `STOP`
//! (`erts/emulator/beam/erl_driver.h:61-63`), so the one flag set serves both spellings.
//!
//! ## Return codes
//!
//! | answer | C | meaning |
//! |---|---|---|
//! | `0` | `:1464`, `:1467` | accepted: a plain registration (`on`, `:1464`) or the "nothing to register" arm (`:1467`) |
//! | `0` | `:1470` | a `CANCEL` that cancelled nothing — the `ERL_NIF_SELECT_*_CANCELLED` bits below are OR-ed in only for an interest that really was armed (`:1470-1482`) |
//! | `INT_MIN \| ERL_NIF_SELECT_INVALID_EVENT` | `:1266` | the event handle is invalid for this platform (`grow_drv_ev_state` refused it), decided **before** any fd state is touched |
//! | `INT_MIN \| ERL_NIF_SELECT_NOTSUP` | `:1305` | the interest is not supported — the C's own example is the `ERROR` bit on a platform whose poller cannot report errors |
//! | `INT_MIN \| ERL_NIF_SELECT_FAILED` | `:1433` | the poller refused the control operation (a closed / `NVAL` fd) |
//! | `ERL_NIF_SELECT_STOP_CALLED` | `:1283` (and `:1502` in the deferred path) | a `STOP` request whose stop callback ran (the C's fast track on an unknown fd) |
//! | `ERL_NIF_SELECT_STOP_SCHEDULED` | `:1335` (and `:1512`) | a `STOP` request whose stop callback was scheduled |
//! | `ERL_NIF_SELECT_READ_CANCELLED` / `_WRITE_CANCELLED` / `_ERROR_CANCELLED` (positive bits, OR-able) | `:1472`, `:1477`, `:1482` | what a `CANCEL` request actually cancelled |
//!
//! **Every negative answer is `INT_MIN | bit`**, so the documented `retval < 0` failure idiom holds
//! for all of them. *Which* negative code a request gets is the **host's** choice — it owns the
//! poller and the platform's capabilities — and the host's answer is passed through unchanged. This
//! crate's own answer for a request no host can service is `-1` (see below), which is negative and
//! therefore lands in the same failure branch.
//!
//! ## Argument handling
//!
//! | case | C behaviour | reproduced here |
//! |---|---|---|
//! | `pid == NULL` | the notification goes to the **calling process** (`:1256`, `recipient = env->proc->common.id`) | the hook is told `None` ("the calling process") |
//! | `pid != NULL` | `recipient = pid->pid` | the pid term from the `ErlNifPid` is handed to the hook |
//! | `enif_select_x` with `msg_env` | the message env's heap fragment is handed to the notification (`:716-731`); with `msg_env == NULL` the message is copied instead | accepted for signature fidelity and unused, exactly as `enif_send` treats it: the term movement is the hook's job (no copy engine lives in this crate) |
//!
//! # Refusals decided here, before the host is consulted
//!
//! * **An invalid event handle** answers `INT_MIN | ERL_NIF_SELECT_INVALID_EVENT` — the C rejects it
//!   before it looks at the mode or the fd state (`:1266`).
//! * **A mask with no documented bit set** answers `INT_MIN | ERL_NIF_SELECT_INVALID_EVENT` too, and
//!   the host is not consulted. The C's `ASSERT(mode)` (`:1293`) makes a zero mode a programming
//!   error: a debug build aborts, a production build degrades it to a silent no-op success. Refusing
//!   it keeps the documented `retval < 0` idiom and never reports a *fabricated* success for a
//!   request that carries no request. A mask carrying only `ERL_NIF_SELECT_CUSTOM_MSG` is
//!   **documented** (the custom-message delivery, `:716-731`) and is forwarded, not refused.
//! * **A `NULL` `obj`** is refused: the C turns it into a resource pointer (`DATA_TO_RESOURCE(obj)`,
//!   `:1245`) and dereferences it. A *forged* non-null `obj` is passed through unchanged — this crate
//!   cannot tell it from a resource handle, exactly as it cannot for a forged term.
//!
//! # Documented substitutions — never a wrong code, never a crash
//!
//! * **A request no host can service answers `-1`.** That is the answer when the env is `NULL` (the
//!   C reads `env->proc`, `:1256`, and faults) or has no host behind it, and when the host's `select`
//!   hook is unsupported (the default). `-1` is this crate's "not serviceable" answer — it is
//!   **negative**, so the documented `retval < 0` idiom sees exactly the C's failure branch, and the
//!   hook returns the C's bit-exact codes whenever it can tell why. (Pinned by `tests/nif_env.rs`.)
//! * **`ERL_NIF_SELECT_ERROR` is passed to the hook** rather than answered `INT_MIN | NOTSUP`: the C
//!   only refuses the error bit when the platform has no error-capable poller (`:1300-1307`), which
//!   is a property of the runtime's poller, not of this layer.
//! * **`ERL_NIF_SELECT_STOP` does not call the resource's `stop` callback here.** The C's fast track
//!   for an unknown fd calls `stop` directly and answers `ERL_NIF_SELECT_STOP_CALLED`
//!   (`:1283`); the resource registry and its callbacks belong to the `resources` family and the
//!   runtime, so the request — including the case where the hook itself calls `stop` — is the hook's
//!   to service.

use libc::{c_int, c_void};

use crate::common::{
    ErlNifSelectFlags, ERL_NIF_SELECT_CANCEL, ERL_NIF_SELECT_CUSTOM_MSG, ERL_NIF_SELECT_ERROR,
    ERL_NIF_SELECT_INVALID_EVENT, ERL_NIF_SELECT_READ, ERL_NIF_SELECT_STOP, ERL_NIF_SELECT_WRITE,
};
use crate::nif::types::{ErlNifEnv, ErlNifEvent, ErlNifPid};

use super::*;

/// The answer for a request this crate cannot service (no env, no host, or no hook support). Negative,
/// so the C's `retval < 0` failure idiom holds.
const SELECT_UNSUPPORTED: c_int = -1;

/// Every documented `ErlNifSelectFlags` member (`erts/emulator/beam/erl_drv_nif.h:56-61`) OR-ed
/// together — the bits a request must carry at least one of. The C asserts its mode is non-zero
/// (`erl_check_io.c:1293`); anything outside this set is undocumented and is refused with it.
const DOCUMENTED_FLAGS: c_int = ERL_NIF_SELECT_READ
    | ERL_NIF_SELECT_WRITE
    | ERL_NIF_SELECT_STOP
    | ERL_NIF_SELECT_CANCEL
    | ERL_NIF_SELECT_CUSTOM_MSG
    | ERL_NIF_SELECT_ERROR;

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_select = Some(enif_select);
    api.enif_select_x = Some(enif_select_x);
    api
}

/// `int enif_select(ErlNifEnv* env, ErlNifEvent e, enum ErlNifSelectFlags flags, void* obj, const ErlNifPid* pid, ERL_NIF_TERM ref);`
///
/// # Safety
///
/// `env` must be null or a live env; `obj` must be null or a live resource handle; `pid` must be null
/// or point at a readable `ErlNifPid`.
#[no_mangle]
pub unsafe extern "C" fn enif_select(
    env: *mut ErlNifEnv,
    e: ErlNifEvent,
    flags: ErlNifSelectFlags,
    obj: *mut c_void,
    pid: *const ErlNifPid,
    reference: ERL_NIF_TERM,
) -> c_int {
    // SAFETY: delegated to `select`, whose contract is the one above.
    unsafe { select(env, e, flags, obj, pid, reference) }
}

/// `int enif_select_x(ErlNifEnv* env, ErlNifEvent e, enum ErlNifSelectFlags flags, void* obj, const ErlNifPid* pid, ERL_NIF_TERM msg, ErlNifEnv* msg_env);`
///
/// # Safety
///
/// As [`enif_select`]; `msg_env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_select_x(
    env: *mut ErlNifEnv,
    e: ErlNifEvent,
    flags: ErlNifSelectFlags,
    obj: *mut c_void,
    pid: *const ErlNifPid,
    msg: ERL_NIF_TERM,
    _msg_env: *mut ErlNifEnv,
) -> c_int {
    // SAFETY: as `enif_select` (the same env/obj/pid contract).
    unsafe { select(env, e, flags, obj, pid, msg) }
}

/// The shared body of [`enif_select`] and [`enif_select_x`]: the C's argument handling, then the host.
///
/// # Safety
///
/// As [`enif_select`].
unsafe fn select(
    env: *mut ErlNifEnv,
    e: ErlNifEvent,
    flags: ErlNifSelectFlags,
    obj: *mut c_void,
    pid: *const ErlNifPid,
    term: ERL_NIF_TERM,
) -> c_int {
    // 1. The invalid-event path, decided exactly where the C decides it: before any fd state is
    //    touched (`grow_drv_ev_state` refuses a negative fd, `erl_check_io.c:1263-1268`).
    if e < 0 {
        return c_int::MIN | ERL_NIF_SELECT_INVALID_EVENT;
    }
    // 2. A mask carrying no documented bit asks for nothing. The C asserts its mode is non-zero
    //    (`ASSERT(mode)`, `:1293`) — a debug build aborts, a release build turns it into a silent
    //    no-op "success". Refusing it keeps the C's `retval < 0` idiom instead of reporting a
    //    fabricated success, and the host is not consulted. `CUSTOM_MSG` alone IS a documented
    //    request (`:716-731`), so it passes.
    if flags.bits() & DOCUMENTED_FLAGS == 0 {
        return c_int::MIN | ERL_NIF_SELECT_INVALID_EVENT;
    }
    // 3. `obj` is dereferenced by the C (`DATA_TO_RESOURCE(obj)`, `:1245`) and must name a resource
    //    the notification is delivered to; a null one has nothing to register.
    if obj.is_null() {
        return SELECT_UNSUPPORTED;
    }
    // 4. The runtime service behind the request: `None` (no host, or no support) is "not serviceable".
    let Some(nif_env) = (unsafe { env_mut(env) }) else {
        return SELECT_UNSUPPORTED;
    };
    // A null `pid` is the calling process (`erl_check_io.c:1256`).
    let recipient = if pid.is_null() {
        None
    } else {
        // SAFETY: `pid` is non-null and readable (caller contract).
        Some(t(unsafe { (*pid).pid }))
    };
    match nif_env.host().select(e, flags, obj, recipient, t(term)) {
        Some(code) => code,
        None => SELECT_UNSUPPORTED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::common::ERL_NIF_SELECT_STOP_CALLED;
    use crate::nif::host::{NifEnv, NifHost};
    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::{make_atom, make_small, Eterm};
    /// Every request the host is given: `(event, flags, recipient)`.
    type Requests = Rc<RefCell<Vec<(ErlNifEvent, c_int, Option<Eterm>)>>>;

    /// A host that records the select requests it is given, and answers `code` (or nothing).
    ///
    /// The log is an `Rc<RefCell<..>>` shared with the test so it can be read back **after** the
    /// `NifEnv` — which holds the only `&mut SelectHost` — has been dropped (no raw-pointer downcast).
    struct SelectHost {
        heap: Heap,
        atoms: AtomTable,
        requests: Requests,
        /// The code the hook answers (`None` = unsupported).
        code: Option<c_int>,
    }

    impl SelectHost {
        fn new(code: Option<c_int>) -> (SelectHost, Requests) {
            let requests: Requests = Rc::new(RefCell::new(Vec::new()));
            let host = SelectHost {
                heap: Heap::new(256).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
                requests: Rc::clone(&requests),
                code,
            };
            (host, requests)
        }
    }

    impl NifHost for SelectHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }

        fn atom_name(&mut self, atom: Eterm) -> Option<String> {
            if !beam_term::prelude::is_atom(atom) {
                return None;
            }
            self.atoms
                .name(beam_term::prelude::atom_val(atom))
                .map(str::to_string)
        }

        fn select(
            &mut self,
            event: ErlNifEvent,
            mode: ErlNifSelectFlags,
            _obj: *mut c_void,
            pid: Option<Eterm>,
            _reference: Eterm,
        ) -> Option<c_int> {
            self.requests.borrow_mut().push((event, mode.bits(), pid));
            self.code
        }
    }

    /// A non-null stand-in for the resource handle (never dereferenced by this layer).
    fn obj() -> *mut c_void {
        0x1000 as *mut c_void
    }

    /// A host that overrides **nothing** beyond the required three: its `select` hook is the trait
    /// default (`None`, "unsupported"), i.e. a runtime with no poller behind the NIF layer.
    struct NoPollerHost {
        heap: Heap,
        atoms: AtomTable,
    }

    impl NifHost for NoPollerHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }

        fn atom_name(&mut self, atom: Eterm) -> Option<String> {
            if !beam_term::prelude::is_atom(atom) {
                return None;
            }
            self.atoms
                .name(beam_term::prelude::atom_val(atom))
                .map(str::to_string)
        }
    }
    #[test]
    fn a_negative_event_is_the_cs_invalid_event_code() {
        let (mut host, requests) = SelectHost::new(Some(0));
        let mut env = NifEnv::new(&mut host);
        let flags = ErlNifSelectFlags::READ;
        // SAFETY: `env` is live, `obj` is non-null, `pid` is null (the calling process).
        let code = unsafe { enif_select(env.as_ptr(), -1, flags, obj(), core::ptr::null(), n(NIL)) };
        assert_eq!(code, c_int::MIN | ERL_NIF_SELECT_INVALID_EVENT);
        assert!(code < 0, "the C's failure idiom is `retval < 0`");
        assert!(
            requests.borrow().is_empty(),
            "the invalid-event path is decided before the host is consulted"
        );
    }

    #[test]
    fn a_serviced_request_passes_the_hook_code_through() {
        let (mut host, _requests) = SelectHost::new(Some(0));
        let mut env = NifEnv::new(&mut host);
        // SAFETY: as above, with a valid (non-negative) event.
        let code =
            unsafe { enif_select(env.as_ptr(), 7, ErlNifSelectFlags::READ, obj(), core::ptr::null(), n(NIL)) };
        assert_eq!(code, 0);
        assert!(env.pending_exception().is_none());
    }

    #[test]
    fn an_unsupported_hook_is_minus_one() {
        let (mut host, _requests) = SelectHost::new(None);
        let mut env = NifEnv::new(&mut host);
        // SAFETY: as above.
        let code =
            unsafe { enif_select(env.as_ptr(), 7, ErlNifSelectFlags::READ, obj(), core::ptr::null(), n(NIL)) };
        assert_eq!(code, -1);
        assert!(code < 0);
    }

    #[test]
    fn a_null_obj_is_refused_before_the_host_is_consulted() {
        let (mut host, requests) = SelectHost::new(Some(0));
        let mut env = NifEnv::new(&mut host);
        // SAFETY: a null `obj` is explicitly refused by the contract; `env` is live.
        let code = unsafe {
            enif_select(
                env.as_ptr(),
                7,
                ErlNifSelectFlags::READ,
                core::ptr::null_mut(),
                core::ptr::null(),
                n(NIL),
            )
        };
        assert_eq!(code, -1);
        assert!(
            requests.borrow().is_empty(),
            "a null obj never reaches the host"
        );
    }

    #[test]
    fn a_null_env_is_minus_one() {
        // SAFETY: a null env is explicitly allowed by the contract.
        let code = unsafe {
            enif_select(
                core::ptr::null_mut(),
                7,
                ErlNifSelectFlags::READ,
                obj(),
                core::ptr::null(),
                n(NIL),
            )
        };
        assert_eq!(code, -1);
    }

    #[test]
    fn a_pid_is_forwarded_and_a_null_pid_means_the_calling_process() {
        let (mut host, requests) = SelectHost::new(Some(0));
        let pid = make_small(4711);
        {
            let mut env = NifEnv::new(&mut host);
            let erl_pid = ErlNifPid { pid: n(pid) };
            // SAFETY: `env` is live and `erl_pid` is a readable `ErlNifPid`.
            let code = unsafe {
                enif_select(
                    env.as_ptr(),
                    7,
                    ErlNifSelectFlags::READ,
                    obj(),
                    &erl_pid,
                    n(NIL),
                )
            };
            assert_eq!(code, 0);
            // SAFETY: as above, with a null pid (the calling process).
            let code = unsafe {
                enif_select(env.as_ptr(), 8, ErlNifSelectFlags::WRITE, obj(), core::ptr::null(), n(NIL))
            };
            assert_eq!(code, 0);
        }
        assert_eq!(
            *requests.borrow(),
            vec![
                (7, ErlNifSelectFlags::READ.bits(), Some(pid)),
                (8, ErlNifSelectFlags::WRITE.bits(), None),
            ]
        );
    }

    #[test]
    fn select_x_forwards_the_message_and_ignores_the_message_env() {
        let (mut host, _requests) = SelectHost::new(Some(0));
        let mut env = NifEnv::new(&mut host);
        let msg = make_small(99);
        // SAFETY: `env` is live; the `msg_env` operand is accepted and unused.
        let code = unsafe {
            enif_select_x(
                env.as_ptr(),
                9,
                ErlNifSelectFlags::READ,
                obj(),
                core::ptr::null(),
                n(msg),
                env.as_ptr(),
            )
        };
        assert_eq!(code, 0);
    }

    /// Every documented flag, alone, reaches the host **bit-identically** — the flag↔mode mapping
    /// pin. `ERROR` is the one that matters most: it is what a host needs to arm an
    /// error-capable interest (`beam_io::Interest::ERROR`), and the C arms it exactly like `READ`
    /// and `WRITE` (`erl_check_io.c:1300-1307`, `:1460-1463`).
    #[test]
    fn every_documented_flag_is_forwarded_bit_identically() {
        let all = [
            ErlNifSelectFlags::READ,
            ErlNifSelectFlags::WRITE,
            ErlNifSelectFlags::ERROR,
            ErlNifSelectFlags::STOP,
            ErlNifSelectFlags::CANCEL,
            ErlNifSelectFlags::CUSTOM_MSG,
        ];
        let (mut host, requests) = SelectHost::new(Some(0));
        {
            let mut env = NifEnv::new(&mut host);
            for flags in all {
                // SAFETY: `env` is live, `obj` is non-null, `pid` is null (the calling process).
                let code =
                    unsafe { enif_select(env.as_ptr(), 7, flags, obj(), core::ptr::null(), n(NIL)) };
                assert_eq!(code, 0, "a documented mask is forwarded, not refused");
            }
        }
        let seen = requests.borrow();
        assert_eq!(seen.len(), all.len(), "one host consult per documented flag");
        for (recorded, flags) in seen.iter().zip(all) {
            assert_eq!(
                recorded.1,
                flags.bits(),
                "the mode reaches the host verbatim (flag {:#x})",
                flags.bits()
            );
        }
        assert_eq!(
            all.map(|f| f.bits()),
            [1, 2, 32, 4, 8, 16],
            "the documented bit assignments of erl_drv_nif.h:56-61"
        );
    }

    /// A combined mask arrives intact, and `from_bits`/`bits` round-trip.
    #[test]
    fn a_combined_mask_arrives_intact() {
        let combined = ErlNifSelectFlags::READ
            .bits()
            | ErlNifSelectFlags::WRITE
            .bits()
            | ErlNifSelectFlags::ERROR.bits();
        assert_eq!(combined, 0b100011);
        let flags = ErlNifSelectFlags::from_bits(combined);
        assert_eq!(flags.bits(), combined, "from_bits/bits round-trip");

        let (mut host, requests) = SelectHost::new(Some(0));
        {
            let mut env = NifEnv::new(&mut host);
            // SAFETY: `env` is live, `obj` is non-null, `pid` is null.
            let code = unsafe { enif_select(env.as_ptr(), 11, flags, obj(), core::ptr::null(), n(NIL)) };
            assert_eq!(code, 0);
        }
        assert_eq!(requests.borrow()[0], (11, combined, None));
    }

    /// `intersects` is the C's `mode & FLAG`: each flag intersects itself, a combined mask intersects
    /// exactly its members, and a zero mask intersects nothing (the C's zero mode is a programming
    /// error, and this crate refuses it before a host sees it).
    #[test]
    fn intersects_is_the_cs_mode_and_flag_test() {
        let all = [
            ErlNifSelectFlags::READ,
            ErlNifSelectFlags::WRITE,
            ErlNifSelectFlags::STOP,
            ErlNifSelectFlags::CANCEL,
            ErlNifSelectFlags::CUSTOM_MSG,
            ErlNifSelectFlags::ERROR,
        ];
        for flag in all {
            assert!(flag.intersects(flag), "a flag intersects itself");
        }
        let combined = ErlNifSelectFlags::from_bits(
            ErlNifSelectFlags::READ.bits() | ErlNifSelectFlags::ERROR.bits(),
        );
        for flag in all {
            let member = flag == ErlNifSelectFlags::READ || flag == ErlNifSelectFlags::ERROR;
            assert_eq!(
                combined.intersects(flag),
                member,
                "READ|ERROR intersects exactly its members"
            );
            assert_eq!(flag.intersects(combined), member, "and the test is symmetric");
        }
        let zero = ErlNifSelectFlags::from_bits(0);
        for flag in all {
            assert!(!zero.intersects(flag), "a zero mask intersects nothing");
            assert!(!flag.intersects(zero));
        }
    }

    /// A zero mask is the C's `ASSERT(mode)` case: refused with the invalid-event code, and the host
    /// is never consulted — never a fabricated success.
    #[test]
    fn a_zero_mask_is_refused_before_the_host_is_consulted() {
        let (mut host, requests) = SelectHost::new(Some(0));
        let mut env = NifEnv::new(&mut host);
        let zero = ErlNifSelectFlags::from_bits(0);
        // SAFETY: `env` is live, `obj` is non-null, `pid` is null.
        let code = unsafe { enif_select(env.as_ptr(), 7, zero, obj(), core::ptr::null(), n(NIL)) };
        assert_eq!(code, c_int::MIN | ERL_NIF_SELECT_INVALID_EVENT);
        assert!(code < 0, "the C's failure idiom is `retval < 0`");
        assert!(
            requests.borrow().is_empty(),
            "a mask with no documented bit never reaches the host"
        );
    }

    /// A mask carrying only *undocumented* bits asks for nothing this ABI defines, so it is refused
    /// exactly like the zero mask.
    #[test]
    fn an_undocumented_only_mask_is_refused() {
        let (mut host, requests) = SelectHost::new(Some(0));
        let mut env = NifEnv::new(&mut host);
        let stray = ErlNifSelectFlags::from_bits(1 << 10);
        // SAFETY: `env` is live, `obj` is non-null, `pid` is null.
        let code = unsafe { enif_select(env.as_ptr(), 7, stray, obj(), core::ptr::null(), n(NIL)) };
        assert_eq!(code, c_int::MIN | ERL_NIF_SELECT_INVALID_EVENT);
        assert!(
            requests.borrow().is_empty(),
            "an undocumented-only mask never reaches the host"
        );
    }

    /// The new refusal must not swallow the two flags the C dispatches into its own select machinery:
    /// a `STOP` request (`:1272-1284`) and a `CANCEL` request (`:1292`) both still reach the host.
    #[test]
    fn a_stop_and_a_cancel_request_still_reach_the_host() {
        let (mut host, requests) = SelectHost::new(Some(ERL_NIF_SELECT_STOP_CALLED));
        {
            let mut env = NifEnv::new(&mut host);
            // SAFETY: `env` is live, `obj` is non-null, `pid` is null.
            let code = unsafe {
                enif_select(
                    env.as_ptr(),
                    7,
                    ErlNifSelectFlags::STOP,
                    obj(),
                    core::ptr::null(),
                    n(NIL),
                )
            };
            assert_eq!(code, ERL_NIF_SELECT_STOP_CALLED, "the host's STOP code passes through");
            // SAFETY: as above.
            let code = unsafe {
                enif_select(
                    env.as_ptr(),
                    7,
                    ErlNifSelectFlags::CANCEL,
                    obj(),
                    core::ptr::null(),
                    n(NIL),
                )
            };
            assert_eq!(code, ERL_NIF_SELECT_STOP_CALLED);
        }
        assert_eq!(
            *requests.borrow(),
            vec![
                (7, ErlNifSelectFlags::STOP.bits(), None),
                (7, ErlNifSelectFlags::CANCEL.bits(), None),
            ]
        );
    }

    /// A host with no poller behind it answers negatively for every documented mask — the trait
    /// default is `-1`, which is the C's failure branch. No fabricated success anywhere.
    #[test]
    fn a_host_without_a_poller_answers_negatively_for_every_documented_flag() {
        let documented = [
            ErlNifSelectFlags::READ,
            ErlNifSelectFlags::WRITE,
            ErlNifSelectFlags::ERROR,
            ErlNifSelectFlags::STOP,
            ErlNifSelectFlags::CANCEL,
            ErlNifSelectFlags::CUSTOM_MSG,
        ];
        let mut host = NoPollerHost {
            heap: Heap::new(64).expect("a small test heap is allocatable"),
            atoms: AtomTable::new(),
        };
        {
            let mut env = NifEnv::new(&mut host);
            for flags in documented {
                // SAFETY: the env is live, `obj` is non-null, `pid` is null.
                let code =
                    unsafe { enif_select(env.as_ptr(), 7, flags, obj(), core::ptr::null(), n(NIL)) };
                assert_eq!(
                    code, -1,
                    "an unserviceable request stays negative (flag {:#x})",
                    flags.bits()
                );
            }
        }
    }
}
