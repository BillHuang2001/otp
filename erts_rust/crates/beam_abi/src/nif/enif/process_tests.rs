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

use crate::nif::host::{NifHost, NifMonitorResult};
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
    /// The ids of the monitors currently held.
    monitors: Vec<u64>,
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

/// The internal reference the arming host hands over — built with the crate's canonical writer, so it
/// is byte-identical to what `beam_process::MonitorRef::to_eterm`, `beam_bifs`'s `make_ref_thing` and
/// `beam_rt` mint (the frozen 4-word M1 block `[header, node, serial, id]`).
fn ref_term(heap: &mut Heap, id: u64) -> Eterm {
    // SAFETY: `REF_THING_WORDS` fresh, exclusively owned words from `heap`.
    let p = unsafe { heap.alloc_words(REF_THING_WORDS) }.expect("the test heap has room");
    // SAFETY: `p` addresses those words; `write_internal_ref` writes exactly `INTERNAL_REF_WORDS`
    // of them before the root escapes.
    unsafe { prelude::write_internal_ref(p, id) }.expect("the id fits the M1 60-bit slot")
}

/// `ERTS_REF_THING_SIZE` (`erl_term.h:990`): the C `ErtsORefThing`'s whole word count — the header
/// plus its three packed `num[]` ids plus the marker, 24 bytes on 64-bit little-endian.
const C_LAYOUT_REF_THING_WORDS: usize = C_LAYOUT_REF_PAYLOAD_WORDS + 1;

/// The **other** internal-reference block shape: the C `ErtsORefThing`
/// `[make_header(2, REF_SUBTAG), num[0..3] | marker]` (`erl_term.h:796-805`), three words, whose
/// whole identity is the packed ids of its two payload words (`erl_term.h:828-869`).
///
/// Nothing in this tree mints it — `beam_term::boxed::write_internal_ref` writes the 4-word M1 block
/// — but a host built on the C layout does (the crate's `tests/nif_process.rs` host is exactly that),
/// so [`ref_block_of`] must accept it. The payload words are written as **raw** words, as the C
/// packs its ids there, not as terms.
fn c_layout_ref_term(heap: &mut Heap, id: u64) -> Eterm {
    // SAFETY: `C_LAYOUT_REF_THING_WORDS` fresh, exclusively owned words from `heap`.
    let p = unsafe { heap.alloc_words(C_LAYOUT_REF_THING_WORDS) }.expect("the test heap has room");
    // SAFETY: `p` addresses those words and every one of them is written before the root escapes: the
    // arity-`C_LAYOUT_REF_PAYLOAD_WORDS` header plus the two payload words that carry the id.
    unsafe {
        *p = prelude::make_header(C_LAYOUT_REF_PAYLOAD_WORDS, prelude::REF_SUBTAG);
        *p.add(1) = Eterm::from_raw(id as usize);
        *p.add(2) = Eterm::from_raw(0);
    }
    prelude::make_boxed(p)
}

