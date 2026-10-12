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

//! Unit tests for the native `file_server_2` service ([`super`]).
//!
//! The reply-shape and request-decoding helpers are pinned directly; the requests
//! themselves are driven through the **real** seams (`serve_file_message`,
//! `serve_server_request`, `serve_device_request`) over real temporary files, and the
//! write-path answers are cross-checked against the live system `erl` (the
//! `resolve_oracle`/`oracle_render` fixtures below; every such test skips **loudly**
//! when no oracle is installed).

use super::*;

/// A runtime with the boot floor loaded, so the shared atom table holds the names the
/// reply shapes need.
///
/// `pub(super)` so the `get_until` child's tests reuse the one runtime fixture rather
/// than booting a second, subtly different one.
pub(super) fn runtime() -> Runtime {
    let mut runtime = Runtime::with_defaults().expect("a runtime");
    runtime.boot().expect("boot");
    runtime
}

/// Builds a term on `runtime`'s arena through a fresh context.
pub(super) fn build(
    runtime: &mut Runtime,
    f: impl FnOnce(&mut RuntimeContext) -> Eterm,
) -> Eterm {
    let mut ctx = RuntimeContext::new(runtime);
    f(&mut ctx)
}

#[test]
fn the_service_is_registered_under_file_server_2() {
    let runtime = runtime();
    let name = runtime
        .atoms()
        .lookup("file_server_2")
        .expect("the name is interned at construction");
    let pid = runtime
        .processes()
        .whereis(name)
        .expect("the service is registered");
    assert_eq!(pid, runtime.file_services.server());
}

/// The `{error, enoent}` value the boot's `file:open("$HOME/.erlang",[read])` receives.
#[test]
fn the_error_tuple_shape_is_the_oracles() {
    let mut runtime = runtime();
    let reply = runtime.error_tuple("enoent").expect("a term");
    let fields = tuple_fields(reply).expect("a tuple");
    assert_eq!(runtime.atom_name(fields[0]).as_deref(), Some("error"));
    assert_eq!(runtime.atom_name(fields[1]).as_deref(), Some("enoent"));
}

/// `file_io_server:parse_options/1` defaults to `list`; only an explicit `binary` in the
/// mode list selects the binary read mode.
#[test]
fn the_open_mode_selects_the_read_mode() {
    let mut runtime = runtime();
    let list_mode = build(&mut runtime, |ctx| {
        let read = ctx.atom("read").unwrap();
        terms::make_list(ctx, &[read]).unwrap()
    });
    assert!(!runtime.open_is_binary(list_mode));
    let binary_mode = build(&mut runtime, |ctx| {
        let read = ctx.atom("read").unwrap();
        let binary = ctx.atom("binary").unwrap();
        terms::make_list(ctx, &[read, binary]).unwrap()
    });
    assert!(runtime.open_is_binary(binary_mode));
}

#[test]
fn errno_atoms_are_the_oracle_names() {
    let failure = std::io::Error::from_raw_os_error(libc::ENOENT);
    assert_eq!(errno_atom(&failure).unwrap(), "enoent");
    let failure = std::io::Error::from_raw_os_error(libc::EISDIR);
    assert_eq!(errno_atom(&failure).unwrap(), "eisdir");
    let failure = std::io::Error::from_raw_os_error(libc::ENOTEMPTY);
    assert_eq!(errno_atom(&failure).unwrap(), "enotempty");
    // An errno this runtime has no atom for is the typed gap, never an invented atom.
    let failure = std::io::Error::from_raw_os_error(9999);
    assert!(errno_atom(&failure).is_err());
}

#[test]
fn iodata_bytes_flattens_lists_nested_lists_and_binaries() {
    let mut runtime = runtime();
    let term = build(&mut runtime, |ctx| {
        let one = make_small(65);
        let two = make_small(66);
        let inner = terms::make_list(ctx, &[two]).unwrap();
        let binary = terms::make_heap_binary(ctx, b"CD").unwrap();
        terms::make_list(ctx, &[one, inner, binary]).unwrap()
    });
    assert_eq!(iodata_bytes(term).unwrap(), b"ABCD");
    // A code point above 255 has no latin1 byte, so it is not a latin1 payload.
    assert!(iodata_bytes(make_small(300)).is_none());
}

/// The `[alias|Ref]` tag `gen:reply/2` sends to the alias reference is detected as an
/// improper list; a plain `{Pid, Ref}` tag is not.
#[test]
fn the_alias_tag_is_detected_as_an_improper_list() {
    let mut runtime = runtime();
    let alias_tag = build(&mut runtime, |ctx| {
        let alias = ctx.atom("alias").unwrap();
        terms::make_cons(ctx, alias, make_small(0)).unwrap()
    });
    assert!(improper_list_head(alias_tag).is_some());
    let proper_tag = build(&mut runtime, |ctx| {
        let alias = ctx.atom("alias").unwrap();
        let nil_tail = terms::make_list(ctx, &[]).unwrap();
        terms::make_cons(ctx, alias, nil_tail).unwrap()
    });
    assert!(improper_list_head(proper_tag).is_none());
    assert!(improper_list_head(make_small(0)).is_none());
}

/// A `{position, bof}` request seeks to the start.
#[test]
fn seek_from_reads_the_position_arguments() {
    let mut runtime = runtime();
    let at = build(&mut runtime, |ctx| ctx.atom("bof").unwrap());
    assert!(matches!(seek_from(at, &runtime).unwrap(), SeekFrom::Start(0)));
    let at = build(&mut runtime, |ctx| {
        let cur = ctx.atom("cur").unwrap();
        terms::make_tuple(ctx, &[cur, make_small(7)]).unwrap()
    });
    assert!(matches!(seek_from(at, &runtime).unwrap(), SeekFrom::Current(7)));
    // A bare integer is the `bof` shorthand of `file.erl`'s `location()`.
    assert!(matches!(
        seek_from(make_small(3), &runtime).unwrap(),
        SeekFrom::Start(3)
    ));
    // An unknown form is the typed gap.
    let at = build(&mut runtime, |ctx| ctx.atom("nope").unwrap());
    assert!(seek_from(at, &runtime).is_err());
}

#[test]
fn tuple_fields_rejects_a_non_tuple() {
    assert!(tuple_fields(make_small(1)).is_none());
}

// -- the write path (contract A.9) ---------------------------------------------------

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_process::SignalAtoms;

/// A path in the OS temp directory, unique per test and per run.
fn unique_path(label: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "beam_rt_file_server_{label}_{}_{unique}",
        std::process::id()
    ))
}

/// A path that is removed when the guard drops.
struct TempPath(PathBuf);

