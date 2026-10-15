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

## `applyrecv.beam` (`applyrecv.erl`) — `tests/call_ext_apply.rs`

The runtime-valued `erlang:apply/3` fixture: `run(M, F, A) -> erlang:apply(M, F, A).`
Because `M`/`F`/`A` are parameters the compiler cannot fold, the body lowers to
`call_ext_only 3 {extfunc,erlang,apply,3}` (the shape the test re-verifies with
`beam_disasm:file/1`). Its applied callee blocks in a `receive`, so it is the
end-to-end evidence for the seam: the *calling* process must suspend in-machine
(`RunOutcome::Wait`) and resume to the oracle value, instead of folding into the
uncatchable `blocked()` at the call site.

| export | Erlang | opcodes exercised |
|---|---|---|
| `run/3` | `run(M, F, A) -> erlang:apply(M, F, A).` | `call_ext_only 3 erlang:apply/3` (the seam), `return` |
| `blocker/0` | `blocker() -> receive {reply, V} -> V end.` | `loop_rec`, `get_tuple_element`, `remove_message`, `wait` |

Expected value (the oracle transcript quoted in the `.erl`, OTP 29 / erts-17.1):

| expression | value |
|---|---|
| `Self=self(), P = spawn(fun() -> Self ! {done, applyrecv:run(applyrecv, blocker, [])} end), P ! {reply, 42}, receive {done, V} -> V end` | `42` |

The engine drives the same scenario in one machine (the mock mailbox plays the
sender): `run/3` resolves `applyrecv:blocker/0` in-machine, the callee's `wait`
suspends, and the resume on the injected `{reply, 42}` returns `42`.
`tests/call_ext_apply.rs` re-runs the transcript under the live system `erl` in a
second test (skipping loudly when there is no runnable oracle) so a fixture
regeneration cannot silently change the baked value.

## `bssmall.beam` (`bssmall.erl`) — `tests/bitstrings_extra_big.rs`

The **at-most-64-bit** integer fields whose value can still exceed `MAX_SMALL`
(`2^59-1`), plus the little-endian fields that are *not* a whole number of bytes. Every
read is `<<Y:N, _/bitstring>> = B` (an `ensure_at_least N 1` followed by an `integer`
command of `N` bits), so a whole-byte subject of at least `N` bits can be supplied from
the test side (the engine has no partial-bit subject builder); `u64x/1` is the plain
exhaustive `<<Y:64>>` form. `e256/1` writes `Value mod 2^256` so a (possibly bignum)
result can be pinned byte-exactly through the crate's public `terms::binary_bytes`.

| exports | shape |
|---|---|
| `u9/1` … `u62/1`, `u64/1` | unsigned big-endian `<<Y:N, _/bitstring>>` |
| `u64x/1` | the exhaustive `<<Y:64>>` form |
| `s59/1`, `s60/1`, `s61/1`, `s64/1` | signed big-endian |
| `u9le/1` … `u64le/1`, `s64le/1` | little-endian, non-byte-multiple fields |
| `c_u64/1` | a `case`-wrapped 64-bit read (the `case_clause` failure path) |
| `e256/1` | the construction helper `<<A:256>>` |

Expected values are the real system `erl` oracle's answers for the same inputs.

## `bsfailseg.beam` (`bsfailseg.erl`) — `tests/bitstrings_create_fail.rs`

The `bs_create_bin` **segment-failure surface**: the `{Class, Reason}` pair a caught
construction failure produces, so the reason atoms `badarg` and `system_limit` are
pinned end to end against the real `erl`.

`pinned_atoms/0` returns the literal list `[badarg, badmatch, system_limit, error]`. A
compiled module does **not** carry the runtime's exception reason atoms in its atom
chunk (`beam_lib:chunks("bsfailseg.beam", [atoms])` lists no `badarg`/`system_limit`; the
runtime keeps them globally interned, `erts/emulator/beam/beam_common.c:382-406`), and
the engine resolves a failure's reason atom from the *running module's* atom table
without substituting a different atom. `beam_loader` interns the atoms of
`pinned_atoms/0`'s `LitT` literal while decoding it, so every failure below lands as a
real `{error, ReasonAtom}` 2-tuple. (`bscreate.beam` pins none of them, which is why its
cases assert the typed `BeamError` instead; see `tests/bitstrings_extra_create.rs`.)

