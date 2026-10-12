# `beam_rt/src/native/file_server/` — the native `file_server_2` service (contract A.9)
## Intent
The runtime-native **file service** that completes the real boot chain. `lib/kernel/src/file.erl`
routes every non-`raw` file operation through `gen_server:call(file_server_2, Request, infinity)`,
and the file service is first *reached* at the `{apply,{c,erlangrc,[]}}` directive of
`bootstrap/bin/start.boot` (`c:erlangrc/0` → `file:path_eval/2` → `file:open("$HOME/.erlang",[read])`).
`FileServices` supplies the missing registered service. It is **not** a BIF: it is a process plus a
scheduler-pass step. Without it that call would die with the oracle-shaped
`{noproc,{gen_server,call,[file_server_2,…]}}` and the boot would stop there.
`file_server.rs` (this node's parent) owns the service; `setopts.rs`, `file_info.rs`, `get_until.rs`
and `get_line.rs` are its child modules; `tests.rs` is its colocated unit suite.
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
keys it in `FileServices::devices` as a `Device`: the open `std::fs::File` plus the **mutable**
`binary`/`list` read mode and the `unicode`/`latin1` stream encoding
(`file_io_server`'s `State#state.read_mode`/`unic`). The mutability is the point of
[`setopts.rs`](setopts.rs): a `{setopts,…}` rewrites them and the next read honours the new value.
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
`{get_until,…}` is the one request that runs **engine code** from this step: its
`EngineContinuation::call` (`get_until.rs`) builds args and runs `Machine::run_export_outcome` under
a fresh `RuntimeContext::new` — the **pumping** root context (`may_pump_scheduler == true`,
`context/mod.rs`), *not* the pass's own non-pumping `for_process` context — even though
`service_file_requests` runs inside a `scheduler_pass` that has already moved the process table out.
A continuation that suspends is reported as this service's **own** typed gap ("the `{get_until,…}`
continuation suspended in a `receive`"), never `beam_engine::blocked()`, so a suspending
`{get_until,…}` is **not** the source of a corpus `opcode: run: process blocked …` failure.
(`{get_line,…}` runs no engine code — its `LineContinuation` is a Rust-native
`io_lib:collect_line/4`.) Hazard: because that context may pump, a blocking
`call_bif`/`run_export_blocking` reached from inside a continuation would re-enter `pump_scheduler`
while the pass holds the table/scheduler out; the step does not guard against it today.
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
| `{open, Name, ModeList}` | a non-list `ModeList` (incl. an improper tail) → `{error,einval}`; a `{encoding, E}` `valid_enc/1` rejects → `{error,badarg}` **before** the open; otherwise the request is served by **the floor's own `prim_file:open/2`** (one owner of flag derivation, the errno vocabulary and the post-open type checks), and the resulting OS file is moved into the new io-device with `prim_file.rs`'s `take_open_file/2` — so `[read]`/`[write]`/`[append]`/`[exclusive]`/`[directory]`/`[sync]` all behave as `get_flags` derives them, `[write]` creates+truncates, and `{ok, IoPid}` names a device whose file is open for the requested access |
| `{write_file, Name, Bin}` | `?PRIM_FILE:write_file(Name, Bin)` (`file_server.erl:111-112`), the body of `file:write_file/2`; `file:write_file/3` rides `open`+`put_chars`+`close` |
| `{read_file,_}`, `{delete,_}`, `{make_dir,_}`, `{del_dir,_}`, `{read_link,_}`, `{altname,_}` | reuse the existing `prim_file` primitives ([`../prim.rs`](../prim.rs)), so the errno remaps are identical |
| `{rename,_,_}`, `{make_link,_,_}`, `{make_symlink,_,_}` | reuse the `prim_file` primitives |
| `{get_cwd}` (bare atom or 1-tuple) / `{get_cwd,_}` | reuse `prim_file:get_cwd` |
| `{list_dir,_}` | `{ok, Names}` from `std::fs::read_dir` in the **host directory order** (`file.erl:1044` documents "The names are not sorted") |
| `{read_file_info, Name \| Name, Opts}` / `{read_link_info, Name \| Name, Opts}` | [`file_info::serve`](file_info.rs) — the `stat`/`lstat` **failure** path is the oracle's `{error, Reason}`, the **success** path is the real `{ok, #file_info{}}` whose `access` slot comes from two `access(2)` probes through `beam_io::fs_access` |
| anything else (`copy`, `write_file_info`, `set_cwd`, `list_dir_all`, `read_link_all`, …) | the typed `unsupported` capability gap, never a wrong value |
### The io-device request surface (`file_io_server.erl:199-233`)
A request is either a **bare command atom** or a tuple whose head is the command: `file.erl` sends
`close`/`sync`/`datasync`/`truncate` as the bare atom (`:1536`) and the positioned form as
`{position,At}` (`:1954`). Served for real: `close` (retires the device), `sync`, `datasync`,
`truncate`, `{position,At}` (`At` is `bof`/`cur`/`eof` or `{Mark,Offset}`, and a bare non-negative
integer is `file.erl`'s `{N,bof}` shorthand), `{get_chars,Enc,Prompt,N}` (shaped `binary`/`list`
from the open mode), `{put_chars,Enc,Chars}`, `{get_until,Enc,Prompt,Mod,Func,XtraArgs}` (plus
the legacy 4-tuple form, whose encoding defaults to `latin1`), `{get_line,Enc,_Prompt}` (plus
the legacy 2-tuple form, encoding defaulted to `latin1` — `file:read_line/1`'s request), and the
**read-mode surface** `getopts` / `{setopts, Opts}` ([`setopts.rs`](setopts.rs)). Two requests run
Erlang code and own their read loop ([`get_until.rs`](get_until.rs), [`get_line.rs`](get_line.rs)).
Every other request is the typed `unsupported` gap.
**The write path is real**: `{open,Name,[binary,write|…]}` yields a device whose `{put_chars,latin1,Bin}`
request reaches `std::fs::File::write_all` (what `file:write/2` and `file:write_file/3` send), and
`{write_file,Name,Bin}` opens the path `[write]`-style and writes it.
The reply tag is **paired** with the request tag, never echoed: `{io_request, …}` is answered
`{io_reply, ReplyAs, Reply}` and `{file_request, …}` with `{file_reply, ReplyAs, Reply}`
(`file_io_server.erl:199-239`), because every caller matches the reply on its **own** tag
(`io.erl:1670`'s `execute_request` matches `io_reply`).
### The shared read buffer (`State#state.buf`)
`{get_line,…}`/`{get_until,…}` read *ahead*: they pull more bytes off the device than they return,
and what they read past the reply is the device's buffer for the next request — the oracle's
`State#state.buf`, held as `Device::buf` (`../file_server.rs`). All three consuming arms share that
one buffer, so any mix of them on one device agrees with the oracle:
* `{get_chars,Enc,_Prompt,N}` serves the buffer **first** (`get_chars(N, OutEnc, #state{buf=Buf,…})`,
  `file_io_server.erl:508-528`): it takes `min(N, buf)` off the front and reads only what is still
  missing until `N` bytes are collected or the read hits end of file; a request that obtained
  **nothing at all** answers the atom `eof` (the `eof when BufSize =:= 0` clause, `:529-530`). A
  request for `0` bytes answers the empty cast and leaves the buffer untouched (`:506-507`). This
  arm reads only what it still needs, so `physical offset == logical position + buf.len()` holds
  after every request (the oracle instead over-reads up to `?READ_SIZE_BINARY` and subtracts the
  buffer again in `position/3` — observationally the same).
* `{position,At}` compensates for the buffer (`file_request({position,At}, _)`, `:319-326` →
  `position/3`, `:970-977`): `cur` → `{cur, -byte_size(Buf)}`, `{cur, Offs}` →
  `{cur, Offs - byte_size(Buf)}`, and every other form (`bof`/`eof`/a bare integer/`{bof,N}`/
  `{eof,N}`) passes through unchanged. A **successful** seek clears the buffer (`std_reply/2`,
  `:344-348`), so the logical position the caller observes is the physical one.
### The `getopts`/`setopts` read-mode surface — `setopts.rs`
`{setopts, Opts}` reproduces `file_io_server:setopts/2` (`:718-728`) faithfully: the three list
transforms in order (`expand_encoding/1` → `proplists:substitute_negations([{list,binary}], …)` →
`proplists:unfold/1`), then `check_valid_opts/1` (`:729-736` — every element must be a 2-tuple
headed `binary`/`encoding`; otherwise the reply is the **value** `{error,enotsup}`), then
`do_setopts/2` (`:737-750` — a `valid_enc/1`-rejected encoding is the value `{error,badarg}`;
`binary` true/false/absent selects or leaves the read mode; success is the bare atom `ok`).
`getopts` answers `[{binary, ReadMode =:= binary}, {encoding, Unic}]` from the **live** device
(`:752-755`). A non-list `Opts` is the `io_request/2` catch-all's `{error,{request,{setopts,Opts}}}`.
`io:getopts/1`/`io:setopts/2` (`io.erl:489-490`/`:654-655`) return the reply verbatim, so `epp`'s
`ok = io:setopts(File, [{encoding, Enc}])` (`epp.erl:803`) works.
### Honest capability gaps (never a fabricated term)
* **`{get_until,Enc,_Prompt,Mod,Func,XtraArgs}` is served** by [`get_until.rs`](get_until.rs): the
  read loop drives an engine-backed `Continuation` running `Mod:Func(Cont, Data, XtraArgs…)`,
  decodes `{more,Cont}`/`{done,Reply,Buf}` and stashes `Buf` as the device's `State#state.buf`.
  Typed gaps: a leftover that is a deep iolist /
  non-charlist / negative code point / non-byte-aligned bitstring; an encoding other than
  `latin1`/`unicode`; a pid with no registered device ("closed io device").
  A continuation that suspends in a `receive` **is** this layer's typed capability gap: the
  device process cannot suspend mid-request, so the request answers `Err(unsupported: …)`,
  which neither retires the device nor reports `{error, Func}`. The `{get_chars,…}`
  arm serves the stashed buffer **before** any fresh read and `{position,At}` compensates for
  it (see *The shared read buffer* above), and a
  `{utf16,_}`/`{utf32,_}` **stream** is read as `latin1` (a divergence).
* **`{get_line,Enc,_Prompt}` is served** by [`get_line.rs`](get_line.rs): the same loop drives the
  `LineContinuation` (the oracle's `io_lib:collect_line/4` + `cast/4`), so
  `file:read_line/1`/`io:get_line/2` complete. Typed gaps: a request `Enc` that is neither `latin1`
  nor `unicode` (incl. a non-atom); invalid UTF-8 on a `unicode` device surfaces `beam_io`'s
  `invalid_unicode` stop where the oracle answers `{error,{no_translation,unicode,latin1}}`, after
  which a further request hits the "closed io device" gap rather than `{error,terminated}`.
* **The `utf16`/`utf32` encodings are not modelled.** `beam_io::io_device::Encoding` has only
  `Latin1`/`Unicode`; an open-time `{encoding,utf16}` therefore reads as `latin1` (a divergence, so
  `getopts` reports `latin1` where the oracle reports `{utf16,big}`), and a `{setopts,…}` that
  SELECTS a `utf16`/`utf32` encoding is a typed `unsupported` gap rather than a fabricated `ok`.
  A `{setopts,…}` `{binary, V}` whose `V` is neither `true` nor `false` is likewise a typed gap
  (the oracle crashes the device's `case`).
* **Other io-device requests stay the typed `unsupported` gap** — `pread`/`pwrite`/`advise`/
  `allocate`/`read_handle_info`, the legacy `put_chars` arities, and a non-`latin1`
  `put_chars` encoding.
* **A `{get_chars,…}` on a `unicode` **stream** counts BYTES, where the oracle counts
  CHARACTERS.** `reply_get_chars` moves `N` raw bytes; the oracle's
  `get_chars(N, OutEnc, #state{unic=unicode})` clause (`file_io_server.erl:536-577`) walks
  `N` characters (`count_and_find/3`) and converts (`cast/4`). Over a file
  `<<"abc",16#c3,16#a5,"def\n">>` (`abcådef\n`), as measured on the live `erl`
  (OTP 29.1.1 / erts-17.1) vs this runtime:

  | call | oracle | this runtime |
  |---|---|---|
  | `file:read(F,2)` | `{ok,"ab"}` | `{ok,"ab"}` |
  | `file:read(F,3)` | `{ok,"abc"}` | `{ok,"abc"}` |
  | `file:read(F,4)` | `{ok,"abcå"}` | `{ok,"abcÃ"}` |
  | `file:read(F,8)` | `{ok,"abcådef\n"}` | `{ok,"abcÃ¥def"}` |
  | `read(4)` then `read_line` | `{{ok,"abcå"},{ok,"def\n"}}` | `exit: invalid_unicode` |

  The divergence is masked whenever the content is ASCII or `N` lands on a character boundary,
  which is why the `latin1` cases agree exactly. It is a **documented divergence, not a typed
  gap**: returning a typed gap for a `unicode` device would regress the ASCII-content case, which
  agrees today. Owner: this node (`reply_get_chars`).
* **The io-device read path completes from library code on every route, `{get_line,…}`
  included.** `io:request/2` loads the `io` module body, `net_kernel:dflag_unicode_io/1` is served
  (the `RuntimeHooks` bridge) and the service answers with the tag **paired** to the request, so
  `file:read/2`, `io:scan_erl_exprs/4`, `io:parse_erl_exprs/4` and `file:read_line/1` all complete
  and agree with the oracle both from the pumped `-eval` root and from a **scheduled** `run`
  process — the first three in `../../tests/io_scan.rs`, `file:read_line/1` through
  the `{get_line,…}` arm above, cross-checked by
  [`get_line/tests.rs`](get_line/tests.rs)'s `the_replies_match_the_live_oracle`; the scheduled
  route by `../../tests/scheduled_io_device.rs`.
## Constraints
- **`#![deny(unsafe_code)]`** — the crate-wide rule covers this whole node. Terms are built and read
  only through `beam_engine::terms`, `beam_codec::term`, `beam_loader::container` and `beam_term`.
- **Return-based errors** (`erts_rust/crates/CONTEXT.md` §3): `Option<Result<Eterm>>`/`Result`,
  never a crate-local error enum, never `panic!`/`unwrap()` on runtime data, never a silent swallow.
  A "value atom" answer the C *returns* is `Ok(atom)`.
- **Files ~1000 lines of production code**: `../file_server.rs` is 956 (single responsibility, kept
  whole); the ~700-line colocated test module lives in [`tests.rs`](tests.rs), `setopts.rs` is its
  own module. Split by responsibility; every public item documented.
- **One owner for the open path.** `{open,…}` routes through the `prim_file:open/2` floor; the
  `setopts` pipeline is a faithful reproduction of `file_io_server`'s, never a simplification.
## Known Issues / Test Strategy
- **A request sent to a retired device is never drained, so its sender parks forever.** `service_file_requests` scans only `self.file_services.server()` + `self.file_services.devices` keys (`file_server.rs:241-243`); `close` — and any raising `{get_until,…}` (above) — removes the pid via `devices.remove(&service)` (`:321-323`). A later `{file_request,…}`/`{io_request,…}` to that pid then sits in its mailbox with **no** drain step, and its sender waits in `file_request/2`'s untimed `receive` (`lib/kernel/src/file.erl:3068-3077`), so the run quiesces to the typed `blocked()` gap instead of surfacing the orphaned request.
- **`serve_file_message`/`serve_server_request` silently consume an unrecognised envelope** (`Ok(None)`: `file_server.rs:300-302`/`:314`/`:316-318`/`:328-356`/`:367-372`) — the oracle's `{noreply,State}`. A well-formed `{file_request,…}`/`{io_request,…}`/`{'$gen_call',…}` never takes it (the `file.erl`/`io.erl` wire uses a plain monitor reference as `ReplyAs`, never the `[alias|Ref]` form, which is instead a typed error at `:341-347`), but a malformed envelope would park its sender with no reply and no `DOWN`.
- [`tests.rs`](tests.rs) pins the wire decoders (`improper_list_head`'s `[alias|Ref]` detection,
  `seek_from`'s `bof`/`cur`/`eof`/bare-integer forms, `iodata_bytes`, `tuple_fields`), the errno→atom
  table, the `binary`/`list` open-mode parse, the write path (`{write_file,…}`, a write-mode
  open/`put_chars`/close round trip, the O_CREAT and `{error,badarg}`-encoding cases) and the
  read-mode surface; the service is proven end to end over real temporary files/directories by
  `../../tests/file_server.rs` (every rendering cross-checked against the live `erl`).
  `tests.rs` also holds the `runtime()`/`build()` fixtures the sibling test modules import.
- `setopts.rs`'s own docs carry the oracle probe results; its coverage is exercised through
  [`tests.rs`](tests.rs) (`getopts_reports_the_open_modes`, `setopts_updates_the_mode_and_encoding`,
  `setopts_reports_enotsup_and_badarg`, `setopts_on_a_utf16_encoding_is_the_typed_gap`,
  `setopts_on_a_non_list_is_the_request_error`, `setopts_changes_what_the_next_read_decodes`, and
  the live-oracle cross-check `the_getopts_answers_match_the_live_oracle`).
- `get_until/tests.rs` pins the `{get_until, …}` request: the protocol decoders (`interpret`,
  `leftover_from`), `Runtime::open_encoding`, the rejections, and the request end to end through
  `Runtime::serve_device_request` over a real temporary file with the real `erl_scan:tokens/4` on
  the engine (the stashed leftover is fed to the second request), and the failure
  classification: a typed capability gap (`lists:flatten/2`'s non-protocol return value)
  leaves the device registered and is never reported as `{error, Func}`, a genuine `Mod:Func`
  raise (`lists:nth/2` on `[]`) still replies `{error, Func}` **and** retires the device, and
  `EngineContinuation::classify/1` itself separates the two. `get_line/tests.rs` pins the
  `LineContinuation` side.
- `file_info.rs`'s units pin both reply paths and the `badarg` path contract.
- Oracle values are quoted in each test's doc comment and were re-verified against the system `erl`
  (`erl -noshell -eval '…'`, OTP 29.1.1 / erts-17.1). Fixtures needing the toolchain skip **loudly**.
## Routing Table
- `../file_server.rs` → the `FileServices` service process, its `Device` io-devices, the
  request/reply wire, the write path (`{write_file,…}` + the delegating `{open,…}`), the decode
  helpers and the errno→atom table
- `setopts.rs` → the io-device read-mode surface: `getopts` + `{setopts, Opts}` and the
  `expand_encoding`/`substitute_negations`/`unfold`/`check_valid_opts`/`valid_enc` reproduction
- `tests.rs` → the colocated unit suite for `../file_server.rs` + the shared `runtime()`/`build()`
  fixtures the sibling test modules import
- `file_info.rs` → `{read_file_info,_[,Opts]}`/`{read_link_info,_[,Opts]}` (the `stat`/`lstat`
  failure half and the `#file_info{}` success half) + its unit tests
- `get_until.rs` (+ `get_until/tests.rs`) → the `{get_until, …}` io-device request
  (`get_until::serve` + the engine-backed `EngineContinuation` implementing
  `beam_io::io_device::Continuation`)
- `get_line.rs` (+ `get_line/tests.rs`) → the `{get_line, …}` io-device request (`get_line::serve` +
  the `LineContinuation`)
- `Runtime::service_file_requests` → the scheduler-pass step (owned by `../../runtime/scheduler.rs`)
- `../../../../erts/emulator/beam/` → legacy C ground truth (read-only); `../../../../erts/emulator/nifs/`
  → `prim_file`'s C NIFs; `../../../../erts/preloaded/src/prim_file.erl` → the wrappers;
  `../../../../../lib/kernel/src/file_server.erl` + `file_io_server.erl` → the request surface and
  the read-mode pipeline