impl TempPath {
    /// A unique path with any leftover removed first, so a `[write]` open must create it.
    fn new(label: &str) -> TempPath {
        let path = unique_path(label);
        let _ = std::fs::remove_file(&path);
        TempPath(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A process table for the `serve_*` seams, which take the runtime **and** the table
/// apart (the shape a scheduler pass has; the runtime's own table is a private field).
/// The devices map is keyed by pid and only ever read by pid, so a table of its own is
/// enough to allocate the io-device pids these tests then drive.
fn table(runtime: &mut Runtime) -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(runtime.code_server.atoms_mut()))
}

/// Moves the runtime's **own** process table out for a scheduler-pass-shaped drive, exactly as
/// `Runtime::take_processes` does (an equivalent empty placeholder sharing the runtime's atom
/// table goes back in). The `file_server_2` service the runtime registered lives in that table,
/// so this is what lets `Runtime::service_file_requests` scan the service's and its io-devices'
/// real mailboxes — the `table` fixture holds neither. Restore with `runtime.processes = table;`.
fn take_table(runtime: &mut Runtime) -> ProcessTable {
    let placeholder = ProcessTable::new(SignalAtoms::from_table(runtime.code_server.atoms_mut()));
    std::mem::replace(&mut runtime.processes, placeholder)
}

/// The char-list path term for `path`.
fn path_term(runtime: &mut Runtime, path: &Path) -> Eterm {
    let text = path.to_str().expect("a UTF-8 temporary path");
    build(runtime, |ctx| ctx.charlist(text).expect("a char list"))
}

/// A mode list of atoms.
fn mode_list(runtime: &mut Runtime, modes: &[&str]) -> Eterm {
    build(runtime, |ctx| {
        let atoms: Vec<Eterm> = modes
            .iter()
            .map(|mode| ctx.atom(mode).expect("an atom"))
            .collect();
        terms::make_list(ctx, &atoms).expect("a list")
    })
}

/// `{open, Name, Modes}`.
fn open_request(runtime: &mut Runtime, name: Eterm, modes: Eterm) -> Eterm {
    build(runtime, |ctx| {
        let open = ctx.atom("open").expect("an atom");
        terms::make_tuple(ctx, &[open, name, modes]).expect("a tuple")
    })
}

/// `{write_file, Name, Payload}` with `payload` as a binary.
fn write_file_request(runtime: &mut Runtime, name: Eterm, payload: &[u8]) -> Eterm {
    build(runtime, |ctx| {
        let write_file = ctx.atom("write_file").expect("an atom");
        let payload = terms::make_heap_binary(ctx, payload).expect("a binary");
        terms::make_tuple(ctx, &[write_file, name, payload]).expect("a tuple")
    })
}

/// The `~w` rendering of `term`, without the printer's trailing newline.
fn render(runtime: &mut Runtime, term: Eterm) -> String {
    let rendered = runtime.format_term_w(term, None).expect("a rendered term");
    rendered
        .strip_suffix('\n')
        .map(str::to_owned)
        .unwrap_or(rendered)
}

/// Drives one `{'$gen_call', {From, Tag}, Request}` through the real service seam
/// (`serve_file_message`, the `gen:call/4` wire `file.erl`'s `call/2` produces) and
/// returns the reply **value**.
fn call_service(runtime: &mut Runtime, table: &mut ProcessTable, request: Eterm) -> Eterm {
    let service = runtime.file_services.server();
    let caller = runtime.self_pid();
    let payload = build(runtime, |ctx| {
        let tag = ctx.atom("$gen_call").expect("an atom");
        let from =
            terms::make_tuple(ctx, &[caller.to_eterm(), make_small(1)]).expect("a tuple");
        terms::make_tuple(ctx, &[tag, from, request]).expect("a tuple")
    });
    let (_, envelope) = runtime
        .serve_file_message(table, service, payload)
        .expect("a served request")
        .expect("a `$gen_call` is always answered");
    terms::tuple_element(envelope, 1).expect("the reply element")
}

/// The io-device pid of an `{ok, IoPid}` reply.
fn device_of(runtime: &mut Runtime, reply: Eterm) -> ProcessId {
    let fields = tuple_fields(reply).expect("an `{ok, IoPid}` tuple");
    assert_eq!(
        runtime.atom_name(fields[0]).as_deref(),
        Some("ok"),
        "expected a live io device, got {}",
        render(runtime, reply)
    );
    ProcessId::from_eterm(fields[1]).expect("the device pid")
}

/// Serves one raw device request (`{put_chars,latin1,Bin}` / `close`) through the real
/// `serve_device_request` seam.
fn device_request(runtime: &mut Runtime, device: ProcessId, request: Eterm) -> (Eterm, bool) {
    runtime
        .serve_device_request(device, request)
        .expect("a served device request")
}

/// `{put_chars, latin1, Payload}` — the io request `file:write/2` sends
/// (`file.erl:1797-1803`'s `io:request(File, {put_chars,latin1,Bin})`).
fn put_chars_request(runtime: &mut Runtime, payload: Eterm) -> Eterm {
    build(runtime, |ctx| {
        let put_chars = ctx.atom("put_chars").expect("an atom");
        let latin1 = ctx.atom("latin1").expect("an atom");
        terms::make_tuple(ctx, &[put_chars, latin1, payload]).expect("a tuple")
    })
}

/// `{write_file, Name, Bin}` is `?PRIM_FILE:write_file(Name, Bin)`
/// (`file_server.erl:111-112`), the body of `file:write_file/2`
/// (`file.erl:1193-1194`): it answers `ok` and really writes the bytes.
#[test]
fn write_file_creates_the_path_and_writes_the_bytes() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("write_file");
    let name = path_term(&mut runtime, file.path());
    let request = write_file_request(&mut runtime, name, b"hello");
    let reply = runtime
        .serve_server_request(&mut table, request)
        .expect("a served request")
        .expect("the oracle answers `{reply, …}`");
    assert_eq!(render(&mut runtime, reply), "ok");
    assert_eq!(
        std::fs::read(file.path()).expect("the written file"),
        b"hello"
    );
}

/// The oracle's failure vocabulary for the same arm: an atom path and a non-iodata
/// payload are the **value** `{error, badarg}`, and a directory is the `open(2)` errno
/// `{error, eisdir}` — never a fabricated `ok`.
#[test]
fn write_file_reports_badarg_and_eisdir() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);

    // A code point above 255 is not `iodata`.
    let file = TempPath::new("write_file_bad_payload");
    let bad = build(&mut runtime, |ctx| {
        let write_file = ctx.atom("write_file").unwrap();
        let name = ctx.charlist(file.path().to_str().unwrap()).unwrap();
        terms::make_tuple(ctx, &[write_file, name, make_small(300)]).unwrap()
    });
    let reply = runtime
        .serve_server_request(&mut table, bad)
        .expect("a served request")
        .expect("a reply");
    assert_eq!(render(&mut runtime, reply), "{error,badarg}");

    // An atom is not a path (`prim_file:encode_path/1` rejects it).
    let atom = build(&mut runtime, |ctx| ctx.atom("nope").unwrap());
    let request = write_file_request(&mut runtime, atom, b"x");
    let reply = runtime
        .serve_server_request(&mut table, request)
        .expect("a served request")
        .expect("a reply");
    assert_eq!(render(&mut runtime, reply), "{error,badarg}");

    // A directory is `EISDIR` — `open(2)`'s own answer for the write flags.
    let dir = unique_path("write_file_dir");
    std::fs::create_dir_all(&dir).expect("a private temporary directory");
    let name = path_term(&mut runtime, &dir);
    let request = write_file_request(&mut runtime, name, b"x");
    let reply = runtime
        .serve_server_request(&mut table, request)
        .expect("a served request")
        .expect("a reply");
    assert_eq!(render(&mut runtime, reply), "{error,eisdir}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The full `file:open/2` + `file:write/2` + `file:close/1` round trip, through the
/// **service** seam for the open and the **device** seam for the write and the close:
/// the bytes land on disk and the close retires the device.
#[test]
fn an_open_write_round_trip_lands_on_disk() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("round_trip");
    let name = path_term(&mut runtime, file.path());
    let modes = mode_list(&mut runtime, &["write"]);
    let request = open_request(&mut runtime, name, modes);
    let reply = call_service(&mut runtime, &mut table, request);
    let device = device_of(&mut runtime, reply);

    let payload = build(&mut runtime, |ctx| {
        terms::make_heap_binary(ctx, b"round trip").unwrap()
    });
    let request = put_chars_request(&mut runtime, payload);
    let (reply, retire) = device_request(&mut runtime, device, request);
    assert_eq!(render(&mut runtime, reply), "ok");
    assert!(!retire, "a `put_chars` never retires the device");

    let close = build(&mut runtime, |ctx| ctx.atom("close").unwrap());
    let (reply, retire) = device_request(&mut runtime, device, close);
    assert_eq!(render(&mut runtime, reply), "ok");
    assert!(retire, "`file:close/1` retires the device");

    assert_eq!(
        std::fs::read(file.path()).expect("the written file"),
        b"round trip"
    );
}

