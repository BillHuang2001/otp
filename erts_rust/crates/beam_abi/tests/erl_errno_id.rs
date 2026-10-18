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

//! End-to-end proof of **`erl_errno_id/1`**'s answer (workspace contract **A.4**): the real C
//! driver `tests/fixtures/erl_errno_id.c` — compiled with the system `cc` against the system
//! `erl_driver.h` — is `dlopen`ed, validated and driven through the Rust loader, and the very same
//! `.so` is then loaded and driven by the **real system ERTS**.
//!
//! # The contract
//!
//! `erl_errno_id(errno)` answers the errno's **lowercase POSIX symbol** — `eacces`, `enoent`,
//! `einval` — and the form `errno_<N>` for any errno the C library cannot name, **including `0`**
//! (`errno_map[0]` is written with `write_unknown_errno()` before the C's name loop starts, so it
//! never reaches the lookup). The names are the C library's `strerrorname_np` spelling
//! ASCII-lowercased (`errno_name()` + `tolower_copy_estring`, `erts/emulator/beam/erl_errno_str.c`),
//! with a fixed answer for the errno values that carry more than one name.
//!
//! # Two halves, both measured
//!
//! * [`a_real_c_driver_sees_the_posix_name_from_this_crates_erl_errno_id`] drives the `.so` through
//!   this crate's driver loader: the fixture's `control` callback asks the crate's own
//!   [`beam_abi::driver::drv::erl_errno_id`] for one errno and publishes the answer's bytes through
//!   `driver_output`, which the recording host captures. Every answer is compared against the
//!   measured oracle value in [`CASES`].
//! * [`the_oracle_answers_the_same_errno_strings_from_real_erts`] loads and drives the *same* `.so`
//!   under the system `erl` (`erl_ddll:load_driver/2` + `open_port({spawn_driver, …})` +
//!   `port_control/3`) and asserts every answer equals the value recorded in [`CASES`] — measured on
//!   OTP 29.1.1 / erts-17.1 — so the two runtimes answer **byte-identical strings**, with no
//!   `strerror` prose on either side.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system OTP include directory (which holds
//! `erl_driver.h`); the oracle half additionally needs `erlc`/`erl`. When a facility is missing the
//! affected test prints a loud `SKIP:` banner and returns — never a silent pass. When the toolchain
//! **is** present every assertion is hard.

mod common;

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::driver::drv::driver_api_table;
use beam_abi::driver::loader::load;
use beam_abi::driver::types::ErlDrvSizeT;
use beam_abi::DriverEnv;
use common::{compile_so, skip, TestDriverHost};
use libc::{c_char, c_int, c_uint};

/// The committed C fixture — a real driver (see the file header for the A.4 rationale).
const FIXTURE_C: &str = include_str!("fixtures/erl_errno_id.c");

/// The entry's `driver_name`, which is also the `.so` name: `erl_ddll:load_driver/2` and [`load`]
/// both reject a library whose `driver_name` differs from the requested name (`bad_driver_name`),
/// so the scratch `.so` is compiled with this stem.
const FIXTURE_NAME: &str = "errnoid";

/// The `control` command that asks for one errno's id (the fixture's `EID_CMD_LOOKUP`); the errno
/// itself travels in the request **data**, so values outside `port_control/3`'s command range
/// (`9999`, `-4711`) can be asked for.
const LOOKUP_COMMAND: c_uint = 1;

/// The `driver_*`/`erl_drv_*` symbols the fixture imports. They must be *undefined* in the `.so`
/// and *defined* in the loading process (`build.rs`'s `-Wl,--export-dynamic` + the `#[used]` api
/// table), so a successful `dlopen(RTLD_NOW)` is itself evidence the family is exported.
const FIXTURE_IMPORTS: [&str; 4] = [
    "driver_alloc",
    "driver_free",
    "driver_output",
    "erl_errno_id",
];

/// The errno values both halves ask about, with the answer each **must** get.
///
/// The answers are the **measured** `erl_errno_id/1` values of the real system ERTS (OTP 29.1.1 /
/// erts-17.1) for this `.so` — `ORACLE n=<errno> id=<answer>` — and the strings this crate pins. The
/// cases cover each route through the implementation: the standard names (`enoent`/`eacces`/
/// `einval`), the collision table (`EAGAIN`/`EWOULDBLOCK` share an integer here, `EBUSY`,
/// `ECONNREFUSED`, `EDEADLK`), the `errno_<N>` form (`0` — the C's forced `errno_map[0]` — `9999`)
/// and negative values (OTP's own `driver_SUITE_data/erl_errno_id_drv.c` pins `-4711` and `-1`).
const CASES: [(c_int, &str); 11] = [
    (0, "errno_0"),
    (libc::ENOENT, "enoent"),
    (libc::EACCES, "eacces"),
    (libc::EAGAIN, "eagain"),
    (libc::EBUSY, "ebusy"),
    (libc::ECONNREFUSED, "econnrefused"),
    (libc::EDEADLK, "edeadlk"),
    (libc::EINVAL, "einval"),
    (9999, "errno_9999"),
    (-1, "errno_-1"),
    (-4711, "errno_-4711"),
];