/// The block `words` would occupy on the heap, little-endian, right-zero-padded to
/// [`REF_THING_BYTES`] — the exact bytes [`ref_block_of`] must produce for a block of that width.
fn block_of_words(words: &[Eterm]) -> [u8; REF_THING_BYTES] {
    let stride = core::mem::size_of::<Eterm>();
    let mut expected = [0u8; REF_THING_BYTES];
    for (i, word) in words.iter().enumerate() {
        expected[i * stride..(i + 1) * stride].copy_from_slice(&word.raw().to_le_bytes());
    }
    expected
}
/// The id of an M1 internal-reference term (`None` when `term` is not one), read through the
/// sanctioned accessor — which validates the header arity before it touches the id word, exactly as
/// [`ref_block_of`] now does.
fn ref_payload(term: Eterm) -> Option<u64> {
    if !is_term_word(term) {
        return None;
    }
    // SAFETY: a non-boxed term is refused by the accessor, and a boxed one has its header word
    // validated (subtag + arity) before any payload word is read.
    unsafe { prelude::internal_ref_id(term) }.ok()
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
        self.monitors.push(id);
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
    assert_eq!(same, 0, "the whole stored reference block is what the comparison sees");
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
        // The compared range is the WHOLE stored reference block (`REF_THING_BYTES`), so the last
        // byte of the holder is inside the comparison — there is no tail outside it any more.
        b.data[0] = 0;
        b.data[MONITOR_BYTES - 1] = 9;
        assert_eq!(
            enif_compare_monitors(&a, &b),
            -1,
            "`REF_THING_BYTES` == `MONITOR_BYTES`: no byte of the holder is outside the comparison"
        );
        b.data[MONITOR_BYTES - 1] = 0;
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

// ---------------------------------------------------------------------------------------------
// The three-way monitor seam — `NifHost::monitor_process_result`
// ---------------------------------------------------------------------------------------------
//
// `enif_monitor_process` has THREE C outcomes (`0` established, `1` dead/undefined target, `-1` the
// resource type has no `down` callback), and they are taken in that order's source order:
// `erl_nif.c:3968-3972` (the resource's monitor table) comes BEFORE `:3975` (the undefined pid), and
// `:4004` (a dead target) comes last. The tests below pin every one of them, plus the legacy
// single-outcome fallback that must keep behaving exactly as it did.

/// A monitor holder pre-filled with `byte`, so "the C left it untouched" and "the C wrote it" are
/// both observable (a zeroed holder would blur them for a payload that happens to start with 0).
fn filled_monitor(byte: u8) -> ErlNifMonitor {
    ErlNifMonitor {
        data: [byte; MONITOR_BYTES],
    }
}

/// The first [`REF_THING_BYTES`] bytes of a holder that was never written (an untouched
/// [`filled_monitor`] value).
fn untouched_block(byte: u8) -> [u8; REF_THING_BYTES] {
    [byte; REF_THING_BYTES]
}

/// **Both internal-reference block shapes are accepted, each at its own width.**
///
/// The frozen M1 block is four words and fills the whole 32-byte payload, its identity in word 3; the
/// C `ErtsORefThing` is three words (24 bytes) whose identity is its two payload words, so the block's
/// last 8 bytes stay zeroed. Neither is over-read: each is copied as far as its header's arity says.
#[test]
fn both_reference_block_shapes_are_accepted_at_their_own_width() {
    assert_eq!(
        REF_THING_BYTES, MONITOR_BYTES,
        "the stored block IS the whole `ErlNifMonitor` payload"
    );
    assert_eq!(REF_THING_BYTES, 32, "four machine words on LP64");
    let mut heap = Heap::new(1024).expect("a test heap is allocatable");

    // --- the C layout: three words, identity in the two payload words, zero tail ------------------
    let c_a = c_layout_ref_term(&mut heap, 0x0000_0000_1122_3344);
    let c_b = c_layout_ref_term(&mut heap, 0x0000_0000_5566_7788);
    let block_a = ref_block_of(c_a).expect("a C-layout internal reference is accepted");
    let block_b = ref_block_of(c_b).expect("a C-layout internal reference is accepted");
    assert_eq!(
        block_a,
        block_of_words(&[
            prelude::make_header(C_LAYOUT_REF_PAYLOAD_WORDS, prelude::REF_SUBTAG),
            Eterm::from_raw(0x1122_3344),
            Eterm::from_raw(0),
        ]),
        "exactly the C block's three words are copied and the rest stays zeroed"
    );
    assert_ne!(block_a, block_b, "the ids live in the payload words");
    let stride = core::mem::size_of::<Eterm>();
    assert_eq!(
        block_a[..stride],
        block_b[..stride],
        "the headers are identical: both are `make_header(2, REF_SUBTAG)`"
    );
    assert_ne!(
        block_a[stride..2 * stride],
        block_b[stride..2 * stride],
        "the ids live in the block's first payload word"
    );
    assert_eq!(
        block_a[2 * stride..],
        block_b[2 * stride..],
        "…and nowhere else: every other byte of the two blocks agrees"
    );
    assert_eq!(
        &block_a[REF_THING_BYTES - 8..],
        &[0u8; 8][..],
        "the C-layout block is one word shorter than `REF_THING_BYTES`: its tail is zeroed"
    );
    assert_eq!(
        &block_b[REF_THING_BYTES - 8..],
        &[0u8; 8][..],
        "the same zero tail for every C-layout reference"
    );

    // --- the M1 layout: four words, identity in word 3, no tail ----------------------------------
    let id = 0x0000_0000_0f0f_1234;
    let m1 = ref_term(&mut heap, id);
    let m1_block = ref_block_of(m1).expect("an M1 internal reference is accepted");
    assert_eq!(
        m1_block,
        block_of_words(&[
            prelude::make_header(prelude::INTERNAL_REF_PAYLOAD_WORDS, prelude::REF_SUBTAG),
            prelude::make_small(0), // node
            prelude::make_small(0), // serial
            prelude::make_small(id as i64),
        ]),
        "all four M1 words are copied — the id word included"
    );
    assert_ne!(
        &m1_block[REF_THING_BYTES - 8..],
        &[0u8; 8][..],
        "the M1 identity is the block's LAST word, so there is no zero tail here"
    );
    assert_eq!(
        &m1_block[REF_THING_BYTES - 8..],
        &prelude::make_small(id as i64).raw().to_le_bytes()[..],
        "…and it is exactly the id word `write_internal_ref` wrote"
    );
    assert_ne!(
        m1_block, block_a,
        "the two shapes never collide: their headers differ (`make_header(3, …)` vs `make_header(2, …)`)"
    );
}

/// **The never-over-read guarantee.** A word that is not a term, a boxed term of another kind, and an
/// internal-reference-**shaped** header of any arity other than the two block shapes are all `None` —
/// so the width-dispatched copy never reads a block whose size its header does not promise, and
/// `enif_monitor_process` answers the C's `1` for all of them.
#[test]
fn a_reference_shaped_word_of_another_arity_is_refused() {
    let mut heap = Heap::new(1024).expect("a test heap is allocatable");

    // Not a term: a header-tagged word (`TAG_PRIMARY_HEADER`), which no term operand may be.
    assert_eq!(
        ref_block_of(Eterm::from_raw(0)),
        None,
        "a non-term word is refused"
    );
    // Immediates of every flavour.
    assert_eq!(ref_block_of(prelude::make_small(7)), None, "a small int");
    let mut atoms = AtomTable::new();
    assert_eq!(
        ref_block_of(prelude::make_atom(atoms.intern("an_atom"))),
        None,
        "an atom"
    );
    assert_eq!(ref_block_of(NIL), None, "the empty list");

    // A tuple: a boxed term with payload words, but not an internal reference — refused on the
    // subtag, not on the arity.
    // SAFETY: three fresh, exclusively owned words.
    let p = unsafe { heap.alloc_words(3) }.expect("the test heap has room");
    // SAFETY: `p` addresses those words and all three are written before the root escapes.
    unsafe {
        *p = prelude::make_header(2, prelude::ARITYVAL_SUBTAG);
        *p.add(1) = prelude::make_small(1);
        *p.add(2) = prelude::make_small(2);
    }
    assert_eq!(
        ref_block_of(prelude::make_boxed(p)),
        None,
        "a tuple is not an internal reference"
    );

    // Reference-shaped headers of an arity that is neither block shape: copying `REF_THING_BYTES` out
    // of any of them would read past the block the header describes.
    for arity in [0usize, 1, 4, prelude::INTERNAL_REF_WORDS] {
        // SAFETY: `arity + 1` fresh, exclusively owned words.
        let p = unsafe { heap.alloc_words(arity + 1) }.expect("the test heap has room");
        // SAFETY: `p` addresses `arity + 1` words; the header plus every payload word is written
        // before the root escapes.
        unsafe {
            *p = prelude::make_header(arity, prelude::REF_SUBTAG);
            for i in 1..=arity {
                *p.add(i) = prelude::make_small(i as i64);
            }
        }
        assert_eq!(
            ref_block_of(prelude::make_boxed(p)),
            None,
            "an internal-reference-shaped block of arity {arity} is neither accepted shape"
        );
    }
}

/// **The end-to-end effect of accepting the C shape:** a host that arms with a C-layout reference gets
/// the C's `0`, and the holder receives that block's three words with a zeroed tail — the regression
/// this shape's acceptance fixes (`enif_monitor_process` used to answer `1`, "could not establish").
#[test]
fn a_c_layout_arm_is_established_and_stores_its_three_words() {
    let mut host = MonitorHost::new();
    host.c_layout = true;
    let armed = host.mint_ref();
    host.result = Some(NifMonitorResult::Armed(armed));
    let target = ErlNifPid {
        pid: n(pid(8)),
    };
    let mut holder = filled_monitor(0xCC);
    {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live and the locals are initialised and writable.
        let code = unsafe {
            enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &target, &mut holder)
        };
        assert_eq!(
            code, MONITOR_ESTABLISHED,
            "a C-layout reference is an arm: the C's 0, not the refusal 1"
        );
    }
    // SAFETY: `holder` is an initialised local.
    let stored = unsafe { monitor_block(&holder) }.expect("a non-null holder");
    let expected = ref_block_of(armed).expect("the C-layout arm's block");
    assert_eq!(stored, expected, "the holder is the arm's whole block");
    assert_eq!(
        &stored[REF_THING_BYTES - 8..],
        &[0u8; 8][..],
        "its zero tail is stored too, so two C-layout monitors stay distinct by their ids"
    );
    // The term route and the holder route name the same monitor — the invariant `beam_rt`'s
    // `deliver_resource_down_id` relies on — for the C-layout shape too.
    // SAFETY: `holder` is an initialised local.
    let from_holder =
        unsafe { crate::nif::enif::resources::MonitorId::from_holder(&holder) }.expect("non-null");
    assert_eq!(
        crate::nif::enif::resources::MonitorId::from_term(armed),
        Some(from_holder),
        "`MonitorId::from_term` and `MonitorId::from_holder` agree for a C-layout block"
    );
}/// A `down` callback for monitor-capable test resource types: this crate only records its *presence*
/// (`resource_type_has_down`), it never calls it, so the body is intentionally empty.
///
/// # Safety
///
/// The C `ErlNifResourceDown` ABI; the body touches no pointer.
unsafe extern "C" fn unused_down(
    _env: *mut crate::nif::types::ErlNifEnv,
    _obj: *mut c_void,
    _pid: *mut ErlNifPid,
    _monitor: *mut ErlNifMonitor,
) {
}