/// A write-mode open now **creates** a missing path — the flag derivation the floor's
/// `prim_file:open/2` owns (`O_WRONLY|O_CREAT|O_TRUNC`, `unix_prim_file.c:152-166`) —
/// while a read-mode open on a missing path is still the oracle's `{error, enoent}`.
#[test]
fn a_write_open_creates_and_a_read_open_reports_enoent() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);

    let file = TempPath::new("write_creates");
    assert!(!file.path().exists(), "the fixture path must be absent");
    let name = path_term(&mut runtime, file.path());
    let modes = mode_list(&mut runtime, &["write"]);
    let request = open_request(&mut runtime, name, modes);
    let reply = call_service(&mut runtime, &mut table, request);
    let _ = device_of(&mut runtime, reply);
    assert!(
        file.path().exists(),
        "a `[write]` open creates the path under both runtimes"
    );

    let missing = TempPath::new("read_absent");
    let name = path_term(&mut runtime, missing.path());
    let modes = mode_list(&mut runtime, &["read"]);
    let request = open_request(&mut runtime, name, modes);
    let reply = call_service(&mut runtime, &mut table, request);
    assert_eq!(render(&mut runtime, reply), "{error,enoent}");
}

// -- retirement: a request to a retired device is answered, never parked --------------------

/// (a)+(b)+(c) A request queued on a **retired** io-device is drained and answered
/// `{io_reply, _, {error, terminated}}`, is counted in the pass's `delivered`, and the retired
/// pid is then **retained** in drain coverage — for the runtime's lifetime, because nothing can
/// know that no further request will ever arrive (see the sibling
/// `a_request_to_a_retired_device_in_a_later_pass_is_answered`, which pins exactly that).
///
/// The oracle's io-device is a real process: once it dies, the caller's monitor fires `'DOWN'
/// noproc` and `io:request/3` answers `{error, terminated}` — the request never reaches a live
/// device. This native device **outlives** its retire (there is no `'DOWN'` to synthesise), so
/// the service answers the queued request directly with the same `{error, terminated}` the
/// oracle's dead device produces, instead of parking the sender forever. The open, the retire
/// and the drain are all driven through the **real** seams; the request is queued the way a
/// scheduler pass would find it (`deliver_simple`, the same copy path the replies use).
#[test]
fn a_request_to_a_retired_device_is_drained_answered_and_the_pid_is_retained() {
    let mut runtime = runtime();
    let mut table = take_table(&mut runtime);
    let caller = table.spawn_default().expect("a caller");

    // A real `[read]` device over a real file, opened through the service seam.
    let file = TempPath::new("retired_device");
    std::fs::write(file.path(), b"x").expect("a fixture file");
    let modes = mode_list(&mut runtime, &["read"]);
    let device = open_device(&mut runtime, &mut table, file.path(), modes);

    // (c) retire it through the real seam: an io-request `close` retires the device.
    let close_request = build(&mut runtime, |ctx| {
        let tag = ctx.atom("io_request").expect("an atom");
        let close = ctx.atom("close").expect("an atom");
        terms::make_tuple(ctx, &[tag, caller.to_eterm(), make_small(7), close])
            .expect("the close request")
    });
    let (target, envelope) = runtime
        .serve_file_message(&mut table, device, close_request)
        .expect("a served close")
        .expect("an io_request is always answered");
    assert_eq!(target, caller);
    assert_eq!(render(&mut runtime, envelope), "{io_reply,7,ok}");
    assert!(
        runtime.file_services.retired.contains(&device),
        "a `close` retires the device"
    );
    assert!(
        !runtime.file_services.devices.contains_key(&device),
        "a retired device is no longer live"
    );

    // (a) queue a fresh, well-formed request on the now-retired device's mailbox.
    let queued = build(&mut runtime, |ctx| {
        let tag = ctx.atom("io_request").expect("an atom");
        let close = ctx.atom("close").expect("an atom");
        terms::make_tuple(ctx, &[tag, caller.to_eterm(), make_small(11), close])
            .expect("the queued request")
    });
    table
        .deliver_simple(device, caller.to_eterm(), queued)
        .expect("the retired device's mailbox accepts the message");

    let delivered = runtime
        .service_file_requests(&mut table)
        .expect("a service pass");
    assert_eq!(
        delivered, 1,
        "the retired device's request is answered and counted"
    );

    let reply = table
        .get_mut(caller)
        .expect("the caller is live")
        .receive(|_| true)
        .expect("the retired device's reply")
        .payload();
    assert_eq!(
        render(&mut runtime, reply),
        "{io_reply,11,{error,terminated}}"
    );

    // (b) the retired pid — its mailbox now empty — stays in drain coverage: nothing can know
    // that no further request will arrive, so it must not be dropped.
    assert!(
        runtime.file_services.retired.contains(&device),
        "a drained retired pid stays in the drain coverage"
    );
    // and a pass over that empty mailbox answers nothing else.
    assert_eq!(
        runtime
            .service_file_requests(&mut table)
            .expect("a second service pass"),
        0,
        "the retired device has nothing left to answer"
    );

    runtime.processes = table;
}

/// **The decisive case.** A request that reaches a retired io-device in a **later** pass —
/// after the pass that retired it has already drained that device's mailbox to empty — is
/// still answered `{error, terminated}` and counted.
///
/// This is the case a "prune the tombstone once the mailbox is empty" rule misses: the pid is
/// gone from the scan set, so the later request sits in its mailbox with no drain step and its
/// sender waits in `file_request/2`'s untimed `receive`
/// (`lib/kernel/src/file.erl:3068-3077`), so the run quiesces to the typed `blocked()` gap.
/// The observable symptom on the CLI is a `run` process that prints nothing and exits 3 with
/// `unsupported: opcode: run: process blocked (no message available)`, where the oracle prints
/// `close=ok after={error,terminated}`. Retaining the retired pid for the runtime's lifetime
/// answers it instead.
#[test]
fn a_request_to_a_retired_device_in_a_later_pass_is_answered() {
    let mut runtime = runtime();
    let mut table = take_table(&mut runtime);
    let caller = table.spawn_default().expect("a caller");

    // A real `[read]` device over a real file, retired through the real io-request `close`.
    let file = TempPath::new("retired_later_pass");
    std::fs::write(file.path(), b"x").expect("a fixture file");
    let modes = mode_list(&mut runtime, &["read"]);
    let device = open_device(&mut runtime, &mut table, file.path(), modes);

    let close_request = build(&mut runtime, |ctx| {
        let tag = ctx.atom("io_request").expect("an atom");
        let close = ctx.atom("close").expect("an atom");
        terms::make_tuple(ctx, &[tag, caller.to_eterm(), make_small(7), close])
            .expect("the close request")
    });
    let (target, envelope) = runtime
        .serve_file_message(&mut table, device, close_request)
        .expect("a served close")
        .expect("an io_request is always answered");
    assert_eq!(target, caller);
    assert_eq!(render(&mut runtime, envelope), "{io_reply,7,ok}");

    // The pass that retired the device finds nothing else in `device`'s mailbox (the close was
    // served directly, not queued), so it answers **no** request for it.
    assert_eq!(
        runtime
            .service_file_requests(&mut table)
            .expect("the retiring pass"),
        0,
        "the retiring pass has nothing left to answer for the device"
    );
    assert!(
        runtime.file_services.retired.contains(&device),
        "the retired pid is still in the drain coverage after that pass"
    );

    // The later request: delivered into the retired pid's mailbox only now, i.e. after the
    // retiring pass already saw an empty mailbox (the point a `prune` rule would have dropped
    // the pid from coverage).
    let later = build(&mut runtime, |ctx| {
        let tag = ctx.atom("io_request").expect("an atom");
        let close = ctx.atom("close").expect("an atom");
        terms::make_tuple(ctx, &[tag, caller.to_eterm(), make_small(13), close])
            .expect("the later request")
    });
    table
        .deliver_simple(device, caller.to_eterm(), later)
        .expect("the retired device's mailbox accepts the message");

    assert_eq!(
        runtime
            .service_file_requests(&mut table)
            .expect("a second service pass"),
        1,
        "the later request is answered and counted"
    );

    let reply = table
        .get_mut(caller)
        .expect("the caller is live")
        .receive(|_| true)
        .expect("the later request's reply")
        .payload();
    assert_eq!(
        render(&mut runtime, reply),
        "{io_reply,13,{error,terminated}}"
    );

    runtime.processes = table;
}

// -- a `$gen_call` the service does not serve is answered, never parked ---------------------

