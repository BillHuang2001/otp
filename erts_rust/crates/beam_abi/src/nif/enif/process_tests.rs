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

//! Path-included tests for [`super`] (`nif::enif::process`) — the process-family `enif_*` host
//! functions.
//!
//! Family-local tests: a [`NifHost`] answering every process-family hook from in-memory tables,
//! driven through the exported `enif_*` functions.
//!
//! Kept in a sibling file so `process.rs` stays under the workspace's ~1000-line convention;
//! included via `#[path]` as a **child** of `process`, so `use super::*;` still reaches the
//! module's items unchanged (the monitor-payload helpers, the shared `enif_*` helpers and the
//! twelve exported `enif_*` functions).

use super::*;

use crate::nif::host::NifHost;
use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::prelude::{
    atom_val, is_atom, make_atom, make_internal_pid, make_internal_port, make_small,
};

/// A host whose hooks answer from hand-written tables.
struct ProcHost {
    heap: Heap,
    atoms: AtomTable,
    self_pid: Option<Eterm>,
    /// Pids the host calls alive.
    live_pids: Vec<Eterm>,
    /// Ports the host calls alive.
    live_ports: Vec<Eterm>,
    /// Registered names → the pid/port term they are registered to.
    named: Vec<(String, Eterm)>,
    /// Every accepted `send`: `(receiver, message)`.
    sent: Vec<(Eterm, Eterm)>,
    /// Every accepted `port_command`: `(port, data)`.
    port_cmds: Vec<(Eterm, Eterm)>,
    /// The payloads of the monitors currently held.
    monitors: Vec<(u64, u64)>,
    /// The next reference id `monitor_process` hands out.
    next_ref: u64,
    /// Whether `schedule_nif` is supported at all.
    schedule_ok: bool,
    /// Every accepted `schedule_nif`: `(name, flags, argc)`.
    scheduled: Vec<(String, c_int, usize)>,
}

impl ProcHost {
    fn new(schedule_ok: bool) -> ProcHost {
        ProcHost {
            heap: Heap::new(1024).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            self_pid: None,
            live_pids: Vec::new(),
            live_ports: Vec::new(),
            named: Vec::new(),
            sent: Vec::new(),
            port_cmds: Vec::new(),
            monitors: Vec::new(),
            next_ref: 1,
            schedule_ok,
            scheduled: Vec::new(),
        }
    }
}

/// The internal reference `erts_make_ref_in_buffer` would build for `id`.
fn ref_term(heap: &mut Heap, id: u64) -> Eterm {
    // SAFETY: `REF_THING_WORDS` fresh, exclusively owned words from `heap`.
    let p = unsafe { heap.alloc_words(REF_THING_WORDS) }.expect("the test heap has room");
    // SAFETY: `p` addresses those words; the block is a valid ref thing.
    unsafe {
        *p = prelude::make_header(REF_THING_WORDS - 1, prelude::REF_SUBTAG);
        *p.add(1) = Eterm::from_raw(id as usize);
        *p.add(2) = Eterm::from_raw(0);
    }
    prelude::make_boxed(p)
}

/// The `(w1, w2)` payload of an internal reference term (`None` when `term` is not one).
fn ref_payload(term: Eterm) -> Option<(u64, u64)> {
    if !is_term_word(term) || !unsafe { prelude::is_internal_ref(term) } {
        return None;
    }
    let p = prelude::boxed_val(term);
    // SAFETY: `term` is a live internal reference, so its block's words 1 and 2 exist.
    unsafe { Some(((*(p.add(1))).raw() as u64, (*(p.add(2))).raw() as u64)) }
}