/// Open (or re-open) a test resource type: `with_down` opens the monitor-capable one through
/// `enif_init_resource_type` with `members = 3` (so `down` is read), the other through the plain
/// `enif_open_resource_type` (`members = 1`, no `down`) — exactly the two C shapes.
///
/// The type carries **no destructor**, so releasing one of its objects never builds an environment
/// (this crate runs the destructor synchronously; the test host is already borrowed by `env`).
fn open_test_type(
    env: &mut NifEnv,
    name: &str,
    with_down: bool,
) -> *mut crate::nif::types::ErlNifResourceType {
    use crate::nif::enif::resources::{enif_init_resource_type, enif_open_resource_type};
    use crate::nif::types::{ErlNifResourceFlags, ErlNifResourceTypeInit};

    let cname = std::ffi::CString::new(name).expect("the test name has no interior NUL");
    let mut tried = ErlNifResourceFlags::from_bits(0);
    if with_down {
        let init = ErlNifResourceTypeInit {
            dtor: None,
            stop: None,
            down: Some(unused_down),
            members: 3,
            dyncall: None,
        };
        // SAFETY: `env` is live, `init` is a live local and `tried` is writable.
        unsafe {
            enif_init_resource_type(
                env.as_ptr(),
                cname.as_ptr(),
                &init,
                ErlNifResourceFlags::CREATE,
                &mut tried,
            )
        }
    } else {
        // SAFETY: as above; the plain variant takes the destructor directly.
        unsafe {
            enif_open_resource_type(
                env.as_ptr(),
                core::ptr::null(),
                cname.as_ptr(),
                None,
                ErlNifResourceFlags::CREATE,
                &mut tried,
            )
        }
    }
}