Each export is a single `bs_create_bin` with the `{f,0}` (`no_fail`) label (confirmed
with `beam_disasm:file/1`), wrapped in `try ... catch K:R -> {K,R} end`:

| export | Erlang | what it exercises |
|---|---|---|
| `bin_one/1` | `<<X:1/binary>>` | a `binary` segment needing one byte of a shorter source |
| `bin_all/1` | `<<X/binary>>` | the whole-tail `append` segment |
| `bin_all_unit/1` | `<<X/binary-unit:8>>` | the whole-tail unit check (`erts_bs_put_binary_all`) |
| `bin_fixed/1` | `<<X:8/binary>>` | a fixed `size*unit` = 64-bit `binary` segment |
| `bin_size/2` | `<<X:S/binary>>` | a dynamic `binary` size |
| `bin_big_size/1` | `<<X:(1 bsl 59)/binary>>` | a positive-bignum size (`2^59 = MAX_SMALL + 1`) |
| `utf8/1`, `utf16/1`, `utf16le/1`, `utf32/1` | `<<Cp/utfN>>` | a non-scalar codepoint / a non-integer value |
| `int_size/2` | `<<V:S/integer>>` | the shared dynamic-size validation |
| `int_fixed/1` | `<<V:3/integer>>` | a fixed 3-bit `integer` segment |
| `float_size/2` | `<<F:S/float>>` | a dynamic float width |

Expected values derived with the oracle in a neutral dir, e.g.
`erl -noshell -eval 'io:format("~p~n",[bsfailseg:bin_one(<<1:4>>)]),halt().'` ⇒ `{error,badarg}`:

| expression | value |
|---|---|
| `pinned_atoms()` | `[badarg,badmatch,system_limit,error]` |
| `bin_one(<<1:4>>)` | `{error,badarg}` |
| `bin_one(<<1:8>>)` | `<<1>>` |
| `bin_all(<<1,2>>)` | `<<1,2>>` |
| `bin_all_unit(<<1:4>>)` | `{error,badarg}` |
| `bin_all_unit(<<1:8>>)` | `<<1>>` |
| `bin_fixed(<<1,2,3>>)` | `{error,badarg}` |
| `bin_fixed(<<1,2,3,4,5,6,7,8>>)` | `<<1,2,3,4,5,6,7,8>>` |
| `bin_size(<<1:4>>, -(1 bsl 59))` | `{error,badarg}` |
| `bin_big_size(<<1:4>>)` | `{error,system_limit}` |
| `utf8/utf16/utf16le/utf32(-1)`, `(16#D800)`, `(16#110000)`, `(oops)` | `{error,badarg}` |
| `int_size(1, -1)` | `{error,badarg}` |
| `int_size(1, 1 bsl 59)` | `{error,system_limit}` |
| `int_size(1, 3)` | `<<1:3>>` |
| `int_fixed(oops)` | `{error,badarg}` |
| `int_fixed(5)` | `<<5:3>>` |
| `float_size(1.0, 0)`, `float_size(1.0, 24)` | `{error,badarg}` |
| `float_size(1.0, 32)` | `<<63,128,0,0>>` |

## `excpaths.beam` (`excpaths.erl`) — `tests/exceptions.rs`, `tests/exceptions_corpus.rs`

The "shared-atom host" fixture. Its atom chunk deliberately contains **none** of the
runtime's well-known exception atoms (`function_clause`, `case_clause`, `if_clause`,
`try_clause`, `badmatch`, `error`, `'EXIT'`, …; verify with
`beam_lib:chunks("excpaths.beam", [atoms])`), so the tests can only pass when the host
seeds those atoms into every loaded module — the host's analogue of the runtime's global
interning (`erts/emulator/beam/beam_common.c:382-406`). It sticks to opcodes the
Milestone-1 interpreter + families execute (`func_info`, `case_end`, `if_end`,
`try_case_end`, `bs_start_match3`/`bs_match`/`bs_get_position`/`bs_set_position`/
`bs_get_tail`, `badmatch`, `try`/`try_case`/`try_end`, `catch`/`catch_end`,
`build_stacktrace`, `put_tuple2`, `init_yregs`, plain calls/returns).