impl NifHost for ProcHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }
    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.atoms.name(atom_val(atom)).map(str::to_string)
    }
    fn self_pid(&mut self) -> Option<Eterm> {
        self.self_pid
    }
    fn is_current_process_alive(&mut self) -> Option<bool> {
        Some(self.self_pid.is_some())
    }
    fn is_process_alive(&mut self, pid: Eterm) -> Option<bool> {
        Some(self.live_pids.iter().any(|p| p.raw() == pid.raw()))
    }
    fn is_port_alive(&mut self, port: Eterm) -> Option<bool> {
        Some(self.live_ports.iter().any(|p| p.raw() == port.raw()))
    }
    fn send(&mut self, to: Eterm, _from: Option<Eterm>, msg: Eterm) -> Option<bool> {
        let alive = self.live_pids.iter().any(|p| p.raw() == to.raw());
        if alive {
            self.sent.push((to, msg));
        }
        Some(alive)
    }
    fn port_command(&mut self, port: Eterm, data: Eterm) -> Option<bool> {
        let alive = self.live_ports.iter().any(|p| p.raw() == port.raw());
        if alive {
            self.port_cmds.push((port, data));
        }
        Some(alive)
    }
    fn whereis_pid(&mut self, name: Eterm) -> Option<Eterm> {
        let name = self.atom_name(name)?;
        self.named
            .iter()
            .find(|(n, v)| *n == name && prelude::is_internal_pid(*v))
            .map(|(_, v)| *v)
    }
    fn whereis_port(&mut self, name: Eterm) -> Option<Eterm> {
        let name = self.atom_name(name)?;
        self.named
            .iter()
            .find(|(n, v)| *n == name && prelude::is_internal_port(*v))
            .map(|(_, v)| *v)
    }
    fn monitor_process(&mut self, pid: Eterm) -> Option<Eterm> {
        if !self.live_pids.iter().any(|p| p.raw() == pid.raw()) {
            return None;
        }
        let id = self.next_ref;
        self.next_ref += 1;
        self.monitors.push((id, 0));
        Some(ref_term(&mut self.heap, id))
    }
    fn demonitor_process(&mut self, monitor: Eterm) -> Option<bool> {
        let Some(payload) = ref_payload(monitor) else {
            return Some(false);
        };
        let before = self.monitors.len();
        self.monitors.retain(|m| *m != payload);
        Some(self.monitors.len() != before)
    }
    fn schedule_nif(
        &mut self,
        fun_name: &str,
        flags: c_int,
        _fptr: unsafe extern "C" fn(
            *mut crate::nif::types::ErlNifEnv,
            c_int,
            *const ERL_NIF_TERM,
        ) -> ERL_NIF_TERM,
        argc: usize,
    ) -> Option<Eterm> {
        if !self.schedule_ok {
            return None;
        }
        self.scheduled.push((fun_name.to_string(), flags, argc));
        Some(make_atom(self.atoms.intern("scheduled")))
    }
}

/// An internal pid term for process number `num`.
fn pid(num: u64) -> Eterm {
    make_internal_pid(prelude::make_pid_data(0, num))
}

/// An internal port term for port number `num`.
fn port_term(num: u64) -> Eterm {
    make_internal_port(prelude::make_port_data(num))
}

/// An all-zero monitor holder.
fn zero_monitor() -> ErlNifMonitor {
    ErlNifMonitor {
        data: [0u8; MONITOR_BYTES],
    }
}

/// The NIF entry point `enif_schedule_nif` is handed (never called here).
///
/// # Safety
///
/// Mirrors the C NIF entry-point ABI; the body only returns `NIL`.
unsafe extern "C" fn scheduled_nif(
    _env: *mut crate::nif::types::ErlNifEnv,
    _argc: c_int,
    _argv: *const ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    bare_badarg()
}

#[test]
fn send_reports_one_when_sent_and_zero_otherwise() {
    let mut host = ProcHost::new(false);
    let (live, dead) = (pid(1), pid(2));
    host.self_pid = Some(live);
    host.live_pids = vec![live];
    let mut env = NifEnv::new(&mut host);
    let (to_live, to_dead) = (ErlNifPid { pid: n(live) }, ErlNifPid { pid: n(dead) });
    let msg = n(make_small(7));
    // SAFETY: `env` is live and both ids are initialised locals.
    let sent = unsafe { enif_send(env.as_ptr(), &to_live, core::ptr::null_mut(), msg) };
    // SAFETY: as above.
    let unsent = unsafe { enif_send(env.as_ptr(), &to_dead, core::ptr::null_mut(), msg) };
    // SAFETY: a null `to_pid`/env is explicitly allowed by the contract.
    let null_pid =
        unsafe { enif_send(env.as_ptr(), core::ptr::null(), core::ptr::null_mut(), msg) };
    // SAFETY: as above.
    let no_env =
        unsafe { enif_send(core::ptr::null_mut(), &to_live, core::ptr::null_mut(), msg) };
    assert_eq!((sent, unsent, null_pid, no_env), (1, 0, 0, 0));
    assert_eq!(host.sent, vec![(live, t(msg))], "only the live send reached the host");
}

