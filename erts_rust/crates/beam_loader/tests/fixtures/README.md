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

## `records_fixture.beam`

`records_fixture.beam` is the committed oracle fixture for the **`Recs` chunk** — a
module's compile-time **native-record** (`-record #name{…}`, OTP 29 syntax) definitions,
which the `records:get_definition/2` BIF serves. The module body is deliberately trivial
(`noop/0`), so the only interesting payload is the record table. It is produced by the
same **system Erlang** toolchain (OTP 29 / `erts-17.1`) as the oracle.

The module is named `records_fixture`, **not** `records`: `records` is a *stdlib* module,
so a fixture named `records` would replace it on `code:load_file/1`, after which the
oracle's own `records:get_definition/2` never returns (an empirical infinite loop). The
record *names* are unaffected by the module name, so the pinned oracle output is
independent of this choice.

### Regeneration

Compile from a neutral scratch dir under `$TMPDIR` with `+deterministic`, exactly as for
`slice.beam`, so the committed binary embeds **no** source path:

```sh
SCRATCH="$TMPDIR/neuter_fixtures"
mkdir -p "$SCRATCH/recordsgen"
cp records_fixture.erl "$SCRATCH/recordsgen/"
( cd "$SCRATCH/recordsgen" && erlc +deterministic records_fixture.erl )
cp "$SCRATCH/recordsgen/records_fixture.beam" records_fixture.beam   # writes records_fixture.beam (752 bytes)
```

`erlc` warns that the three non-exported records are unused (the body never constructs
one); that is expected and harmless.

`records_fixture.erl` (the input, also committed):

```erlang
-module(records_fixture).

-export([noop/0]).

%% Native records (OTP 29 syntax). Every definition below ends up in the `Recs`
%% chunk; the module body is deliberately trivial so the only interesting payload
%% is the record table.
-record #user{id = -1, name, city}.

-record #empty{}.

%% Mixed defaults: a small-int `0` (distinct from an *absent* field), an absent
%% field, an atom default, a tuple literal, a list literal and a bignum.
-record #mixed{a = 0, b, c = undefined, d = {1, 2}, e = [a, b],
               f = 10000000000000000000000000}.

%% An exported record (`-export_record([...])`) — `is_exported => true`.
-record #shared{key, value = none}.

-export_record([shared]).

noop() -> ok.
```

Any byte change in the fixture invalidates the expected values in `tests/records.rs`;
re-derive them with `beam_disasm:file("records_fixture.beam")` plus a byte decode, do
**not** guess.

### Wire summary (verified against the oracle, `beam_disasm` + byte decode)

File size **752 bytes**; `FOR1` form length **744** (= file_size − 8); form type `BEAM`.
Chunks in file order (id · payload size):

| # | id | size | parsed |
|---|----|------|--------|
| 1 | `AtU8` | 148 | atom names |
| 2 | `Code` | 70 | `Code` header + instruction stream |
| 3 | `StrT` | 0 | empty string table |
| 4 | `ImpT` | 28 | 2 imports |
| 5 | `ExpT` | 40 | 3 exports |
| 6 | `LitT` | 36 | 2 literals |
| 7 | `Meta` | 45 | skipped |
| 8 | `Recs` | 80 | native-record table (the subject of this fixture) |
| 9 | `LocT` | 4 | parsed (empty JIT-only local-function table) |
| 10 | `Attr` | 39 | raw attributes |
| 11 | `CInf` | 27 | raw compile info (path-free) |
| 12 | `Dbgi` | 66 | recognized id, payload unparsed (debug info) |
| 13 | `Line` | 22 | line table |
| 14 | `Type` | 10 | type table |

`Dbgi` is a recognized id whose payload is deliberately left unparsed (like `Meta`); the
`Recs` payload *is* parsed.

### `AtU8` — atoms
`i32` count = **−25** ⇒ **long (tagged) lengths**; real count 25. Wire index 0 is the NIL
placeholder. At wire indices 1..=25:

`records_fixture, noop, ok, module_info, erlang, get_module_info, user, false, id, name,
city, empty, mixed, a, b, c, undefined, d, e, f, shared, true, key, value, none`

The record names, field names, and the two default-tagging atoms `false` (8) and `true`
(22) all live here — the `Recs` parser needs the atom *names* because the export flag is
compared by name, exactly as the C compares against `am_true`/`am_false`.

