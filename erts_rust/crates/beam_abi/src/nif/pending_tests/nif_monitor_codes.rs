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

//! End-to-end proof of **`enif_monitor_process`'s three return codes** (contract A.3).
//!
//! `tests/fixtures/nif_monitor_codes.c` is a real C NIF — compiled at test time with the system `cc`
//! against the system `erl_nif.h` — whose five entry points each run one `enif_monitor_process` and
//! answer `{ReturnCode, WasHolderUntouched}` (the second element is `1` when the 32-byte
//! `ErlNifMonitor` still holds its zero bytes, `0` when the C wrote the reference block):
//!
//! Ground truth `erts/emulator/beam/erl_nif.c:3954-4011`, **in the C's own order**:
//!
//! | entry | situation | C | oracle |
//! |---|---|---|---|
//! | `arm_live/0` | capable type, live target | `0` (armed, holder written) | `{0,0}` |
//! | `arm_undefined/0` | capable type, `enif_set_pid_undefined` | `1` (holder untouched) | `{1,1}` |
//! | `arm_no_down/0` | type opened **without** a `down` callback | `-1` (holder untouched) | `{-1,1}` |
//! | `arm_no_down_undefined/0` | no `down` **and** an undefined pid | `-1` — the `-1` precedes the pid check | `{-1,1}` |
//! | `arm_null_holder/0` | capable type, null holder pointer | `0` (nothing to write) | `0` |
//!
//! The oracle column is the measured output of the same `.so` under OTP 29.1.1 / erts-17.1.
//!
//! # The one recorded divergence (named, not hidden)
//!
//! Arming a **real** monitor needs the host (`NifHost::monitor_process_result`): looking the pid up,
//! minting the internal reference and delivering `'DOWN'` later. `beam_abi` cannot do that itself, so
//! a host **without** the hook falls back to the C's `1` ("could not establish") where the oracle
//! answers `0` — exactly what `the_codes_this_crate_answers_without_an_arming_host` asserts. Closing
//! it is `beam_rt`'s `NifHostImpl` job (see `src/nif/CONTEXT.md` → *Known gaps*), **not** something
//! this crate can fake. `an_arming_host_reproduces_the_oracles_zero` then proves the write path is
//! already correct once a host supplies the reference.
//!
//! ⚠️ There is deliberately **no** `enif_demonitor_process` probe here: calling it with a zeroed
//! holder crashes the oracle VM, so such a fixture could not be cross-checked at all.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system `erl_nif.h`; `compile_so` prints a loud
//! `SKIP:` banner and returns `None` when either is missing (the calling test then returns). The
//! oracle half skips loudly when `erl`/`erlc` are absent. Whenever the toolchain **is** present every
//! assertion is hard.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::nif::enif::{
    enif_alloc_resource, enif_init_resource_type, enif_monitor_process,
};
use beam_abi::nif::host::NifMonitorResult;
use beam_abi::nif::loader::{load, NifLibrary};
use beam_abi::nif::types::{
    ErlNifMonitor, ErlNifPid, ErlNifResourceDown, ErlNifResourceFlags, ErlNifResourceType,
    ErlNifResourceTypeInit,
};
use beam_abi::{nif_from_eterm, NifEnv, NifHost};
use beam_term::atom::AtomTable;
use beam_term::prelude::{
    atom_val, boxed_val, is_atom, is_internal_ref, is_small, make_atom, make_boxed, make_header,
    make_internal_pid, make_pid_data, signed_value, tuple_element, tuple_size, Eterm, REF_SUBTAG,
};

use common::{compile_so, skip};

/// The committed C fixture (see its header for what it proves).
const FIXTURE_C: &str = include_str!("fixtures/nif_monitor_codes.c");

/// The fixture's `ERL_NIF_INIT` name — the loader *and* the oracle require it to equal the calling
/// module, so it doubles as the name of the Erlang module the oracle test generates.
const MODULE: &str = "nif_monitor_codes";

