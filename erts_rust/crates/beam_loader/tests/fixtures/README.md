# `beam_loader` test fixtures

`slice.beam` is the committed oracle fixture used by the container/IR/negative
integration tests. It is a real `.beam` produced by the **system Erlang** toolchain
(OTP 29 / `erts-17.1`), which is also the differential oracle.

## Regeneration

The fixtures are compiled with `+deterministic`, which strips the embedded source
path from the `CInf` (compile info) and `Dbgi` (debug info) chunks — the committed
`.beam` files are therefore **path-free** (no worktree/`$TMPDIR` path leaks). Compile
from a neutral scratch dir under `$TMPDIR` so nothing about the local checkout is
recorded:

```sh
SCRATCH="$TMPDIR/neuter_fixtures"
mkdir -p "$SCRATCH/slicegen"
cp slice.erl "$SCRATCH/slicegen/"
( cd "$SCRATCH/slicegen" && erlc +deterministic slice.erl )
cp "$SCRATCH/slicegen/slice.beam" slice.beam   # writes slice.beam (712 bytes)
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

File size **712 bytes**; `FOR1` form length **704** (= file_size − 8); form type
`BEAM`. Chunks in file order (id · payload size):

| # | id | size | parsed |
|---|----|------|--------|
| 1 | `AtU8` | 64 | atom names |
| 2 | `Code` | 163 | `Code` header + instruction stream |
| 3 | `StrT` | 0 | empty string table |
| 4 | `ImpT` | 52 | 4 imports |
| 5 | `ExpT` | 64 | 5 exports |
| 6 | `Meta` | 45 | skipped |
| 7 | `LocT` | 16 | parsed (JIT-only local-function table, `ExpT` layout) |
| 8 | `Attr` | 39 | raw attributes |
| 9 | `CInf` | 27 | raw compile info (path-free) |
| 10 | `Dbgi` | 66 | recognized id, payload unparsed (debug info) |
| 11 | `Line` | 25 | line table |
| 12 | `Type` | 30 | type table |

Note `Dbgi` (debug info) is *not* the legacy `DbgB` chunk: `Dbgi` is a recognized id
(`ChunkId::DBGI`) whose payload is deliberately left unparsed (debug info is tooling-only).

### `LocT` — local-function table (JIT-only)
Payload `00000001 00000005 00000003 00000006` = `i32 count` 1 followed by one
`{ i32 function_atom, i32 arity, i32 label }` triple `{ 5, 3, 6 }`: the non-exported local
`fib/3` (wire atom index 5) at entry label 6. Byte-for-byte an `ExpT` payload, so the loader
parses it with the export-table reader (the C does the same); it is irrelevant to
interpretation and surfaces only as `Module.locals`.

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

## `coverage.beam`

`coverage.beam` is a second committed oracle fixture. Where `slice.beam` covers only a
handful of generic opcodes, `coverage.beam` exercises a broader slice of idiomatic Erlang:
tuples (construction + element access + `case`/pattern match on size), lists
(construction + head/tail), anonymous funs that capture a free variable (both *creating* —
`make_fun3` — and *calling* — `call_fun2` — a closure, which also populates the `FunT`
chunk), `case`, `try … catch`, float arithmetic, maps (`#{…}` construct/`=>`/`:=` +
`maps:get`/`get_map_elements`) and bitstrings (construction + matching). It also carries a
non-empty `LitT` literal pool. It is produced by the same **system Erlang** toolchain
(OTP 29 / `erts-17.1`) as the oracle.

### Regeneration

Compile from a neutral scratch dir under `$TMPDIR` with `+deterministic`, exactly as for
`slice.beam`, so the committed binary embeds **no** source path:

```sh
SCRATCH="$TMPDIR/neuter_fixtures"
mkdir -p "$SCRATCH/coveragegen"
cp coverage.erl "$SCRATCH/coveragegen/"
( cd "$SCRATCH/coveragegen" && erlc +deterministic coverage.erl )
cp "$SCRATCH/coveragegen/coverage.beam" coverage.beam   # writes coverage.beam (1716 bytes)
```

With `+deterministic` there is **no** source path in `CInf` at all — the fixture is
path-free (it no longer records a temp `/tmp/…` path). Any byte change in the fixture
invalidates the expected values; re-derive them with `beam_disasm:file("coverage.beam")`
plus a byte decode, do **not** guess.

`coverage.erl` (the input, also committed):