### `Code` header
`head_size = 16`, `format_number = 0`, `max_opcode = 181`, `label_count = 7`,
`function_count = 3`. The 50-byte instruction stream ends after the `int_code_end`
instruction. The three functions are `noop/0`, `module_info/0` and `module_info/1`.

### `ImpT` — imports (file order)
```
0: {erlang, get_module_info, 1}
1: {erlang, get_module_info, 2}
```

### `ExpT` — exports (file order — preserved, *not* sorted)
```
{module_info, 1} -> label 6
{module_info, 0} -> label 4
{noop, 0}        -> label 2
```
`Export.label` is the function **entry** label. `LocT` is present but empty (`i32 count`
0), so `Module.locals` is empty.

### `LitT` — literal pool
`uncompressed_size = 0` ⇒ the payload is stored **verbatim** (not zlib-compressed). `i32`
count = **2**; each literal is `{i32 ext_size, ext_size bytes}` of Erlang external-term
bytes:

```
0:  7 bytes  83 68 02 61 01 61 02              = {1, 2}
1: 13 bytes  83 6c 00 00 00 02 77 01 61 77 01 62 6a
                                               = [a, b]
```

(`0x83` = `VERSION_MAGIC` 131, `0x68`/104 = `SMALL_TUPLE_EXT`, `0x61`/97 =
`SMALL_INTEGER_EXT`, `0x6c`/108 = `LIST_EXT`, `0x77`/119 = `ATOM_UTF8_EXT`, `0x6a`/106 =
`NIL`.) Both literals are the defaults of `#mixed.d` and `#mixed.e`, referenced from the
`Recs` payload by a `z,4` operand.

### `Recs` — native-record table
Raw payload (80 bytes), with the 12-byte header split out:

```
00 00 00 00   version = 0 (a non-zero version means "no record table", not an error)
00 00 00 04   record_count = 4
00 00 00 0b   total_field_count = 11 (= 3 + 0 + 6 + 2, informational)
```

then one entry per record, each encoded as one `call_last`-shaped instruction
(`05` = `call_last`, reused only for its three operands: name, export flag, field list):

| record | entry bytes | export flag | fields, in declaration order |
|--------|-------------|-------------|------------------------------|
| `user`   | `05 72 82 17 60 92 19 ff ff a2 00 b2 00` | `82` = atom 8 = `false` | `{id, -1}` · `name` · `city` |
| `empty`  | `05 c2 82 17 00` | `82` = atom 8 = `false` | *(none)* |
| `mixed`  | `05 d2 82 17 c0 e2 01 f2 00 0a 10 0a 11 0a 12 47 00 0a 13 47 10 0a 14 f9 20 08 45 95 16 14 01 48 4a 00 00 00` | `82` = atom 8 = `false` | `{a, 0}` · `b` · `{c, undefined}` · `{d, {1,2}}` · `{e, [a,b]}` · `{f, 10000000000000000000000000}` |
| `shared` | `05 0a 15 0a 16 17 40 0a 17 00 0a 18 0a 19` | `0a 16` = atom 22 = `true` | `key` · `{value, none}` |

Decoding the operands of the first entry, byte by byte:

- `72` = `a` tag, value `7` → record name = atom 7 = `user`.
- `82` = `a` tag, value `8` → the export flag = `false`.
- `17` = `z` tag, sub-tag `1` → the `z,1` argument list (the record's fields).
- `60` = `u` tag, value `6` → the list length (6 = 3 `(name, default)` pairs).
- `92` = `a`, value `9` → field `id`; then `19 ff ff` = an `i` tag with an explicit
  2-byte magnitude `FFFF`, sign-extended to **−1** (still a small integer, so the loader
  inlines it: `Some(-1)`).
- `a2` = `a`, value `10` → field `name`; then `00` = `u,0` → **no default**.
- `b2` = `a`, value `11` → field `city`; then `00` = `u,0` → **no default**.

The `u`/`i` distinction is the whole reason defaults are decoded from the **raw tag**
rather than through the shared operand decoder (which collapses both to `Integer`):
`#mixed.a` is encoded `01` = `i,0` (**a real default of the integer 0**) while `#mixed.b`
is encoded `00` = `u,0` (**absent**). They share a magnitude and differ only by tag.

`#mixed.f` is a **wide `i`** field — the `f9 20 …` form: tag byte `0xF9` (both length
bits set, sub-length selector `7`), a `u`-tagged sub-length `20` (⇒ `0x02 + 9 = 11`
magnitude bytes), then the 11-byte two's-complement magnitude `08 45 95 16 14 01 48 4a 00
00 00` = `10^25`. It is **not** in the file's `LitT`; the loader appends it to the
synthesized-literal set, so `Module::literals().len() == 3` and `Module::literal(2)` is
this bignum (see `tests/records.rs`).

`#mixed.d`/`#mixed.e` use `z,4` (static literal) operands — `47 00` = a `LitT` index of
`0` (`00` = `u,0`) and `47 10` = index `1` (`10` = `u,1`).

### `Line`
`version 0`, `flags 0`, `instr_count 3`, `item_count 1`, `name_count 0`. The 2-byte item
stream `09 19` is one `TAG_i` item with line `25`. The loader prepends the implicit
"undefined location" entry, giving `items.len() == 2` and
`files == ["records_fixture.erl"]`.

### `Type`
`version 4`, `count 1`; the single entry is the "any" entry `{raw: 0x1FFF}`. A one-entry
table is *not* the fallback `TypeTable` (`fallback == false`).

## `literals_verbatim.beam` / `literals_compressed.beam`

The `LitT` literal-pool fixture pair used by `tests/literals.rs`. Both are the **same
source** (`literals_fixture.erl`) compiled by the system Erlang toolchain, differing only
in the `LitT` framing:

* `literals_verbatim.beam` — plain `erlc` (OTP 28+ verbatim `LitT`:
  `uncompressed_size == 0`, the record table follows the header).
* `literals_compressed.beam` — `erlc +compressed_literals` (the OTP 27-and-earlier
  framing: `uncompressed_size` is the pre-compression size and the rest of the chunk is a
  zlib/RFC 1950 stream — see `beam_asm.erl:build_literal_chunk`).

`literals_fixture.erl` returns one constant term per function, so the whole constant set
lands in the module's literal pool (9 literals in both files):

| idx | term | kind |
|-----|------|------|
| 0 | `{a,1,{b,2},[]}` | tuple (nested tuple, nil) |
| 1 | `[1,2,[3,4],{ok,5},"abc"]` | list (nested list, tuple, string) |
| 2 | `#{a => 1, {x,y} => [1,2], <<"k">> => 2.0}` | map (tuple key, binary key, float value) |
| 3 | `[1.5,-2.25,1.0e300,0.0]` | floats |
| 4 | `[123456789012345678901234567890,-99999999999999999999999999]` | bignums (positive + negative) |
| 5 | `[<<1,2,3>>,<<"hello">>,<<1:3>>]` | binaries (incl. a non-byte-aligned bitstring) |
| 6 | `[fun lists:reverse/1, fun maps:get/2]` | external (`EXPORT_EXT`) funs |
| 7 | `{#{list => [a,b], float => 1.5, bin => <<7,8>>}, 10^25, [<<"x">>]}` | composite (map + bignum + list) |
| 8 | `{ok,[1,2,3],{a,1.5},#{k => v}}` | composite (list + float tuple + small map) |

### Regeneration

Compile from a neutral scratch dir under `$TMPDIR` with `+deterministic`, exactly as for
`slice.beam`, so the committed binaries embed **no** source path:

```sh
SCRATCH="$TMPDIR/neuter_fixtures"
mkdir -p "$SCRATCH/verbatim" "$SCRATCH/compressed"
cp literals_fixture.erl "$SCRATCH/"
( cd "$SCRATCH" && erlc +deterministic -o verbatim literals_fixture.erl )
( cd "$SCRATCH" && erlc +deterministic +compressed_literals -o compressed literals_fixture.erl )
cp "$SCRATCH/verbatim/literals_fixture.beam"   literals_verbatim.beam    # 1228 bytes
cp "$SCRATCH/compressed/literals_fixture.beam" literals_compressed.beam  # 1148 bytes
```

### Wire summary (verified against the oracle `beam_lib:all_chunks/1`)

Both files carry the same chunks in the same order; only `LitT` differs.

| id | verbatim | compressed |
|----|----------|------------|
| `AtU8` | 13 atoms: the module name, the nine function names, `module_info`, `erlang`, `get_module_info` | same |
| `Code` | header `(format 0, max_opcode 181, 23 labels, 11 functions)` | same |
| `StrT` | empty | same |
| `ImpT` | 2 imports (`erlang:get_module_info/1,2`, used by the generated `module_info/0,1`) | same |
| `ExpT` | 11 exports (the nine functions + `module_info/0,1`) | same |
| **`LitT`** | 399 bytes: `00 00 00 00` + a 395-byte record table | 298 bytes: `00 00 01 8B` (= 395) + a 294-byte zlib stream (`78 9C…`) |
| `Meta`/`LocT`/`Attr`/`CInf`/`Dbgi`/`Line`/`Type` | as the compiler emits them (`+deterministic`) | same |

Both `LitT` payloads decode to the same nine literal records; the compressed one is the
zlib stream of the verbatim record table (395 bytes -> 294 bytes). `tests/literals.rs`
asserts the two pools carry byte-identical payloads and decode to identically re-encoded
terms — and that each literal re-encodes to what the real ERTS produces from that same
payload, which for a **map-free** literal means byte-exact.

A **map-bearing** literal (idx 2, 7, 8) is asserted against the oracle rather than
byte-for-byte, because it need not be reproducible: `+deterministic` writes a flat map's
keys in name-lexicographic order, while ERTS *stores* them in `erts_cmp_flatmap_keys` order
(for atom keys: atom-table index order). Literal 7 therefore arrives as
`#{bin => …, float => …, list => …}` and is stored as `#{list => …, float => …, bin => …}`
— its payload bytes cannot be reproduced from the decoded term at all. `tests/literals.rs`
asserts Rust == the oracle's re-encode for such literals and pins both orders (verbatim
payload vs. stored) in
`map_bearing_fixture_literal_is_normalized_to_erts_key_order_on_load`.