#[test]
fn port_command_reports_one_for_a_live_port_and_zero_otherwise() {
    let mut host = ProcHost::new(false);
    let (live, dead) = (port_term(1), port_term(2));
    host.live_ports = vec![live];
    let mut env = NifEnv::new(&mut host);
    let to_live = ErlNifPort { port_id: n(live) };
    let to_dead = ErlNifPort { port_id: n(dead) };
    let data = make_small(3);
    // SAFETY: `env` is live and both ids are initialised locals.
    let ok = unsafe { enif_port_command(env.as_ptr(), &to_live, core::ptr::null_mut(), n(data)) };
    // SAFETY: as above.
    let no = unsafe { enif_port_command(env.as_ptr(), &to_dead, core::ptr::null_mut(), n(data)) };
    // SAFETY: a null `to_port`/env is explicitly allowed by the contract.
    let null_port = unsafe {
        enif_port_command(env.as_ptr(), core::ptr::null(), core::ptr::null_mut(), n(data))
    };
    // SAFETY: as above.
    let no_env = unsafe {
        enif_port_command(core::ptr::null_mut(), &to_live, core::ptr::null_mut(), n(data))
    };
    assert_eq!((ok, no, null_port, no_env), (1, 0, 0, 0));
    assert_eq!(host.port_cmds, vec![(live, data)]);
}

#[test]
fn whereis_answers_the_hit_and_the_miss() {
    let mut host = ProcHost::new(false);
    let (live, a_port) = (pid(4), port_term(9));
    let reg = host.intern_atom("registered").expect("interns");
    let port_name = host.intern_atom("registered_port").expect("interns");
    let missing = host.intern_atom("nothing").expect("interns");
    host.named = vec![
        ("registered".to_string(), live),
        ("registered_port".to_string(), a_port),
    ];
    let mut env = NifEnv::new(&mut host);
    let mut out = ErlNifPid { pid: n(pid(0)) };
    // SAFETY: `env` is live and `out` is writable.
    let hit = unsafe { enif_whereis_pid(env.as_ptr(), n(reg), &mut out) };
    assert_eq!((hit, t(out.pid)), (1, live));
    // SAFETY: as above; the remaining calls only read the same locals.
    unsafe {
        assert_eq!(enif_whereis_pid(env.as_ptr(), n(missing), &mut out), 0, "a miss");
        assert_eq!(
            enif_whereis_pid(env.as_ptr(), n(make_small(1)), &mut out),
            0,
            "`is_not_atom(name)` returns 0 before any lookup"
        );
        assert_eq!(
            enif_whereis_pid(env.as_ptr(), n(port_name), &mut out),
            0,
            "a port name is not a pid (`enif_get_local_pid`'s type check)"
        );
        assert_eq!(
            enif_whereis_pid(env.as_ptr(), n(reg), core::ptr::null_mut()),
            0,
            "a null out-parameter is reported, not written through"
        );
    }
    let mut port_out = ErlNifPort {
        port_id: n(port_term(0)),
    };
    // SAFETY: `env` is live and `port_out` is writable.
    let port_hit = unsafe { enif_whereis_port(env.as_ptr(), n(port_name), &mut port_out) };
    assert_eq!((port_hit, t(port_out.port_id)), (1, a_port));
    // SAFETY: as above.
    let wrong_kind = unsafe { enif_whereis_port(env.as_ptr(), n(reg), &mut port_out) };
    assert_eq!(wrong_kind, 0, "a pid name is not a port");
}

#[test]
fn liveness_predicates_report_the_hosts_answer() {
    let mut host = ProcHost::new(false);
    let (live, dead) = (pid(1), pid(2));
    let (a_port, no_port) = (port_term(5), port_term(6));
    host.self_pid = Some(live);
    host.live_pids = vec![live];
    host.live_ports = vec![a_port];
    let mut env = NifEnv::new(&mut host);
    let mut live_pid = ErlNifPid { pid: n(live) };
    let mut dead_pid = ErlNifPid { pid: n(dead) };
    let mut live_port = ErlNifPort {
        port_id: n(a_port),
    };
    let mut dead_port = ErlNifPort {
        port_id: n(no_port),
    };
    // SAFETY: `env` is live and every out-pointer is an initialised local.
    unsafe {
        assert_eq!(enif_is_current_process_alive(env.as_ptr()), 1);
        assert_eq!(enif_is_process_alive(env.as_ptr(), &mut live_pid), 1);
        assert_eq!(enif_is_process_alive(env.as_ptr(), &mut dead_pid), 0);
        assert_eq!(enif_is_port_alive(env.as_ptr(), &mut live_port), 1);
        assert_eq!(enif_is_port_alive(env.as_ptr(), &mut dead_port), 0);
        // Nulls and a null env answer the C's "not alive" 0, never a crash.
        assert_eq!(enif_is_process_alive(env.as_ptr(), core::ptr::null_mut()), 0);
        assert_eq!(enif_is_port_alive(env.as_ptr(), core::ptr::null_mut()), 0);
        assert_eq!(enif_is_process_alive(core::ptr::null_mut(), &mut live_pid), 0);
        assert_eq!(enif_is_current_process_alive(core::ptr::null_mut()), 0);
    }
}

