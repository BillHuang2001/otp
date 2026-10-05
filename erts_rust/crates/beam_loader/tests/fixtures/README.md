# `beam_loader` test fixtures

`slice.beam` is the committed oracle fixture used by the container/IR/negative
integration tests. It is a real `.beam` produced by the **system Erlang** toolchain
(OTP 29 / `erts-17.1`), which is also the differential oracle.

## Regeneration

```sh
cd erts_rust/crates/beam_loader/tests/fixtures
erlc slice.erl        # writes slice.beam (828 bytes)
```

`slice.erl` (the input, also committed):

```erlang
-module(slice).
-export([add/2, fib/1, pick/1]).

add(A, B) -> A + B.

fib(N) -> fib(N, 0, 1).

fib(0, A, _B) -> A;
fib(N, A, B) -> fib(N-1, B, A+B).

pick(a) -> 1;
pick(_) -> 2.
```

Any byte change in the fixture invalidates the expected values in `tests/*.rs`;
re-derive them with `beam_disasm:file("slice.beam")` plus a byte decode, do **not**
guess.

## Wire summary (verified against the oracle, `beam_disasm` + byte decode)

File size **828 bytes**; `FOR1` form length **820** (= file_size − 8); form type
`BEAM`. Chunks in file order (id · payload size):

| # | id | size | parsed |
|---|----|------|--------|
| 1 | `AtU8` | 64 | atom names |
| 2 | `Code` | 163 | `Code` header + instruction stream |
| 3 | `StrT` | 0 | empty string table |
| 4 | `ImpT` | 52 | 4 imports |
| 5 | `ExpT` | 64 | 5 exports |
| 6 | `Meta` | 45 | skipped |
| 7 | `LocT` | 16 | skipped (JIT-only) |
| 8 | `Attr` | 39 | raw attributes |
| 9 | `CInf` | 105 | raw compile info |
| 10 | `Dbgi` | 104 | skipped (debug info) |
| 11 | `Line` | 25 | line table |
| 12 | `Type` | 30 | type table |

Note `Dbgi` (debug info) is *not* `DbgB`; it is not a known chunk id and is skipped.

### `AtU8` — atoms
`i32` count = **−10** ⇒ **long (tagged) lengths**; real count 10. Wire index 0 is the
NIL placeholder. At wire indices 1..=10:

`slice, add, erlang, '+', fib, '-', pick, 'a', module_info, get_module_info`

### `Code` header
`head_size = 16`, `format_number = 0`, `max_opcode = 181`, `label_count = 15`,
`function_count = 6`. Instruction stream starts at `data[4 + 16..]`, ends after the
`int_code_end` instruction.

### `ImpT` — imports (file order)
```
0: {erlang, '+', 2}
1: {erlang, '-', 2}
2: {erlang, get_module_info, 1}
3: {erlang, get_module_info, 2}
```

### `ExpT` — exports (file order — preserved, *not* sorted)
```
{module_info, 1} -> label 14
{module_info, 0} -> label 12
{pick, 1}        -> label  9
{fib, 1}         -> label  4
{add, 2}         -> label  2
```
`Export.label` is the function **entry** label.

### `Line`
`version 0`, `flags 0`, `instr_count 7`, `item_count 5`, `name_count 0`. The 5-byte
item stream is `41 61 81 91 b1` = five `TAG_i` items with line values `4, 6, 8, 9, 11`
(all with `name_index 0`). The wire `item_count` counts the **tagged** items; the
loader prepends the implicit "undefined location" entry, giving `items.len() == 6`
(`items[0] == {0,0}`), and `files == ["slice.erl"]`.

### `Type`
`version 4`, `count 3`; entries: `{raw: 0x1FFF}` (the "any" entry),
`{raw: 0x2020, lower: 0}`, `{raw: 0x2020, lower: 1}`. The `tr`-annotated registers in
`fib/3` reference type indices 1 and 2.