## `literals_oracle.escript`

The oracle for `tests/literals.rs`. It is **not** a fixture in the `.beam` sense: it is the
Erlang-side half of the literal checks. It has two modes. Run as

```sh
escript literals_oracle.escript <otp-lib-root>
```

it walks every `*.beam` under the root and prints one TAB-separated line per `LitT`
literal — `<path relative to the root> <TAB> <index> <TAB> <hex>` — where the hex is
`term_to_binary(binary_to_term(Payload))` (OTP's own decode/encode round-trip). Run as

```sh
escript literals_oracle.escript --reencode <beam-file>
```

it does the same for a single named module, printing `<index> <TAB> <hex>` (no path field);
that is the mode the committed-fixture test uses to obtain ERTS's per-literal image. Either
way it parses the `LitT` chunks with `beam_lib`/`zlib`, i.e. an implementation independent of
`beam_loader`'s framing, so a framing bug cannot validate itself. `binary_to_term/1`
normalizes a flat map's key order to ERTS's canonical order — the reason a map-bearing
payload is compared against this oracle rather than byte-for-byte, and why the install sweep
compares terms semantically (`beam_term::compare`).

## `nr_def.beam` / `nr_use.beam` — the OTP-29 native-record family (186-191)

The positive control for the OTP-29 **native-record** opcode family
(`tests/native_record.rs`). The pair is compiled *together* by the system `erlc`
(the consumer uses `-import_record(nr_def, [rec])`), and together they exercise every
family member the installed OTP-29 compiler can emit: `is_any_native_record` (186),
`is_native_record` (187), `get_record_elements` (188), `put_record` (189),
`is_record_accessible` (190) and `get_record_field` (191). Opcodes **192/193**
(`get_record_elements_id`, `update_record_id`) were added in OTP 30 and are absent
from the installed OTP-29 `beam_opcodes` table, so no system `erlc` can emit them.

`nr_def` *defines* an exported native record; `nr_use` imports it, defines its own
local one, and reads/updates/builds both.

### Regeneration

```sh
SCRATCH="$TMPDIR/neuter_native_records"
mkdir -p "$SCRATCH/gen"
cp nr_def.erl nr_use.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic nr_def.erl nr_use.erl )
cp "$SCRATCH/gen/nr_def.beam" nr_def.beam   # writes nr_def.beam (576 bytes)
cp "$SCRATCH/gen/nr_use.beam" nr_use.beam   # writes nr_use.beam (940 bytes)
```

`nr_def.erl`:

```erlang
-module(nr_def).
-export([mk/0]).
-export_record([rec]).
-record #rec{a, b}.
mk() -> #rec{a = 1, b = 2}.
```

`nr_use.erl`:

```erlang
-module(nr_use).
-export([local_mk/0, local_peek/1, local_fld/1, def_mk/0, def_upd/1, def_fld/1]).
-import_record(nr_def, [rec]).
-record #local{a, b}.
local_mk() -> #local{a = 1, b = 2}.
local_peek(R) -> #local{a = A, b = B} = R, {A, B}.
local_fld(R) -> R#local.a.
def_mk() -> #recdef:rec{a = 1, b = 2}.
def_upd(R) -> R#recdef:rec{a = 7}.
def_fld(R) -> R#recdef:rec.a.
```

### Wire summary (verified against the oracle, `beam_disasm` + byte decode)

`nr_def.beam`: **576 bytes**, `FOR1` form length **568**; chunks in file order
(id · payload size):

| # | id | size | parsed |
|---|----|------|--------|
| 1 | `AtU8` | 62 | atom names |
| 2 | `Code` | 79 | `Code` header + instruction stream |
| 3 | `StrT` | 0 | empty string table |
| 4 | `ImpT` | 28 | 2 imports (`get_module_info/1,2`) |
| 5 | `ExpT` | 40 | 3 exports (`mk/0` plus `module_info/0,1`) |
| 6 | `Meta` | 45 | skipped |
| 7 | `Recs` | 21 | the `#rec` native-record definition |
| 8 | `LocT` | 4 | parsed (JIT-only local-function table) |
| 9 | `Attr` | 39 | raw attributes |
| 10 | `CInf` | 27 | raw compile info (path-free) |
| 11 | `Dbgi` | 66 | recognized id, payload unparsed (debug info) |
| 12 | `Line` | 21 | line table |
| 13 | `Type` | 10 | type table |

`Code` header: `head_size = 16`, `format_number = 0`, **`max_opcode = 189`**,
`label_count = 7`, `function_count = 3` (functions `mk/0`, `module_info/0`,
`module_info/1`). Atoms (wire indices 1..=9): `nr_def, mk, rec, a, b, module_info,
erlang, get_module_info, true`. Imports: `{erlang, get_module_info, 1}` and
`{erlang, get_module_info, 2}`. `Line`: `version 0`, `flags 0`, `instr_count 3`,
`item_count 1`, `name_count 0` (⇒ `files == ["nr_def.erl"]`).

Opcodes emitted (`beam_disasm` names; family names normalized):
`call_ext_only`, `func_info`, `line`, `move`, **`put_record`**.

`nr_def`'s `Recs` payload (21 bytes):

```
00 00 00 00   version = 0
00 00 00 01   record_count = 1
00 00 00 02   total_field_count = 2
05 32 92 17 40 42 00 52 00   the single record entry
```

One `call_last`-shaped entry: `05` = `call_last`; `32` = atom 3 = `rec` (the record
name); `92` = atom 9 = `true` (the export flag, from `-export_record([rec])`);
`17` = `z,1`; `40` = `u` count `4`; then the two `(name, default)` pairs `42 00` =
`{a, u,0}` / `52 00` = `{b, u,0}` — both defaults are the `u,0` **absent** sentinel
(⇒ `RecordField.default == None`).

`nr_use.beam`: **940 bytes**, `FOR1` form length **932**; chunks in file order:

| # | id | size | parsed |
|---|----|------|--------|
| 1 | `AtU8` | 135 | atom names |
| 2 | `Code` | 224 | `Code` header + instruction stream |
| 3 | `StrT` | 0 | empty string table |
| 4 | `ImpT` | 28 | 2 imports (`get_module_info/1,2`) |
| 5 | `ExpT` | 100 | 8 exports (the 6 user functions plus `module_info/0,1`) |
| 6 | `LitT` | 28 | literal pool (the `{recdef, rec}` record-identity literals) |
| 7 | `Meta` | 45 | skipped |
| 8 | `Recs` | 22 | the `#local` native-record definition |
| 9 | `LocT` | 4 | parsed (JIT-only local-function table) |
| 10 | `Attr` | 83 | raw attributes (incl. `-import_record`) |
| 11 | `CInf` | 27 | raw compile info (path-free) |
| 12 | `Dbgi` | 66 | recognized id, payload unparsed (debug info) |
| 13 | `Line` | 32 | line table |
| 14 | `Type` | 12 | type table |

`Code` header: `head_size = 16`, `format_number = 0`, **`max_opcode = 191`**,
`label_count = 19`, `function_count = 8` (the six exported functions plus
`module_info/0,1`). Atoms (wire indices 1..=17): `nr_use, local_mk, local, a, b,
local_peek, local_fld, def_mk, def_upd, recdef, rec, external, def_fld, module_info,
erlang, get_module_info, false`. `Line`: `version 0`, `flags 0`, `instr_count 8`,
`item_count 6`, `name_count 0` (⇒ `files == ["nr_use.erl"]`).

Opcodes emitted (`beam_disasm` names; the family's `beam_disasm` renames undone):

```
is_any_native_record      (beam_disasm: {test,is_record,F,Src})
is_native_record          (beam_disasm: {test,is_record,F,Src,Mod,Name})
get_record_elements
put_record
is_record_accessible      (beam_disasm: {test,is_record_accessible,...})
get_record_field          (beam_disasm: {test,get_record_field,...})
```

plus the supporting `badmatch`, `badrecord`, `call_ext_only`, `func_info`, `line`,
`move`, `put_tuple2`, `test_heap`. The two `{recdef, rec}` identity operands are the
`put_record`/`get_record_field` `Id` literals for the cross-module `#recdef:rec`
accesses (a `z,4` `LitT` reference).

`nr_use`'s `Recs` payload (22 bytes):

```
00 00 00 00   version = 0
00 00 00 01   record_count = 1
00 00 00 02   total_field_count = 2
05 32 0a 11 17 40 42 00 52 00   the single record entry
```

`05` = `call_last`; `32` = atom 3 = `local` (record name); `0a 11` = atom 17 =
`false` (the export flag — `#local` is **not** exported); `17 40` = a `z,1` list of
length 4; then `42 00` = `{a, absent}` and `52 00` = `{b, absent}`.

## `linecov.beam` — `executable_line` (183)

Compiled with `+line_coverage`, so the compiler emits one `executable_line/2` per
executable source line (`tests/line_info.rs`).

### Regeneration

```sh
SCRATCH="$TMPDIR/neuter_linecov"
mkdir -p "$SCRATCH/gen"
cp linecov.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic +line_coverage linecov.erl )
cp "$SCRATCH/gen/linecov.beam" linecov.beam   # writes linecov.beam (596 bytes)
```

`linecov.erl`:

```erlang
-module(linecov).
-export([f/1, g/0]).

f(X) when X > 0 -> X;
f(_) -> 0.

g() ->
    A = 1,
    B = 2,
    A + B.
```

### Wire summary

**596 bytes**, `FOR1` form length **588**; chunks in file order (id · size):
`AtU8` 51 · `Code` 106 · `StrT` 0 · `ImpT` 28 · `ExpT` 52 · `Meta` 45 · `LocT` 4 ·
`Attr` 39 · `CInf` 27 · `Dbgi` 81 · `Line` 29 · `Type` 10. `Code` header:
`format_number 0`, `max_opcode 183`, `label_count 10`, `function_count 4`. Atoms
(1..=6): `linecov, f, g, module_info, erlang, get_module_info`. `Line`: `version 0`,
**`flags 1`** (bit 0 = the line-coverage flag), `instr_count 9`, `item_count 6`,
`name_count 0` (⇒ `files == ["linecov.erl"]`). Opcodes: `executable_line` plus
`call_ext_only`, `func_info`, `line`, `move` and the `is_lt` test (`f/1`'s guard).

## `dbgline.beam` — `debug_line` (184)

Compiled with `+beam_debug_info`, so the compiler emits `debug_line/4` breakpoint
markers (`tests/line_info.rs`).

### Regeneration

```sh
SCRATCH="$TMPDIR/neuter_dbgline"
mkdir -p "$SCRATCH/gen"
cp dbgline.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic +beam_debug_info dbgline.erl )
cp "$SCRATCH/gen/dbgline.beam" dbgline.beam   # writes dbgline.beam (740 bytes)
```

`dbgline.erl`:

```erlang
-module(dbgline).
-export([f/1]).

f(X) ->
    Y = X + 1,
    Y * 2.
```

### Wire summary

**740 bytes**, `FOR1` form length **732**; chunks in file order (id · size):
`AtU8` 64 · `Code` 100 · `StrT` 0 · `ImpT` 52 · `ExpT` 40 · `LitT` 30 · `Meta` 45 ·
`DbgB` 41 · `LocT` 4 · `Attr` 39 · `CInf` 27 · `Dbgi` 126 · `Line` 23 · `Type` 10.
`+beam_debug_info` adds both the legacy `DbgB` chunk (recognized, payload unparsed)
and the modern `Dbgi` chunk. `Code` header: `format_number 0`, `max_opcode 184`,
`label_count 7`, `function_count 3`. Atoms (1..=9): `dbgline, f, entry, line, erlang,
'+', '*', module_info, get_module_info` (the `entry`/`line` atoms are the `debug_line`
`Kind` operand). `Line`: `version 0`, `flags 0`, `instr_count 8`, `item_count 3`,
`name_count 0` (⇒ `files == ["dbgline.erl"]`). Opcodes: `debug_line` plus
`call_ext_only`, `func_info`, `gc_bif`, `line`, `move`.

## `bstail.beam` — `bs_test_tail2` (121)

The only committed fixture that carries the standalone `bs_test_tail2/3` generic
opcode (`tests/bs_test_tail.rs`). By default `beam_ssa_opt`'s `ssa_opt_bs_ensure`
pass annotates every binary-matching block, and `beam_ssa_codegen`'s `bs_translate/1`
then folds each `bs_test_tail` into a `bs_match` `test_tail`/`ensure_exactly` command —
so the generic opcode is never emitted. Compiling with **`+no_ssa_opt`** disables that
pass (`beam_ssa_codegen.erl`'s `NoBsMatch` path), leaving the standalone instruction in
the `.beam`.

### Regeneration

```sh
SCRATCH="$TMPDIR/neuter_bstail"
mkdir -p "$SCRATCH/gen"
cp bstail.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic +no_ssa_opt bstail.erl )
cp "$SCRATCH/gen/bstail.beam" bstail.beam   # writes bstail.beam (564 bytes)
```

`bstail.erl`:

```erlang
-module(bstail).
-export([f/1]).

f(<<X:8>>) -> X.
```

### Wire summary

**564 bytes**, `FOR1` form length **556**; chunks in file order (id · size):
`AtU8` 48 · `Code` 102 · `StrT` 0 · `ImpT` 28 · `ExpT` 40 · `Meta` 45 · `LocT` 4 ·
`Attr` 39 · `CInf` 27 · `Dbgi` 78 · `Line` 22 · `Type` 10. `Code` header:
`format_number 0`, `max_opcode 181`, `label_count 8`, `function_count 3`. Atoms
(1..=5): `bstail, f, module_info, erlang, get_module_info`. `Line`: `version 0`,
`flags 0`, `instr_count 3`, `item_count 1`, `name_count 0` (⇒ `files ==
["bstail.erl"]`). Opcodes: **`bs_test_tail2`** plus `bs_start_match3`,
`bs_get_position`, `bs_get_integer2`, `bs_set_position`, `bs_get_tail`, `jump`,
`call_ext_only`, `func_info`, `line`, `move`. In `f/1` the stream is

```
{test,bs_start_match3,{f,1},1,[{x,0}],{x,1}}
{bs_get_position,{x,1},{x,0},2}
{test,bs_get_integer2,{f,3},2,[{x,1},{integer,8},1,{field_flags,[]}],{x,2}}
{test,bs_test_tail2,{f,3},[{x,1},0]}      % fail label 3, ctx X(1), bits 0
{move,{x,2},{x,0}}
return
```

i.e. the loader lowers it to `Instr::BsTestTail2 { fail: Label(3), ctx: X(1),
bits: 0 }` — the `<<X:8>>` pattern requires exactly 0 bits to remain after the byte.
