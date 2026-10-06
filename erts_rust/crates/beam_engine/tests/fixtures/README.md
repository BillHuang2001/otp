# `beam_engine` test fixtures

Committed oracle `.beam` fixtures driven by the engine's per-family integration
tests. Every fixture is a **real `.beam`** produced by the **system Erlang**
toolchain (OTP 29 / `erts-17.1`), which is also the differential oracle.

The repo-root `.gitignore` has a global `*.beam` rule, so fixtures are committed
with `git add -f`; `erts_rust/crates/beam_engine/.gitignore` re-includes
`tests/fixtures/*.beam`.

Any byte change in a fixture invalidates the expected values in the matching
test; re-derive them with `beam_disasm:file/1` plus the oracle, do **not** guess.

## Regeneration

Build from a *neutral* directory with `erlc +deterministic` so no worktree path
is baked into the committed binary (the `CInf`/`Dbgi` chunks record the source
path; `+deterministic` strips it, so a plain `erlc` would leak an absolute path):

```sh
mkdir -p "$TMPDIR/fixgen"
cp erts_rust/crates/beam_engine/tests/fixtures/<name>.erl "$TMPDIR/fixgen/"
env -C "$TMPDIR/fixgen" erlc +deterministic <name>.erl
cp "$TMPDIR/fixgen/<name>.beam" erts_rust/crates/beam_engine/tests/fixtures/<name>.beam
```

Verify no path leaked before committing (must print nothing):

```sh
strings erts_rust/crates/beam_engine/tests/fixtures/*.beam | grep -a -e /home -e /tmp -e genesis -e worker_
```

`+deterministic` changes only the `CInf`/`Dbgi` chunks relative to a plain
`erlc`; every other chunk (`Code`, `Line`, `Type`, `AtU8`, `ExpT`, `ImpT`,
`StrT`, `LocT`, `Attr`, `Meta`) stays byte-identical, so the expected values
below remain valid.

## `tuples.beam` (`tuples.erl`) — `tests/tuples_lists.rs`

| export | Erlang | opcodes |
|---|---|---|
| `mk/2` | `mk(A,B) -> {A,B}.` | `put_tuple2` |
| `first/1` | `case T of {A,_B} -> A end.` | `get_tuple_element` |
| `swap/1` | `case T of {A,B} -> {B,A} end.` | `get_tuple_element` ×2 + `put_tuple2` |
| `classify/1` | `case T of {a}->1; {a,b}->2; {a,b,c}->3 end.` | `select_tuple_arity` |

## `lists.beam` (`lists.erl`) — `tests/tuples_lists.rs`

| export | Erlang | opcodes |
|---|---|---|
| `build/2` | `build(H,T) -> [H|T].` | `put_list` |
| `head/1` | `case L of [H|_T] -> H end.` | `get_hd` |
| `tail/1` | `case L of [_H|T] -> T end.` | `get_tl` |
| `split/1` | `case L of [H|T] -> {H,T} end.` | `get_list` + `put_tuple2` |

`case` (rather than function-head clauses) keeps a bad argument on the `case_end`
path instead of a `func_info` clause failure, so the fixtures exercise only the
families the engine implements.

## `exceptions.beam` (`exceptions.erl`) — `tests/exceptions.rs`

Idiomatic `try ... catch ... end`, old-style `catch Expr`, a `case` with no
matching clause, a `badmatch`, an `if` with no true branch, nested `try`, and a
cross-frame unwind (`deep/1`), plus several uncaught-raise entry points. Drives
`try`/`catch`/`try_case`/`try_end`/`catch_end`/`badmatch`/`case_end`/`try_case_end`/`if_end`
and the catch-frame unwinding model.

## `floats.beam` (`floats.erl`) — `tests/floats.rs`

Ordinary Erlang whose compiled output exercises every opcode the float family
owns. The `is_float/1` guards are what make the compiler emit the *dedicated*
float opcodes (`add(A, B) -> A + B.` without them compiles to `gc_bif '+'` and
never reaches the family).

| function | opcodes exercised |
|---|---|
| `add/2`, `sub/2`, `mul/2`, `divide/2` | `fmove {x,N} {fr,N}` → `fadd`/`fsub`/`fmul`/`fdiv` → `fmove {fr,0} {x,0}` |
| `negate/1` | `fmove`, `fnegate`, `fmove` |
| `mixed/1` (`A * 2.0 + 1.0`) | `fconv`, two `fmove {float,K}` **literals**, `fmul`, `fadd` |
| `temps/4` (`A*B + C*D - A`) | four FR temporaries (FR0..FR3), `fmul`×2, `fadd`, `fsub` |
| `conv/1` (`I * 1.0`) | `fconv` from an untyped register, a float literal, `fmul` |
| `scale/2` | `fconv` from a provably-integer register next to an `fmove` |