#[test]
fn monitors_round_trip_and_follow_the_c_codes() {
    let mut host = ProcHost::new(false);
    let (live, dead) = (pid(1), pid(2));
    host.self_pid = Some(live);
    host.live_pids = vec![live];
    let mut env = NifEnv::new(&mut host);
    let live_pid = ErlNifPid { pid: n(live) };
    let dead_pid = ErlNifPid { pid: n(dead) };
    let mut mon = zero_monitor();
    let mut undefined = ErlNifPid { pid: UNDEFINED_PID };
    // SAFETY: `env` is live; the locals are initialised and writable.
    let (established, dead_target, undef) = unsafe {
        let established =
            enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &live_pid, &mut mon);
        let dead_target =
            enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &dead_pid, &mut mon);
        let undef =
            enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &undefined, &mut mon);
        (established, dead_target, undef)
    };
    assert_eq!(established, MONITOR_ESTABLISHED);
    // The holder now carries the first monitor's payload; the C stores only on success
    // (`if (monitor) erts_ref_to_driver_monitor(...)`, `erl_nif.c:4007`) and only after the
    // target check (`:3975`), so the two failed attempts must have left it untouched.
    let first_block = unsafe { monitor_block(&mon) }.expect("a non-null monitor");
    assert_eq!(dead_target, MONITOR_TARGET_DEAD, "the C's `>0` code for a dead pid");
    assert_eq!(undef, MONITOR_TARGET_DEAD, "the C's early return for an undefined pid");
    assert_eq!(
        unsafe { monitor_block(&mon) }.expect("still non-null"),
        first_block,
        "a failed `enif_monitor_process` does not touch the caller's holder"
    );

    // Establish a second, independent monitor and check the payload round-trips both ways.
    undefined.pid = n(live);
    // SAFETY: as above.
    let code =
        unsafe { enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &undefined, &mut mon) };
    assert_eq!(code, MONITOR_ESTABLISHED);
    let expected = *host.monitors.last().expect("the monitor was recorded");
    let mut rebuilt = zero_monitor();
    // SAFETY: `mon` is an initialised local.
    let stored = unsafe { monitor_block(&mon) }.expect("a non-null monitor");
    assert_ne!(stored, first_block, "a second establishment writes a new payload");
    store_monitor(&mut rebuilt, &stored);
    // SAFETY: `env` is live and both holders are initialised locals.
    let (same, term) = unsafe {
        (
            enif_compare_monitors(&mon, &rebuilt),
            enif_make_monitor_term(env.as_ptr(), &mon),
        )
    };
    assert_eq!(same, 0, "the stored 24-byte block is what the comparison sees");
    let payload = ref_payload(t(term));
    assert!(
        payload.is_some(),
        "the C rebuilds an ordinary internal reference (`io.c:7002-7010`)"
    );
    assert_eq!(
        payload,
        Some(expected),
        "the rebuilt term carries the payload the host handed out"
    );
    let (removed, again, null_monitor, no_env) = {
        // SAFETY: `env` is live; the monitor is an initialised local.
        unsafe {
            (
                enif_demonitor_process(env.as_ptr(), core::ptr::null_mut(), &mon),
                enif_demonitor_process(env.as_ptr(), core::ptr::null_mut(), &mon),
                enif_demonitor_process(env.as_ptr(), core::ptr::null_mut(), core::ptr::null()),
                enif_demonitor_process(core::ptr::null_mut(), core::ptr::null_mut(), &mon),
            )
        }
    };
    assert_eq!(removed, DEMONITOR_REMOVED, "0 = identified and removed");
    assert_eq!(again, DEMONITOR_NOT_FOUND, "1 = could not be identified");
    assert_eq!(null_monitor, DEMONITOR_NOT_FOUND);
    assert_eq!(no_env, DEMONITOR_NOT_FOUND);
}

#[test]
fn an_unknown_monitor_is_reported_not_found() {
    let mut host = ProcHost::new(false);
    let mut env = NifEnv::new(&mut host);
    let mut bogus = zero_monitor();
    bogus.data[0] = 0x7f; // a payload no `monitor_process` call ever handed out
    // SAFETY: `env` is live and `bogus` is an initialised local.
    let code = unsafe { enif_demonitor_process(env.as_ptr(), core::ptr::null_mut(), &bogus) };
    assert_eq!(code, DEMONITOR_NOT_FOUND);
}