/// (d) A well-formed `{'$gen_call', …}` whose request the service does **not** serve is answered
/// with the typed `{error, unsupported}` — the DIRECTED divergence from the oracle.
///
/// The oracle's `handle_call(Request, From, State) -> {noreply, State}` catch-all
/// (`lib/kernel/src/file_server.erl:197-201`) replies nothing, so the caller of a well-formed
/// `gen:call/4` parks forever in its untimed `receive`. This runtime never parks a caller
/// silently: an unserved request is answered `{error, unsupported}` instead. The request here is
/// a **bare atom** the service does not serve, which is `serve_server_request`'s `Ok(None)`
/// (`self.atom_name(request)` is not `get_cwd`, and a non-tuple has no command head). It is
/// deliberately NOT a tuple like `{bogus, 1}`: a tuple whose head is an atom reaches
/// `serve_server_request`'s catch-all `Err(unsupported)`, which propagates as an error rather
/// than an `Ok(None)` reply — a different path with a different outcome.
#[test]
fn a_gen_call_the_service_does_not_serve_is_answered_unsupported() {
    let mut runtime = runtime();
    let mut table = take_table(&mut runtime);
    let caller = table.spawn_default().expect("a caller");

    let request = build(&mut runtime, |ctx| ctx.atom("bogus").expect("an atom"));
    let payload = build(&mut runtime, |ctx| {
        let tag = ctx.atom("$gen_call").expect("an atom");
        let from = terms::make_tuple(ctx, &[caller.to_eterm(), make_small(3)]).expect("a tuple");
        terms::make_tuple(ctx, &[tag, from, request]).expect("the `$gen_call`")
    });
    let service = runtime.file_services.server();
    table
        .deliver_simple(service, caller.to_eterm(), payload)
        .expect("the service's mailbox accepts the message");

    assert_eq!(
        runtime
            .service_file_requests(&mut table)
            .expect("a service pass"),
        1,
        "the unanswered `$gen_call` is answered and counted"
    );

    let reply = table
        .get_mut(caller)
        .expect("the caller is live")
        .receive(|_| true)
        .expect("the answer")
        .payload();
    assert_eq!(render(&mut runtime, reply), "{3,{error,unsupported}}");

    runtime.processes = table;
}

/// A `valid_enc/1`-rejected encoding is the caller's `{error, badarg}`
/// (`file_io_server.erl:159-178`) and is decided **before** the open, so it never
/// creates — or truncates — the path.
#[test]
fn an_invalid_encoding_is_rejected_before_the_open() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("bad_encoding");
    let name = path_term(&mut runtime, file.path());
    let modes = build(&mut runtime, |ctx| {
        let key = ctx.atom("encoding").unwrap();
        let value = ctx.atom("bogus").unwrap();
        let pair = terms::make_tuple(ctx, &[key, value]).unwrap();
        terms::make_list(ctx, &[pair]).unwrap()
    });
    let request = open_request(&mut runtime, name, modes);
    let reply = call_service(&mut runtime, &mut table, request);
    assert_eq!(render(&mut runtime, reply), "{error,badarg}");
    assert!(
        !file.path().exists(),
        "the rejected encoding never reaches `open(2)`"
    );
    // The accepted forms are not rejected (`valid_enc/1`'s whole table).
    for modes in [&["latin1"][..], &["unicode"], &["utf8"]] {
        let modes = mode_list(&mut runtime, modes);
        // A rejected encoding would be `badarg` here; these must reach `open(2)` and so
        // report the absent path instead.
        let file = TempPath::new("valid_encoding");
        let name = path_term(&mut runtime, file.path());
        let request = open_request(&mut runtime, name, modes);
        let reply = call_service(&mut runtime, &mut table, request);
        assert_eq!(render(&mut runtime, reply), "{error,enoent}");
    }
}

/// The 2-arity `read_file_info`/`read_link_info` forms (`file_server.erl:144-145`,
/// `:159-160`) reach the same `read_info_1/3`, whose `Opts` is read from `args.get(1)`.
#[test]
fn the_read_file_info_opt_form_reaches_the_same_reader() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let missing = TempPath::new("info_opts");
    let empty = build(&mut runtime, |ctx| terms::make_list(ctx, &[]).unwrap());
    for command in ["read_file_info", "read_link_info"] {
        let name = path_term(&mut runtime, missing.path());
        let request = build(&mut runtime, |ctx| {
            let command = ctx.atom(command).unwrap();
            terms::make_tuple(ctx, &[command, name, empty]).unwrap()
        });
        let reply = runtime
            .serve_server_request(&mut table, request)
            .expect("a served request")
            .expect("a reply");
        assert_eq!(
            render(&mut runtime, reply),
            "{error,enoent}",
            "`{command}/2`"
        );
    }
}

// -- the mutable read-mode/encoding surface (`getopts`/`setopts`) ---------------------

/// The bare-atom `getopts` request `io:getopts/1` sends (`io.erl:489-490`,
/// `file_io_server.erl:399-401`).
fn getopts_request(runtime: &mut Runtime) -> Eterm {
    build(runtime, |ctx| ctx.atom("getopts").expect("an atom"))
}

/// The `{setopts, Opts}` request `io:setopts/2` sends (`io.erl:654-655`,
/// `file_io_server.erl:395-397`).
fn setopts_request(runtime: &mut Runtime, opts: Eterm) -> Eterm {
    build(runtime, |ctx| {
        let setopts = ctx.atom("setopts").expect("an atom");
        terms::make_tuple(ctx, &[setopts, opts]).expect("a tuple")
    })
}

/// An `Opts` list of bare atoms.
fn opts_atoms(runtime: &mut Runtime, names: &[&str]) -> Eterm {
    build(runtime, |ctx| {
        let elements: Vec<Eterm> = names
            .iter()
            .map(|name| ctx.atom(name).expect("an atom"))
            .collect();
        terms::make_list(ctx, &elements).expect("a list")
    })
}

/// The `{Key, Value}` pair a mode option is written as.
fn opts_pair(runtime: &mut Runtime, key: &str, value: &str) -> Eterm {
    build(runtime, |ctx| {
        let key = ctx.atom(key).expect("an atom");
        let value = ctx.atom(value).expect("an atom");
        terms::make_tuple(ctx, &[key, value]).expect("a tuple")
    })
}

/// An `Opts` list of already-built elements.
fn opts_list(runtime: &mut Runtime, elements: &[Eterm]) -> Eterm {
    build(runtime, |ctx| terms::make_list(ctx, elements).expect("a list"))
}

/// Opens `path` with `modes` through the **service** seam (`{open,Name,ModeList}`) and
/// returns the live io-device pid.
fn open_device(
    runtime: &mut Runtime,
    table: &mut ProcessTable,
    path: &Path,
    modes: Eterm,
) -> ProcessId {
    let name = path_term(runtime, path);
    let request = open_request(runtime, name, modes);
    let reply = call_service(runtime, table, request);
    device_of(runtime, reply)
}

/// Drives one device request and renders the reply value.
fn device_render(runtime: &mut Runtime, device: ProcessId, request: Eterm) -> String {
    let (reply, _) = device_request(runtime, device, request);
    render(runtime, reply)
}

/// `getopts` answers the **live** read mode and stream encoding
/// (`file_io_server.erl:752-755`): `list`/`latin1` by default, `binary` and `unicode` when
/// the open mode list selected them.
#[test]
fn getopts_reports_the_open_modes() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("getopts_open");
    std::fs::write(file.path(), b"x").expect("a fixture file");

    // `[]` — `file_io_server:parse_options/1`'s defaults (`:131-148`).
    let empty = build(&mut runtime, |ctx| terms::make_list(ctx, &[]).expect("a list"));
    let device = open_device(&mut runtime, &mut table, file.path(), empty);
    let request = getopts_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[{binary,false},{encoding,latin1}]"
    );

    // `[binary]`.
    let modes = mode_list(&mut runtime, &["read", "binary"]);
    let device = open_device(&mut runtime, &mut table, file.path(), modes);
    let request = getopts_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[{binary,true},{encoding,latin1}]"
    );

    // `[{encoding,unicode}]`.
    let option = opts_pair(&mut runtime, "encoding", "unicode");
    let modes = opts_list(&mut runtime, &[option]);
    let device = open_device(&mut runtime, &mut table, file.path(), modes);
    let request = getopts_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[{binary,false},{encoding,unicode}]"
    );
}