/// The five entries, in the order every table in this file uses.
const ENTRIES: [&str; 5] = [
    "arm_live",
    "arm_undefined",
    "arm_no_down",
    "arm_no_down_undefined",
    "arm_null_holder",
];

/// The measured oracle answers (OTP 29.1.1 / erts-17.1).
const ORACLE: [&str; 5] = ["{0,0}", "{1,1}", "{-1,1}", "{-1,1}", "0"];

/// What this crate answers **without** an arming host: `arm_live`/`arm_null_holder` report the C's
/// `1` ("could not establish") because no host armed the monitor — the one recorded divergence.
const WITHOUT_HOOK: [&str; 5] = ["{1,1}", "{1,1}", "{-1,1}", "{-1,1}", "1"];

/// `ERTS_REF_THING_SIZE * sizeof(Eterm)` — the monitor bytes the C copies (`io.c:6994-7010`).
const REF_THING_BYTES: usize = 24;

/// The `ErlNifMonitor` payload length (`sizeof(void *) * 4`).
const MONITOR_BYTES: usize = 32;

const _: () = assert!(REF_THING_BYTES <= MONITOR_BYTES);

// ---------------------------------------------------------------------------------------------
// The hosts
// ---------------------------------------------------------------------------------------------

/// A minimal [`NifHost`] with a `self_pid`, so the fixture's `enif_self` succeeds — and **no** monitor
/// hook, which is the default state of every host that has not adopted
/// [`NifHost::monitor_process_result`].
struct PlainMonitorHost {
    heap: beam_alloc::Heap,
    atoms: AtomTable,
}

impl PlainMonitorHost {
    fn new() -> PlainMonitorHost {
        PlainMonitorHost {
            heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
        }
    }
}

impl NifHost for PlainMonitorHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
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

    /// `erlang:self()` — an internal pid term (`enif_self` needs one to fill its `ErlNifPid`).
    fn self_pid(&mut self) -> Option<Eterm> {
        Some(make_internal_pid(make_pid_data(0, 1)))
    }
}

/// The same host plus the **three-way** monitor hook: it answers
/// [`NifMonitorResult::Armed`] with the internal reference it mints on its own heap, which is what a
/// real host does (the `beam_rt` gap).
struct ArmingHost {
    inner: PlainMonitorHost,
    /// What [`NifHost::monitor_process_result`] answers (`None` = decline, fall back to the legacy
    /// hook).
    result: Option<NifMonitorResult>,
}

impl ArmingHost {
    fn new() -> ArmingHost {
        ArmingHost {
            inner: PlainMonitorHost::new(),
            result: None,
        }
    }

    /// Mint the internal reference the C's `erts_make_ref_in_buffer` builds (`erl_nif.c:3978`): a
    /// 3-word `[header, id, 0]` block tagged `REF_SUBTAG`.
    fn mint_ref(&mut self) -> Eterm {
        // SAFETY: three fresh, exclusively owned words from the test heap.
        let p = unsafe { self.inner.heap.alloc_words(3) }.expect("the test heap has room");
        // SAFETY: `p` addresses those three words.
        unsafe {
            *p = make_header(2, REF_SUBTAG);
            *p.add(1) = Eterm::from_raw(0x5eed_0001);
            *p.add(2) = Eterm::from_raw(0);
        }
        make_boxed(p)
    }
}

impl NifHost for ArmingHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.inner.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        self.inner.intern_atom(name)
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        self.inner.atom_name(atom)
    }

    fn self_pid(&mut self) -> Option<Eterm> {
        self.inner.self_pid()
    }

    fn monitor_process_result(&mut self, _pid: Eterm) -> Option<NifMonitorResult> {
        self.result
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The fixture compiled into a shared object, or `None` (after a loud skip) when the toolchain is
/// absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so(MODULE, FIXTURE_C)
}

