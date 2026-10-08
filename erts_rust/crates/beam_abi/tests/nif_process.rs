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

//! End-to-end proof of the NIF **process family** (workspace contract **A.3**): a **real C NIF
//! shared object** — compiled with the system `cc` against the system `erl_nif.h` — is `dlopen`ed,
//! validated, and called through the Rust loader, with the twelve process-family `enif_*` imports
//! resolving out of the *test binary's* dynamic symbol table.
//!
//! `tests/fixtures/nif_process.c` exercises every slot `src/nif/enif/process.rs` populates:
//! `enif_send`, `enif_port_command`, `enif_whereis_pid`, `enif_whereis_port`,
//! `enif_is_current_process_alive`, `enif_is_process_alive`, `enif_is_port_alive`,
//! `enif_monitor_process`, `enif_demonitor_process`, `enif_compare_monitors`,
//! `enif_make_monitor_term` and `enif_schedule_nif` (through `reschedule/1` + a real `do_work/1`).
//! Because the fixture references those names as *undefined* dynamic symbols, a successful
//! `dlopen(..., RTLD_NOW)` is the empirical proof that this crate exports them (`build.rs` passes
//! `-Wl,--export-dynamic` for the test targets and `#[used] static ENIF_API` keeps the
//! `#[no_mangle]` symbols in the link).
//!
//! # The host
//!
//! The twelve functions are driven through [`NifHost`]. `common::TestHost` only implements
//! heap/atoms, so this file defines its own [`ProcHost`]: in-memory pid/port/registered-name tables,
//! a `sent`/`port_cmds` log, monitor bookkeeping that hands out real **internal references** (the
//! 3-word block the C stores in an `ErlNifMonitor`), and a `schedule_nif` that records the request.
//! It is the same shape as the unit tests' `ProcHost` in `src/nif/enif/process_tests.rs`, lifted to
//! the integration level so the C ABI — not the Rust API — is what gets exercised.
//!
//! # Oracle cross-check (system OTP 29.1.1 / erts-17.1)
//!
//! The fixture compiles **as-is** with `cc -shared -fPIC -I<erts include>` and loads into the real
//! runtime as module `nif_process`, so the codes below were observed from real ERTS (not inferred):
//!
//! ```text
//! send_to_self(5)             -> 1          ; self() received {5,<0.10.0>}
//! alive()                     -> {1,1}
//! is_alive(self())            -> 1
//! is_alive(dead pid)          -> 0          ; erlang:is_process_alive(dead) -> false
//! is_alive(42)                -> error:badarg
//! is_port_alive(live port)    -> 1          ; port_info(P,connected) -> {connected,Owner}
//! is_port_alive(closed port)  -> 0          ; port_info(P,connected) -> undefined
//! port_command_code(live)     -> 1          ; the 4-byte binary really reached the port ("ping")
//! port_command_code(closed)   -> 0          ; erlang:port_command(closed,_) -> error:badarg
//! compare_monitors()          -> {0,-1,1}
//! whereis(registered)         -> {1,<0.10.0>}
//! whereis(unregistered)       -> {0,undefined}
//! whereis(42)                 -> {0,undefined}   ; erlang:whereis(42) -> error:badarg
//! whereis_port(registered)    -> {1,#Port<0.3>}
//! whereis_port(unregistered)  -> {0,undefined}
//! whereis_port(pid name)      -> {0,undefined}
//! reschedule(7)               -> {dirty,7}   ; the scheduled do_work/1 really ran
//! do_work(7)                  -> {dirty,7}
//! ```
//!
//! The two **monitor** NIFs cannot be driven through the oracle: `enif_monitor_process(env, obj,
//! …)` dereferences `obj` (`DATA_TO_RESOURCE`, `erl_nif.c:3958`) and the only `obj` this runtime can
//! supply is `NULL` (the `resources` family is a stub, so `enif_alloc_resource` is not even
//! exported) — running them on the oracle is a **hard crash** (`SIGSEGV`, exit 139, observed for
//! both `monitor_code/1` and `monitor_roundtrip/1`). Their *semantics*, though, are pinned by the
//! oracle through the `erlang` API: `erlang:monitor(process, Pid)` returns a reference for a live
//! pid, and for a dead pid it returns one too but immediately delivers
//! `{'DOWN',Ref,process,Pid,noproc}` — i.e. the monitor was not established, which is exactly the C's
//! `1` (`erts_proc_sig_send_monitor` fails, `erl_nif.c:4001-4012`); the C's `0` is "established".
//! `enif_demonitor_process`'s `1` is "the reference is not (or no longer) in the resource's monitor
//! tree" (`erl_nif.c:4040-4051`) — the codes are asserted below against a host that reproduces that
//! tree, and the `enif_*` codes are read from the C source they were reproduced from.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system `erl_nif.h`; inspecting symbol tables needs
//! `nm`/`readelf`. When a *toolchain* facility is missing the affected test prints a loud `SKIP:`
//! banner and returns — but when the toolchain **is** present, every assertion is hard (a mismatch
//! fails, it never silently passes).

