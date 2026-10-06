# `beam_rt` test fixtures

`mailbox.beam` and `sched.beam` are committed oracle fixtures: real `.beam` images
produced by the **system Erlang** toolchain (OTP 29 / `erts-17.1`), which is also the
differential oracle.

* `mailbox.beam` is the receive/message end-to-end image — `tests/mailbox.rs` (the
  runtime slice) and the receive-seam unit tests in `src/context.rs` run it through the
  real `RuntimeContext`.
* `sched.beam` is the **sender** image for the cross-process scheduler test
  (`tests/scheduler.rs`): it sends the non-matching head `42` and then `{bar, N + 7}`,
  the message `recv:scan/0` (from `../beam_engine/tests/fixtures/recv.beam`) matches —
  something `recv:bomb/1` (a constant `42` only) cannot do.

## Regeneration

Both fixtures are compiled with `+deterministic`, which strips the embedded source path
from the `CInf` (compile info) and `Dbgi` (debug info) chunks — the committed `.beam`
is therefore **path-free**. Compile from a neutral scratch dir under `$TMPDIR` so
nothing about the local checkout is recorded:

```sh
# mailbox.beam (740 bytes)
SCRATCH="$TMPDIR/beam_rt_mailbox_fix"
mkdir -p "$SCRATCH/mailboxgen"
cp mailbox.erl "$SCRATCH/mailboxgen/"
( cd "$SCRATCH/mailboxgen" && erlc +deterministic mailbox.erl )
cp "$SCRATCH/mailboxgen/mailbox.beam" mailbox.beam

# sched.beam (592 bytes)
SCRATCH="$TMPDIR/beam_rt_sched_fix"
mkdir -p "$SCRATCH/schedgen"
cp sched.erl "$SCRATCH/schedgen/"
( cd "$SCRATCH/schedgen" && erlc +deterministic sched.erl )
cp "$SCRATCH/schedgen/sched.beam" sched.beam
```

The repo-root `.gitignore` ignores `*.beam` globally, so a (re)built fixture must be
**force-added**: `git add -f sched.beam`.

`sched.erl` (the input, also committed):

```erlang
-module(sched).
-export([send2/2]).

send2(Pid, N) ->
    Pid ! 42,
    Pid ! {bar, N + 7},
    ok.
```

Any byte change in a fixture invalidates the expected values in the tests that load it;
re-derive them from the system `erl`, do **not** guess.

## Oracle values (verified against `erl -noshell`)

| export | oracle expression | value |
|---|---|---|
| `mailbox:selfsend/0` | `mailbox:selfsend()` | `hello` |
| `mailbox:scan/0` | `mailbox:scan()` | `7` |
| `mailbox:wait/0` | `mailbox:wait()` | blocks (empty mailbox) |
| `sched:send2/2` | a receiver matching `{bar, X}` after `sched:send2(R, 0)` | `7` |
| `sched:send2/2` | `sched:send2(R, N)` | returns `ok` |

`selfsend/0` compiles to `bif self` + `send` + `loop_rec` + `remove_message` (with a
`wait` fail branch); `scan/0` additionally emits `loop_rec_end`, because `42` fails the
`{bar, X}` test and the scan must advance to the second message. `wait/0` is a bare
`loop_rec`/`wait` loop and therefore suspends on an empty mailbox. `send2/2` emits two
`send`s and builds `{bar, N + 7}` from the **run-time** argument `N` (an ordinary
`copy_struct` across two processes, not a folded literal).

`marked/0`-style exports that would drive the `recv_marker_*` prologue are deliberately
**not** in either fixture: the prologue is emitted for a receive that matches on a
reference created before the send, which needs `erlang:make_ref/0` — a `beam_bifs` BIF
that Milestone 1 does not register. The marker seam is covered directly instead (the
unit tests in `src/mailbox.rs` and `src/context.rs`).
