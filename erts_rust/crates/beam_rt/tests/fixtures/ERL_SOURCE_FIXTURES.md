# `beam_rt` test fixtures — the `.erl` fixtures compiled at test time

The fixtures in this doc are **sources only**: no `.beam` is committed for them, and each suite `include_str!`-embeds (or copies) the source into a per-case scratch directory and compiles it there with the system `erlc`.
A fixture compiled into a scratch directory holding **only** that module is a safe single `-pa` for the oracle, which is why every suite here compiles into such a directory and never names `tests/fixtures/` as a whole (that directory holds many modules with colliding names and committed images).
The sources whose detail is not load-bearing are indexed by [`README.md`](README.md)'s `## Every other fixture file in this directory`, which names the suite that consumes each one.
The general skip rule — a missing toolchain is a loud `SKIP`, a missing committed fixture **fails** — is [`README.md`](README.md)'s `## Skip-loudly rule`; each section below adds only its own suite's specifics.

## The scheduled-route `epp` pair (`epp_forms.erl` / `epp_input.erl`, compiled at test time)

`tests/compile_file.rs::a_scheduled_process_parses_a_source_file_like_the_oracle` drives the
`epp:parse_file/3` read path from a **scheduled `run`** process, which is the route on which a
delivery-driven collection of the parked receiver is declined by the runner's park-window
relocation exemption instead of moving the receiver's live scan options:

* `epp_forms.erl` is the **driver**: `go/1` takes the input source file as a char list, runs
  `epp:parse_file(File, [], [])` and answers `{length(Forms), [element(1, F) || F <- Forms]}` — a
  pure `~w`-printing value, so `beam_rt run …` stdout and the oracle's
  `io:format("~w~n", [epp_forms:go(File)])` are directly comparable.
* `epp_input.erl` is the **input**: `-module(epp_input). -export([f/0]). f() -> 1.` — legacy
  syntax only (no maps, no binaries), so the scan options stay on the plain cons cell.