#[test]
fn compare_monitors_is_a_memcmp_style_ordering() {
    let a = zero_monitor();
    let mut b = zero_monitor();
    // SAFETY: both holders are initialised locals.
    unsafe {
        assert_eq!(enif_compare_monitors(&a, &b), 0);
        b.data[0] = 1;
        assert_eq!(enif_compare_monitors(&a, &b), -1);
        assert_eq!(enif_compare_monitors(&b, &a), 1);
        // Bytes past the 3-word block are outside the C's comparison (`erl_nif.c:4057`).
        b.data[0] = 0;
        b.data[MONITOR_BYTES - 1] = 9;
        assert_eq!(enif_compare_monitors(&a, &b), 0);
        // Nulls: the C faults; the documented substitute is a total order.
        assert_eq!(enif_compare_monitors(core::ptr::null(), core::ptr::null()), 0);
        assert_eq!(enif_compare_monitors(core::ptr::null(), &a), -1);
        assert_eq!(enif_compare_monitors(&a, core::ptr::null()), 1);
    }
}

#[test]
fn schedule_raises_badarg_without_host_support() {
    let mut host = ProcHost::new(false);
    let mut env = NifEnv::new(&mut host);
    let name = c"reschedule";
    // SAFETY: `env` is live, `name` is NUL-terminated and `scheduled_nif` is a valid entry
    // point with no arguments.
    let term = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            name.as_ptr(),
            0,
            Some(scheduled_nif),
            0,
            core::ptr::null(),
        )
    };
    let badarg = env.host().intern_atom("badarg").expect("interns");
    assert_eq!(t(term), badarg, "no host support ⇒ badarg, never a fabricated success");
    assert_eq!(env.pending_exception(), Some(badarg), "the refusal is raised");
    // SAFETY: `env` is live.
    unsafe { enif_clear_env(env.as_ptr()) };
    // The C `enif_clear_env` frees temporary objects but never clears `exception_thrown`
    // (`erl_nif.c:700-723`), so the refusal is still pending afterwards.
    assert_eq!(
        env.pending_exception(),
        Some(badarg),
        "enif_clear_env must not clear the exception"
    );
    assert!(host.scheduled.is_empty(), "nothing was scheduled");
}

#[test]
fn schedule_returns_the_hosts_term_and_enforces_the_c_rules() {
    let mut host = ProcHost::new(true);
    let mut env = NifEnv::new(&mut host);
    let name = c"reschedule";
    // SAFETY: `env` is live, `name` is NUL-terminated and `scheduled_nif` is a valid entry
    // point with two arguments.
    let term = unsafe {
        enif_schedule_nif(
            env.as_ptr(),
            name.as_ptr(),
            ERL_DIRTY_JOB_CPU_BOUND,
            Some(scheduled_nif),
            2,
            core::ptr::null(),
        )
    };
    let scheduled = env.host().intern_atom("scheduled").expect("interns");
    assert_eq!(t(term), scheduled);
    assert_eq!(
        host.scheduled,
        vec![("reschedule".to_string(), ERL_DIRTY_JOB_CPU_BOUND, 2)]
    );

    // The rules the C enforces before any host is consulted (`erl_nif.c:3569,3597,3571`).
    let badarg = env.host().intern_atom("badarg").expect("interns");
    // SAFETY: `env` is live and `name`/`scheduled_nif` are valid for every call.
    unsafe {
        let probes = [
            (name.as_ptr(), 0, MAX_ARG + 1),
            (name.as_ptr(), 4, 0),
            (name.as_ptr(), 0, -1),
            (core::ptr::null(), 0, 0),
        ];
        for (fun_name, flags, argc) in probes {
            let term = enif_schedule_nif(
                env.as_ptr(),
                fun_name,
                flags,
                Some(scheduled_nif),
                argc,
                core::ptr::null(),
            );
            assert_eq!(t(term), badarg, "flags={flags} argc={argc} must be refused");
        }
        let null_fptr = enif_schedule_nif(
            env.as_ptr(),
            name.as_ptr(),
            0,
            None,
            0,
            core::ptr::null(),
        );
        assert_eq!(t(null_fptr), badarg);
    }
    assert_eq!(
        host.scheduled.len(),
        1,
        "only the one legal call reached the host"
    );
}
