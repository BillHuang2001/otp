# `beam_io::io_device` — the io-device request layer (`{get_until, Enc, Prompt, Mod, Func, XtraArgs}`)

## Intent
The **device side** of the incremental-reader io request, term-free and engine-free: the
`file_io_server` read loop (`get_chars/5` → `get_chars_empty/6` / `get_chars_notempty/6` →
`get_chars_apply/7`) plus `io_lib:get_until/4`'s `latin1`/`unicode` narrowing, tail carry-over and
reply cast. Reproduces every step *around* the one that runs Erlang code —
`apply(Mod, Func, [Cont, Chars | XtraArgs])` (`erl_scan:tokens/4` for the `io` module) — which is
the caller-supplied [`Continuation`] seam.

The crate-root `../CONTEXT.md` owns the subsystem framing; legacy ground truth (read-only) is
`../../../../lib/kernel/src/file_io_server.erl` (`io_request/2`, `get_chars*`, `cast`/`cast_binary`,
`read_size/1`), `../../../../lib/stdlib/src/io_lib.erl` (`get_until/4`) and
`../../../../lib/stdlib/src/erl_scan.erl` (`tokens/4`).

## API Surface
All re-exported from `mod.rs` and via `beam_io::prelude::*`.

- `IoDevice` (`device.rs`) — `file_io_server`'s `#state{handle,buf,read_mode,unic}`: `new(source,
  encoding, read_mode)` / `with_buffer` / `encoding`/`set_encoding` / `read_mode`/`set_read_mode` /
  `buffer`/`set_buffer`/`take_buffer`/`has_stashed_buffer` / `source_mut`, plus the request entry
  `get_until::<C: Continuation>(&mut C, out_encoding) -> Result<C::Reply>`.
- `READ_SIZE_BINARY = 8*1024`, `READ_SIZE_LIST = 128`, `read_size(FileDataMode)` — `?READ_SIZE_*`
  (`file_io_server.erl:39-40,815-818`), keyed off the read mode.
- `Encoding` (`encoding.rs`) — `Latin1 | Unicode`; `name()`/`from_name()` (exact, case-sensitive).
- `ByteSource` (`source.rs`) — the device's `read`: `read_chunk(size) -> Result<Option<Vec<u8>>>`
  (`None` = eof, **sticky**). Impls: `std::fs::File` (EINTR-retrying), `SliceSource<'a>`
  (`new`/`remaining`/`is_empty`), `EmptySource`.
- `Continuation<'Cont, Reply>` (`continuation.rs`) — `apply(cont, ContData, out_encoding) ->
  Result<Applied<Cont, Reply>>` + a **defaulted identity** `cast_reply(reply, is_data_binary,
  encoding)`. `ContData::{Chars(Vec<u32>), Eof}`; `Applied::{More(Cont), Done{reply, leftover}}`;
  `Leftover::{None, Binary(Vec<u8>), Chars(Vec<u32>)}` (flat, `Default = None`).
- `NoEngineContinuation` + `NO_ENGINE_CONTINUATION` — the not-installed default: every `apply` is
  the typed `unsupported` raise naming the missing `erl_scan:tokens/4` (never a fabricated reply).

## The request loop (frozen behaviour — mirror the oracle exactly)
- **Stashed-leftover-first**: a non-empty device buffer is fed to the continuation *before* any
  read and is cleared in the process (`device.rs:244-251`; `file_io_server.erl:586-589`).
- **Read sizes**: `read_size(read_mode)` per read (8 KiB binary / 128 list).
- **`{more,…}` loop**: `More` re-reads — the `latin1` clause via `get_chars_empty`
  (`device.rs:343-350`, the clause never carries a tail), the general clause via
  `get_chars_notempty` with the freshly computed incomplete tail prepended to the next read
  (`device.rs:405-413`, `:270-304`).
- **`latin1` fast path** (`device.rs:318-323`): taken **only** when the request's `Enc` is
  `latin1` *and* the device stream encoding (`unic`) is `latin1`; any other combination takes the
  general clause with `InEnc = unic`, `OutEnc = Enc` (`file_io_server.erl:628-691`).
- **Incomplete-UTF-8 tail**: `Decoded::Incomplete`/`Transcoded::Incomplete` yield the decoded
  prefix as the data and the trailing bytes as the tail to re-feed (`device.rs:368-398`,
  `encoding.rs:134-221`); `Invalid` with an empty prefix is the `invalid_unicode` exit; with a
  non-empty prefix it behaves like `Incomplete` (`file_io_server.erl:642-671`).