/// A host that implements the NEW three-way hook, the legacy one, or both — and records which of
/// them `enif_monitor_process` actually consulted.
struct MonitorHost {
    heap: Heap,
    /// What [`NifHost::monitor_process_result`] answers; `None` = this host does not implement it.
    result: Option<NifMonitorResult>,
    /// Whether [`NifHost::monitor_process`] (the legacy hook) establishes a monitor.
    legacy: bool,
    /// Every pid the legacy hook was consulted for, in call order.
    legacy_asks: Vec<Eterm>,
    /// The last reference [`MonitorHost::mint_ref`] built (`None` until one is minted).
    last_minted: Option<Eterm>,
    /// The next reference id the hooks mint.
    next_ref: u64,
    /// Which block shape [`MonitorHost::mint_ref`] mints: `false` = this tree's 4-word M1 block,
    /// `true` = the C's 3-word `ErtsORefThing` (what `tests/nif_process.rs`'s host builds).
    c_layout: bool,
}

impl MonitorHost {
    fn new() -> MonitorHost {
        MonitorHost {
            heap: Heap::new(1024).expect("a test heap is allocatable"),
            result: None,
            legacy: false,
            legacy_asks: Vec::new(),
            last_minted: None,
            next_ref: 1,
            c_layout: false,
        }
    }

