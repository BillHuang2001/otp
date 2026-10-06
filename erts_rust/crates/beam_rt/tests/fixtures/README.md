# `beam_rt` test fixtures

`mailbox.beam` is a committed oracle fixture: a real `.beam` produced by the **system
Erlang** toolchain (OTP 29 / `erts-17.1`), which is also the differential oracle. It is
the receive/message end-to-end image — `tests/mailbox.rs` (the runtime slice) and the
receive-seam unit tests in `src/context.rs` run it through the real `RuntimeContext`.

## Regeneration

The fixture is compiled with `+deterministic`, which strips the embedded source path
from the `CInf` (compile info) and `Dbgi` (debug info) chunks — the committed `.beam`
is therefore **path-free**. Compile from a neutral scratch dir under `$TMPDIR` so
nothing about the local checkout is recorded:

```sh
SCRATCH="$TMPDIR/beam_rt_mailbox_fix"
mkdir -p "$SCRATCH/mailboxgen"
cp mailbox.erl "$SCRATCH/mailboxgen/"
( cd "$SCRATCH/mailboxgen" && erlc +deterministic mailbox.erl )
cp "$SCRATCH/mailboxgen/mailbox.beam" mailbox.beam   # writes mailbox.beam (740 bytes)
```

`mailbox.erl` (the input, also committed):

```erlang
-module(mailbox).
-export([selfsend/0, wait/0, scan/0]).

selfsend() ->
    self() ! hello,
    receive X -> X end.

wait() ->
    receive X -> X end.

scan() ->
    self() ! 42,
    self() ! {bar, 7},
    receive {bar, X} -> X end.
```

Any byte change in the fixture invalidates the expected values in `tests/mailbox.rs`
and `src/context.rs`; re-derive them from the system `erl`, do **not** guess.

## Oracle values (verified against `erl -noshell`)

| export | oracle expression | value |
|---|---|---|
| `selfsend/0` | `mailbox:selfsend()` | `hello` |
| `scan/0` | `mailbox:scan()` | `7` |
| `wait/0` | `mailbox:wait()` | blocks (empty mailbox) |

`selfsend/0` compiles to `bif self` + `send` + `loop_rec` + `remove_message` (with a
`wait` fail branch); `scan/0` additionally emits `loop_rec_end`, because `42` fails the
`{bar, X}` test and the scan must advance to the second message. `wait/0` is a bare
`loop_rec`/`wait` loop and therefore suspends on an empty mailbox.

`marked/0`-style exports that would drive the `recv_marker_*` prologue are deliberately
**not** in the fixture: the prologue is emitted for a receive that matches on a
reference created before the send, which needs `erlang:make_ref/0` — a `beam_bifs` BIF
that Milestone 1 does not register. The marker seam is covered directly instead (the
unit tests in `src/mailbox.rs` and `src/context.rs`).