```erlang
-module(coverage).
-export([tuple_ops/1, list_ops/1, make_adder/1, apply_adder/2, classify/1,
         safe_div/2, float_ops/2, map_ops/2, bits/1]).

%% Tuples: construction + element access + matching on tuple size.
tuple_ops({A, B}) ->
    {B, A, A + B};
tuple_ops({A, B, C}) ->
    {C, B, A, C - A};
tuple_ops(T) when is_tuple(T) ->
    element(1, T).

%% Lists: construction, head/tail, recursion.
list_ops([H | T]) ->
    [H * 2 | list_ops(T)];
list_ops([]) ->
    [].

%% Anonymous fun capturing a free variable -> make_fun3 / call_fun + FunT chunk.
make_adder(N) ->
    fun(X) -> X + N end.

%% Invoking an anonymous fun -> call_fun2.
apply_adder(N, X) ->
    Adder = make_adder(N),
    Adder(X).

%% `case` over patterns.
classify(0) -> zero;
classify(N) when is_integer(N), N > 0 -> positive;
classify(N) when is_integer(N) -> negative;
classify(_) -> other.

%% `try`/`catch`.
safe_div(_A, 0) ->
    {error, division_by_zero};
safe_div(A, B) ->
    try A / B of
        Q -> {ok, Q}
    catch
        error:badarith -> {error, badarith}
    end.

%% Float arithmetic.
float_ops(A, B) ->
    Sum = A + B,
    Diff = A - B,
    Prod = A * B,
    Sum + Diff + Prod.

%% Maps: construct, update with `=>`, update with `:=`, lookup with maps:get.
map_ops(Key, Value) ->
    M0 = #{a => 1, b => 2},
    M1 = M0#{c => Value},
    M2 = M1#{a := maps:get(c, M1) + 10},
    maps:get(Key, M2, 0).

%% Bitstrings: construction and matching.
bits(<<H:8, Rest/binary>>) ->
    <<Rest/binary, H:8>>;
bits(Bin) ->
    Bin.
```

### Wire summary (verified against the oracle, `beam_disasm` + byte decode)

File size **1716 bytes**; `FOR1` form length **1708** (= file_size − 8); form type `BEAM`.
Chunks in file order (id · payload size):

| # | id | size | parsed |
|---|----|------|--------|
| 1 | `AtU8` | 262 | atom names |
| 2 | `Code` | 695 | `Code` header + instruction stream |
| 3 | `StrT` | 0 | empty string table |
| 4 | `ImpT` | 76 | 6 imports |
| 5 | `ExpT` | 136 | 11 exports |
| 6 | `FunT` | 28 | 1 lambda |
| 7 | `LitT` | 84 | 3 literals |
| 8 | `Meta` | 45 | skipped |
| 9 | `LocT` | 16 | parsed (JIT-only local-function table, `ExpT` layout) |
| 10 | `Attr` | 39 | raw attributes |
| 11 | `CInf` | 27 | raw compile info (path-free) |
| 12 | `Dbgi` | 66 | recognized id, payload unparsed (debug info) |
| 13 | `Line` | 62 | line table |
| 14 | `Type` | 43 | type table |

Unlike `slice.beam`, this module carries **`FunT`** (the closure table) and a non-empty
**`LitT`**. `Dbgi` (debug info) is again a recognized id whose payload is left unparsed.

### `AtU8` — atoms
`i32` count = **−32** ⇒ **long (tagged) lengths**; real count 32. Wire index 0 is the NIL
placeholder. At wire indices 1..=32:

`coverage, tuple_ops, erlang, '-', '+', element, list_ops, '*', make_adder, apply_adder,
classify, zero, positive, negative, other, safe_div, ok, error, badarith, float_ops, map_ops,
c, a, bits, ensure_at_least, integer, get_tail, append, all, module_info, get_module_info,
'-make_adder/1-fun-0-'`

Indices 25..=29 (`ensure_at_least`, `integer`, `get_tail`, `append`, `all`) are the bitstring
match-spec / binary-construction command atoms emitted by `bs_match`/`bs_create_bin`; they are
ordinary atoms in the table.

### `Code` header
`head_size = 16`, `format_number = 0`, `max_opcode = 182`, `label_count = 38`,
`function_count = 12`. Instruction stream starts at `data[4 + 16..]`, ends after the
`int_code_end` instruction (the last stream byte is `0x03`). The 12 functions are
`tuple_ops/1`, `list_ops/1`, `make_adder/1`, `apply_adder/2`, `classify/1`, `safe_div/2`,
`float_ops/2`, `map_ops/2`, `bits/1`, `module_info/0`, `module_info/1` and the closure
`'-make_adder/1-fun-0-'/2`.

