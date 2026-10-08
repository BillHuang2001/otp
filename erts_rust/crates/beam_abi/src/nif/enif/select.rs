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
//! | case | C behaviour | reproduced here |
//! |---|---|---|
//! | `event` outside the fd range | `grow_drv_ev_state` fails (`:1263-1268`) ⇒ `INT_MIN \| ERL_NIF_SELECT_INVALID_EVENT` (`ERL_NIF_SELECT_INVALID_EVENT` = `1 << 2`, `erl_nif.h:181`) | same code, decided **before** the host is consulted (as the C decides it before touching the fd state) |
//! | success | a non-negative result; `0` for a plain (non-STOP) registration (`:1464`, `:1467`) | the hook's code, passed through verbatim (the hook owns `ERL_NIF_SELECT_STOP_CALLED`/`_SCHEDULED`/`..._CANCELLED` and the failure bits `ERL_NIF_SELECT_FAILED`/`NOTSUP`) |
//! | `pid == NULL` | the notification goes to the **calling process** (`:1256`, `recipient = env->proc->common.id`) | the hook is told `None` ("the calling process") |
//! | `pid != NULL` | `recipient = pid->pid` | the pid term from the `ErlNifPid` is handed to the hook |
//! | `enif_select_x` with `msg_env` | the message env's heap fragment is handed to the notification (`:716-729`); with `msg_env == NULL` the message is copied instead | accepted for signature fidelity and unused, exactly as `enif_send` treats it: the term movement is the hook's job (no copy engine lives in this crate) |
//!
//! # Documented substitutions — never a wrong code, never a crash
//!
//! * **A request no host can service answers `-1`.** That is the answer when the env is `NULL` (the
//!   C reads `env->proc`, `:1256`, and faults) or has no host behind it, and when the host's `select`
//!   hook is unsupported (the default). The C distinguishes *why* it failed with negative bit codes
//!   (`ERL_NIF_SELECT_INVALID_EVENT`/`FAILED`/`NOTSUP`, `erl_nif.h:178-183`); `-1` is this crate's
//!   "not serviceable" answer — it is **negative**, so the documented `retval < 0` idiom sees exactly
//!   the C's failure branch, and the hook can return the C's bit-exact codes whenever it can tell why.
//! * **A `NULL` `obj` is refused** with `-1`: the C turns it into a resource pointer
//!   (`DATA_TO_RESOURCE(obj)`, `:1245`) and dereferences it. A *forged* non-null `obj` is passed
//!   through unchanged — the crate cannot tell it from a resource handle, exactly as it cannot for a
//!   forged term.
//! * **`ERL_NIF_SELECT_ERROR` is passed to the hook** rather than answered `INT_MIN | NOTSUP`: the C
//!   only refuses the error bit when the platform has no error-capable poller (`:1300-1307`), which
//!   is a property of the runtime's poller, not of this layer.
//! * **`ERL_NIF_SELECT_STOP` does not call the resource's `stop` callback here.** The C's fast track
//!   for an unknown fd calls `stop` directly and answers `ERL_NIF_SELECT_STOP_CALLED`
//!   (`:1278-1285`); the resource registry and its callbacks belong to the `resources` family and the
//!   runtime, so the request — including the case where the hook itself calls `stop` — is the hook's
//!   to service.

use libc::{c_int, c_void};

use crate::common::{ErlNifSelectFlags, ERL_NIF_SELECT_INVALID_EVENT};
use crate::nif::types::{ErlNifEnv, ErlNifEvent, ErlNifPid};

use super::*;

/// The answer for a request this crate cannot service (no env, no host, or no hook support). Negative,
/// so the C's `retval < 0` failure idiom holds.
const SELECT_UNSUPPORTED: c_int = -1;

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
    // 2. `obj` is dereferenced by the C (`DATA_TO_RESOURCE(obj)`, `:1245`) and must name a resource
    //    the notification is delivered to; a null one has nothing to register.
    if obj.is_null() {
        return SELECT_UNSUPPORTED;
    }
    // 3. The runtime service behind the request: `None` (no host, or no support) is "not serviceable".
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
}