// ---------------------------------------------------------------------------------------------
// Helpers (the idiom of `tests/driver_port_term.rs`)
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

/// `true` iff every name in `names` is an **undefined** import of `path`.
fn all_symbols_are_undefined_imports(path: &Path, names: &[&str]) -> Option<bool> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--undefined-only", &p]) {
        return Some(names.iter().all(|name| out.contains(name)));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(names.iter().all(|name| {
            out.lines()
                .any(|line| line.contains(name) && line.contains(" UND "))
        }));
    }
    None
}

/// Compile the committed fixture; `None` (after a loud skip) when the toolchain is absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so(FIXTURE_NAME, FIXTURE_C)
}

/// Keep the `driver_*`-carrying codegen units in the link: the fixture's undefined imports must
/// resolve against this test binary's dynamic symbol table, and the api table is what publishes
/// them (`#[used]` static + `-Wl,--export-dynamic` from `build.rs`).
fn link_the_families() {
    let table = driver_api_table();
    assert_eq!(
        core::mem::size_of_val(table),
        core::mem::size_of::<beam_abi::driver::api::ErlDrvApi>()
    );
    core::hint::black_box(table);
}

// ---------------------------------------------------------------------------------------------
// Half 1: through the C ABI, in this crate
// ---------------------------------------------------------------------------------------------

#[test]
fn a_real_c_driver_sees_the_posix_name_from_this_crates_erl_errno_id() {
    link_the_families();

    let Some(so) = compile_fixture() else {
        return;
    };
    match all_symbols_are_undefined_imports(&so, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "{:?} must leave every fixture import undefined (that is the whole mechanism)",
            so
        ),
        None => skip("no nm/readelf: cannot inspect the fixture's dynamic symbols"),
    }
    let Some(path) = so_path(&so) else { return };

    // --- dlopen(RTLD_NOW) + the `ErlDrvEntry` validation ---------------------------------------
    let lib = load(&path, FIXTURE_NAME)
        .expect("a driver whose imports resolve from this test binary must load");
    assert_eq!(lib.name(), FIXTURE_NAME);

    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command = CString::new("start").expect("no interior NUL");
    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; `command` is NUL-terminated.
    let data = lib
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(
        !data.is_null(),
        "start must return the private state, not a failure code"
    );

    // Nothing borrows `host` across a driver call — the recording host sits behind `DriverEnv`'s
    // raw pointer for the duration — so `host` is read strictly before and after each call.
    let mut answers: Vec<String> = Vec::new();
    for (error, expected) in CASES {
        let request = CString::new(error.to_string()).expect("no interior NUL");
        let len = request.as_bytes().len() as ErlDrvSizeT;
        let outputs_before = host.outputs().len();
        let mut reply: *mut c_char = std::ptr::null_mut();
        // SAFETY: `data` is the live state `start` returned, the request buffer is `len` readable
        // bytes, and a null reply pointer with `rlen == 0` is the "length only" shape
        // `port_control/3` uses.
        let n = lib
            .control(
                data,
                LOOKUP_COMMAND,
                request.as_ptr() as *mut c_char,
                len,
                &mut reply,
                0,
            )
            .expect("the fixture implements control");
        assert_eq!(
            n, 0,
            "the fixture answers through driver_output, never the (truncating) control reply buffer"
        );
        let outputs = host.outputs();
        assert_eq!(
            outputs.len(),
            outputs_before + 1,
            "the request for errno {error} must publish exactly one answer"
        );
        let answer = String::from_utf8(outputs.last().expect("just checked").clone())
            .expect("the answers are ASCII");
        assert_eq!(
            answer, expected,
            "erl_errno_id({error}) must answer the measured oracle string"
        );
        answers.push(answer);
    }
    assert_eq!(
        answers.len(),
        CASES.len(),
        "every case must produce exactly one answer"
    );
    assert_eq!(host.outputs().len(), CASES.len(), "no stray output");

    // The two properties that make the answers the oracle's rather than `strerror` prose: no
    // uppercase, and no spaces (a lowercased POSIX symbol or the `errno_<N>` form).
    for answer in &answers {
        assert!(
            answer
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
            "the answer {answer:?} must be a bare lowercase errno symbol"
        );
    }

    // The same answers, reached in-crate (no `.so` in the way), so the fixture's route and the direct
    // call cannot drift apart.
    for (error, expected) in CASES {
        // SAFETY: `erl_errno_id` accepts any `int` and answers a NUL-terminated string valid until
        // the next call on this thread.
        let p = unsafe { beam_abi::driver::drv::erl_errno_id(error) };
        assert!(!p.is_null());
        // SAFETY: non-null and NUL-terminated (the documented contract); read immediately.
        let direct = unsafe { CStr::from_ptr(p) }.to_str().expect("ASCII answer");
        assert_eq!(direct, expected, "the direct answer for errno {error}");
    }

    // Tidy up: the driver's `stop` frees the state `start` allocated.
    lib.stop(data).expect("the fixture implements stop");
}