- **EOF**: applied as the atom `eof`; eof **with a non-empty pending tail** is the
  `invalid_unicode` stop and the tail stays in the device buffer (`device.rs:292-298`). A read
  error keeps the pending tail too (`device.rs:299-302`).
- **Done**: the reply is cast (`Continuation::cast_reply`) and the leftover is converted back to
  the stream encoding and joined with the incomplete tail — `latin1` via `cast_binary`
  (`device.rs:471-484`), general via `leftover_to_stream_encoding` + `tail` (`device.rs:488-507`,
  `file_io_server.erl:672-680,805-812`).
- **`io_lib:get_until/4` narrowing** (`device.rs:433-468`): the charlist handed to the user
  function is `unicode:characters_to_list(Data, utf8)` when `Enc = unicode`, else
  `binary_to_list(Data)`; `IsDataBinary` is derived from the **first** data of the request and
  carried (`Mm::Start`/`Mm::Cont`) — only `Start` inspects it.

## Constraints
- **Frozen inter-crate seam**: `Continuation`/`ContData`/`Applied`/`Leftover` and the `IoDevice`
  request entry are what `beam_rt` designs against — extend additively with defaults only.
- Term-free by design (workspace `../CONTEXT.md` §3): no `Eterm`, no engine, no allocation policy
  beyond `Vec`. Errors are `beam_common::{Result, BeamError}` only — never `unwrap`/panic/`todo!`
  on runtime data; a genuine capability gap is a typed `unsupported`, never a wrong result.
- No `unsafe` in this subtree (pure byte/code-point arithmetic + `std::io`).
- Files stay ~1000 lines; `device.rs` (507) is the biggest.

## Deviations from the oracle (deliberate, documented in the module docs)
- **The `{stop, Reason, {error, Func}, State}` boxing is not performed** — a raising continuation
  propagates its error unchanged; the reply wrapping needs `Mod`/`Func`, which the runtime owns.
- **`cast_reply` is defaulted to the identity** — the protocol's real continuations
  (`erl_scan:tokens/4`) return tuples, so `io_lib`'s list→binary cast (`io_lib.erl:1869-1887`) is
  the implementor's job.
- **Only `latin1`/`unicode` are modelled.** The tuple encodings (`{utf16,binary}`, `{utf32,little}`, …)
  that a device's `encoding` option also accepts are **refused, not guessed**; a caller must reject
  them before building a device (a capability gap for `unic`/`Enc` tuple forms).
- **`Leftover` is a flat `binary | charlist | nothing`** — a *deep* iolist `Buf` is not
  representable (every protocol continuation returns a flat buffer).
- **No output-encoding cache / no `erl_scan` internals** — the `erl_scan`-specific
  `invalid_unicode_error/4` position hack (`file_io_server.erl:695-707`) is not reproduced.
- The `("get_until", 5)` request is **not yet wired** in `beam_rt`: its
  `native/file_server.rs::serve_device_request` still answers the typed `unsupported` for every
  device request (see Handoff).

## Test Strategy
Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io`.
**40 in-module tests + 1 doctest**: `tests.rs` 25 (a scripted `ByteSource` + a scripted
`Continuation` pin every branch: read sizes, `{more,…}` re-reads, stashed-leftover-first, EOF,
the incomplete-tail carry in both read modes, `invalid_unicode` on both the empty-prefix and
eof-with-tail paths, the `latin1`/`unicode` clause selection incl. `Enc=latin1` on a `unicode`
device, every leftover shape incl. the `badarg` of a non-latin1 charlist, the reply cast for both
read modes, a read error, `NoEngineContinuation`, a raising continuation, state accessors),
`encoding.rs` 7 (latin1 byte-wise, UTF-8 incomplete/invalid incl. overlong/surrogate/`>U+10FFFF`
starters, the four transcode directions, `encode_code_points`, the typed sentinels),
`source.rs` 5 (successive/empty slices, `EmptySource`, a real file, a directory read as
`BeamError::Io`), `continuation.rs` 3.
No `erl` oracle case yet — the layer is verified against the Erl sources by reading, not by the
differential harness (the gate cannot express a device request today).

## Known Issues
- **The device buffer is not restored to the caller-provided tail on an error inside
  `get_chars_apply` reached from `get_chars_notempty`.** `device.rs:299-302` / `:292-298` set
  `self.buffer` on `get_chars_notempty`'s *own* error paths, but the `Ok(Some(..))` branch
  (`device.rs:279-288`) leaves `self.buffer` untouched (it is the empty buffer taken by
  `get_until`) while the oracle passes `State#state{buf=B}` into `get_chars_apply`, whose `catch`
  returns that same `State` — so on an `invalid_unicode` / raising continuation in a **second or
  later** `{more,…}` iteration the oracle keeps the previous `NewBuff` as the device buffer and
  this layer keeps `<<>>`. Only a residual-state divergence on an error path (the request already
  failed); observable if the same device is reused for a later request.
  Fix (in this crate): in `get_chars_notempty`'s success branch set `self.buffer = pending.clone()`
  before calling `get_chars_apply` (the `Done` branch overwrites it; the `More` branch re-sets it
  each iteration), mirroring the oracle's state threading exactly.

