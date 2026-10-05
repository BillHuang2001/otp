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

```sh
cd erts_rust/crates/beam_loader/tests/fixtures
erlc coverage.erl     # writes coverage.beam (1840 bytes)
```

The committed `coverage.beam` was produced by **copying `coverage.erl` into a scratch
directory and running `erlc` there**, so its `CInf` source path is a temp path — the same
convention as `slice.beam` (whose `CInf` records `/tmp/…/slicegen/slice.erl`). This keeps the
worktree path out of the committed binary. Any byte change in the fixture invalidates the
expected values; re-derive them with `beam_disasm:file("coverage.beam")` plus a byte decode,
do **not** guess.

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

File size **1840 bytes**; `FOR1` form length **1832** (= file_size − 8); form type `BEAM`.
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
| 9 | `LocT` | 16 | skipped (JIT-only) |
| 10 | `Attr` | 39 | raw attributes |
| 11 | `CInf` | 111 | raw compile info |
| 12 | `Dbgi` | 107 | skipped (debug info) |
| 13 | `Line` | 62 | line table |
| 14 | `Type` | 43 | type table |

Unlike `slice.beam`, this module carries **`FunT`** (the closure table) and a non-empty
**`LitT`**. `Dbgi` (debug info) is again not a known chunk id and is skipped.

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
`beam_disasm` reports **34** distinct top-level instruction names:

```
allocate
bif
bs_create_bin
bs_get_position
bs_get_tail
bs_match
bs_set_position
call
call_ext_only
call_fun2
deallocate
fconv
fmove
func_info
gc_bif
get_list
get_map_elements
get_tuple_element
init_yregs
label
line
make_fun3
move
put_list
put_map_assoc
put_map_exact
put_tuple2
return
select_tuple_arity
test
test_heap
'try'
try_case
try_end
```

Plus the stream terminator **`int_code_end`/0** (genop `3`), which `beam_disasm` does not
surface — so the code stream contains **35 distinct opcodes**.

Relative to `slice.beam`'s subset (`call_ext_only call_only func_info gc_bif label line move
return swap test`), the unusual additions are:

- **funs/closures** — `make_fun3` (create) and `call_fun2` (invoke); note the fun is invoked
  through `call_fun2`, not `call_fun`.
- **exceptions** — `'try'`, `try_end`, `try_case`.
- **floats** — `fconv` (X↔float-register conversion), `fmove`.
- **maps** — `put_map_assoc`, `put_map_exact`, `get_map_elements`.
- **bitstrings** — `bs_match` (this is what raises `max_opcode` to 182), `bs_start_match3`,
  `bs_get_position`, `bs_set_position`, `bs_get_tail`, `bs_create_bin`.
- **tuples** — `select_tuple_arity`, `put_tuple2`, `get_tuple_element`.
- **lists** — `get_list`, `put_list`.
- **frame/stack** — `allocate`, `deallocate`, `test_heap`, `init_yregs`.
- the plain **`bif`** instruction (used for `element/2`, `raise/2`, `fdiv/2`) in addition to
  `gc_bif`.

The `test` instruction covers the `is_tuple`/`is_nonempty_list`/`is_nil`/`is_eq_exact`/
`is_integer`/`is_ge` family, and the local `call` (not `call_ext`) is used for the module-local
call to `make_adder/1`.