mod common;

use std::path::Path;
use std::process::Command;

use beam_abi::common::ERL_DIRTY_JOB_CPU_BOUND;
use beam_abi::nif::enif::enif_api_table;
use beam_abi::nif::loader::{load, NifCallError, NifLoadError};
use beam_abi::{NifEnv, NifHost};
use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::prelude::{
    atom_val, bitstring_parts, boxed_val, is_atom, is_internal_ref, is_tuple, make_atom,
    make_boxed, make_header, make_internal_pid, make_internal_port, make_pid_data, make_port_data,
    make_small, tuple_element, tuple_size, Eterm, REF_SUBTAG,
};

use common::{compile_so, skip};
use libc::c_int;

/// The committed C fixture — a real NIF module (see the file header for the A.3 rationale).
const FIXTURE_C: &str = include_str!("fixtures/nif_process.c");

/// Every `enif_*` symbol the fixture imports: they must be *undefined* in the `.so` and *defined* in
/// the loading process. The first twelve are the process family this crate owns.
const FIXTURE_IMPORTS: [&str; 22] = [
    // The twelve process-family slots.
    "enif_send",
    "enif_port_command",
    "enif_whereis_pid",
    "enif_whereis_port",
    "enif_is_current_process_alive",
    "enif_is_process_alive",
    "enif_is_port_alive",
    "enif_monitor_process",
    "enif_demonitor_process",
    "enif_compare_monitors",
    "enif_make_monitor_term",
    "enif_schedule_nif",
    // The helpers the fixture composes them with.
    "enif_self",
    "enif_get_local_pid",
    "enif_get_local_port",
    "enif_get_int",
    "enif_make_int",
    "enif_make_atom",
    "enif_make_new_binary",
    "enif_make_tuple_from_array",
    "enif_make_badarg",
    "enif_is_ref",
];

/// The twelve `{name, arity}` pairs the fixture declares, in its declared order.
const EXPECTED_FUNCS: [(&str, u32); 12] = [
    ("send_to_self", 1),
    ("alive", 0),
    ("is_alive", 1),
    ("is_port_alive", 1),
    ("port_command_code", 1),
    ("monitor_code", 1),
    ("monitor_roundtrip", 1),
    ("compare_monitors", 0),
    ("whereis", 1),
    ("whereis_port", 1),
    ("reschedule", 1),
    ("do_work", 1),
];

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `None` (after a loud skip) when the compiled `.so` path is not valid UTF-8.
fn so_path(so: &Path) -> Option<String> {
    match so.to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// Run `cmd args…`, returning its stdout on success (`None` when the tool is missing or fails).
fn run_tool(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Every `enif_*` name in a `nm -D`/`readelf --dyn-syms` dump whose undefined/defined flag matches
/// `undefined` — the symbol is always the dump line's last whitespace-separated field, and an
/// undefined symbol is the one marked `U` (`nm`) or `UND` (`readelf`).
fn enif_names(dump: &str, undefined: bool) -> Vec<String> {
    dump.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next_back()?;
            if !name.starts_with("enif_") {
                return None;
            }
            let is_undefined = line
                .split_whitespace()
                .any(|field| field == "U" || field == "UND");
            (is_undefined == undefined).then(|| name.to_string())
        })
        .collect()
}

