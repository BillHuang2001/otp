# `beam_rt/src/native/file_server/` — the native `file_server_2` service (contract A.9)

## Intent
The runtime-native **file service** that completes the real boot chain. `lib/kernel/src/file.erl`
routes every non-`raw` file operation through `gen_server:call(file_server_2, Request, infinity)`,
and the file service is first *reached* at the `{apply,{c,erlangrc,[]}}` directive of
`bootstrap/bin/start.boot` (`c:erlangrc/0` → `file:path_eval/2` → `file:open("$HOME/.erlang",[read])`).
`FileServices` supplies the missing registered service. It is **not** a BIF: it is a process plus a
scheduler-pass step. Without it that call would die with the oracle-shaped
`{noproc,{gen_server,call,[file_server_2,…]}}` and the boot would stop there.

`file_server.rs` (this node's parent) owns the service; `file_info.rs`, `get_until.rs` and
`get_line.rs` are its child modules.

## API Surface
### The process model (never enqueued) — `../file_server.rs`
`FileServices::start(&mut ProcessTable, &mut AtomTable, HeapPolicy)` is called from `Runtime::new`
next to the `init` registration and runs **one** process, born `ProcessState::Runnable`, `insert`ed
into the table and registered under the atom `file_server_2`, but **never enqueued** and given no
engine — the pattern the runtime's own `init`/`self_pid` process already uses. Because
`Process::wake` only wakes a `Waiting`/`Suspended` process, a message delivered into the service's
mailbox never puts it on a run queue, so the runner never sees it and
`error::unknown_process_entry` is unreachable. `whereis(file_server_2)`, `monitor(process, Name)`
and a send all resolve it (`server()`/`is_running()` accessors). A successful `{open,…}` spawns a
**second** `Runnable`, never-enqueued process — the **io-device** — under the same heap policy, and
keys it in `FileServices::devices` as a `Device` (the open `std::fs::File` plus the `binary`/`list`
read mode `file_io_server:parse_options/1` derived).

### The scheduler-pass step
`Runtime::service_file_requests(&mut ProcessTable) -> Result<usize>` drains every service/io-device
mailbox (`Process::receive`) and delivers each reply through `ProcessTable::deliver_simple`,
returning the delivery count; a reply to a since-dead caller is dropped
(`BeamError::TermNotFound`), exactly as `gen:reply/2` swallows its own send failure.
`scheduler_pass` calls it as an **additive step right after the run queue drains** (the same place
the port half runs); a non-zero count is added to the pass's `delivered` total and followed by
`scheduler.resume_woken`, so a serviced-only pass never looks like "nothing happened" and a caller
blocked in `gen_server:call`'s `receive` makes progress. The requesting process is normally the
runtime's own `Runnable` `self_pid`, which the delivery therefore does **not** enqueue; it resumes
through the engine's blocking-aware entry (`RunOutcome::Wait` → `Machine::resume_outcome`), not a
scheduler slice.

### The wire protocols (verbatim from `lib/`)
| Request | Reply |
|---|---|
| `{'$gen_call', {Pid, Tag}, Request}` (the local `infinity` form, `gen.erl:231-250`) | `{Tag, Reply}` to `Pid`; a `[alias\|Ref]` tag is the typed `unsupported` gap (the alias delivery this runtime cannot perform), never a fabricated delivery |
| `{file_request\|io_request, From, ReplyAs, Request}` (`file_io_server.erl:199-233`) | `{file_reply\|io_reply, ReplyAs, Reply}` to `From` |
An unknown `file_server` request is the oracle's `{noreply, State}`: nothing is delivered. A
`FileServices` process never runs and never collects, so a mailbox payload (already copied onto the
service's own heap by `deliver_simple`) is stable for as long as it is read.

### The served request surface (`file_server:handle_call/3`, `file_server.erl:101-201`)
| Request | Behaviour |
|---|---|
| `{open, Name, ModeList}` | `{error,einval}` for a non-list mode; `{error,badarg}` for a path `prim_file` rejects; `{error,eisdir}` for a **directory** (an explicit `metadata().is_dir()` check is required, because Linux `open(2)` **succeeds** on a directory while the oracle's `efile_open` answers `EISDIR`); another OS failure → the oracle's errno atom; a readable file → `{ok, IoPid}` with a fresh native io-device |
| `{read_file,_}`, `{delete,_}`, `{make_dir,_}`, `{del_dir,_}`, `{read_link,_}`, `{altname,_}` | reuse the existing `prim_file` primitives ([`../prim.rs`](../prim.rs)), so the errno remaps are identical |
| `{rename,_,_}`, `{make_link,_,_}`, `{make_symlink,_,_}` | reuse the `prim_file` primitives |
| `{get_cwd}` (bare atom or 1-tuple) / `{get_cwd,_}` | reuse `prim_file:get_cwd` |
| `{list_dir,_}` | `{ok, Names}` from `std::fs::read_dir` in the **host directory order** (`file.erl:1044` documents "The names are not sorted") |
| `{read_file_info,_}` / `{read_link_info,_}` | [`file_info::serve`](file_info.rs) — the `stat`/`lstat` **failure** path only (a success would need the `#file_info{}` `access` slot; see below) |
| anything else (`write_file`, `copy`, `write_file_info`, …) | the typed `unsupported` capability gap, never a wrong value |

### The io-device request surface (`file_io_server.erl:199-233`)
A request is either a **bare command atom** or a tuple whose head is the command: `file.erl` sends
`close`/`sync`/`datasync`/`truncate` as the bare atom (`:1536`) and the positioned form as
`{position,At}` (`:1954`). Served for real: `close` (retires the device), `sync`, `datasync`,
`truncate`, `{position,At}` (`At` is `bof`/`cur`/`eof` or `{Mark,Offset}`, and a bare non-negative
integer is `file.erl`'s `{N,bof}` shorthand), `{get_chars,Enc,Prompt,N}` (shaped `binary`/`list`
from the open mode), `{put_chars,Enc,Chars}`, `{get_until,Enc,Prompt,Mod,Func,XtraArgs}` (plus
the legacy 4-tuple form, whose encoding defaults to `latin1`), and `{get_line,Enc,_Prompt}` (plus
the legacy 2-tuple form, encoding defaulted to `latin1` — `file:read_line/1`'s request). Two
requests run Erlang code and own their read loop ([`get_until.rs`](get_until.rs),
[`get_line.rs`](get_line.rs)) — see the capability-gaps subsection below.
Every other request is the typed `unsupported` gap.
The reply tag is **paired** with the request tag, never echoed: `{io_request, …}` is answered
`{io_reply, ReplyAs, Reply}` and `{file_request, …}` with `{file_reply, ReplyAs, Reply}`
(`file_io_server.erl:199-239`), because every caller matches the reply on its **own** tag
(`io.erl:1670`'s `execute_request` matches `io_reply`).

### Honest capability gaps (never a fabricated term)
* **`{get_until,Enc,_Prompt,Mod,Func,XtraArgs}` is served** by [`get_until.rs`](get_until.rs): the
  read loop drives an engine-backed `Continuation` running `Mod:Func(Cont, Data, XtraArgs…)`,
  decodes `{more,Cont}`/`{done,Reply,Buf}` and stashes `Buf` as the device's `State#state.buf`.
  Typed gaps: a continuation suspending in a `receive`; a leftover that is a deep iolist /
  non-charlist / negative code point / non-byte-aligned bitstring; an encoding other than
  `latin1`/`unicode`; a pid with no registered device ("closed io device"). The `{get_chars,…}` arm
  ignores the stashed buffer and a `{utf16,_}`/`{utf32,_}` **stream** is read as `latin1` (both
  shared divergences).
* **`{get_line,Enc,_Prompt}` is served** by [`get_line.rs`](get_line.rs): the same loop drives the
  `LineContinuation` (the oracle's `io_lib:collect_line/4` + `cast/4`), so
  `file:read_line/1`/`io:get_line/2` complete. Typed gaps: a request `Enc` that is neither `latin1`
  nor `unicode` (incl. a non-atom); invalid UTF-8 on a `unicode` device surfaces `beam_io`'s
  `invalid_unicode` stop where the oracle answers `{error,{no_translation,unicode,latin1}}`, after
  which a further request hits the "closed io device" gap rather than `{error,terminated}`.
* **`{read_file_info,_}`/`{read_link_info,_}` serve the `stat`/`lstat` failure path only**
  ([`file_info.rs`](file_info.rs)): an absent path is the oracle's `{error,enoent}` (so
  `filelib:is_dir/1` &c. answer `false`), while a *successful* stat is the typed gap
  `file_server: the #file_info{} access slot needs access(2), which no safe std API exposes`. The
  full argument for why the success path is a fabrication rather than an approximation is
  `file_info.rs`'s rustdoc; closing it needs one **`beam_io`** facility — a safe
  `path_access(Path) -> (read, write)` over `access(2)` (which also closes the whole `#file_info{}`
  family in [`../prim.rs`](../prim.rs)) plus `process_credentials() -> (euid, egid)` for
  `read_handle_info/1,2`.
* **Other io-device requests stay the typed `unsupported` gap** — `setopts`/`getopts`/
  `pread`/`pwrite`/`advise`/`allocate`/`read_handle_info`, the legacy `put_chars` arities, a
  non-`latin1` encoding and `read_file_info`/`write_file`.
* **`file_server.rs` does not serve `{write_file,Name,Data}`** though `prim_file:write_file/2` IS
  implemented in [`../prim.rs`](../prim.rs) (the `prim_file` floor is a separate dispatch site, not
  reachable as a `file_server` request).
* **The `{open,Name,ModeList}` arm opens the path READ-ONLY** (`../file_server.rs::reply_open` uses
  `std::fs::File::open` and inspects `ModeList` only for `binary`/`encoding`), and its
  `std::fs::metadata` pre-check rejects a non-existent path as `{error,enoent}`. A write/append mode
  is therefore ignored: `file:open(_,[write])` on a new file answers `{error,enoent}` (it never
  creates/truncates), and a device obtained through it cannot be written. Observed:
  `file:write_file("/tmp/x","hi")` ⇒ exit 3 `unsupported: file_server: the `write_file/2` request is
  not implemented`; `file:write_file(Path,"hi",[])` and `file:open(Path,[write])` ⇒ `{error,enoent}`.
  **No write path is served.**
* **In-process compilation (`compile:file/2`) cannot run here.** Besides the write gaps above, the
  compiler front end `epp` needs `io:setopts/2` on a device (`lib/stdlib/src/epp.erl:803`;
  `setopts`/`getopts` are the typed io-device gap) and **spawns** an `epp` server (`epp.erl:386`);
  `epp:open/2` fails with `unsupported: opcode: run: process blocked` even on the single-pass `-eval`
  route. Every `file:*` call made from a **scheduled** process (`beam_rt run …`) folds into the same
  `beam_engine` `blocked()` seam first, because `file.erl` routes through
  `gen_server:call(file_server_2, …)` (`file.erl:3039-3044`); the read path only completes on the
  `-eval`/single-pass/boot routes, whose context may pump.
* **A scheduled process's `gen_server:call` to the service cannot complete** — the engine's
  non-blocking `fold_outcome` folds the callee's suspend into `error::blocked()`. Owner:
  **`beam_engine`**.
* **The io-device read path completes from library code, `{get_line,…}` included.**
  `io:request/2` loads the `io` module body, `net_kernel:dflag_unicode_io/1` is served (the
  `RuntimeHooks` bridge) and the service answers with the tag **paired** to the request, so
  `file:read/2`, `io:scan_erl_exprs/4`, `io:parse_erl_exprs/4` and `file:read_line/1` all complete
  and agree with the oracle — the first three in `../../tests/io_scan.rs`, `file:read_line/1` through
  the `{get_line,…}` arm above, cross-checked by
  [`get_line/tests.rs`](get_line/tests.rs)'s `the_replies_match_the_live_oracle`.

## Constraints
- **`#![deny(unsafe_code)]`** — the crate-wide rule covers this whole node. Terms are built and read
  only through `beam_engine::terms`, `beam_codec::term`, `beam_loader::container` and `beam_term`.
- **Return-based errors** (`erts_rust/crates/CONTEXT.md` §3): `Option<Result<Eterm>>`/`Result`,
  never a crate-local error enum, never `panic!`/`unwrap()` on runtime data, never a silent swallow.
  A "value atom" answer the C *returns* is `Ok(atom)`.
- **Files ~1000 lines of production code**; `../file_server.rs` is ~985 (single responsibility, kept
  whole). Split by responsibility; every public item documented.

## Known Issues / Test Strategy
- `../file_server.rs`'s colocated units pin the wire decoders (`improper_list_head`'s `[alias|Ref]`
  detection, `seek_from`'s `bof`/`cur`/`eof`/bare-integer forms, `iodata_bytes`, `tuple_fields`), the
  errno→atom table and the `binary`/`list` open-mode parse; the service is proven end to end over
  real temporary files/directories by `../../tests/file_server.rs` (every rendering cross-checked
  against the live `erl`).
- `get_until/tests.rs` pins the `{get_until, …}` request: the protocol decoders (`interpret`,
  `leftover_from`), `Runtime::open_encoding`, the rejections, and the request end to end through
  `Runtime::serve_device_request` over a real temporary file with the real `erl_scan:tokens/4` on
  the engine (the stashed leftover is fed to the second request). `get_line/tests.rs` pins the
  `LineContinuation` side.
- `file_info.rs`'s units pin the failure-path replies and the `badarg` path contract; a *successful*
  stat is pinned as the `unsupported` outcome with a `// documented divergence:` comment.
- Oracle values are quoted in each test's doc comment and were re-verified against the system `erl`
  (`erl -noshell -eval '…'`, OTP 29.1.1 / erts-17.1). Fixtures needing the toolchain skip **loudly**.

## Routing Table
- `../file_server.rs` → the `FileServices` service process, its `Device` io-devices, the
  request/reply wire, the decode helpers and the errno→atom table + unit tests
- `file_info.rs` → `{read_file_info,_}`/`{read_link_info,_}` (the `stat`/`lstat` failure half) + its
  unit tests
- `get_until.rs` (+ `get_until/tests.rs`) → the `{get_until, …}` io-device request
  (`get_until::serve` + the engine-backed `EngineContinuation` implementing
  `beam_io::io_device::Continuation`)
- `get_line.rs` (+ `get_line/tests.rs`) → the `{get_line, …}` io-device request (`get_line::serve` +
  the `LineContinuation`)
- `Runtime::service_file_requests` → the scheduler-pass step (owned by `../../runtime/scheduler.rs`)
- `../../../../erts/emulator/beam/` → legacy C ground truth (read-only); `../../../../erts/emulator/nifs/`
  → `prim_file`'s C NIFs; `../../../../erts/preloaded/src/prim_file.erl` → the wrappers