`max_opcode = 182` = `bs_match/3` (one higher than `slice.beam`'s 181), because this module
uses bitstring matching.

### `ImpT` — imports (file order)
```
0: {erlang, '-', 2}
1: {erlang, '+', 2}
2: {erlang, element, 2}
3: {erlang, '*', 2}
4: {erlang, get_module_info, 1}
5: {erlang, get_module_info, 2}
```

### `ExpT` — exports (file order — preserved, *not* sorted)
```
{module_info, 1} -> label 35
{module_info, 0} -> label 33
{bits, 1}        -> label 29
{map_ops, 2}     -> label 26
{float_ops, 2}   -> label 24
{safe_div, 2}    -> label 19
{classify, 1}    -> label 14
{apply_adder, 2} -> label 12
{make_adder, 1}  -> label 10
{list_ops, 1}    -> label  7
{tuple_ops, 1}   -> label  2
```
`Export.label` is the function **entry** label.

### `FunT` — lambda table
`i32` count = **1** (chunk size 28 = 4 + 1 × 24). Entry 0 (raw bytes
`00 00 00 20 00 00 00 02 00 00 00 25 00 00 00 00 00 00 00 01 00 35 EA F3`):

```
{function_atom 32 = '-make_adder/1-fun-0-', arity 2, label 37,
 index 0, num_free 1, old_uniq 3533555}
```

`label 37` is the closure body's **entry** label (its `func_info` sits at label 36). The
matching `make_fun3` inside `make_adder/1` names the same fun with `index 0`,
`old_uniq 3533555` and a captured-free list of one register (`{list,[{x,0}]}`, i.e.
`num_free 1`). The closure is invoked in `apply_adder/2` via
`call_fun2,{f,37},1,{tr,{x,1},{t_fun,any,any,any}}`.

### `LitT` — literal pool
`uncompressed_size = 0` ⇒ the payload is stored **verbatim** (not zlib-compressed). `i32`
count = **3** (chunk size 84 = 4 + 4 + (4+28) + (4+20) + (4+16)); each literal is
`{i32 ext_size, ext_size bytes}` of Erlang external-term bytes:

```
0: 28 bytes  {error, division_by_zero}   (tuple, referenced by move,{literal,…} in safe_div/2)
1: 20 bytes  {error, badarith}           (tuple, referenced by move,{literal,…} in safe_div/2)
2: 16 bytes  #{a => 1, b => 2}           (map, the base of put_map_assoc in map_ops/2)
```

### `Line`
`version 0`, `flags 0`, `instr_count 25`, `item_count 23`, `name_count 0`. The 42-byte item
stream is
`a1 d1 b1 f1  09 12  09 13  09 18  09 1c  09 22  09 25  09 2c  09 2d  09 2e  09 2f  09 30  09 33  09 36  09 3a  09 3b  09 19`.
Decoding each tagged item (tag = `byte & 0x07`; single-byte when `byte & 0x08 == 0` ⇒ value
`byte >> 4`, else a 2-byte `09 XX` big-endian value) yields **23 `TAG_i`** line items, in
instruction order, with line values:

`10, 13, 11, 15, 18, 19, 24, 28, 29, 30, 33, 39, 42, 49, 50, 51, 52, 53, 56, 59, 63, 64, 25`

(The first four are single-byte `0xN1` items; the remaining nineteen are 2-byte `09 XX` items;
the trailing `25` is the closure body.) There are no `TAG_a` file-change items, so every item
uses `name_index 0`. The loader prepends the implicit "undefined location" entry, giving
`items.len() == 24`, and `files == ["coverage.erl"]`.

### `Type`
`version 4`, `count 9`; entries:

```
0: {raw: 0x1FFF}                          (the "any" entry)
1: {raw: 0x0800}
2: {raw: 0x0010}
3: {raw: 0x0020}
4: {raw: 0x0001}
5: {raw: 0x0028}
6: {raw: 0x0040}
7: {raw: 0x8002, unit: 8}                 (unit byte stored 7; loader stores stored + 1)
8: {raw: 0x6020, lower: 0, upper: 255}
```

`tr`-annotated registers appear in `tuple_ops/1` (`t_tuple`), `classify/1` (`t_integer`,
`t_atom`), `float_ops/2` (`t_number`), `map_ops/2` (`t_map`), `bits/1` (`t_bs_matchable`,
`t_integer{0,255}`) and `apply_adder/2` (`t_fun`).

### Opcodes emitted (the exact set)
The `Code` stream holds **177 instructions**. Decoding it instruction-by-instruction (using the
`genop.tab` opcode/arity roster and the `beam_file.c` tagged-operand rules — the same rules the
loader implements) consumes the stream **exactly** (0 leftover bytes) and lands on the
`int_code_end` terminator, which cross-checks against the 176 instructions `beam_disasm`
reports (it omits `int_code_end`). The stream emits **43 distinct opcodes** — these are the
*generic-opcode roster* names of `lib/compiler/src/genop.tab`, i.e. exactly the variants of
`beam_loader`'s `Opcode` enum (opcode number · name/arity):

```
  1  label/1               60  select_tuple_arity/3 154  put_map_assoc/5
  2  func_info/3           64  move/2               155  put_map_exact/5
  3  int_code_end/0        65  get_list/3           158  get_map_elements/3
  4  call/2                66  get_tuple_element/3  164  put_tuple2/2
 11  bif2/5                69  put_list/3           165  bs_get_tail/3
 12  allocate/2            78  call_ext_only/2      166  bs_start_match3/4
 16  test_heap/2           96  fmove/2              167  bs_get_position/3
 18  deallocate/1          97  fconv/2              168  bs_set_position/2
 19  return/0             101  fdiv/4               171  make_fun3/3
 40  is_ge/3              104  try/2                172  init_yregs/1
 43  is_eq_exact/3        105  try_end/1            177  bs_create_bin/6
 45  is_integer/2         106  try_case/1           178  call_fun2/3
 52  is_nil/2             108  raise/2              182  bs_match/3
 56  is_nonempty_list/2   125  gc_bif2/6
 57  is_tuple/2           153  line/1
```

**How `beam_disasm` renders these.** `beam_disasm` folds several *distinct wire opcodes* into a
shared generic mnemonic, so its output shows only **34** top-level names (excluding
`int_code_end`). The folds present in this module:

- `bif2/5`, `fdiv/4`, `raise/2` all print as `{bif, Name, …}` (e.g. `element/2`, `fdiv/2`,
  `raise/2`) — 3 wire opcodes ⇒ 1 display name.
- `gc_bif2/6` prints as `{gc_bif, Name, …}` — 1 ⇒ 1.
- `is_eq_exact/3`, `is_ge/3`, `is_integer/2`, `is_nil/2`, `is_nonempty_list/2`, `is_tuple/2`
  **and** `bs_start_match3/4` all print as `{test, Kind, …}` — 7 wire opcodes ⇒ 1 display name
  (see `lib/compiler/src/beam_disasm.erl:1319`, where `bs_start_match3` is resolved to a `test`
  form).
- `try/2`, `try_end/1`, `try_case/1` print as `'try'`, `try_end`, `try_case`.

So `beam_disasm`'s 34 display names under-represent the wire: the file really carries the 43
opcodes listed above.

Relative to `slice.beam`'s wire subset (`call_only, call_ext_only, func_info, gc_bif2,
is_eq_exact, label, line, move, return, swap`, plus `int_code_end`), the unusual additions are:

- **funs/closures** — `make_fun3/3` (create) and `call_fun2/3` (invoke). Note the closure is
  invoked via `call_fun2`, not `call_fun` (this app's fun is a plain closure, not an
  `erlang:apply/2` dispatch).
- **exceptions** — `try/2`, `try_end/1`, `try_case/1` (plus the raising BIF `raise/2`).
- **floats** — `fmove/2`, `fconv/2` (X-register ↔ float-register conversion) and the dedicated
  `fdiv/4` (float division BIF).
- **maps** — `put_map_assoc/5`, `put_map_exact/5`, `get_map_elements/3`.
- **bitstrings** — `bs_match/3` (this is what raises `max_opcode` to 182), `bs_start_match3/4`,
  `bs_get_position/3`, `bs_set_position/2`, `bs_get_tail/3`, `bs_create_bin/6`.
- **tuples** — `select_tuple_arity/3`, `put_tuple2/2`, `get_tuple_element/3`.
- **lists** — `get_list/3`, `put_list/3`.
- **frame/stack** — `allocate/2`, `deallocate/1`, `test_heap/2`, `init_yregs/1`.
- the plain BIF forms — `bif2/5` (for `element/2`) in addition to `gc_bif2/6` (for `+`, `-`,
  `*`); and a module-local `call/2` (not `call_ext*`) for the call to `make_adder/1`.

The six `is_*`/`bs_start_match3` opcodes are the type tests shown by `beam_disasm` as
`{test, …}`.