/// The `enif_*` names `path` resolves at load time (its unresolved imports), or `None` when neither
/// `nm` nor `readelf` is usable.
fn undefined_enif_imports(path: &Path) -> Option<Vec<String>> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--undefined-only", &p]) {
        return Some(enif_names(&out, true));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(enif_names(&out, true));
    }
    None
}

/// `Some(true)`/`Some(false)` when a symbol-dump tool inspected `path`, `None` when neither `nm` nor
/// `readelf` is usable. `true` iff every name in `names` is **exported** (defined) by `path`.
fn all_symbols_exported(path: &Path, names: &[&str]) -> Option<bool> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--defined-only", &p]) {
        return Some(names.iter().all(|name| out.contains(name)));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(names.iter().all(|name| {
            out.lines()
                .any(|line| line.contains(name) && !line.contains(" UND "))
        }));
    }
    None
}

/// The elements of a live tuple term (`None` when `term` is not a tuple).
fn tuple_elements(term: Eterm) -> Option<Vec<Eterm>> {
    // SAFETY: `is_tuple` only dereferences the header of a boxed term.
    if !unsafe { is_tuple(term) } {
        return None;
    }
    // SAFETY: `term` is a tuple (checked above), so `tuple_size` and every index below are in range.
    let n = unsafe { tuple_size(term) };
    Some(
        (0..n)
            // SAFETY: as above — `i < tuple_size(term)`.
            .map(|i| unsafe { tuple_element(term, i) })
            .collect(),
    )
}

