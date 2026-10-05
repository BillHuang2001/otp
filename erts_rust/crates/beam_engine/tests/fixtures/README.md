# `beam_engine` test fixtures

Committed oracle `.beam` fixtures for the engine's tuples + lists instruction-family
tests (`tests/tuples_lists.rs`). Both are real `.beam` files produced by the **system
Erlang** toolchain (OTP 29 / `erts-17.1`), which is also the differential oracle.

## Regeneration

```sh
cd erts_rust/crates/beam_engine/tests/fixtures
erlc tuples.erl   # writes tuples.beam
erlc lists.erl    # writes lists.beam
```

`.beam` files are git-ignored (repo `.gitignore: *.beam`), so the fixtures are
committed with `git add -f`.

## `tuples.beam` (`tuples.erl`)

Exports and the tuple opcodes each one drives:

| export | Erlang | opcodes |
|---|---|---|
| `mk/2` | `mk(A,B) -> {A,B}.` | `put_tuple2` |
| `first/1` | `case T of {A,_B} -> A end.` | `get_tuple_element` |
| `swap/1` | `case T of {A,B} -> {B,A} end.` | `get_tuple_element` ×2 + `put_tuple2` |
| `classify/1` | `case T of {a}->1; {a,b}->2; {a,b,c}->3 end.` | `select_tuple_arity` |

## `lists.beam` (`lists.erl`)

| export | Erlang | opcodes |
|---|---|---|
| `build/2` | `build(H,T) -> [H|T].` | `put_list` |
| `head/1` | `case L of [H|_T] -> H end.` | `get_hd` |
| `tail/1` | `case L of [_H|T] -> T end.` | `get_tl` |
| `split/1` | `case L of [H|T] -> {H,T} end.` | `get_list` + `put_tuple2` |

`case` (rather than function-head clauses) keeps a bad argument on the `case_end`
path instead of a `func_info` clause failure, so the fixtures exercise only the four
families the engine implements.

Any byte change in a fixture invalidates the expected values in `tests/tuples_lists.rs`;
re-derive them with `beam_disasm:file/1`, do **not** guess.