    /// Mint the internal reference `erts_make_ref_in_buffer` builds, with a fresh payload id — in
    /// whichever block shape this host was configured for.
    fn mint_ref(&mut self) -> Eterm {
        let id = self.next_ref;
        self.next_ref += 1;
        let term = if self.c_layout {
            c_layout_ref_term(&mut self.heap, id)
        } else {
            ref_term(&mut self.heap, id)
        };
        self.last_minted = Some(term);
        term
    }
}

impl NifHost for MonitorHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// Interning is genuinely unsupported here: the monitor family never needs an atom.
    fn intern_atom(&mut self, _name: &str) -> Option<Eterm> {
        None
    }

    /// No atom table either — the monitor family never resolves a name.
    fn atom_name(&mut self, _atom: Eterm) -> Option<String> {
        None
    }

    fn monitor_process_result(&mut self, _pid: Eterm) -> Option<NifMonitorResult> {
        self.result
    }

    fn monitor_process(&mut self, pid: Eterm) -> Option<Eterm> {
        self.legacy_asks.push(pid);
        if !self.legacy {
            return None;
        }
        Some(self.mint_ref())
    }
}

/// **The arm path of the three-way seam.** `Armed(ref)` ⇒ the C's `0`, `*monitor` receives that
/// reference's whole block (the frozen 4-word M1 block, `REF_THING_BYTES` = 32 bytes, all of the
/// `ErlNifMonitor` payload), and the legacy hook is never consulted.
///
/// Oracle measurement (OTP 29.1.1 / erts-17.1, a `cc`-built NIF arming on `enif_self` through a
/// resource type opened with a `down` callback from the `load` callback): code `0`, holder written.
#[test]
fn the_three_way_hook_arms_and_writes_the_reference_block() {
    let mut host = MonitorHost::new();
    let armed = host.mint_ref();
    host.result = Some(NifMonitorResult::Armed(armed));
    let target = ErlNifPid {
        pid: n(pid(1)),
    };
    let mut holder = filled_monitor(0xAA);
    let (code, null_holder_code);
    {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live and every local below is initialised and writable.
        code = unsafe {
            enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &target, &mut holder)
        };
        // A null holder is legal on the success path (`if (monitor) ...`, `erl_nif.c:4007`).
        // SAFETY: as above; the null holder is explicitly tolerated.
        null_holder_code = unsafe {
            enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &target, core::ptr::null_mut())
        };
    }
    assert_eq!(code, MONITOR_ESTABLISHED, "the C's 0 = established");
    assert_eq!(null_holder_code, MONITOR_ESTABLISHED, "a null holder is tolerated");
    // The stored payload IS the hook's reference block — the C's `erts_ref_to_driver_monitor`.
    // SAFETY: `holder` is an initialised local.
    let stored = unsafe { monitor_block(&holder) }.expect("a non-null holder");
    assert_eq!(
        Some(stored),
        ref_block_of(armed),
        "the whole payload is the hook's internal-reference block"
    );
    assert_eq!(
        &holder.data[REF_THING_BYTES..],
        &filled_monitor(0).data[REF_THING_BYTES..],
        "there is no tail: the block fills the holder exactly (`REF_THING_BYTES` == `MONITOR_BYTES`)"
    );
    assert!(
        host.legacy_asks.is_empty(),
        "the legacy hook is not consulted when the three-way hook answers"
    );
}