/// `setopts` applies the read mode and the encoding, and the next `getopts` observes them
/// (`file_io_server.erl:737-750`).
#[test]
fn setopts_updates_the_mode_and_encoding() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("setopts_update");
    std::fs::write(file.path(), b"x").expect("a fixture file");
    let modes = mode_list(&mut runtime, &["read"]);
    let device = open_device(&mut runtime, &mut table, file.path(), modes);

    // `[binary,{encoding,utf8}]` — `utf8` is `valid_enc/1`'s `{ok,unicode}`.
    let binary = build(&mut runtime, |ctx| ctx.atom("binary").expect("an atom"));
    let encoding = opts_pair(&mut runtime, "encoding", "utf8");
    let opts = opts_list(&mut runtime, &[binary, encoding]);
    let request = setopts_request(&mut runtime, opts);
    assert_eq!(device_render(&mut runtime, device, request), "ok");

    let request = getopts_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[{binary,true},{encoding,unicode}]"
    );

    // `[list]` — `substitute_negations([{list,binary}], _)` turns it into `{binary,false}`.
    let opts = opts_atoms(&mut runtime, &["list"]);
    let request = setopts_request(&mut runtime, opts);
    assert_eq!(device_render(&mut runtime, device, request), "ok");
    let request = getopts_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[{binary,false},{encoding,unicode}]"
    );

    // `[{binary,false}]` — the explicit form of the same negation.
    let pair = opts_pair(&mut runtime, "binary", "false");
    let opts = opts_list(&mut runtime, &[pair]);
    let request = setopts_request(&mut runtime, opts);
    assert_eq!(device_render(&mut runtime, device, request), "ok");

    // `[]` — `do_setopts/2`'s `undefined` clause leaves both fields alone.
    let opts = build(&mut runtime, |ctx| terms::make_list(ctx, &[]).expect("a list"));
    let request = setopts_request(&mut runtime, opts);
    assert_eq!(device_render(&mut runtime, device, request), "ok");
    let request = getopts_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[{binary,false},{encoding,unicode}]"
    );

    // `[{list,true}]` is the negation form: it selects `list` (the substitution is the
    // negation, `proplists.erl:438-459`) — verified against the oracle.
    let pair = opts_pair(&mut runtime, "list", "true");
    let opts = opts_list(&mut runtime, &[pair]);
    let request = setopts_request(&mut runtime, opts);
    assert_eq!(device_render(&mut runtime, device, request), "ok");
    let request = getopts_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[{binary,false},{encoding,unicode}]"
    );

    // ... and `{list,false}` selects `binary`.
    let pair = opts_pair(&mut runtime, "list", "false");
    let opts = opts_list(&mut runtime, &[pair]);
    let request = setopts_request(&mut runtime, opts);
    assert_eq!(device_render(&mut runtime, device, request), "ok");
    let request = getopts_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[{binary,true},{encoding,unicode}]"
    );
}

/// The two failure shapes the oracle **returns** (`file_io_server.erl:729-736`,
/// `:159-178`): a `check_valid_opts/1`-rejected element is `{error, enotsup}`, and a
/// `valid_enc/1`-rejected encoding is `{error, badarg}`.
#[test]
fn setopts_reports_enotsup_and_badarg() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("setopts_errors");
    std::fs::write(file.path(), b"x").expect("a fixture file");
    let modes = mode_list(&mut runtime, &["read"]);
    let device = open_device(&mut runtime, &mut table, file.path(), modes);

    // A bare unknown atom becomes `{bogus,true}` (`unfold/1`) ⇒ `check_valid_opts` false.
    let opts = opts_atoms(&mut runtime, &["bogus"]);
    let request = setopts_request(&mut runtime, opts);
    assert_eq!(device_render(&mut runtime, device, request), "{error,enotsup}");

    // A valid pair naming an encoding `valid_enc/1` rejects.
    let pair = opts_pair(&mut runtime, "encoding", "bogus");
    let opts = opts_list(&mut runtime, &[pair]);
    let request = setopts_request(&mut runtime, opts);
    assert_eq!(device_render(&mut runtime, device, request), "{error,badarg}");

    // The failures left the device untouched.
    let request = getopts_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[{binary,false},{encoding,latin1}]"
    );
}

/// A `valid_enc/1`-accepted but unmodelled encoding (`utf16`/`utf32`, a 2/4-byte stream) is
/// the typed capability gap — the oracle answers `ok`, this runtime must not fabricate one.
#[test]
fn setopts_on_a_utf16_encoding_is_the_typed_gap() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("setopts_utf16");
    std::fs::write(file.path(), b"x").expect("a fixture file");
    let modes = mode_list(&mut runtime, &["read"]);
    let device = open_device(&mut runtime, &mut table, file.path(), modes);

    for (key, value) in [
        ("encoding", "utf16"),
        ("encoding", "utf32"),
    ] {
        let pair = opts_pair(&mut runtime, key, value);
        let opts = opts_list(&mut runtime, &[pair]);
        let request = setopts_request(&mut runtime, opts);
        let err = runtime
            .serve_device_request(device, request)
            .expect_err("the unmodelled encoding is a typed capability gap");
        let reason = crate::error::unsupported_reason(&err).expect("an unsupported reason");
        assert!(
            reason.contains("utf16"),
            "expected the utf16/utf32 gap, got `{reason}`"
        );
    }
}

/// A non-list `Opts` never reaches `setopts/2`: it is the `io_request/2` catch-all's
/// `{error, {request, {setopts, Opts}}}` (`file_io_server.erl:418-421`).
#[test]
fn setopts_on_a_non_list_is_the_request_error() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("setopts_non_list");
    std::fs::write(file.path(), b"x").expect("a fixture file");
    let modes = mode_list(&mut runtime, &["read"]);
    let device = open_device(&mut runtime, &mut table, file.path(), modes);

    let opts = build(&mut runtime, |ctx| ctx.atom("foo").expect("an atom"));
    let request = setopts_request(&mut runtime, opts);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "{error,{request,{setopts,foo}}}"
    );
}

/// A `{setopts, …}` encoding change is visible to the **readers** on the same device — the
/// `{get_line, …}`/`{get_until, …}` family reads `device.encoding` (`get_line.rs`), which is
/// what makes `epp`'s `io:setopts`/`io:getopts` pair meaningful.
#[test]
fn setopts_changes_what_the_next_read_decodes() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let contents = b"abc\xc3\xa5def\n";
    let latin1_file = TempPath::new("setopts_read_latin1");
    std::fs::write(latin1_file.path(), contents).expect("a fixture file");
    let unicode_file = TempPath::new("setopts_read_unicode");
    std::fs::write(unicode_file.path(), contents).expect("a fixture file");

    // `{get_line, latin1, Prompt}` — the request `file:read_line/1` sends.
    let get_line_request = |runtime: &mut Runtime| {
        build(runtime, |ctx| {
            let get_line = ctx.atom("get_line").expect("an atom");
            let latin1 = ctx.atom("latin1").expect("an atom");
            let prompt = ctx.atom("").expect("an atom");
            terms::make_tuple(ctx, &[get_line, latin1, prompt]).expect("a tuple")
        })
    };

    // Default `latin1` stream: the raw UTF-8 bytes come back as code points. The wire reply
    // for `{get_line, …}` is the **bare** line — `file:read_line/1` is what wraps it in
    // `{ok, Data}` (`file.erl:1695-1701`).
    let modes = mode_list(&mut runtime, &["read"]);
    let device = open_device(&mut runtime, &mut table, latin1_file.path(), modes);
    let request = get_line_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[97,98,99,195,165,100,101,102,10]"
    );

    // The same file after `setopts(F, [{encoding,unicode}])`: the pairs decode.
    let modes = mode_list(&mut runtime, &["read"]);
    let device = open_device(&mut runtime, &mut table, unicode_file.path(), modes);
    let pair = opts_pair(&mut runtime, "encoding", "unicode");
    let opts = opts_list(&mut runtime, &[pair]);
    let request = setopts_request(&mut runtime, opts);
    assert_eq!(device_render(&mut runtime, device, request), "ok");
    let request = get_line_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[97,98,99,229,100,101,102,10]"
    );
}

