# `beam_rt/src/native/prim_file/` — the `prim_file` floor's fd-half children (A.7)

## Intent
The three submodules of the `prim_file` floor's **file-descriptor** half: `advise.rs` (the `advise/4` hint and its non-exported `advise_nif/4` stub), `sendfile.rs` (the userspace `sendfile/8` over the port-command seam) and `readahead.rs` (`read_line/1` over the descriptor's `prim_buffer` queue).
The modules themselves are the sibling files `../prim.rs` (the path half) and `../prim_file.rs` (the fd half), and the dispatch is `../native.rs::prim_file_call`'s alone — this node adds no second list.
The floor's served-key census, its typed gaps and its fall-through rules are [`../CONTEXT.md`](../CONTEXT.md)'s `prim_file` subsection; the per-entry ground truth is each module's rustdoc.

## API Surface
### The floor's shape (both halves)
`RuntimeContext::prim_file_call(function, arity, args) -> Option<Result<Eterm>>` over the host file system (`std::fs`/`std::env`); `prim_file` is a **preloaded** module whose BIFs are the C `prim_file_nif.c` NIFs, and the floor claims only the subset whose whole observable contract is pinned byte-for-byte against the oracle — everything else returns `None`, falling through to the preloaded `.beam` (an honest `erlang:nif_error(undef)`).
**Served — 39 of the 43 `(name, arity)` keys the dispatch arms**: the **path half** (`prim.rs`) `get_cwd/0,1`, `read_file/1`, `delete/1`, `rename/2`, `make_dir/1`, `del_dir/1`, `read_link/1`, `make_link/2`, `make_symlink/2`, `altname/1`, `list_dir/1`, `list_dir_all/1`, `read_file_info/1,2`, `read_link_info/1,2`; the **fd half** (`prim_file.rs`) `open/2`, `read/2`, `write/2`, `close/1`, `position/2`, `pread/2,3`, `pwrite/2,3`, `truncate/1`, `sync/1`, `datasync/1`, `write_file/2`, `set_cwd/1`, `ipread_s32bu_p32bu/3`, `get_handle/1`, `read_handle_info/1,2`, `advise/4`, the userspace `sendfile/8` and `read_line/1`, plus the module's **local, non-exported** stub `advise_nif/4` (reached only through `ExecutionContext::resolve_local_call`).
**Invariants.** Every path-only entry is a direct NIF call wrapped by `try … catch error:badarg -> {error,badarg}` in `prim_file.erl`, so a **bad path is always the value `{error,badarg}`, never a raise**; the C's errno REMAPs are reproduced explicitly (`unlink` `EISDIR`→`EPERM`; `rename`/`rmdir` `ENOTEMPTY`→`EEXIST`, and the process's own non-empty cwd → `EINVAL`); `encode_path/1` takes a **binary** (bytes verbatim, a NUL ⇒ badarg) or an **iolist of code points** flattened iteratively with an explicit stack and encoded UTF-8, so nesting depth cannot exhaust the Rust stack.
`open/2` hands out the oracle's `{file_descriptor, prim_file, Data}` map shape, follows the C's flag derivation (`read`→`O_RDONLY`; `write` alone→`O_TRUNC|O_WRONLY|O_CREAT`; read+write→`O_RDWR|O_CREAT`; `append`→`O_APPEND` and no `O_TRUNC`), silently ignores every unmodelled option exactly as the C does, and **serves** a `directory`-mode open; its descriptor build interns every name it needs **before** taking the shared atom table's handle (an intern under a live handle deep-clones the whole table — [`../../CONTEXT.md`](../../CONTEXT.md) *The one process-lifetime `Rc<AtomTable>` lease*), and that order is part of the observable contract (the descriptor map's key order).
**Typed gaps** (explicit `unsupported`, never a silent wrong value): `write_file_info/2,3`, `allocate/3`, `file_desc_to_ref/2`.
**Deliberately unclaimed** because the fall-through already reaches the oracle's own answer (an arm would *create* a divergence): `copy/2`, `write_file/3`, `delayed_close/1`, `internal_get_nif_resource/1`; `copy/3` works (pure Erlang).
### `advise/4` and the local `advise_nif/4` stub — `advise.rs`
`advise/4` is a **hint** — `posix_fadvise(2)` is never called and a live descriptor answers `ok`, which is the oracle's own observable behaviour.
Its **local, non-exported** stub `advise_nif/4` is what `file:sendfile/8` reaches at `prim_file.erl:433` (a LOCAL call, so `call_ext` never sees it; `ExecutionContext::resolve_local_call` is the one seam).
`LOCAL_KEYS` is one key only (`("advise_nif", 4)`), gated by a module-name equality, so the predicate allocates nothing and reads no runtime state.
### `sendfile/8` — `sendfile.rs`
`sendfile/8` is a **userspace** sendfile because `beam_io` refuses the driver's `TCP_REQ_SENDFILE` (opcode 45); its non-empty-`Headers`/`Trailers` clause answers the **value** `{error,enotsup}` exactly as `prim_file.erl:438-439` does (so `file:sendfile/5` takes OTP's own Erlang `sendfile_fallback`), and one call's payload is capped at **16 MiB** — a larger extent is a typed capability gap decided before a byte is read or written, the documented residual.
**A `sendfile/8` payload is a `port_command` on an `inet` port, so it carries the driver's `[n:be16][tag]` caller prefix** — `prim_inet:send/3` is what normally frames it (`prim_inet.erl:587-589`) and the driver strips exactly one per command (`init_caller`, `inet_drv.c:1820-1890`) — and this route sends the C's **zero-length** tag `[0,0]`, which is consumed with no `{inet_reply, …}` staged and the payload written unchanged, so `port_info(Port, output)`/`statistics(io)` charge payload+2 exactly as the C's `prt->bytes_out += size` does (`io.c:1520`).
An **untagged** command is not a payload the driver merely trims: its first two bytes are read as an oversized tag, the strip fails and the driver signals `EINVAL`, which **kills the port while `port_command` still answers `Done`** — the bytes are silently lost, and the oracle behaves the same (`../../tests/port_command_caller.rs`).
### The read-ahead queue and `read_line/1` — `readahead.rs`
**A read-shaped descriptor owns a `prim_buffer` read-ahead queue** (`prim_file/readahead.rs`): `open/2` with the read shape registers one `prim_buffer::PrimBuffer` under the **descriptor id**, and `r_buffer` in the descriptor map **is** the `handle` reference — the oracle mints a second reference for the queue (`build_fd_data/5`'s `r_buffer => prim_buffer:new()`), so `display(Fd)` shows one ref here where the oracle shows two; that is the module's single documented divergence, and one table key is what makes `close/1` unable to leak either half.
Only `read_line/1` fills the queue (`read_line_1/4`'s `max(?MIN_READLINE_SIZE, r_ahead_size)` chunk loop), and the two siblings that must respect it do: `read/2` answers from the queue while it holds enough and concatenates a partial queue with a fresh host read otherwise, while `position/2` wipes it (`position_1/3`) and `{cur, Offset}` first shifts back by the queued byte count.
**`read_line/1` answers the oracle's four shapes** (measured: a 3-line file ⇒ `{ok,<<"alpha
">>}`, `{ok,<<"beta
">>}`, `{ok,<<"
">>}`, `{ok,<<"gamma">>}`, then `eof`; a `
` terminator answers a bare `
`; a drained descriptor keeps answering `eof`; a **write**-shaped descriptor is `{error,badarg}` and a closed one `{error,einval}`), and a line past the 64-byte on-heap limit is still a binary (the `make_binary` route).
`read_ahead` sizes the **chunk**, never a whole read — this runtime never over-reads on `read/2`'s behalf, which no answer and no position can observe.
**`flatten_iodata` accepts a binary improper tail** — `write/2`, `pwrite/2` and `write_file/2` flatten an iolist whose last element is a byte-aligned **binary** (`[<<"a">> | <<"b">>]`), which is what `dets`' segment writes hand it; a non-binary improper tail stays `{error,badarg}` exactly as `iolist_to_iovec` answers.
**Divergences:** `get_handle/1`'s fd *number* is this runtime's own; a path built from a file's raw bytes goes through `beam_codec::term::make_binary` on the pointer-stable `eval_heap` (no 64-byte limit).

## Constraints
- `#![deny(unsafe_code)]` (crate-wide): a descriptor's handle is never decoded, so `open/2` carries the registry id in the clear under the private `fd_registry_id` key.
- Return-based errors: an unsupported facility is `BeamError::unsupported`, a bad argument the *value* `{error,badarg}` where the Erlang wrapper catches it, and a host failure its `errno` tuple.
- A descriptor's file lives in `Runtime::open_files` (keyed by the runtime's own reference id), never in a process-global table: several runtimes can live in one thread.

## Test Strategy
- The opt-in/`#[ignore]`d evidence is `../../tests/prim_buffer.rs` (the queue's and `read_line/1`'s oracle-cross-checked matrix) and `../../tests/io_scan.rs` (`file:read_line/1` reached through the `io` device path).
- The untagged-command kill (`EINVAL`, no reply, port reaped) is pinned by `../../tests/port_command_caller.rs`.
- Each module's colocated suite (`advise.rs`, `sendfile.rs`, `readahead.rs`) drives the floor through `../test_support.rs`'s dispatch, so the served surface is what is pinned; a suite needing the system `erl` skips **loudly**.

## Routing Table
- `advise.rs` → `advise/4` + the local `advise_nif/4` stub (`LOCAL_KEYS`, `serves_local_call`, the three-way `AdviseOutcome` classification) and its unit tests.
- `sendfile.rs` → the userspace `sendfile/8`: the header/trailer refusal, the extent read and the port-command frame.
- `readahead.rs` → the `prim_buffer` queue, `read_line/1`, and the four queue operations `read/2` and `position/2` share.
- `../prim_file.rs` → the fd half's own entries (`open`/`read`/`write`/`close`/`position`/`pread`/`pwrite`/`truncate`/`sync`/`datasync`/`write_file`); `../prim.rs` → the path half and the `prim_file_call` dispatch.
- `../../../beam_io/` → the port/`inet` driver seam `sendfile/8` writes through; `../../../beam_term/` → `prim_buffer`'s queue types are not here (they are `../prim_buffer.rs`'s).

## See Also
- [`../CONTEXT.md`](../CONTEXT.md) → the floor's dispatch order, the served-key census, the not-implemented index.
- [`../prim_buffer.rs`](../prim_buffer.rs) → the built-in `prim_buffer` NIF library whose queue the read-ahead drives.
- `../../../../erts/emulator/nifs/unix/prim_file_nif.c` + `../../../../erts/preloaded/src/prim_file.erl` → the C NIFs and the Erlang wrappers (read-only ground truth).