/// The path `load` takes — the compiled `.so` **without** its suffix, which the loader appends
/// itself — or `None` (after a loud skip) when it is not valid UTF-8.
fn load_path(so: &Path) -> Option<String> {
    match so.with_extension("").to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// Render the fixture's answer the way `~w` does: a bare small integer, or a tuple of small integers.
#[track_caller]
fn render(term: Eterm) -> String {
    if is_small(term) {
        return signed_value(term).to_string();
    }
    // SAFETY: the fixture's compound answer is a 2-tuple; `tuple_size`/`tuple_element` only read that
    // block and the index is bounded by `tuple_size`.
    let parts: Vec<String> = unsafe {
        (0..tuple_size(term))
            .map(|i| tuple_element(term, i))
            .collect::<Vec<Eterm>>()
    }
    .into_iter()
    .map(|element| {
        assert!(
            is_small(element),
            "unexpected element in the fixture's answer: {element:?}"
        );
        signed_value(element).to_string()
    })
    .collect();
    format!("{{{}}}", parts.join(","))
}

/// Run every entry through the loaded library and render the answers.
#[track_caller]
fn run_all<H: NifHost>(lib: &NifLibrary, host: &mut H) -> Vec<String> {
    let mut raws = Vec::with_capacity(ENTRIES.len());
    {
        let mut env = NifEnv::new(host);
        for name in ENTRIES {
            let term = lib
                .call(&mut env, name, 0, &[])
                .unwrap_or_else(|err| panic!("{name}/0 must be callable: {err:?}"));
            raws.push(term);
        }
    }
    raws.into_iter().map(render).collect()
}

/// A `down` callback for a monitor-capable test type: this crate only records its *presence*
/// (`resource_type_has_down`), it never calls it, so the body is intentionally empty.
///
/// # Safety
///
/// The C `ErlNifResourceDown` ABI; the body touches no pointer.
unsafe extern "C" fn unused_down(
    _env: *mut beam_abi::nif::types::ErlNifEnv,
    _obj: *mut core::ffi::c_void,
    _pid: *mut ErlNifPid,
    _monitor: *mut ErlNifMonitor,
) {
}

/// The 24 reference bytes an `ErlNifMonitor` carries for the internal reference `term` — what
/// `enif_monitor_process` must store on the `0` path.
#[track_caller]
fn ref_block(term: Eterm) -> [u8; REF_THING_BYTES] {
    // SAFETY: `is_internal_ref` dereferences only the header word of a boxed term.
    assert!(
        unsafe { is_internal_ref(term) },
        "the arm payload must be an internal reference"
    );
    let p = boxed_val(term);
    let mut block = [0u8; REF_THING_BYTES];
    for index in 0..(REF_THING_BYTES / core::mem::size_of::<usize>()) {
        // SAFETY: `term` is a live internal reference, so `boxed_val` points at its
        // `[header, w1, w2]` block, which is exactly `REF_THING_BYTES` bytes long.
        let word = unsafe { *p.add(index) }.raw();
        let at = index * core::mem::size_of::<usize>();
        block[at..at + core::mem::size_of::<usize>()].copy_from_slice(&word.to_ne_bytes());
    }
    block
}

// ---------------------------------------------------------------------------------------------
// The Rust side
// ---------------------------------------------------------------------------------------------

/// The three codes this crate can answer **without** an arming host: `-1` (no `down` callback) and
/// `1` (undefined target) are exact, while `arm_live`/`arm_null_holder` answer the C's `1`
/// ("could not establish") because nothing armed the monitor — the `beam_rt`-owned gap, named in the
/// module docs rather than papered over.
#[test]
fn the_codes_this_crate_answers_without_an_arming_host() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = PlainMonitorHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    let ours = run_all(&lib, &mut host);

    assert_eq!(
        ours,
        WITHOUT_HOOK
            .iter()
            .map(|answer| answer.to_string())
            .collect::<Vec<_>>(),
        "host-free codes: -1 (no down) and 1 (undefined target) are exact; an unarmed live target \
         reports the C's 1 until beam_rt implements NifHostImpl::monitor_process_result"
    );
    // The `-1` really is home-free: it comes from the crate's own resource registry, before the pid
    // is read.
    assert_eq!(ours[2], "{-1,1}");
    assert_eq!(ours[3], "{-1,1}", "the -1 precedes the pid check");
}