// -- the shared read buffer (`State#state.buf`) ---------------------------------------

/// The `{get_chars, latin1, '', N}` io request `file:read/2` sends (`file.erl:1634`).
fn get_chars_request(runtime: &mut Runtime, n: i64) -> Eterm {
    build(runtime, |ctx| {
        let get_chars = ctx.atom("get_chars").expect("an atom");
        let latin1 = ctx.atom("latin1").expect("an atom");
        let prompt = ctx.atom("").expect("the empty atom");
        terms::make_tuple(ctx, &[get_chars, latin1, prompt, make_small(n)]).expect("a tuple")
    })
}

/// The `{get_line, latin1, ''}` io request `file:read_line/1` sends
/// (`file.erl:1695-1696`).
fn get_line_request(runtime: &mut Runtime) -> Eterm {
    build(runtime, |ctx| {
        let get_line = ctx.atom("get_line").expect("an atom");
        let latin1 = ctx.atom("latin1").expect("an atom");
        let prompt = ctx.atom("").expect("the empty atom");
        terms::make_tuple(ctx, &[get_line, latin1, prompt]).expect("a tuple")
    })
}

/// The `{position, At}` file request `file:position/2` sends (`file.erl:1954-1955`).
fn position_request(runtime: &mut Runtime, at: Eterm) -> Eterm {
    build(runtime, |ctx| {
        let position = ctx.atom("position").expect("an atom");
        terms::make_tuple(ctx, &[position, at]).expect("a tuple")
    })
}

/// The bare `cur` location of `file.erl`'s `location()`.
fn cur_atom(runtime: &mut Runtime) -> Eterm {
    build(runtime, |ctx| ctx.atom("cur").expect("an atom"))
}

/// A `{cur, Offs}` location.
fn cur_offset(runtime: &mut Runtime, offset: i64) -> Eterm {
    build(runtime, |ctx| {
        let cur = ctx.atom("cur").expect("an atom");
        terms::make_tuple(ctx, &[cur, make_small(offset)]).expect("a tuple")
    })
}

/// The `{get_chars, latin1, '', N}` reads on one handle must not lose the bytes the
/// `{get_line, …}` in between read ahead. File `<<"line1\nline2\nrest">>` opened `[read]`;
/// the live oracle (`erl -noshell -eval … -s init stop`) answers
///
/// ```text
/// read(6)/read_line/read(4) -> {ok,"line1\n"} {ok,"line2\n"} {ok,"rest"}
/// ```
///
/// The bare device replies asserted below are the same three values the `file:*` wrappers
/// box (the third one is the regression: it is the bytes the `{get_line, …}` had stashed,
/// which a buffer-blind `get_chars` answered `eof` for).
#[test]
fn get_chars_serves_the_buffer_a_get_line_stashed() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("stashed_list");
    std::fs::write(file.path(), b"line1\nline2\nrest").expect("a fixture file");
    let modes = mode_list(&mut runtime, &["read"]);
    let device = open_device(&mut runtime, &mut table, file.path(), modes);

    let request = get_chars_request(&mut runtime, 6);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[108,105,110,101,49,10]"
    );
    let request = get_line_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[108,105,110,101,50,10]"
    );
    let request = get_chars_request(&mut runtime, 4);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[114,101,115,116]"
    );
}

/// The same mixed sequence on a `[read,binary]` device: the oracle answers
///
/// ```text
/// {ok,<<"line1\n">>} {ok,<<"line2\n">>} {ok,<<"rest">>}
/// ```
///
/// A `0`-count read on the same device is the empty binary — `cast(<<>>, binary, latin1,
/// latin1)` (`file_io_server.erl:507`, `:784-785`).
#[test]
fn get_chars_serves_the_stashed_buffer_in_binary_mode() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("stashed_binary");
    std::fs::write(file.path(), b"line1\nline2\nrest").expect("a fixture file");
    let modes = mode_list(&mut runtime, &["read", "binary"]);
    let device = open_device(&mut runtime, &mut table, file.path(), modes);

    let request = get_chars_request(&mut runtime, 6);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "<<108,105,110,101,49,10>>"
    );
    let request = get_line_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "<<108,105,110,101,50,10>>"
    );
    let request = get_chars_request(&mut runtime, 0);
    assert_eq!(device_render(&mut runtime, device, request), "<<>>");
    let request = get_chars_request(&mut runtime, 4);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "<<114,101,115,116>>"
    );
}

/// `{position, cur}` after a `{get_line, …}` answers the **logical** position, not the
/// device's physical offset: the read-ahead left 10 bytes in the buffer, so the oracle's
/// `position/3` (`file_io_server.erl:970-977`) subtracts them. File
/// `<<"line1\nline2\nrest">>`, the live oracle answers
///
/// ```text
/// read_line/position(cur)/read(4)       -> {ok,"line1\n"} {ok,6} {ok,"line"}
/// read_line/position({cur,2})/read(4)   -> {ok,"line1\n"} {ok,8} {ok,"ne2\n"}
/// ```
///
/// and the seek clears the stashed buffer (`std_reply/2`, `file_io_server.erl:344-348`), so
/// the following read comes from the logical position.
#[test]
fn position_cur_compensates_for_the_stashed_buffer() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("position_cur");
    std::fs::write(file.path(), b"line1\nline2\nrest").expect("a fixture file");
    let modes = mode_list(&mut runtime, &["read"]);

    // `cur`: 6 - 10 = the physical 16 - 10.
    let device = open_device(&mut runtime, &mut table, file.path(), modes.clone());
    let request = get_line_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[108,105,110,101,49,10]"
    );
    let at = cur_atom(&mut runtime);
    let request = position_request(&mut runtime, at);
    assert_eq!(device_render(&mut runtime, device, request), "{ok,6}");
    let request = get_chars_request(&mut runtime, 4);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[108,105,110,101]"
    );

    // `{cur, Offs}`: 6 + 2 = the physical 16 - 10 + 2.
    let device = open_device(&mut runtime, &mut table, file.path(), modes);
    let request = get_line_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[108,105,110,101,49,10]"
    );
    let at = cur_offset(&mut runtime, 2);
    let request = position_request(&mut runtime, at);
    assert_eq!(device_render(&mut runtime, device, request), "{ok,8}");
    let request = get_chars_request(&mut runtime, 4);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[110,101,50,10]"
    );
}

/// A `0`-count read answers the empty cast and leaves the stashed leftover **untouched**
/// (`get_chars(0, Enc, State)`, `file_io_server.erl:506-507`). File
/// `<<"line1\nline2\nrest">>`; the live oracle answers
///
/// ```text
/// position(cur)/read(0)/read_line -> {ok,0} {ok,[]} {ok,"line1\n"}
/// ```
///
/// and the `0`-count read repeated after that line keeps the line reader's leftover
/// (`"line2\nrest"`) in place for the reads that follow.
#[test]
fn get_chars_zero_preserves_the_stashed_buffer() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("zero_count");
    std::fs::write(file.path(), b"line1\nline2\nrest").expect("a fixture file");
    let modes = mode_list(&mut runtime, &["read"]);
    let device = open_device(&mut runtime, &mut table, file.path(), modes);

    let at = cur_atom(&mut runtime);
    let request = position_request(&mut runtime, at);
    assert_eq!(device_render(&mut runtime, device, request), "{ok,0}");
    let request = get_chars_request(&mut runtime, 0);
    assert_eq!(device_render(&mut runtime, device, request), "[]");
    let request = get_line_request(&mut runtime);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[108,105,110,101,49,10]"
    );

    // A non-empty buffer survives the `0`-count read: the next reads serve `"line2\nrest"`
    // from it without touching the file.
    let request = get_chars_request(&mut runtime, 0);
    assert_eq!(device_render(&mut runtime, device, request), "[]");
    let request = get_chars_request(&mut runtime, 4);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[108,105,110,101]"
    );
    let request = get_chars_request(&mut runtime, 6);
    assert_eq!(
        device_render(&mut runtime, device, request),
        "[50,10,114,101,115,116]"
    );
}