`tests/exceptions.rs` calls the raising primitives uncaught (asserting the propagated
error) and the catching wrappers (asserting the `{Class, Reason}` pair);
`tests/exceptions_corpus.rs` loads the same file as its corpus fixture.

Derived with the oracle (system `erl`):

| expression | value |
|---|---|
| `excpaths:fc(b)` | `error:function_clause` |
| `excpaths:cc(b)` | `error:{case_clause,b}` |
| `excpaths:ii(-1)` | `error:if_clause` |
| `excpaths:bm(a)` | `error:{badmatch,a}` |
| `excpaths:tc(b)` | `error:{try_clause,b}` |
| `excpaths:catch_fc(b)` | `{error,function_clause}` |
| `excpaths:catch_cc(b)` | `{error,{case_clause,b}}` |
| `excpaths:catch_ii(-1)` | `{error,if_clause}` |
| `excpaths:catch_bm(a)` | `{error,{badmatch,a}}` |
| `excpaths:catch_tc(b)` | `{error,{try_clause,b}}` |
| `excpaths:catch_class(b)` | `error` |
| `excpaths:oldcatch_bm(a)` | `{'EXIT',{{badmatch,a},Stacktrace}}` |
| `excpaths:trace(b)` | `{error,{case_clause,b},Stacktrace}` |

## `error_path_targets.txt` — `tests/exceptions_corpus.rs`

The oracle-checked error-path call corpus for the corpus error-path suite
(`corpus_error_paths_execute_and_agree_with_the_oracle`). Every non-comment line is

```text
<module>:<function>/<arity>  <POISONS>  =>  <EXPECTED>   # oracle: <erl command>
```

`<EXPECTED>` is the exception the real system Erlang (OTP 29 / erts-17.1) raises for
`Module:Function(<poison args>)`, and the trailing comment quotes the exact
`erl -noshell -eval` invocation that reproduces it. `POISONS` is one entry per argument
(`atom` = the atom `genesis_poison`, `int` = `7`, `nil` = `[]`, `tuple0` = `{}`), so its
length equals the arity. `<EXPECTED>` is a bare-atom `function_clause`, a `{tag}`
2-tuple reason (`case_clause`/`badmatch`/`badrecord`, whose payload is the **last**
poison), or `unsupported` for a call that needs host machinery the test does not serve.
The file documents its deterministic capping rule and the un-capped sweep totals; the
suite enforces `MODULE_FLOOR = 213` over the modules whose error paths execute and
agree. It is *data*: regenerate it, never hand-edit a value.

## `stdlib_breadth_targets.txt` — `tests/stdlib_lists.rs`

The oracle-checked call corpus for the breadth suite
(`breadth_runs_a_large_fraction_of_the_installed_corpus`). Every line is

```text
<module>:<function>(<args>) => <expected>   # oracle: erl -noshell -eval '<command>'
```

where `<expected>` is the verbatim `~w` rendering the real system Erlang (OTP 29 /
erts-17.1) produced for `Module:Function(Args)`, and the trailing comment quotes the
exact `erl -noshell -eval …` invocation that produces it — one `# oracle: erl …` command
per row, copy-pasteable to re-verify any single entry. The corpus was generated by
evaluating every expression in one `erl` node (`erl_eval:exprs/2` inside a spawned
process whose group leader is a sink, so no library output leaks into the result) and
printing `~w`. The suite enforces the floors `BREADTH_MODULE_FLOOR = 171` (distinct
corpus modules that actually execute) and `BREADTH_CALL_FLOOR = 480` (oracle-checked
calls that execute); `#`-prefixed and blank lines are ignored. It is *data*: regenerate
it, never hand-edit a value.

## `stdlib_exports_harness.beam` (`stdlib_exports_harness.erl`) — `tests/stdlib_exports_more.rs`

The harness for the exports slice: each export is a thin wrapper around a **real
system stdlib** function that a direct call cannot enter (a NIF stub, or a
fun-taking export whose `fun` only the caller can build).

| export | Erlang | why a wrapper |
|---|---|---|
| `lists_keysearch/3` | `lists:keysearch(Key, N, TupleList).` | the `lists` `keysearch/3` NIF: the installed `.beam`'s own body is the `erlang:nif_error(undef)` stub, so it is only reachable through a caller's `call_ext` |
| `ordsets_map_double/1` | `ordsets:map(fun(E) -> E * 2 end, S).` | `ordsets:map/2` takes a `fun` (`make_fun3` + `call_fun*`) |
| `ordsets_filtermap_double/1` | `ordsets:filtermap(fun(E) -> {true, E * 2} end, S).` | same, with the `{true, Value}` result shape |