/// The elements of a live tuple as small ints (`None` for a non-tuple or a non-small element).
fn tuple_smalls(term: Eterm) -> Option<Vec<i64>> {
    let elements = tuple_elements(term)?;
    elements
        .into_iter()
        .map(|e| beam_term::prelude::is_small(e).then(|| beam_term::prelude::signed_value(e)))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The test host: in-memory process/port/name/monitor tables
// ---------------------------------------------------------------------------------------------

/// The monitor payload's reference block — `ERTS_REF_THING_SIZE` words (`process.rs`).
const REF_THING_WORDS: usize = 3;

/// A [`NifHost`] answering every process-family hook from hand-written tables, and recording what
/// the C NIF drove through them.
struct ProcHost {
    heap: Heap,
    atoms: AtomTable,
    /// `enif_self`'s answer.
    self_pid: Eterm,
    /// Pids the host calls alive.
    live_pids: Vec<Eterm>,
    /// Ports the host calls alive.
    live_ports: Vec<Eterm>,
    /// Registered names → the pid/port term they are registered to.
    named: Vec<(String, Eterm)>,
    /// Every accepted `enif_send`: `(receiver, message)`.
    sent: Vec<(Eterm, Eterm)>,
    /// Every accepted `enif_port_command`: `(port, data)`.
    port_cmds: Vec<(Eterm, Eterm)>,
    /// The payloads of the monitors currently held.
    monitors: Vec<(u64, u64)>,
    /// The next reference id `monitor_process` hands out.
    next_ref: u64,
    /// Every accepted `schedule_nif`: `(name, flags, argc)`.
    scheduled: Vec<(String, c_int, usize)>,
}

impl ProcHost {
    /// A host whose `self()` is process 42, with processes 42/7 and port 3 live and the two
    /// registered names the tests use.
    fn new() -> ProcHost {
        let mut host = ProcHost {
            heap: Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            self_pid: pid(42),
            live_pids: Vec::new(),
            live_ports: Vec::new(),
            named: Vec::new(),
            sent: Vec::new(),
            port_cmds: Vec::new(),
            monitors: Vec::new(),
            next_ref: 1,
            scheduled: Vec::new(),
        };
        host.live_pids = vec![pid(42), pid(7)];
        host.live_ports = vec![port_term(3)];
        host.named = vec![
            ("nif_process_registered".to_string(), pid(7)),
            ("nif_process_port".to_string(), port_term(3)),
        ];
        host
    }

    /// Intern `name` in this host's atom table and return the atom term.
    fn atom(&mut self, name: &str) -> Eterm {
        make_atom(self.atoms.intern(name))
    }

    /// The `(w1, w2)` payload of an internal reference term (`None` when `term` is not one).
    fn ref_payload(&self, term: Eterm) -> Option<(u64, u64)> {
        if !unsafe { is_internal_ref(term) } {
            return None;
        }
        let p = boxed_val(term);
        // SAFETY: `term` is a live internal reference, so its block's words 1 and 2 exist.
        unsafe { Some(((*p.add(1)).raw() as u64, (*p.add(2)).raw() as u64)) }
    }

    /// The internal reference `erts_make_ref_in_buffer` would build for `id` — the 3-word
    /// `[header, w1, w2]` block the C stores into an `ErlNifMonitor` (`io.c:6994-7010`).
    fn ref_term(&mut self, id: u64) -> Eterm {
        // SAFETY: `REF_THING_WORDS` fresh, exclusively owned words from the host heap.
        let p = unsafe { self.heap.alloc_words(REF_THING_WORDS) }.expect("the test heap has room");
        // SAFETY: `p` addresses those words; the block is a valid ref thing.
        unsafe {
            *p = make_header(REF_THING_WORDS - 1, REF_SUBTAG);
            *p.add(1) = Eterm::from_raw(id as usize);
            *p.add(2) = Eterm::from_raw(0);
        }
        make_boxed(p)
    }
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
        Some(self.self_pid)
    }

    fn is_current_process_alive(&mut self) -> Option<bool> {
        Some(true)
    }

    fn is_process_alive(&mut self, pid: Eterm) -> Option<bool> {
        Some(self.live_pids.iter().any(|p| p.raw() == pid.raw()))
    }

    fn is_port_alive(&mut self, port: Eterm) -> Option<bool> {
        Some(self.live_ports.iter().any(|p| p.raw() == port.raw()))
    }

    fn send(&mut self, to: Eterm, _from: Option<Eterm>, msg: Eterm) -> Option<bool> {
        // `enif_send` is "send to a live local process"; a dead receiver is the C's 0.
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
            .find(|(n, v)| *n == name && beam_term::prelude::is_internal_pid(*v))
            .map(|(_, v)| *v)
    }

    fn whereis_port(&mut self, name: Eterm) -> Option<Eterm> {
        let name = self.atom_name(name)?;
        self.named
            .iter()
            .find(|(n, v)| *n == name && beam_term::prelude::is_internal_port(*v))
            .map(|(_, v)| *v)
    }

    fn monitor_process(&mut self, pid: Eterm) -> Option<Eterm> {
        // A dead target cannot be monitored: `None` is the C's `1` ("target not alive").
        if !self.live_pids.iter().any(|p| p.raw() == pid.raw()) {
            return None;
        }
        let id = self.next_ref;
        self.next_ref += 1;
        self.monitors.push((id, 0));
        Some(self.ref_term(id))
    }

    fn demonitor_process(&mut self, monitor: Eterm) -> Option<bool> {
        let Some(payload) = self.ref_payload(monitor) else {
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
            *mut beam_abi::nif::types::ErlNifEnv,
            c_int,
            *const beam_abi::ERL_NIF_TERM,
        ) -> beam_abi::ERL_NIF_TERM,
        argc: usize,
    ) -> Option<Eterm> {
        self.scheduled.push((fun_name.to_string(), flags, argc));
        Some(make_atom(self.atoms.intern("scheduled")))
    }
}

