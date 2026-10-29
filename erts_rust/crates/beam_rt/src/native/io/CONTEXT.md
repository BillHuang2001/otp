# `beam_rt/src/native/io/` — the `io` device floor (A.7)

## Intent
The `io` **device** floor: `../io.rs` resolves a device argument and answers the request natively, and this node's two children carry the pieces it needs — `convert.rs` the `unicode:characters_to_binary/3` byte conversion and `request.rs` the stream devices' request vocabulary.
It exists because every covered key is an `io.erl` wrapper over `o_request/2` (`io.erl:160-207`), which posts `{io_request, self(), Mref, Request}` to the resolved device and blocks in a `receive`; the default device is the group leader, where no io server runs, so that round trip never completes.
The device catalog is `../io.rs`'s `o_request/2` model (`Stream`/`File`/`Retired`/`CallingSelf`/`UnknownAtom`/`NoClause`/`DeadPid`/`Unreachable`), and the routing predicate every non-stream device takes is [`../../context/io_route.rs`](../../context/io_route.rs)'s.

## API Surface
### The key set
The floor covers **14** keys — `io:put_chars/1,2`, `io:nl/0,1`, `io:fwrite/1,2,3`, `io:write/1,2`, `io:format/3`, `io:request/1,2`, `io:requests/1,2` — while `request/3,4` are deliberately absent (`io.erl` exports no such arity, so the oracle's own `undef` stands) and `io_lib:format/2,3` is the renderer, which writes nothing.
The key table and the measured routing table are [`../../context/io_route.rs`](../../context/io_route.rs)'s.
### The three routes
An output call takes one of **three** routes: with a **stream** leader — the root pid, `user`, or the registered `standard_error` — the floor answers it itself through `RuntimeContext::io_device`/`io_stream` and the **registered** `io_lib:format/2`, so the request never leaves the runtime; with any other **live** leader the floor declines and OTP's own `io.erl` body runs in-machine, suspending on its `receive` and resuming on the device's `{io_reply, …}`; a **file device** is served synchronously, below.
### The device classes
The device classes (shared by **every** device-taking arm, `io:put_chars/1,2` included): omitted / `standard_io` / `user` / the caller's **group-leader pid** ⇒ the process's output stream (**explicitly** `Device::Stream` — the omitted case is the documented substitution that keeps the installed compiler's `io:fwrite/2` diagnostics printing); the caller **itself** (`self()`, classified **first**) or an atom **registered to the caller** ⇒ `error:calling_self`; `standard_error` (or its registered pid) ⇒ **fd 2**; a pid the native file service **owns** ⇒ `Device::File`, below; a **retired** file device, or any pid that is not a live process ⇒ `error:terminated`; any other **live** device pid ⇒ `Device::Unreachable`, which the floor **declines** on a route that may pump (so the in-machine body runs the round trip) and answers as the typed `unsupported` inside a scheduler slice; an **unregistered** atom ⇒ `error:badarg`; a non-atom/non-pid ⇒ the bare `function_clause`; the device is resolved **before** the payload is read.
### A file device
A **file device** is the pid `file:open/2` hands out, and `Runtime::serve_io_request`/`serve_io_request_value` answer its `{io_request, …}` surface (`{put_chars, Enc, Chars}`, `{put_chars, Enc, Mod, F, A}` through `apply/3`, and `{requests, […]}` — the last reply is the answer and the first `{error, _}` ends the run) plus its `{file_request, …}` one (`close`, `{position, At}`, `sync`, `truncate`, `{read_handle_info, Opts}`) under the device's own stream encoding, so `io:format(Fd, "~tp.\n", [Opt])` — `ct_run:install/2`'s variables file (`ct_run.erl:848`) — writes where the oracle writes, byte for byte (`[write]` is a **latin1** device, so a `unicode` request re-encodes `[229,228,246]` to `e5 e4 f6`, and `io:write/2` appends no newline of its own).
A request arriving for a **retired** device is answered the oracle's `{error, terminated}` for the runtime's lifetime rather than parked forever, so a request queued behind a `close` still gets its reply — `../../../tests/io_file_device.rs`.
That order is what keeps `erlc`'s diagnostics on stderr: `erl_compile.erl`'s `compile_cmdline1/0` sets `group_leader(whereis(standard_error), self())` in its worker and `compile.erl` prints with the omitted device, so a warning and a compile error both land on fd 2 with stdout empty, byte-identical to the system `erlc`.
### The arity truth
**`io:format/3` / `io:fwrite/3` are `(Device, Format, Args)`, never options-taking**: OTP has no such arity, so `io:format("~p~n",[[1,2]],[])` passes the format string as the device and is the bare `function_clause` on the oracle and here; the `{chars_limit,_}` options belong to `io_lib:format/3`.
### The byte conversion — `convert.rs`
The io-device protocol carries character data in a **named encoding** and every device transcodes it to its own, so reproducing the oracle's *bytes* means reproducing `unicode:characters_to_binary/3` — `String::from_utf8_lossy` is not it (it would replace every non-UTF-8 byte with `U+FFFD`).
Its three observable outcomes are all load-bearing: the encoded bytes; `{error|incomplete, Encoded, Rest}` as `Chars::NoTranslation`, which a device reports as `{error, {no_translation, In, Out}}`; and a **`badarg` raise** for a term that is not chardata at all, which kills the oracle's device process so its caller sees `{error, terminated}` (`Chars::BadArg`).
`unicode.erl` decodes **runs** of adjacent binary/bitstring elements as one UTF-8 stream (an integer element in between breaks the run), so `[<<195>>, <<169>>]` is the one code point `U+00E9`, not an incomplete sequence — the walk keeps that grouping.

## Constraints
- `#![deny(unsafe_code)]` (crate-wide); terms come through `beam_engine::terms`, the printer is the **registered** `io:format/2` (`../native.rs::render`), never a private one.
- Return-based errors: `Option<Result<Eterm>>` from the floor; a device-class answer is the oracle's own `error:calling_self`/`error:terminated`/`{error,badarg}`/bare `function_clause`, and only a scheduler-slice round trip that cannot pump is the typed `unsupported` gap.

## Test Strategy
- Pinned end to end against the live oracle by `../../../tests/io_device.rs` (the device differential, the arity matrix, the `calling_self` entry-point matrix), `../../../tests/io_diagnostic_unmask.rs`, `../../../tests/io_leader_capture.rs` (group-leader capture, the `standard_error` leader, a dead leader, an `{error, boom}` reply), `../../../tests/eunit_capture.rs` (a passing `eunit` run prints only its summary; a failing one reports the test's captured output) and `../../../tests/io_file_device.rs` (file-device bytes, the latin1 re-encoding, a dead/closed device, the scheduler-slice boundary).
- `../io.rs` adds a colocated fd-2 probe whose child half runs in its own OS process, since the assertion is about what reached the descriptor.
- `../../../tests/scheduled_io_device.rs` pins the same device surface reached from a **scheduled** process, where the round trip really suspends and resumes.

## Routing Table
- `convert.rs` → `unicode:characters_to_binary/3` over the device path (`Chars::{Bytes,NoTranslation,BadArg}`, the consecutive-binary grouping).
- `request.rs` → the **stream** devices (`standard_error` and this runtime's output stream): the `{io_request, From, ReplyAs, Request}` server side, the request vocabulary and its honest residuals.
- `../io.rs` → the device classifier (`o_request`'s model), the 14 keys and the file-device arm into `../file_server.rs`'s `serve_io_request`.
- `../../context/io_route.rs` → the routing predicate (`is_output_call`, `io_routes_through_the_protocol`) the external-call seam consults.
- `../../native/file_server.rs` → the file-device request surface (`{io_request,…}`/`{file_request,…}`) the `Device::File` arm lands in.

## See Also
- [`../CONTEXT.md`](../CONTEXT.md) → the floor's dispatch order, the constraints and the not-implemented index.
- [`../file_server/CONTEXT.md`](../file_server/CONTEXT.md) → the service that owns a file device, its request wire and its encoding.
- `../../../../lib/stdlib/src/io.erl` + `../../../../lib/kernel/src/{file_io_server,standard_error}.erl` → the OTP wrappers and device servers (read-only ground truth).