// ---------------------------------------------------------------------------------------------
// Half 2: the same `.so`, driven by the real system ERTS
// ---------------------------------------------------------------------------------------------

/// The Erlang probe: load the driver, open a port, ask for each errno's id in turn and print one
/// `ORACLE n=<errno> id=<answer>` line per request. `REQUESTS` is substituted with the request list
/// (quoted decimal strings) taken from [`CASES`].
///
/// The answer arrives as `{Port,{data,Data}}` — the fixture publishes it through `driver_output`,
/// like any driver reporting to its owner — and `~s` renders it whether the port is in list or
/// binary mode.
const ORACLE_MODULE_SRC: &str = r#"%% The oracle probe for `erl_errno_id/1`, written by the test.
-module(errnoid_probe).
-export([main/1]).

main([Dir]) ->
    case erl_ddll:load_driver(Dir, "errnoid") of
        ok -> ok;
        {error, Reason} ->
            io:format("LOAD_ERROR ~p~n", [Reason]),
            halt(2)
    end,
    Port = open_port({spawn_driver, "errnoid"}, []),
    [run(Port, N) || N <- REQUESTS],
    halt(0).

run(Port, N) ->
    _ = port_control(Port, 1, N),
    receive
        {Port, {data, Data}} -> io:format("ORACLE n=~s id=~s~n", [N, Data])
    after 5000 -> io:format("ORACLE n=~s id=timeout~n", [N])
    end.
"#;

/// Run the probe under the real system ERTS; `None` (after a loud skip) when a tool is missing or
/// the driver cannot be loaded there.
fn oracle_lines(so: &Path) -> Option<Vec<String>> {
    let dir = so.parent()?.to_path_buf();
    let requests = CASES
        .iter()
        .map(|(error, _)| format!("\"{error}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let source = ORACLE_MODULE_SRC.replace("REQUESTS", &format!("[{requests}]"));
    let src = dir.join("errnoid_probe.erl");
    if let Err(err) = std::fs::write(&src, source) {
        skip(&format!("errnoid oracle: cannot write {src:?}: {err}"));
        return None;
    }
    match Command::new("erlc").arg("-o").arg(&dir).arg(&src).output() {
        Err(err) => {
            skip(&format!("errnoid oracle: cannot run erlc ({err})"));
            return None;
        }
        Ok(out) if !out.status.success() => {
            skip(&format!(
                "errnoid oracle: erlc failed (exit {:?}):\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            ));
            return None;
        }
        Ok(_) => {}
    }

    // `-run Mod Func Arg`: the function gets the arguments as a list of strings.
    let run = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-run")
        .arg("errnoid_probe")
        .arg("main")
        .arg(&dir)
        .output();
    let output = match run {
        Err(err) => {
            skip(&format!("errnoid oracle: cannot run erl ({err})"));
            return None;
        }
        Ok(out) => out,
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        skip(&format!(
            "errnoid oracle: erl failed (exit {:?}):\nstdout: {stdout}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    if let Some(reason) = stdout.lines().find(|line| line.starts_with("LOAD_ERROR")) {
        skip(&format!(
            "errnoid oracle: erl_ddll:load_driver/2 refused the fixture ({})",
            reason.trim()
        ));
        return None;
    }
    let lines: Vec<String> = stdout
        .lines()
        .filter(|line| line.starts_with("ORACLE "))
        .map(|line| line.trim_end().to_string())
        .collect();
    if lines.len() != CASES.len() {
        skip(&format!(
            "errnoid oracle: expected {} `ORACLE …` lines, got {}:\n{stdout}",
            CASES.len(),
            lines.len()
        ));
        return None;
    }
    Some(lines)
}

#[test]
fn the_oracle_answers_the_same_errno_strings_from_real_erts() {
    link_the_families();

    let Some(so) = compile_fixture() else {
        return;
    };
    let Some(lines) = oracle_lines(&so) else {
        return;
    };

    // Line by line: the real ERTS answers exactly the strings this crate pins (the same constants the
    // Rust half asserts), so the two runtimes are byte-identical for every case.
    for ((line, (error, expected)), i) in lines.iter().zip(CASES.iter()).zip(0..) {
        let want = format!("ORACLE n={error} id={expected}");
        assert_eq!(
            *line,
            want,
            "the real ERTS's probe line {} must equal the pinned value",
            i + 1
        );
        // Not vacuous: no answer may ever look like `strerror` prose (a space or an uppercase letter
        // would be the old divergence), and none may be a timeout.
        assert!(
            !line.contains("id=timeout"),
            "the oracle answered within the budget for errno {error}: {line:?}"
        );
    }
}