* The driver is compiled with the system `erlc` into a scratch directory (both files'
  **contents** are `include_str!`-embedded by the suite, so nothing is read relative to the
  test's CWD; the scratch dir is the only `-pa` given to either runtime).

Oracle (the same artifacts under the real system `erl`): `epp` prepends its own
`{attribute, _, file, _}` form, so the answer is the three source forms plus that attribute and
the trailing `eof`:

```sh
erl -noshell -pa "$DIR" -eval 'io:format("~w~n",[epp_forms:go("'$DIR'/epp_input.erl")]), halt(0).'
# {5,[attribute,attribute,attribute,function,eof]}
```

## The `{active, …}` socket-input fixture (`inet_active.erl`, compiled at test time)
`tests/inet_active.rs` pins the driver's **active-input channel** end to end (contract A.4): a
socket in `{active, true | once | N}` really delivers `{tcp, S, Data}` / `{udp, S, IP, Port, Data}`
and `{tcp_passive, S}` / `{udp_passive, S}` to its owner's mailbox, and a compiled `receive` gets
it. The single committed source `inet_active.erl` (module `inet_active`, exports `t1/0 … t7/0`) is
`include_str!`-embedded and compiled by the suite into a **per-case scratch directory** with the
system `erlc`, and only that directory is ever named on a `-pa`.

### The transport is the driver itself — exactly what `prim_inet` does
Every case opens its socket with `erlang:open_port({spawn_driver, "tcp_inet" | "udp_inet"},
[binary])` and drives it with **numeric** `erlang:port_control/3` requests, each offset by
`ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`16#03f1a300`): `INET_REQ_OPEN` 1, `INET_REQ_CONNECT` 3,
`INET_REQ_BIND` 6, `INET_REQ_SETOPTS` 7, `INET_REQ_ACCEPT` 26, `INET_REQ_LISTEN` 27. No
`gen_tcp`/`gen_udp`/`prim_inet` is involved, so the identical source runs under both runtimes with
nothing library-level that could differ. The helpers mirror the driver's own encodings:
* an option record is `[code, value_be32]` (`{mode,list}` = `[24,0,0,0,0]`, `{mode,binary}` =
  `[24,0,0,0,1]`, `{packet,2}` = `[23,0,0,0,2]`), with a trailing **16-bit counter only for
  `{active, N}`** (`{active,true}` = `[22,0,0,0,1]`, `{active,once}` = `[22,0,0,0,2]`,
  `{active,2}` = `[22,0,0,0,3,0,2]` — `prim_inet:enc_opt_val/2`'s exact split);
* `bind` answers `[1] ++ <<Port:16/big>>` (the kernel-assigned port, bound on `127.0.0.1` with
  port `0` — the address record `[4]` is `INET_AF_LOOPBACK`);
* `connect` normally completes inside the call (a bare `[1]`) and otherwise answers
  `[1] ++ <<Id:16/big>>` plus `{inet_async, P, Id, ok}`; `accept` is always the async form, ending
  in `{inet_async, L, Id, {ok, Port2}}`;
* a send is `prim_inet:send/3`'s wire shape — `[<<Size:16, TagBin/binary>>, Data]` where `TagBin` is
  `term_to_binary(Ref, [local])`, the driver echoing the decoded tag as the fourth element of
  `{inet_reply, S, ok, Tag}`; a datagram's destination record is
  `[INET_AF_INET, Port:16, A, B, C, D, 0:32]` (a zero ancillary-data length).

### Every case is self-contained in one VM
The fixture binds a listener, reads the assigned port out of the `bind` reply and opens its **own
peer** socket in the same run — so no external peer is needed and the bytes the owner receives are
bytes a real loopback socket carried. The last line of every case is `ok` (the `run` action's
printed result on this runtime's side, the fixture's own `~w`-printed return value on the oracle's).

### What each case pins, and the oracle's rendering of it (OTP 29.1.1 / erts-17.1)
`'$port'` stands for the port term (a per-runtime table slot) and `'$source'` for a UDP source port
(a per-run value); **everything else is compared byte for byte**.

| case | configuration | pinned shape | oracle output |
|---|---|---|---|
| `t1` | `{active,true}` `{mode,binary}`, peer writes `<<"hello">>` then `<<"again">>` | every write becomes a `{tcp, S, Data}` | `t1_first={tcp,'$port',<<104,101,108,108,111>>}`, `t1_socket_is_port=true`, `t1_second={tcp,'$port',<<97,103,97,105,110>>}` |
| `t2` | `{active,2}` | two deliveries, exactly one `{tcp_passive, S}`, silence, then a re-armed delivery | `t2_one={tcp,'$port',<<111,110,101>>}`, `t2_two={tcp,'$port',<<116,119,111>>}`, `t2_passive={tcp_passive,'$port'}`, `t2_silent=timeout`, `t2_rearmed={tcp,'$port',<<116,104,114,101,101>>}` |
| `t3` | `{active,once}` | one delivery, a withheld spent-grant write, the re-arm that hands it over, and the fresh write at the next re-arm (distinct payloads, so the wrong write fails) | `t3_first={tcp,'$port',<<104,101,108,108,111>>}`, `t3_silent=timeout`, `t3_second={tcp,'$port',<<98,98,98,98>>}`, `t3_silent_again=timeout`, `t3_third={tcp,'$port',<<99,99,99,99>>}` |
| `t4` | `{mode,binary}` vs `{mode,list}`, same `<<"hello">>` | binary vs charlist for one payload | `t4_binary={tcp,'$port',<<104,101,108,108,111>>}`, `t4_list={tcp,'$port',[104,101,108,108,111]}`, `t4_list_is_hello=true` |
| `t5` | `{packet,2}`, peer writes `<<5:16,"hello">>` | the framing header is stripped before delivery | `t5_stripped={tcp,'$port',<<104,101,108,108,111>>}`, `t5_body_is_hello=true` |
| `t6` | UDP `{active,true}`, one datagram | `{udp, S, {127,0,0,1}, SourcePort, Data}` with the peer's real bound port | `t6_datagram={udp,'$port',{127,0,0,1},'$source',<<104,105>>}`, `t6_source_is_peer=true`, `t6_source_is_real=true`, `t6_address={127,0,0,1}` |
| `t7` | listener `{packet,2}` + `{mode,list}` + `{active,2}`, one accepted connection | the listener's options reach the accepted port (charlist, header stripped), and its own re-armed `{active,2}` reports `{tcp_passive, S}` | `t7_accepted_is_port=true`, `t7_first={tcp,'$port',[104,101,108,108,111]}`, `t7_second={tcp,'$port',[119,111,114,108,100]}`, `t7_passive={tcp_passive,'$port'}` |

The two content rules the table encodes: `{mode,binary}` must be **asked for** (the driver's
effective default for a `[binary]` port is a *list* — measured, which is why `prim_inet:open/5`'s
`[binary]` alone is not enough), and `{active, …}` is the one option block an accepted socket does
**not** inherit (the C copies everything else), so `t7` re-arms the accepted port itself.

### A spent grant keeps the peer's bytes in the socket
A write that arrives while an `{active,once}` / `{active,N}` grant is spent is delivered to nobody:
the read interest is disarmed, so the bytes stay in the kernel until a re-arm's inline read hands
them over, and that delivery spends the re-arm's own grant. `t3` pins the whole sequence with
distinct payloads (`t3_second` is the withheld `<<"bbbb">>`, never the fresh `<<"cccc">>` written
after the re-arm, which only `t3_third` delivers). `t2` writes nothing during its passive window and
re-arms `{active,true}`, because the counter's own `{tcp_passive, S}` is what that case pins
(`src/port/active_input_e2e_tests.rs` pins the counter's withheld-write shape).

### Skip-loudly rule
Each test skips **loudly** (`beam_rt inet_active SKIPPED` plus a return, never a silent pass) when
`erlc` or `erl` is missing — a **toolchain** absence the host may legitimately have. A fixture that
fails to compile, a non-zero exit, a run that outlives the harness's 30 s probe bound
(`support::PROBE_TIMEOUT`) or any line that differs from the oracle's is a **failure**: the fixture
is part of this repository.

## Execution-position fixtures (`recv.erl` / `pos_self.erl`, never committed — compiled at test time)

`tests/position.rs` (contract A.8, `erlang:process_info/1,2`'s execution-position items: the
in-machine BIF door) drives two shapes through these sources. Both are compiled at test time by the
system `erlc` **with the scratch directory as cwd and the bare file names as arguments**, so the
name recorded in each `.beam`'s `Line` chunk is exactly `"recv.erl"` / `"pos_self.erl"` — which is
what the oracle reports in `current_location`/`current_stacktrace` (an absolute or repo-relative
argument would record *that* string instead and the two would disagree):

* `recv.erl` is the **probed-process** fixture. `wait/0`'s clause head sits on **line 12** so a
  process parked in its `receive` reports the oracle's `{line,12}`; `probe_b/1` reads the four
  items of a parked process (`current_function`, `initial_call`, `current_location`,
  `current_stacktrace`) in that order; `main/0` is `probe_b(self())` — the **self** query, asking
  for the position of the process *running it* (the shape a scheduler pass cannot answer from its
  runner state, because the executing process's machine is taken out of it for the whole slice —
  hence the in-machine door).
* `pos_self.erl` is the **self-query driver**: `go/1` runs `recv:main/0` inside the *scheduled*
  process and sends `{result, R}` to the parent it is given; `sink/0` is the built runtime's parent
  — it receives that report and returns it, so the test reads it back through
  `beam_rt::Runtime::process_result`. (A spawned export process's return value is unobservable in
  Erlang, so message passing is the only way to observe it; the oracle's parent is the `erl`
  probe's own root process, which receives the same message.)

Oracle values (verified against `erl -noshell`, erts-17.1 / OTP 29.1.1):

```sh
D=$(mktemp -d); cp recv.erl pos_self.erl "$D"; (cd "$D" && erlc recv.erl pos_self.erl)

# a process parked in recv:wait/0, probed from a second process
erl -noshell -pa "$D" -eval 'P = spawn(recv, wait, []), timer:sleep(50),
    io:format("~p~n", [process_info(P, [current_function, initial_call,
        current_location, current_stacktrace])]), halt(0).'
# [{current_function,{recv,wait,0}},
#  {initial_call,{recv,wait,0}},
#  {current_location,{recv,wait,0,[{file,"recv.erl"},{line,12}]}},
#  {current_stacktrace,[{recv,wait,0,[{file,"recv.erl"},{line,12}]}]}]

# recv:main/0's **self** query, from compiled code
erl -noshell -pa "$D" -eval 'Self = self(), _ = spawn(pos_self, go, [Self]),
    R = receive {result, X} -> X end, io:format("~p~n", [R]), halt(0).'
# [{current_function,{recv,probe_b,1}},
#  {initial_call,{pos_self,go,1}},
#  {current_location,{recv,probe_b,1,[{file,"recv.erl"},{line,18}]}},
#  {current_stacktrace,[{recv,probe_b,1,[{file,"recv.erl"},{line,18}]},
#                       {pos_self,go,1,[{file,"pos_self.erl"},{line,24}]}]}]
```

(The `recv:main/0` frame is elided by its tail call into `probe_b/1`, exactly as the oracle elides
it; the test compares the frame **count** and the **head** frame of the stacktrace, not the second
frame.)

Documented divergences of the self-query, recorded in `tests/position.rs` and `M1_GAPS.md`: the
machine's `initial_call` is the process's **entry MFA**, where the oracle answers
`{erlang,apply,2}` for some spawn shapes (the scheduled-export shape above agrees); and a **nested**
fun-applied run started inside the published window reads the **outer** machine's position for the
same pid, because the published slot is runtime-wide rather than per-`RuntimeContext`.

## `zlib` fixtures (compiled at test time, never committed as `.beam`)

The six `zlib_*` sources below are the `zlib` images for the four `zlib` suites.
All are source only, all are compiled by the system `erlc` at test time into a per-fixture scratch directory holding only that module, and all are skipped **loudly** when `erlc` is missing.

### `zlib_stream_levels_fixture.erl` — compiled at test time, never committed as a `.beam`
`zlib_stream_levels_fixture.erl` is the **`zlib` per-level / flush / init-form** image for
`tests/zlib_stream_levels.rs`, driven through the scheduler `run` route
(`beam_rt run <scratch>/zlib_stream_levels_fixture.beam zlib_stream_levels_fixture:<f>/0`). Like
`zlib_stream.erl` it is **source only**: the suite compiles it into a per-fixture scratch directory
holding only this module (a safe single `-pa` for the oracle) with the system `erlc` at test time.
Six exported functions cover the six surfaces — `multi_sync/0` (the four-`sync`-then-`finish`
transcript per compression level 0–9 with its per-call byte-count vector, total and md5),
`flush_modes/0` (the whole payload with each flush), `init_forms/0` (`deflateInit/1,2,6`),
`reset_mid_stream/0` (`deflateReset/1`), `deflate_params/0` (`deflateParams/3`) and `round_trip/0`
(deflate → `inflate/2`).
Every function **prints** its transcript with `io:format/2` and returns `ok`, never a binary: the
`run` route renders its result with `~w`, and this runtime's binary printing is `<<55,56,…>>` where
the oracle prints `<<"AB">>` for printable content (a pre-existing printer limitation, unrelated to
`zlib`). Sizes are integers and digests are `binary:encode_hex/1` charlists, and the payload's own
`bytes`/`md5` line pins it to the same 1 024 bytes the `-eval` probes build. A missing `erlc` skips
the `run`-route half **loudly** (the `-eval` half still runs).
### `zlib_stream.erl` — compiled at test time, never committed as a `.beam`
`zlib_stream.erl` is the **`zlib` stream-API** image for `tests/zlib_stream_dict.rs`, driven through
the scheduler `run` route (`beam_rt run <scratch>/zlib_stream.beam zlib_stream:<f>/0`). Unlike the
committed images above it is **source only**: the suite copies it into a scratch directory and
compiles it there with the system `erlc` at test time (the file is the source of truth, and a
committed `.beam` would age independently of it). Five exported functions cover the five cases:
`params_stash/0` (`deflateParams/3`'s stash path), `dict_protocol/0` (the `need_dictionary`
protocol), `safe_inflate_chunks/0` (`safeInflate/2` chunking), `reset_and_dictionary/0`
(`inflateReset/1`, `inflateGetDictionary/1`, `deflateReset/1`) and `truncated_stream/0` (a stream
missing its Adler trailer is only caught by `inflateEnd/1`).

Every function returns a term, **never a binary**: the `run` route prints its result with `~w`, and
this runtime's binary printing is `<<55,56,…>>` where the oracle prints `<<"AB">>` for printable
content (a pre-existing printer limitation, unrelated to `zlib`). Hex digests therefore travel as
`binary_to_list(binary:encode_hex/1)` integer lists and payloads as sizes plus `=:=` booleans; the
scratch directory holds **only** this module, so it is also a safe single `-pa` for the oracle.
A missing `erlc` skips the `run`-route half **loudly** (the `-eval` half still runs).
### `zlib_one_shot_fixture.erl` — compiled at test time, never committed as a `.beam`
`zlib_one_shot_fixture.erl` is the **one-shot `zlib`** image (the six convenience functions
`compress/1`, `uncompress/1`, `zip/1`, `unzip/1`, `gzip/1`, `gunzip/1`) for
`tests/zlib_one_shot.rs`, driven through the scheduler `run` route
(`beam_rt run <scratch>/zlib_one_shot_fixture.beam zlib_one_shot_fixture:<f>/0`). Like
`zlib_stream.erl` it is **source only**: the suite compiles it into a scratch directory with the
system `erlc` at test time. Five exported functions cover the five surfaces — `goldens/0` (the
three named hex goldens), `fast_paths/0` (each function's bytes plus the three round trips over
the six-payload table), `wrapper/0` (`deflateInit/6` + `deflate(…, finish)` + `deflateEnd` +
`close`, at window bits 15 / -15 / 31, against the one-shot image), `libz/0` (the whole
`zlib:compress/1` image — A.9 libz byte exactness) and `concat/0` (`gunzip/1` over concatenated
gzip members).
Every function **prints** its transcript with `io:format/2` and returns `ok`, **never a binary**:
the `run` route renders its result with `~w`, and this runtime's binary printing is `<<55,56,…>>`
where the oracle prints `<<"AB">>` for printable content (a pre-existing printer limitation,
unrelated to `zlib`). Sizes are integers and digests/hex images are
`binary_to_list(binary:encode_hex/1)` charlists. The payload table (`payload/1`) is
byte-identical to the `-eval` expressions' `PAYLOADS_ERL` list, and the transcript's own
`payload <label> bytes <N> md5 <HEX>` line is what pins the two constructions together. The
scratch directory holds **only** this module, so it is also a safe single `-pa` for the oracle; a
missing `erlc` skips the `run`-route half **loudly** (the `-eval` half still runs).

### `zlib_guards.erl` / `zlib_guards_unnamed.erl` — compiled at test time, never committed as `.beam`
The **`zlib` guard-class / controlling-process** images for `tests/zlib_guards.rs`. Both are source
only and are compiled by the suite with the system `erlc` into a scratch directory holding **only
that module** (a safe single `-pa` for the oracle).
`zlib_guards.erl` exports `guard_atoms/0`, `classes/0` (the `data_error` / `stream_error` /
`not_initialized` / `already_initialized` / `badarg` families and the `ok` boundaries around them),
`controlling/0` (`not_on_controlling_process` and the `set_controlling_process/2` transfer both
ways, plus a dead pid and a non-pid) and `closed_controller/0` (`set_controlling_process/2` across
the fresh / ended / closed states, which all answer `ok` on both runtimes — the C's
`set_controller_nif` imposes no state precondition). `guard_atoms/0` is **documentation** of the
class set: it names the six guard atoms so they enter the module's atom table, but nothing depends
on that — a raise whose reason atom is absent from the raising module's atom table is catchable
anyway, because the runtime names a BIF raise's reason atom in its one shared table.
`zlib_guards_unnamed.erl` is that probe shape *without* the atom reference (`deflate_end_twice/0`,
`deflate_init_twice/0`, `after_finish/0`); the suite pins that it answers the **same** caught
`{exception,error,<atom>}` lines as the system `erl` oracle and as the named fixture. Nothing in it
may spell a guard atom — not even as a function name, since an exported name *is* an atom in the
table (measured).
Both fixtures print their transcript with `io:format/2` and return `ok`, so the compared observable
is the printed transcript and never the `run` route's `~w` result line; no binary is ever printed
(probes answer atoms, integers, booleans and small tuples).

## See Also

* [`README.md`](README.md) — the index, the committed `.beam` images, the `## Regeneration` recipe and the `## Oracle values` table.
* [`PORT_AND_DRIVER_FIXTURES.md`](PORT_AND_DRIVER_FIXTURES.md) — the C driver and port fixtures.