`divide/2` with a zero divisor is a **non-finite** result, so it is the fixture's
`badarith` case (`fdiv` carries no fail label); `conv([])` is the `fconv`
non-numeric case.

Note `add(1, 2)` (an integer argument) hits the failed `is_float` guard, which jumps
to the function's `func_info` label — the oracle raises `function_clause` there, but
the M1 interpreter treats `func_info` as a no-op, so that path is deliberately *not*
exercised by `tests/floats.rs`.

## `bsstring.beam` (`bsstring.erl`) — `tests/bitstrings_extra_string.rs`

The `bs_create_bin` **`string` constant segment**: the compiler emits one whenever a
binary pattern concatenates a literal byte string with a dynamic part
(`<<"prefix", X/binary>>`). The literal goes into the `StrT` string table and the
segment carries its byte **offset** as the value operand and its byte **count** as the
size operand (`beam_ssa_codegen.erl:1970-1976`, `generators.tab:994-1001`). The
committed `.beam`'s `StrT` is `<<"abhicdabczxyQ">>` (`beam_lib:chunks(F, ["StrT"])`),
so the segment offsets are not simply 0/2/4 and the `reuse/1`/`combo/2` cases read a
substring out of the middle of the table.

| export | Erlang | what it exercises |
|---|---|---|
| `prefix/1` | `<<"ab", X/binary>>` | string at `StrT` offset 0 |
| `two/1` | `<<"ab", A:8, "cd">>` | two string segments, the second at offset 4 |
| `reuse/1` | `<<"abc", X/binary, "bc">>` | a trailing string reusing a suffix of the first (offset 7) |
| `mixed/1` | `<<A:4, "hi", 3:4>>` | string written at a **non-byte-aligned** bit position (total stays 24 bits) |
| `unaligned_tail/1` | `<<A:12, "Q">>` | string after a non-byte-aligned tail; the 20-bit result is a **bitstring** |
| `combo/2` | `<<A:8, "xy", B/binary, "z">>` | strings between two dynamic segments ("z" reuses offset 9) |
| `one/1` | `<<"z", A:8>>` | one-byte string |
| `at_zero/1` | `<<"hi", A:8>>` | string reused from offset 2 |
| `empty_prefix/1` | `<<"", X/binary>>` | the empty literal is dropped and rewritten as a 0-bit integer segment |

Expected values were derived with the oracle, e.g.
`erl -noshell -eval 'io:format("~w~n",[binary_to_list(bsstring:unaligned_tail(16#AAB))])'`
(a bitstring result is asserted bit-exactly through `beam_term::bitstring_parts`).

## `mapfx.beam` (`mapfx.erl`) — `tests/maps.rs`

| export | Erlang | opcodes |
|---|---|---|
| `get/1` | `case M of #{a := A, b := B} -> {A,B} end.` | `get_map_elements` (+ `put_tuple2`) |
| `has/1` | `case M of #{a := _, b := _} -> yes; _ -> no end.` | `has_map_fields` |
| `assoc/1` | `M#{c => 3}.` | `put_map_assoc` |
| `exact/1` | `M#{a := 9}.` | `put_map_exact` |
| `assoc_exact/1` | `M#{a := 9, c => 3}.` | `put_map_assoc` + `put_map_exact` |

Each update is preceded by the compiler's `test,is_map` guard, whose fail branch
builds `{badmap, M}` and calls `erlang:error/1` (never reached for a map argument).
`exact/1` on a map without key `a` is the fixture's `{badkey, a}` case: the
`put_map_exact` opcode carries `no_fail`, so the interpreter raises the reason
itself. `get/1` on a map without both keys falls to its `case_end` fail label
(`{case_clause, M}`). The map arguments are built by the engine's flat-map builder
and keyed by the module's atom table, so the fixtures' key operands compare equal.

## `bignum.beam` (`bignum.erl`) — `tests/bignum_compiled.rs`

Integers outside the 60-bit immediate range, in two shapes.

**Big integer literals as compiler operands.** Each zero-arity function compiles
to `move {integer, V} {x,0}` + `return`, with `V` inline in the code stream; the
engine must materialise it as an exact bignum (`terms::make_integer`).

| export | value | oracle repr |
|---|---|---|
| `lit_2pow59/0` | `576460752303423488` (2^59) | bignum |
| `lit_2pow59_plus1/0` | `576460752303423489` | bignum |
| `lit_nines/0` | `999999999999999999` | bignum |
| `lit_2pow61/0` | `2305843009213693952` (2^61) | bignum |
| `lit_neg_2pow59/0` | `-576460752303423488` (MIN_SMALL) | small |
| `lit_neg_nines/0` | `-999999999999999999` | bignum |
| `lit_neg_2pow59_minus1/0` | `-576460752303423489` (MIN_SMALL - 1) | bignum |
| `max_small/0` | `576460752303423487` (MAX_SMALL) | small |
| `min_small/0` | `-576460752303423488` (MIN_SMALL) | small |