/// An internal pid term for process number `num`.
fn pid(num: u64) -> Eterm {
    make_internal_pid(make_pid_data(0, num))
}

/// An internal port term for port number `num`.
fn port_term(num: u64) -> Eterm {
    make_internal_port(make_port_data(num))
}

// ---------------------------------------------------------------------------------------------
// The frozen `enif_*` table
// ---------------------------------------------------------------------------------------------

#[test]
fn the_enif_table_is_populated_for_every_process_family_slot() {
    // Calling `enif_api_table()` both pins the frozen table shape and forces the
    // `enif_*`-carrying codegen unit into the link, so `--export-dynamic` can publish it.
    let table = enif_api_table();
    assert!(table.enif_send.is_some());
    assert!(table.enif_port_command.is_some());
    assert!(table.enif_whereis_pid.is_some());
    assert!(table.enif_whereis_port.is_some());
    assert!(table.enif_is_current_process_alive.is_some());
    assert!(table.enif_is_process_alive.is_some());
    assert!(table.enif_is_port_alive.is_some());
    assert!(table.enif_monitor_process.is_some());
    assert!(table.enif_demonitor_process.is_some());
    assert!(table.enif_compare_monitors.is_some());
    assert!(table.enif_make_monitor_term.is_some());
    assert!(table.enif_schedule_nif.is_some());
    // The helpers the fixture composes them with.
    assert!(table.enif_self.is_some());
    assert!(table.enif_get_local_pid.is_some());
    assert!(table.enif_get_local_port.is_some());
    assert!(table.enif_make_new_binary.is_some());
    // The C-variadic slots are populated only when the C shim linked (that is the whole point of
    // the `beam_abi_variadic_shim` cfg — see `src/nif/enif/variadic.rs`); the fixture avoids them
    // either way, since stable Rust cannot define a C-variadic `fn` itself.
    assert_eq!(table.enif_make_tuple.is_some(), cfg!(beam_abi_variadic_shim));
    assert_eq!(table.enif_make_list.is_some(), cfg!(beam_abi_variadic_shim));
}

// ---------------------------------------------------------------------------------------------
// The `--export-dynamic` mechanism, checked directly
// ---------------------------------------------------------------------------------------------

#[test]
fn the_test_binary_exports_the_process_family_symbols_dynamically() {
    // Force the `enif_*` definitions into the link before inspecting it.
    assert!(enif_api_table().enif_send.is_some());

    let Ok(exe) = std::fs::canonicalize("/proc/self/exe") else {
        skip("the_test_binary_exports_the_process_family_symbols: /proc/self/exe is unavailable");
        return;
    };
    match all_symbols_exported(&exe, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "`build.rs` emits `-Wl,--export-dynamic` for the test targets, so {} must appear in \
             {:?}'s dynamic symbol table",
            FIXTURE_IMPORTS.join("/"),
            exe
        ),
        None => skip("the_test_binary_exports_the_process_family_symbols: no nm/readelf available"),
    }
}

// ---------------------------------------------------------------------------------------------
// End-to-end: load a real NIF `.so` and call the process family through the C ABI
// ---------------------------------------------------------------------------------------------