/// **The two failure outcomes of the three-way seam.** `NotAlive` ⇒ `1` and `NoDownCallback` ⇒ `-1`,
/// both leaving the caller's holder untouched (`erl_nif.c:4004`, `:3968-3972`).
///
/// Oracle measurements: an `enif_set_pid_undefined` target on a `down`-capable resource ⇒ `{1,1}`
/// (code, holder-untouched); a type opened without `down` ⇒ `-1`.
#[test]
fn the_three_way_hook_reports_not_alive_and_no_down_callback() {
    let mut host = MonitorHost::new();
    host.result = Some(NifMonitorResult::NotAlive);
    let target = ErlNifPid {
        pid: n(pid(2)),
    };
    let mut holder = filled_monitor(0x5A);
    {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live and the locals are initialised and writable.
        let code = unsafe {
            enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &target, &mut holder)
        };
        assert_eq!(code, MONITOR_TARGET_DEAD, "the C's 1 = the target is not alive");
    }
    // SAFETY: `holder` is an initialised local.
    assert_eq!(
        unsafe { monitor_block(&holder) },
        Some(untouched_block(0x5A)),
        "a failed establishment never touches the holder (`erl_nif.c:4007`)"
    );

    // The same host, now answering the "the type cannot hold monitors" outcome.
    let mut host = MonitorHost::new();
    host.result = Some(NifMonitorResult::NoDownCallback);
    let mut holder = filled_monitor(0x77);
    {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: as above.
        let code = unsafe {
            enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &target, &mut holder)
        };
        assert_eq!(code, MONITOR_NO_DOWN_CALLBACK, "the C's -1");
        assert_eq!(code, -1, "the literal the C returns");
    }
    // SAFETY: `holder` is an initialised local.
    assert_eq!(
        unsafe { monitor_block(&holder) },
        Some(untouched_block(0x77)),
        "the -1 path does not touch the holder either"
    );
}

/// An undefined or null target pid answers `1` **before** any hook is consulted, and leaves the
/// holder untouched (`erl_nif.c:3975-3976`).
#[test]
fn an_undefined_or_null_target_answers_one_and_never_touches_the_holder() {
    let mut host = MonitorHost::new();
    let armed = host.mint_ref();
    host.result = Some(NifMonitorResult::Armed(armed));
    host.legacy = true;
    let undefined = ErlNifPid { pid: UNDEFINED_PID };
    let mut holder = filled_monitor(0x11);
    {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live; the locals are initialised, and the null pid is explicitly handled.
        let (undef, null_pid) = unsafe {
            (
                enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &undefined, &mut holder),
                enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), core::ptr::null(), &mut holder),
            )
        };
        assert_eq!(undef, MONITOR_TARGET_DEAD, "an undefined pid is the C's 1");
        assert_eq!(null_pid, MONITOR_TARGET_DEAD, "a null target pid is the C's 1");
    }
    // SAFETY: `holder` is an initialised local.
    assert_eq!(
        unsafe { monitor_block(&holder) },
        Some(untouched_block(0x11)),
        "the early returns never write the holder"
    );
    assert!(
        host.legacy_asks.is_empty(),
        "neither hook is consulted for an undefined/null target"
    );
}

/// A hook that answers `Armed` with something that is not an internal reference is refused as `1`
/// rather than stored — an un-reconstructable payload is never fabricated.
#[test]
fn a_non_reference_arm_is_refused_rather_than_stored() {
    let mut host = MonitorHost::new();
    host.result = Some(NifMonitorResult::Armed(make_small(7)));
    let target = ErlNifPid {
        pid: n(pid(4)),
    };
    let mut holder = filled_monitor(0x22);
    {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live and the locals are initialised and writable.
        let code = unsafe {
            enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &target, &mut holder)
        };
        assert_eq!(code, MONITOR_TARGET_DEAD);
    }
    // SAFETY: `holder` is an initialised local.
    assert_eq!(
        unsafe { monitor_block(&holder) },
        Some(untouched_block(0x22)),
        "nothing was stored"
    );
}