**Runtime overflow** (the arguments make the expression unfoldable, so the
arithmetic runs in the BIF):

| export | Erlang | opcodes |
|---|---|---|
| `add/2` | `A + B.` | `gc_bif '+'` |
| `sub/2` | `A - B.` | `gc_bif '-'` |
| `mul/2` | `A * B.` | `gc_bif '*'` |
| `add_to_big/1` | `576460752303423488 + X.` | `gc_bif '+'` with a bignum **operand** |
| `big_minus/1` | `999999999999999999 - X.` | `gc_bif '-'` with a bignum operand |
| `scale_big/1` | `2305843009213693952 * X.` | `gc_bif '*'` with a bignum operand |

`add_to_big/1`/`big_minus/1`/`scale_big/1` are the case where a bignum the
*engine* materialised from the code stream is handed to `call_bif`.

There is deliberately **no constant-foldable overflow expression** in the fixture:
`erlc` folds e.g. `123456789012345678 * 98` into a 9-byte operand, which
`beam_loader` rejects with `system_limit` at load time.

The test's expected values were derived with the oracle, e.g.
`erl -noshell -eval 'io:format("~p~n",[bignum:mul(123456789012345678,123456789012345678)]),halt().'`
(the values above the immediate range are asserted bit-exactly against
`beam_term::big_parts`, and the representation — small vs bignum — is asserted
too, matching ERTS' canonical choice).

## `recv.beam` (`recv.erl`) — `tests/receive.rs`

The **receive/message** family: `send`, `loop_rec`, `loop_rec_end`, `wait`,
`wait_timeout`, `timeout`, `remove_message` and the four `recv_marker_*` opcodes.
Each export picks one shape the compiler emits for `receive` (verified with
`beam_disasm:file/1`; the family itself only *positions* the mailbox scan — the
`test`/`select_val` instructions after `loop_rec` do the pattern matching).

| export | Erlang | opcodes exercised |
|---|---|---|
| `wait/0` | `receive X -> X end` | `loop_rec` → `remove_message` → `wait` |
| `bomb/1` | `Pid ! 42, ok` | `send` |
| `sel/0` | `receive foo -> a; Bar -> Bar end` | `loop_rec`, `test` (the literal clause), `remove_message`, `wait` |
| `afterzero/0` | `receive _ -> got after 0 -> empty end` | `loop_rec` → `remove_message`; the empty path is a **direct jump to `timeout`** (a constant `after 0` is folded, so no `wait_timeout` is emitted) |
| `scan/0` | `receive foo -> foo; {bar,X} -> X end` | `loop_rec` … `loop_rec_end` (a non-matching head is scanned past) |
| `aftervar/1` | `receive foo -> foo after T -> empty end` | `loop_rec_end`, `wait_timeout` with a **runtime** `{y,0}` time operand, `timeout` |
| `marked/0` | `Ref = make_ref(), self() ! {Ref,7}, receive {Ref,R} -> R end` | all four `recv_marker_reserve`/`_bind`/`_use`/`_clear` (+ `call_ext make_ref`, `bif self`, `send`) |

Expected values (derived with the oracle — `erlc +deterministic recv.erl` in a
neutral dir, then
`erl -noshell -pa "$TMPDIR/fixgen" -eval '<expr>, halt().'`):

| expression | value |
|---|---|
| `Self=self(), Self ! hello, recv:wait()` | `hello` |
| `Self=self(), recv:bomb(Self), receive M -> M end` | `42` |
| `Self=self(), Self ! 99, recv:sel()` | `99` |
| `Self=self(), Self ! foo, recv:sel()` | `a` |
| `recv:afterzero()` (empty mailbox) | `empty` |
| `Self=self(), Self ! 42, Self ! {bar,7}, recv:scan()` | `7` |
| `Self=self(), Self ! foo, recv:scan()` | `foo` |
| `recv:aftervar(0)` (empty mailbox) | `empty` |
| `Self=self(), Self ! foo, recv:aftervar(0)` | `foo` |
| `recv:aftervar(5)` (empty mailbox) | `empty` (times out) |
| `recv:marked()` | `7` |

**Known gap.** Milestone 1 has no host timer, so `aftervar(5)` on an empty mailbox
does **not** return `empty`: `wait_timeout` suspends and the run reports
`RunOutcome::Wait` (the timer is the host's job). `tests/receive.rs` asserts that
honest outcome instead of the oracle value; every other row matches the oracle.