/// With a host that supplies the reference, the arm path reproduces the oracle's `0` **and** writes
/// the reference block into the caller's holder — i.e. the ABI half of the gap is already correct.
#[test]
fn an_arming_host_reproduces_the_oracles_zero() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = ArmingHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil()).expect("the fixture must load");
    let armed = host.mint_ref();
    host.result = Some(NifMonitorResult::Armed(armed));

    let ours = run_all(&lib, &mut host);
    assert_eq!(
        ours[0], "{0,0}",
        "an armed monitor answers 0 and writes the holder — the oracle's own answer"
    );
    assert_eq!(
        ours[4], "0",
        "and a null holder is legal on the success path"
    );
    // The `-1`/`1` rows are unaffected by the hook: both are decided before it is consulted.
    assert_eq!(ours[1], "{1,1}");
    assert_eq!(ours[2], "{-1,1}");
    assert_eq!(ours[3], "{-1,1}");
}

/// The stored payload is the hook's internal reference — all 24 bytes of it, with bytes 24..32
/// zeroed — which is what makes `enif_make_monitor_term`/`enif_demonitor_process` able to rebuild it
/// later.
#[test]
fn the_armed_holder_carries_the_reference_block() {
    // This sub-case drives the `enif_*` entry point directly (its own type, its own object), so it
    // needs neither the fixture nor the `cc`/`erl` toolchain: it always runs.
    let mut host = ArmingHost::new();
    let armed = host.mint_ref();
    host.result = Some(NifMonitorResult::Armed(armed));
    let expected = ref_block(armed);

    // A monitor-capable type of this test's own (`members = 3`, `down` present, no destructor).
    let (obj, pid) = {
        let mut env = NifEnv::new(&mut host);
        let name =
            std::ffi::CString::new("nif_monitor_codes_test_with_down").expect("no interior NUL");
        let init = ErlNifResourceTypeInit {
            dtor: None,
            stop: None,
            down: Some(unused_down as ErlNifResourceDown),
            members: 3,
            dyncall: None,
        };
        let mut tried = ErlNifResourceFlags::from_bits(0);
        // SAFETY: `env` is live, `name` is NUL-terminated, `init` is a live local and `tried` is
        // writable.
        let ty: *mut ErlNifResourceType = unsafe {
            enif_init_resource_type(
                env.as_ptr(),
                name.as_ptr(),
                &init,
                ErlNifResourceFlags::CREATE,
                &mut tried,
            )
        };
        assert!(!ty.is_null(), "the test type must open");
        // SAFETY: `ty` came from the open call above.
        let obj = unsafe { enif_alloc_resource(ty, 4) };
        assert!(!obj.is_null(), "the object must be allocatable");
        let pid = ErlNifPid {
            pid: nif_from_eterm(make_internal_pid(make_pid_data(0, 7))),
        };
        (obj, pid)
    };

    let mut holder = ErlNifMonitor {
        data: [0u8; MONITOR_BYTES],
    };
    let code = {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live, `obj` is a live payload pointer of a monitor-capable type, `pid` is
        // an initialised local and `holder` is writable.
        unsafe { enif_monitor_process(env.as_ptr(), obj, &pid, &mut holder) }
    };
    assert_eq!(code, 0, "the C's MONITOR_ESTABLISHED");
    assert_eq!(
        holder.data[..REF_THING_BYTES],
        expected,
        "the holder carries the hook's 3-word reference block"
    );
    assert_eq!(
        holder.data[REF_THING_BYTES..],
        [0u8; MONITOR_BYTES - REF_THING_BYTES],
        "and the bytes past the block are zeroed"
    );

    // The object was never exported as a term, so its allocator reference is the only one.
    // SAFETY: `obj` is a live payload pointer with one outstanding reference.
    unsafe { beam_abi::nif::enif::enif_release_resource(obj) };
}