/// A read is `eof` only when **nothing at all** was obtained — the buffered bytes still
/// count, and a short read that returns fewer than `N` bytes is not an end of file. File
/// `<<"line1\nline2\nrest">>`; the live oracle answers
///
/// ```text
/// read(16)/read(4) -> {ok,"line1\nline2\nrest"} eof
/// read(20)         -> {ok,"line1\nline2\nrest"}
/// ```
#[test]
fn get_chars_reports_eof_only_when_nothing_was_obtained() {
    let mut runtime = runtime();
    let mut table = table(&mut runtime);
    let file = TempPath::new("eof_tail");
    std::fs::write(file.path(), b"line1\nline2\nrest").expect("a fixture file");
    let modes = mode_list(&mut runtime, &["read"]);
    let whole = "[108,105,110,101,49,10,108,105,110,101,50,10,114,101,115,116]";

    let device = open_device(&mut runtime, &mut table, file.path(), modes.clone());
    let request = get_chars_request(&mut runtime, 16);
    assert_eq!(device_render(&mut runtime, device, request), whole);
    let request = get_chars_request(&mut runtime, 4);
    assert_eq!(device_render(&mut runtime, device, request), "eof");

    // A request for more than the file holds answers what is left, never `eof`.
    let device = open_device(&mut runtime, &mut table, file.path(), modes);
    let request = get_chars_request(&mut runtime, 20);
    assert_eq!(device_render(&mut runtime, device, request), whole);
    let request = get_chars_request(&mut runtime, 1);
    assert_eq!(device_render(&mut runtime, device, request), "eof");
}

// -- the live-oracle cross-check ------------------------------------------------------

/// How long an oracle probe may run before it is killed and reaped.
const ORACLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `true` for an existing executable file.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The oracle `erl`: `$ERTS_ORACLE` when it is an executable, else `erl` on `$PATH`.
/// A **filesystem** probe only — the oracle is never run just to locate it.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("erl"))
        .find(|candidate| is_executable(candidate))
}