## Notes for Agents
- Read `../CONTEXT.md` (this crate's API Surface + Handoff) and
  `../../../../lib/kernel/src/file_io_server.erl:586-691` + `io_lib.erl:1849-1889` before changing
  the loop; the clause-selection and tail rules are subtle and easy to "simplify" wrongly.
- The **`latin1` clause is a fast path, not a default**: `Enc = latin1` on a `unicode` device must
  go through the general clause (transcode to `latin1`).
- `Decoded`/`Transcoded` carry *three* outcomes (complete / incomplete-tail / invalid); collapsing
  incomplete and invalid loses the tail and fabricates an `invalid_unicode`.
- `Encoding` has no tuple forms; reject them at the caller rather than defaulting.
- Build/test with the wrapper (`erts_rust/scripts/cargo.sh … --manifest-path erts_rust/Cargo.toml`);
  sandbox `$HOME` is read-only.

## Routing Table
- `mod.rs` → module docs, wiring, the public re-exports.
- `device.rs` → `IoDevice`, the read loop (`get_chars_empty`/`get_chars_notempty`/`get_chars_apply` +
  the `latin1`/`unicode` clauses), `read_size`/`READ_SIZE_*`, `middleman`, `cast_binary`,
  `leftover_to_stream_encoding`.
- `encoding.rs` → `Encoding`, the module-private `Decoded`/`Transcoded`, `bytes_to_code_points`,
  `decode`, `transcode`, `encode_code_points`, the `invalid_unicode`/`badarg` sentinels, the UTF-8
  scanner (`scan_utf8`/`next_utf8`/`decode_utf8_into`/`push_utf8`).
- `source.rs` → `ByteSource`, the `std::fs::File` impl, `SliceSource`, `EmptySource`.
- `continuation.rs` → `Continuation`, `ContData`, `Applied`, `Leftover`, `NoEngineContinuation`,
  `NO_ENGINE_CONTINUATION`.
- `tests.rs` → the device-loop suite.
- `../registry.rs` / `../port.rs` → the io request reaches a device through `beam_rt`, not this crate.
- `../../../beam_rt/src/native/file_server.rs` → the runtime's io-server floor that must adopt the
  `Continuation` seam (sibling — read-only).

## Handoff to `beam_rt` (owner: `beam_rt`, out of this node)
- Replace the `_ => error::unsupported(…)` fallback in
  `crates/beam_rt/src/native/file_server.rs::serve_device_request` with a `("get_until", 5)` arm
  that decodes `Enc`/`Mod`/`Func`/`XtraArgs`, builds `IoDevice::new(self.device_file(service)?, enc,
  read_mode)`, and installs an **engine-backed `Continuation`** (running the real `Mod:Func`;
  `cast_reply` = `io_lib`'s list→binary cast). Until then the request stays the typed `unsupported`.
- The same `unicode:characters_to_binary/3` conversion also unblocks `reply_get_chars`/
  `reply_put_chars`, which today hard-refuse every non-`latin1` encoding.

## See Also
- `../CONTEXT.md` → the crate's Intent/API Surface/Constraints and the `{get_until,…}` handoff.
- `../../CONTEXT.md` → crate-layer contracts; `../../../CONTEXT.md` → workspace contracts §3 (errors).
- `../../../../lib/kernel/src/{file_io_server.erl,prim_file.erl}`, `../../../../lib/stdlib/src/{io_lib.erl,io.erl,erl_scan.erl}` → ground truth.