// ---------------------------------------------------------------------------------------------
// The system-Erlang oracle (skipped loudly when the toolchain is absent)
// ---------------------------------------------------------------------------------------------

/// Whether `cmd` can be spawned with `args` at all (the probe for the oracle toolchain; a non-zero
/// exit is fine — e.g. `erlc -h` — only a missing binary counts as absent).
fn runnable(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok()
}

/// Load the fixture into the **system** Erlang and print one `~w` line per entry.
///
/// Every declared NIF must be exported by the generated module (the C's load refuses one it does not
/// find, `erl_nif.c:4874`), and the resource types are opened from the NIF `load` callback only —
/// `erl_nif.c:2708` refuses to open a type from a call.
fn oracle_print(so: &Path) -> Option<Vec<String>> {
    if !runnable("erl", &["-noshell", "-eval", "halt(0)."]) || !runnable("erlc", &["-h"]) {
        skip("the erl/erlc oracle toolchain is not available");
        return None;
    }
    let dir = so.parent()?.to_path_buf();
    // `erlang:load_nif/2` appends the shared-object suffix itself, so hand it the path WITHOUT `.so`.
    let stem = so.file_stem()?.to_str()?;
    let nif_path = dir.join(stem);
    let nif_path = nif_path.to_str()?;

    let exports: Vec<String> = ENTRIES.iter().map(|name| format!("{name}/0")).collect();
    let mut erl = format!("-module({MODULE}).\n");
    erl.push_str(&format!("-export([{}]).\n", exports.join(", ")));
    erl.push_str("-on_load(init/0).\n\n");
    erl.push_str(&format!(
        "init() ->\n    erlang:load_nif(\"{nif_path}\", 0).\n\n"
    ));
    for name in ENTRIES {
        erl.push_str(&format!("{name}() -> erlang:nif_error(nif_not_loaded).\n"));
    }

    let erl_file = dir.join(format!("{MODULE}.erl"));
    std::fs::write(&erl_file, erl).expect("the oracle module is writable");

    let compiled = Command::new("erlc")
        .arg("-o")
        .arg(&dir)
        .arg(&erl_file)
        .output()
        .expect("erlc is runnable (probed above)");
    assert!(
        compiled.status.success(),
        "erlc failed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut expr = String::new();
    for name in ENTRIES {
        expr.push_str(&format!("io:format(\"~w~n\", [{MODULE}:{name}()]),\n"));
    }
    expr.push_str("halt(0).\n");

    let out = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-eval")
        .arg(&expr)
        .output()
        .expect("erl is runnable (probed above)");
    assert!(
        out.status.success(),
        "the oracle run failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    println!("oracle: {}", lines.join(" | "));
    Some(lines)
}

/// The measured oracle codes — the constants the Rust assertions above are anchored on.
#[test]
fn the_same_fixture_agrees_with_the_measured_oracle_codes() {
    let Some(so) = compile_fixture() else { return };
    let Some(lines) = oracle_print(&so) else {
        return;
    };
    assert_eq!(
        lines,
        ORACLE
            .iter()
            .map(|answer| answer.to_string())
            .collect::<Vec<_>>(),
        "the measured enif_monitor_process codes (live, undefined, no-down, no-down+undefined, \
         null holder)"
    );
    // The divergence the Rust half documents is real, not a guess: only the two rows that need an
    // armed monitor differ from what this crate answers without an arming host.
    assert_eq!(ORACLE[2], WITHOUT_HOOK[2], "the -1 row is exact");
    assert_eq!(ORACLE[3], WITHOUT_HOOK[3], "the no-down+undefined row is exact");
    assert_ne!(ORACLE[0], WITHOUT_HOOK[0], "the live-target row diverges");
}