/// The oracle's `~w` rendering of `probe` — `erl -noshell -eval 'io:format("~w",[…])'
/// -s init stop`, **bounded** by [`ORACLE_TIMEOUT`] so a probe can never hang the suite.
/// `None` (loudly) when the oracle is absent or cannot answer.
fn oracle_render(oracle: &Path, probe: &str) -> Option<String> {
    use std::process::{Command, Stdio};

    let mut command = Command::new(oracle);    command
        .arg("-noshell")
        .arg("-eval")
        .arg(format!("io:format(\"~w\", [{probe}])"))
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "warning: cannot spawn the oracle `{}`: {error}",
                oracle.display()
            );
            return None;
        }
    };
    let deadline = std::time::Instant::now() + ORACLE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    eprintln!("warning: the oracle probe outlived the bound and was killed");
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => {
                eprintln!("warning: cannot poll the oracle probe: {error}");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    match child.wait_with_output() {
        Ok(output) if output.status.success() => Some(
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string(),
        ),
        Ok(output) => {
            eprintln!(
                "warning: the oracle exited with {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
            None
        }
        Err(error) => {
            eprintln!("warning: cannot collect the oracle output: {error}");
            None
        }
    }
}

/// Renders `call` under the runtime's own `-eval` (`~w`).
fn runtime_render(call: &str) -> String {
    let mut runtime = runtime();
    let value = runtime.eval(call).expect(call);
    render(&mut runtime, value)
}

/// Runs `template` (whose `{p}` stands for `path`) under the runtime's `-eval` and under
/// the live oracle, and returns their **shared** `~w` rendering — or `None` (loudly) when
/// the oracle could not answer.
fn compare_with_oracle(oracle: &Path, template: &str, path: &Path) -> Option<String> {
    let call = template.replace("{p}", path.to_str().expect("an ASCII temporary path"));
    let Some(expected) = oracle_render(oracle, &call) else {
        eprintln!("warning: the oracle could not answer `{call}` — skipped");
        return None;
    };
    let actual = runtime_render(&call);
    assert_eq!(
        actual, expected,
        "`{call}` renders differently than the oracle"
    );
    Some(expected)
}

/// Every write-path answer re-run against the **live** oracle: the rendered values and
/// the bytes on disk must both agree. Skips **loudly** when no oracle is installed.
#[test]
fn the_write_answers_match_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("========== beam_rt file_server write oracle SKIPPED ==========");
        eprintln!("no `erl` on $PATH (and no executable $ERTS_ORACLE): the oracle");
        eprintln!("comparison cannot run. Set $ERTS_ORACLE (or install Erlang/OTP)");
        eprintln!("and re-run. NOT a pass.");
        eprintln!("==============================================================");
        eprintln!();
        return;
    };
    let dir = unique_path("oracle_write");
    std::fs::create_dir_all(&dir).expect("a private temporary directory");

    // `file:write_file/2` — the service's `{write_file, Name, Bin}` arm.
    let path = dir.join("write2.bin");
    let Some(rendered) =
        compare_with_oracle(&oracle, "file:write_file(\"{p}\", <<\"hello\">>)", &path)
    else {
        return;
    };
    assert_eq!(rendered, "ok");
    assert_eq!(std::fs::read(&path).expect("the written file"), b"hello");

    // `file:write_file/3` — its `open([binary,write])` + `write` + `close` over the
    // service and device seams.
    let path = dir.join("write3.bin");
    let Some(rendered) =
        compare_with_oracle(&oracle, "file:write_file(\"{p}\", <<\"abc\">>, [])", &path)
    else {
        return;
    };
    assert_eq!(rendered, "ok");
    assert_eq!(std::fs::read(&path).expect("the written file"), b"abc");

    // The explicit `file:open/2` + `file:write/2` + `file:close/1` sequence.
    let path = dir.join("seq.bin");
    let Some(rendered) = compare_with_oracle(
        &oracle,
        "begin {ok,F}=file:open(\"{p}\",[write]), R1=file:write(F,<<\"zig\">>), \
         R2=file:close(F), {R1,R2} end",
        &path,
    ) else {
        return;
    };
    assert_eq!(rendered, "{ok,ok}");
    assert_eq!(std::fs::read(&path).expect("the written file"), b"zig");

    // A write-mode open **creates** a missing path.
    let path = dir.join("open_write.bin");
    let Some(rendered) =
        compare_with_oracle(&oracle, "element(1, file:open(\"{p}\", [write]))", &path)
    else {
        return;
    };
    assert_eq!(rendered, "ok");
    assert!(path.exists(), "the `[write]` open created the path");

    // ... and an append-mode one too (`O_CREAT` without `O_TRUNC`).
    let path = dir.join("open_append.bin");
    let Some(rendered) =
        compare_with_oracle(&oracle, "element(1, file:open(\"{p}\", [append]))", &path)
    else {
        return;
    };
    assert_eq!(rendered, "ok");
    assert!(path.exists(), "the `[append]` open created the path");

    // A read-mode open on a missing path is still `{error, enoent}`.
    let path = dir.join("absent");
    let Some(rendered) = compare_with_oracle(&oracle, "file:open(\"{p}\", [read])", &path)
    else {
        return;
    };
    assert_eq!(rendered, "{error,enoent}");

    // An invalid encoding is `{error, badarg}` **before** the open, on both sides.
    let path = dir.join("bad_encoding.bin");
    let Some(rendered) =
        compare_with_oracle(&oracle, "file:open(\"{p}\", [{encoding,bogus}])", &path)
    else {
        return;
    };
    assert_eq!(rendered, "{error,badarg}");
    assert!(
        !path.exists(),
        "the rejected encoding never reached `open(2)`"
    );

    // A directory is `{error, eisdir}` for both the write BIF and the write open.
    let Some(rendered) =
        compare_with_oracle(&oracle, "file:write_file(\"{p}\", <<\"x\">>)", &dir)
    else {
        return;
    };
    assert_eq!(rendered, "{error,eisdir}");

    // `file:read_file/1` reads back a file this runtime wrote through the same arms.
    let path = dir.join("readback.bin");
    std::fs::write(&path, b"on disk").expect("a fixture file");
    let Some(rendered) = compare_with_oracle(&oracle, "file:read_file(\"{p}\")", &path) else {
        return;
    };
    assert_eq!(rendered, "{ok,<<111,110,32,100,105,115,107>>}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every `getopts`/`setopts` answer re-run against the **live** oracle: the whole
/// open→`setopts`→`getopts`→close sequence must render identically. Skips **loudly** when no
/// oracle is installed.
#[test]
fn the_getopts_answers_match_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("========== beam_rt file_server getopts oracle SKIPPED ==========");
        eprintln!("no `erl` on $PATH (and no executable $ERTS_ORACLE): the oracle");
        eprintln!("comparison cannot run. Set $ERTS_ORACLE (or install Erlang/OTP)");
        eprintln!("and re-run. NOT a pass.");
        eprintln!("==============================================================");
        eprintln!();
        return;
    };
    let dir = unique_path("oracle_getopts");
    std::fs::create_dir_all(&dir).expect("a private temporary directory");

    // Every served `Opts` shape under the oracle's own `io:getopts/1`/`io:setopts/2`, plus
    // the two returned failures and the non-list catch-all. The rendered tuple is the whole
    // `{…}` sequence; a mismatch in any element fails the case.
    let cases: [&str; 12] = [
        // The default open modes, through `file:open/2`'s empty mode list.
        "begin {ok,F}=file:open(\"{p}\",[]), R=io:getopts(F), file:close(F), R end",
        // The `binary` mode at open time.
        "begin {ok,F}=file:open(\"{p}\",[binary]), R=io:getopts(F), file:close(F), R end",
        // The open-time encoding.
        "begin {ok,F}=file:open(\"{p}\",[{encoding,unicode}]), R=io:getopts(F), file:close(F), R end",
        // `setopts` then `getopts`.
        "begin {ok,F}=file:open(\"{p}\",[read]), A=io:setopts(F,[binary,{encoding,utf8}]), \
         B=io:getopts(F), file:close(F), {A,B} end",
        "begin {ok,F}=file:open(\"{p}\",[read]), A=io:setopts(F,[list]), \
         B=io:getopts(F), file:close(F), {A,B} end",
        "begin {ok,F}=file:open(\"{p}\",[read]), A=io:setopts(F,[{binary,false}]), \
         B=io:getopts(F), file:close(F), {A,B} end",
        "begin {ok,F}=file:open(\"{p}\",[read]), A=io:setopts(F,[]), \
         B=io:getopts(F), file:close(F), {A,B} end",
        "begin {ok,F}=file:open(\"{p}\",[read]), A=io:setopts(F,[{list,true}]), \
         B=io:getopts(F), file:close(F), {A,B} end",
        "begin {ok,F}=file:open(\"{p}\",[read]), A=io:setopts(F,[{list,false}]), \
         B=io:getopts(F), file:close(F), {A,B} end",
        // The two returned failures.
        "begin {ok,F}=file:open(\"{p}\",[read]), A=io:setopts(F,[bogus]), \
         B=io:getopts(F), file:close(F), {A,B} end",
        "begin {ok,F}=file:open(\"{p}\",[read]), A=io:setopts(F,[{encoding,bogus}]), \
         B=io:getopts(F), file:close(F), {A,B} end",
        // A non-list `Opts` is the `{error,{request,{setopts,…}}}` reply.
        "begin {ok,F}=file:open(\"{p}\",[read]), A=io:setopts(F,foo), \
         B=io:getopts(F), file:close(F), {A,B} end",
    ];
    for (index, template) in cases.iter().enumerate() {
        let path = dir.join(format!("case{index}.bin"));
        std::fs::write(&path, b"hello\n").expect("a fixture file");
        let Some(rendered) = compare_with_oracle(&oracle, template, &path) else {
            return;
        };
        eprintln!("case {index}: {rendered}");
    }

    // A live read after `setopts` decodes the stream the same way on both sides.
    let path = dir.join("decode.bin");
    std::fs::write(&path, b"abc\xc3\xa5def\n").expect("a fixture file");
    let Some(rendered) = compare_with_oracle(
        &oracle,
        "begin {ok,F}=file:open(\"{p}\",[read]), A=io:setopts(F,[{encoding,unicode}]), \
         B=file:read_line(F), file:close(F), {A,B} end",
        &path,
    ) else {
        return;
    };
    assert_eq!(rendered, "{ok,{ok,[97,98,99,229,100,101,102,10]}}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The mixed `file:read/2`/`file:read_line/1`/`file:position/2` sequences re-run against
/// the **live** oracle on one handle per case, over a file whose content is exactly
/// `<<"line1\nline2\nrest">>`. Every case mixes the readers on the same device, so a reply
/// that ignores the stashed `State#state.buf` diverges here. Skips **loudly** when no
/// oracle is installed.
#[test]
fn the_buffered_read_answers_match_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("========== beam_rt file_server buffered-read oracle SKIPPED ==========");
        eprintln!("no `erl` on $PATH (and no executable $ERTS_ORACLE): the oracle");
        eprintln!("comparison cannot run. Set $ERTS_ORACLE (or install Erlang/OTP)");
        eprintln!("and re-run. NOT a pass.");
        eprintln!("=====================================================================");
        eprintln!();
        return;
    };
    let dir = unique_path("oracle_buffered_read");
    std::fs::create_dir_all(&dir).expect("a private temporary directory");

    let sequence = |body: &str| {
        format!("begin {{ok,F}}=file:open(\"{{p}}\",[read]), {body}, file:close(F), {{A,B,C,D}} end")
    };
    let cases: [String; 8] = [
        // The repro: a `read_line` between two `read`s.
        "A=file:read(F,6), B=file:read_line(F), C=file:read(F,4), D=ok".to_string(),
        // A `read_line` after a 2-byte read.
        "A=file:read(F,2), B=file:read_line(F), C=file:read(F,4), D=ok".to_string(),
        // Three fixed-size reads straddling the second line.
        "A=file:read(F,6), B=file:read(F,4), C=file:read(F,4), D=file:read(F,4)".to_string(),
        // `position(cur)` compensates for the line reader's read-ahead.
        "A=file:read_line(F), B=file:position(F,cur), C=file:read(F,4), D=ok".to_string(),
        // ... and the seek clears the buffer, so the next line reader starts there.
        "A=file:read_line(F), B=file:position(F,cur), C=file:read_line(F), D=ok".to_string(),
        // A `0`-count read at the start, then the first line.
        "A=file:position(F,cur), B=file:read(F,0), C=file:read_line(F), D=ok".to_string(),
        // Reading the whole file then past its end.
        "A=file:read(F,16), B=file:read(F,4), C=ok, D=ok".to_string(),
        // A `0`-count read does not consume the line reader's leftover.
        "A=file:read_line(F), B=file:read(F,0), C=file:read(F,4), D=ok".to_string(),
    ];
    for (index, body) in cases.iter().enumerate() {
        let path = dir.join(format!("case{index}.bin"));
        std::fs::write(&path, b"line1\nline2\nrest").expect("a fixture file");
        let template = sequence(body);
        let Some(rendered) = compare_with_oracle(&oracle, &template, &path) else {
            return;
        };
        eprintln!("case {index}: {rendered}");
    }

    // The same repro on a `[read,binary]` device.
    let path = dir.join("binary.bin");
    std::fs::write(&path, b"line1\nline2\nrest").expect("a fixture file");
    let Some(rendered) = compare_with_oracle(
        &oracle,
        "begin {ok,F}=file:open(\"{p}\",[read,binary]), A=file:read(F,6), \
         B=file:read_line(F), C=file:read(F,4), file:close(F), {A,B,C} end",
        &path,
    ) else {
        return;
    };
    assert_eq!(
        rendered,
        "{{ok,<<108,105,110,101,49,10>>},{ok,<<108,105,110,101,50,10>>},{ok,<<114,101,115,116>>}}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