/// The C's `-1` gate, answered **without any host**, from this crate's own resource registry: an
/// object of a type opened without a `down` callback answers `-1` — and it does so *before* the pid is
/// examined, so an undefined target on such a resource also answers `-1` (C ordering).
///
/// Measured on the oracle: without `down` ⇒ `{-1,1}` for both a live and an undefined target;
/// with `down` ⇒ `{0,0}` (live) / `{1,1}` (undefined). See also
/// [`resources::resource_type_has_down`]'s doc for the probe shape.
#[test]
fn a_resource_without_a_down_callback_answers_minus_one_before_looking_at_the_pid() {
    use crate::nif::enif::resources::{enif_alloc_resource, enif_release_resource};

    let mut host = MonitorHost::new();
    let live = pid(5);
    let target = ErlNifPid { pid: n(live) };
    let undefined = ErlNifPid { pid: UNDEFINED_PID };
    let mut holder = filled_monitor(0x33);
    let (no_down, with_down) = {
        let mut env = NifEnv::new(&mut host);
        // Distinct names per test: the resource registry is process-wide and keyed by name.
        let no_down = open_test_type(&mut env, "monitor_rt_no_down", false);
        let with_down = open_test_type(&mut env, "monitor_rt_with_down", true);
        assert!(!no_down.is_null() && !with_down.is_null(), "both types open");
        // SAFETY: both handles are live types from the open calls above.
        let obj_no_down = unsafe { enif_alloc_resource(no_down, 4) };
        // SAFETY: as above.
        let obj_with_down = unsafe { enif_alloc_resource(with_down, 4) };
        assert!(!obj_no_down.is_null() && !obj_with_down.is_null());
        // The registry knows both, and only the second is monitor-capable.
        assert_eq!(
            crate::nif::enif::resources::resource_type_has_down(obj_no_down),
            Some(false)
        );
        assert_eq!(
            crate::nif::enif::resources::resource_type_has_down(obj_with_down),
            Some(true)
        );

        // SAFETY: `env` is live, both objects are live payload pointers of `env`'s host and the
        // holders are initialised locals.
        let (live_no_down, undef_no_down, live_with_down) = unsafe {
            (
                enif_monitor_process(env.as_ptr(), obj_no_down, &target, &mut holder),
                enif_monitor_process(env.as_ptr(), obj_no_down, &undefined, &mut holder),
                enif_monitor_process(env.as_ptr(), obj_with_down, &target, &mut holder),
            )
        };
        assert_eq!(
            live_no_down, MONITOR_NO_DOWN_CALLBACK,
            "a resource type without a `down` callback cannot hold monitors (`erl_nif.c:3968-3972`)"
        );
        assert_eq!(
            undef_no_down, MONITOR_NO_DOWN_CALLBACK,
            "the resource is consulted BEFORE the pid, exactly as in the C"
        );
        assert_eq!(
            live_with_down, MONITOR_TARGET_DEAD,
            "a monitor-capable type passes the gate; with no host support the arm still answers 1"
        );

        // SAFETY: both objects have exactly the allocator's reference outstanding; each release frees
        // its block (the types carry no destructor, so no environment is built).
        unsafe {
            enif_release_resource(obj_no_down);
            enif_release_resource(obj_with_down);
        }
        // A released object is no longer recognised — the query never reads freed memory.
        assert_eq!(
            crate::nif::enif::resources::resource_type_has_down(obj_no_down),
            None
        );
        (no_down, with_down)
    };
    assert!(!no_down.is_null() && !with_down.is_null());
    // SAFETY: `holder` is an initialised local.
    assert_eq!(
        unsafe { monitor_block(&holder) },
        Some(untouched_block(0x33)),
        "none of the three calls wrote the holder"
    );
}

/// **The legacy fallback is unchanged.** A host that implements only `monitor_process` still gets
/// the pre-existing behaviour: a reference term ⇒ `0` + the payload; `None` ⇒ `1` and an untouched
/// holder.
#[test]
fn a_legacy_only_host_keeps_its_single_outcome_behaviour() {
    let mut host = MonitorHost::new();
    host.legacy = true;
    let target = ErlNifPid {
        pid: n(pid(6)),
    };
    let mut holder = filled_monitor(0x44);
    let stored;
    {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live and the locals are initialised and writable.
        let code = unsafe {
            enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &target, &mut holder)
        };
        assert_eq!(code, MONITOR_ESTABLISHED, "a reference term is still an arm");
        // SAFETY: `holder` is an initialised local.
        stored = unsafe { monitor_block(&holder) }.expect("a non-null holder");
    }
    let minted = host.last_minted.expect("the legacy hook minted a reference");
    assert_eq!(
        Some(stored),
        ref_block_of(minted),
        "the payload is the legacy hook's own reference block"
    );
    assert_eq!(host.legacy_asks, vec![t(target.pid)], "the legacy hook was asked once");

    // The legacy hook declining: `None` ⇒ the C's 1, holder untouched.
    host.legacy = false;
    let mut holder = filled_monitor(0x55);
    {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live and the locals are initialised and writable.
        let code = unsafe {
            enif_monitor_process(env.as_ptr(), core::ptr::null_mut(), &target, &mut holder)
        };
        assert_eq!(code, MONITOR_TARGET_DEAD);
    }
    // SAFETY: `holder` is an initialised local.
    assert_eq!(
        unsafe { monitor_block(&holder) },
        Some(untouched_block(0x55)),
        "a declining legacy hook writes nothing"
    );
    assert_eq!(host.legacy_asks.len(), 2, "both calls went through the legacy hook");
}