#[test]
fn loads_and_calls_the_process_family_through_the_c_abi() {
    // Load-bearing: this pins the frozen `enif_*` table AND keeps the `enif_*`-carrying codegen unit
    // in the link (without it the `.so`'s imports would not resolve and `dlopen` would fail).
    assert!(enif_api_table().enif_schedule_nif.is_some());

    let Some(so) = compile_so("nif_process", FIXTURE_C) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    // --- the `.so` really imports the process family (bound at load time, not at link time) -------
    // The set is compared exactly: a fixture that imported a slot this crate leaves `None` would make
    // `dlopen(RTLD_NOW)` fail, so this is the load-bearing half of the A.3 proof.
    match undefined_enif_imports(&so) {
        Some(mut found) => {
            let mut expected: Vec<String> =
                FIXTURE_IMPORTS.iter().map(|n| (*n).to_string()).collect();
            found.sort();
            expected.sort();
            assert_eq!(
                found,
                expected,
                "the fixture must import exactly the slots this crate exports; inspect {} with \
                 `nm -D --undefined-only`",
                so.display()
            );
        }
        None => skip("loads_and_calls_the_process_family: no nm/readelf to inspect the .so"),
    }

    // --- dlopen(RTLD_NOW) + the `ErlNifEntry` validation ----------------------------------------
    let mut host = ProcHost::new();
    let lib = load(&path, "nif_process", &mut host, Eterm::nil())
        .expect("a real NIF library must load: its enif_* imports resolve from the test binary");

    assert_eq!(lib.module_name(), "nif_process");
    assert_eq!(lib.funcs().len(), EXPECTED_FUNCS.len());
    for (name, arity) in EXPECTED_FUNCS {
        assert!(
            lib.funcs().iter().any(|(n, a)| n == name && *a == arity),
            "the fixture declares {name}/{arity}; found {:?}",
            lib.funcs()
        );
    }
    assert!(lib.priv_data().is_null(), "`ERL_NIF_INIT(..., NULL, ...)` stores no priv");

    let mut env = NifEnv::new(&mut host);

    // --- enif_send: 1 for a live receiver, and the constructed message really left the NIF -------
    // ORACLE: `send_to_self(5) -> 1`, with `{5, self()}` delivered to the calling process.
    assert_eq!(
        lib.call(&mut env, "send_to_self", 1, &[make_small(5)])
            .expect("send_to_self/1"),
        make_small(1)
    );
    assert_eq!(host.sent.len(), 1, "exactly one send reached the host");
    let (to, msg) = host.sent[0];
    assert_eq!(
        (to, tuple_elements(msg)),
        (pid(42), Some(vec![make_small(5), pid(42)])),
        "the message is the 2-tuple {{Arg, self()}} (immediates compare exactly)"
    );

    // --- the liveness predicates -----------------------------------------------------------------
    // ORACLE: `alive() -> {1,1}`; `is_alive(self()) -> 1`; `is_alive(dead) -> 0`.
    assert_eq!(
        tuple_smalls(lib.call(&mut env, "alive", 0, &[]).expect("alive/0")),
        Some(vec![1, 1])
    );
    assert_eq!(
        lib.call(&mut env, "is_alive", 1, &[pid(42)]).expect("is_alive/1"),
        make_small(1)
    );
    assert_eq!(
        lib.call(&mut env, "is_alive", 1, &[pid(9)]).expect("is_alive/1"),
        make_small(0),
        "a pid the host does not know is the C's dead-process 0"
    );
    // ORACLE: `is_alive(42) -> error:badarg` (the fixture vetoes a non-pid argument).
    let not_a_pid = host.atom("not_a_pid");
    match lib.call(&mut env, "is_alive", 1, &[not_a_pid]) {
        Err(NifCallError::Raised(reason)) => {
            assert_eq!(reason, env.host().intern_atom("badarg").expect("interns"))
        }
        other => panic!("expected Raised(badarg) for is_alive(atom), got {other:?}"),
    }
    env.clear();

    // ORACLE: `is_port_alive(live) -> 1`, `is_port_alive(closed) -> 0`.
    assert_eq!(
        lib.call(&mut env, "is_port_alive", 1, &[port_term(3)])
            .expect("is_port_alive/1"),
        make_small(1)
    );
    assert_eq!(
        lib.call(&mut env, "is_port_alive", 1, &[port_term(4)])
            .expect("is_port_alive/1"),
        make_small(0)
    );

    // --- enif_port_command: 1 for a live port, and the payload is the binary the NIF built -------
    // ORACLE: `port_command_code(live) -> 1` and the port really received `"ping"`;
    //         `port_command_code(closed) -> 0` (`erlang:port_command/2` raises `badarg` there).
    assert_eq!(
        lib.call(&mut env, "port_command_code", 1, &[port_term(3)])
            .expect("port_command_code/1"),
        make_small(1)
    );
    assert_eq!(host.port_cmds.len(), 1, "exactly one command reached the host");
    let (port, data) = host.port_cmds[0];
    assert_eq!(port, port_term(3));
    // SAFETY: the recorded term is a live on-heap binary built by `enif_make_new_binary`.
    let (base, offset, bits) = unsafe { bitstring_parts(data) };
    assert_eq!((offset, bits), (0, 32), "`<<\"ping\">>` is 4 byte-aligned bytes");
    // SAFETY: `base` heads `bits // 8 == 4` readable bytes of a live term on the host's heap.
    assert_eq!(unsafe { core::slice::from_raw_parts(base, 4) }, b"ping");
    assert_eq!(
        lib.call(&mut env, "port_command_code", 1, &[port_term(4)])
            .expect("port_command_code/1"),
        make_small(0),
        "a closed port is the C's 0"
    );
    assert_eq!(host.port_cmds.len(), 1, "the refused command was not recorded");

    // --- enif_monitor_process / enif_demonitor_process / enif_make_monitor_term ------------------
    // The C's codes: 0 = established, 1 = the target is dead/undefined, 1 = the demonitor could not
    // identify the monitor (see the module docs for why the oracle cannot drive these two NIFs).
    assert_eq!(
        lib.call(&mut env, "monitor_code", 1, &[pid(42)]).expect("monitor_code/1"),
        make_small(0)
    );
    assert_eq!(
        lib.call(&mut env, "monitor_code", 1, &[pid(9)]).expect("monitor_code/1"),
        make_small(1),
        "a dead pid is the C's `return 1` (`erl_nif.c:4004`)"
    );
    assert_eq!(
        tuple_smalls(
            lib.call(&mut env, "monitor_roundtrip", 1, &[pid(42)])
                .expect("monitor_roundtrip/1")
        ),
        Some(vec![0, 0, 1, 0, 1]),
        "{{established, demonitored, second demonitor not found, memcmp self == 0, ref}}"
    );
    assert_eq!(
        tuple_smalls(
            lib.call(&mut env, "monitor_roundtrip", 1, &[pid(9)])
                .expect("monitor_roundtrip/1")
        ),
        Some(vec![1, -1, -1, -1, -1]),
        "a dead target establishes nothing, so the holder is never a payload"
    );
    // The host really held (and then released) a monitor: the reference payload round-tripped.
    assert_eq!(host.monitors.len(), 1, "one monitor is still held by `monitor_code`");
    let monitor = host.monitors[0];

    // --- enif_compare_monitors --------------------------------------------------------------------
    // ORACLE: `compare_monitors() -> {0,-1,1}`.
    assert_eq!(
        tuple_smalls(lib.call(&mut env, "compare_monitors", 0, &[]).expect("compare_monitors/0")),
        Some(vec![0, -1, 1])
    );

    // --- enif_whereis_pid / enif_whereis_port -----------------------------------------------------
    let registered = host.atom("nif_process_registered");
    let port_name = host.atom("nif_process_port");
    let unregistered = host.atom("nif_process_nothing");
    let undefined = host.atom("undefined");
    // ORACLE: `whereis(registered) -> {1,<0.10.0>}`.
    let hit = lib
        .call(&mut env, "whereis", 1, &[registered])
        .expect("whereis/1");
    assert_eq!(tuple_elements(hit), Some(vec![make_small(1), pid(7)]));
    // ORACLE: `whereis(unregistered) -> {0,undefined}` and `whereis(42) -> {0,undefined}` (the C's
    // `is_not_atom(name)` early return leaves the out-parameter untouched).
    let miss = lib
        .call(&mut env, "whereis", 1, &[unregistered])
        .expect("whereis/1");
    assert_eq!(tuple_elements(miss), Some(vec![make_small(0), undefined]));
    let not_atom = lib
        .call(&mut env, "whereis", 1, &[make_small(42)])
        .expect("whereis/1");
    assert_eq!(tuple_elements(not_atom), Some(vec![make_small(0), undefined]));
    // ORACLE: `whereis_port(registered) -> {1,#Port<0.3>}`, a pid-registered name `-> {0,undefined}`.
    let port_hit = lib
        .call(&mut env, "whereis_port", 1, &[port_name])
        .expect("whereis_port/1");
    assert_eq!(tuple_elements(port_hit), Some(vec![make_small(1), port_term(3)]));
    let wrong_kind = lib
        .call(&mut env, "whereis_port", 1, &[registered])
        .expect("whereis_port/1");
    assert_eq!(tuple_elements(wrong_kind), Some(vec![make_small(0), undefined]));

    // --- enif_schedule_nif (through `reschedule/1` + the real `do_work/1` entry point) ------------
    // ORACLE: `reschedule(7) -> {dirty,7}` — the scheduled call really ran with the caller's argv.
    // Here the host is the runtime: it records the request and supplies the term.
    let scheduled = host.atom("scheduled");
    assert_eq!(
        lib.call(&mut env, "reschedule", 1, &[make_small(7)])
            .expect("reschedule/1"),
        scheduled
    );
    assert_eq!(
        host.scheduled,
        vec![("do_work".to_string(), ERL_DIRTY_JOB_CPU_BOUND, 1)],
        "the name, the dirty-job flag and the argument count reach the host"
    );
    // ORACLE: `do_work(7) -> {dirty,7}`.
    let dirty = host.atom("dirty");
    assert_eq!(
        tuple_elements(lib.call(&mut env, "do_work", 1, &[make_small(7)]).expect("do_work/1")),
        Some(vec![dirty, make_small(7)])
    );

    // --- a wrong arity is never callable ----------------------------------------------------------
    // ORACLE: `do_work(1,2) -> error:undef` — a NIF that is only declared as `/1` has no `/2`.
    assert!(matches!(
        lib.call(&mut env, "do_work", 2, &[make_small(1), make_small(2)]),
        Err(NifCallError::UnknownFunction { name, arity })
            if name == "do_work" && arity == 2
    ));
    assert!(matches!(
        lib.call(&mut env, "does_not_exist", 1, &[]),
        Err(NifCallError::UnknownFunction { .. })
    ));

    // The monitor the host still holds is a real reference payload: `enif_make_monitor_term` built
    // it from the bytes `enif_monitor_process` stored (`io.c:6994-7010`), which is what makes the
    // round trip possible at all.
    drop(env);
    let mut monitor_host = ProcHost::new();
    let reference = monitor_host.ref_term(monitor.0);
    assert_eq!(monitor_host.ref_payload(reference), Some(monitor));
}

// ---------------------------------------------------------------------------------------------
// The loader's module-name check, through the same real library
// ---------------------------------------------------------------------------------------------

#[test]
fn rejects_the_fixture_when_the_calling_module_differs() {
    let Some(so) = compile_so("nif_process", FIXTURE_C) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    // `ERL_NIF_INIT(nif_process, …)` seeds the entry name, so the oracle's check 9
    // (`Library module name '…' does not match calling module '…'`, `erl_nif.c:4841`) rejects any
    // other calling module.
    let mut host = ProcHost::new();
    let err = load(&path, "nif_process_other", &mut host, Eterm::nil())
        .expect_err("the entry name must not match");
    assert!(matches!(err, NifLoadError::BadLib(_)), "got {err:?}");
    assert_eq!(err.reason_atom(), "bad_lib");
    assert_eq!(
        err.text(),
        "Library module name 'nif_process' does not match calling module 'nif_process_other'"
    );
}