Rebuild it with the neutral-directory `erlc +deterministic` recipe above.
## `catchtrace.beam` (`catchtrace.erl`) — `tests/exceptions.rs`
A real `try … catch C:R:S -> S` whose `S` is the derived stacktrace (the raise snapshot cooked by `build_stacktrace`).
| export | Erlang | what it pins |
|---|---|---|
| `run/0` | `try erlang:error(badarg) catch C:R:S -> S end.` (inline raise) | the raising frame is the catching frame |
| `run2/0` | a non-tail call into `raise_badarg/0` under the same `try` | the raising frame first, then the live caller |
## `catchclasses.beam` (`catchclasses.erl`) — `tests/exceptions.rs`
The old-style `catch` fold, one export per exception class (the `catch_end` three-way fold: C `emu/instrs.tab:1188-1211` plus `beam_common.c:730-735`).
| export | Erlang | what it pins |
|---|---|---|
| `plain/0` | `catch ok.` | no exception: the value passes through unchanged |
| `thrown/0` | `catch erlang:raise(throw, ball, []).` | class `throw` ⇒ the thrown value **bare** |
| `exited/0` | `catch erlang:raise(exit, bye, []).` | class `exit` ⇒ the bare `{'EXIT',bye}`, **no** stacktrace element |
| `errored/0` | `catch (a = b).` | class `error` ⇒ `{'EXIT',{{badmatch,b},Stacktrace}}` (C's `add_stacktrace`) |
| `nested/0` | a non-tail call into `deeper/0` under the same `catch` | the raising frame first, then the live caller |
## `raise_frames.beam` (`raise_frames.erl`) — `tests/raise_stacktrace.rs`
The uncaught-raise frames: `outer/1` makes a **non-tail** `call` into `boom/1` (which raises `error:badarg`), so the uncaught `Raise.stacktrace` names `boom/1` then `outer/1`.
## `xmod_caller.beam` (`xmod_caller.erl`) / `xmod_callee.beam` (`xmod_callee.erl`) — `tests/raise_stacktrace.rs`
An in-machine cross-module raise: `xmod_caller:run/1` makes a non-tail `call_ext` into `xmod_callee:boom/1`, which raises — the frames name the callee first, then the caller, each resolved in its own module context. Both built with the neutral-directory recipe above.

## `stacktrace_frames.beam` (`stacktrace_frames.erl`) — `tests/stacktrace_frames.rs`

The **cooked catch-stacktrace** shapes that pin DEFECT 2 (a reported "duplicated top stacktrace frame" that does NOT reproduce — C's `gather_stacktrace` records each continuation pointer once). Each export is a `try … catch _C:_R:S -> S end` that returns its cooked `S`; the leading `stacktrace_frames` frames are the oracle-exact ones the test asserts, the trailing `erl_eval`/`init` frames of the oracle's own `S` live outside the engine's run.

| export | Erlang | oracle `S` (leading `stacktrace_frames` frames) |
|---|---|---|
| `t0/0` | `try f() catch _C:_R:S -> S end`; `f() -> erlang:error(boom).` | `[{stacktrace_frames,f,0,{line,4}},{stacktrace_frames,t0,0,{line,3}}]` |
| `t2/0` | the same shape as `t0/0` with `hh/0` | `[{stacktrace_frames,hh,0,{line,7}},{stacktrace_frames,t2,0,{line,8}}]` |
| `t3/0` | `try g2() catch _C:_R:S -> S end`; `g2() -> hh2() + 1.`; `hh2() -> erlang:error(boom).` | `[{stacktrace_frames,hh2,0,{line,11}},{stacktrace_frames,t3,0,{line,9}}]` (the tail-called `g2/0` is elided) |

`t3/0`'s `g2/0` is elided because `sys_core_fold` proves `hh2/0` never returns, so `g2/0` compiles to a **tail** call into `hh2/0` (its body is `call_only {f,hh2}`); the tail call reuses the frame, so the oracle never names it. Rebuild with the neutral-directory `erlc +deterministic` recipe above.
