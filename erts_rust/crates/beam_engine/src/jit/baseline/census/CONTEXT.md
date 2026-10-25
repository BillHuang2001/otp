# `census` — the baseline tier's deopt, execution and code-size censuses

## Intent
The test-only instruments that rank what the tiers still fail to lower, which shapes force a
fallback, and how much code each lowered shape costs: the evidence a widening of an inline set is
chosen from, and the reference record the tier audit reads.
Every instrument here is `#[cfg(test)]`-gated at its call site, so they cost a production build
nothing, and every one of them asserts only **liveness/consistency** — never a threshold — so the
printed tables are the evidence and the tests keep passing as an inline set changes.
The figures below are recorded **with their method** rather than as expectations: the corpus follows
the installed OTP tree, so a count moves when the host's OTP moves.
This node owns the census *record*; the instruments' code lives in the files the routing table names.

## API Surface (crate-visible; test builds only)
- `../counters.rs` — `Counters`, its `record_*` call sites, `snapshot()`, and the static deopt maps
  (`instr_instances`/`instr_deopts`); `variant_name` re-exports `crate::exec_census::variant_name`
  (one attribution, never a copy).
  - the `jit_step_one`/`jit_step_ext` deopts, with a `Test` deopt keyed by kind (`"Test(IsEqExact)"`,
    `"Test(IsTuple)"`, …) in `deopts_by_variant`, so which `test` shapes still deopt is readable per
    predicate;
  - the driver round-trips;
  - the prologue position/publish calls;
  - the **register-cache traffic** (`record_spill`/`record_fill`/`record_block`: per-block
    IR/byte/cache/write-set sizes plus spill/fill stores by site), where one recorded run yields all
    **three** spill **store** models (the **dirty** set `spill` emits via `spill_slots_at(pc)`, the
    whole-block **written** set and the **full cache**) and both fill models;
  - the **Y-window traffic** (`record_y_access`/`record_y_invalidation`/`record_block`: emitted guarded
    `Y` loads/stores and their emitted bytes, `Y`-touching blocks, the in-block refresh sites, and the
    same-block `Y`-slot reuse count);
  - the **static deopt census** (`record_instr_instance`/`record_instr_deopt` plus the
    `instr_instances`/`instr_deopts` maps driving `../census.rs`); `record_instr_instance` runs at the
    top of `../emit/block.rs`'s range loop and `record_instr_deopt` in its `emit_deopt` wrapper.
  - `blocks` / `block_ir_instructions` — the emitted-block count and Σ `end − start`, the second of the
    three counting bases below.
- `../census.rs` — four views, each printing a table: the static corpus census, the runtime
  hot-shape census (deopts **and**, from the same workloads on the interpreter tier, the
  `exec_census` per-variant execution ranking plus typed-error table via its
  `dynamic_execution_census` helper), the register-cache traffic census, and the per-kind
  emitted-code-size table.
- `../emit/test/refusal_census.rs` (`#[cfg(test)] mod refusal_census` from `../emit/test.rs`) —
  `the_refusal_attribution_census_is_printed`, attributing `test`/`select_val`/`select_tuple_arity`
  refusals to exact predicates/operand shapes, listing the `bif`/`gc_bif`/`bif0` deopt pools by BIF
  name, and classifying every `Bs*` instance as inlined / refused-by-one-closed-reason / no-arm
  (Table 4b). The pools are **self-maintaining**: `already_inlined(module, instr)` consults the emitters'
  real shape predicates (`comparison_shape`/`fast_arith`/`bif_shape`) rather than a name allowlist,
  so a newly lowered name leaves its pool automatically (the `bif0` pool has no arm to consult —
  nothing lowers `Bif0`).
- `../operand_tests.rs` — `the_move_deopt_breakdown_over_the_corpus_is_printed`: every `Instr::Move`
  instance attributed to its refusal class, asserting only that the classifier is **total** (no
  unclassified `??` class).
- The *variant coverage* census is **not** here: `crates/beam_engine/tests/census.rs` is the
  executable gate (`EXECUTED_VARIANTS`/`ZERO_CORPUS_VARIANTS`), and its figures are recorded in
  `../../../MEASURED.md` §Execution coverage.

## The evidence record (measured; never a gate)
Recorded on the tree at `85e5293043` with the oracle `erl` at OTP **29.1.1 / erts-17.1**
(`/nix/store/khp1dd3fk8476mwg62cinwvk5a17r53s-erlang-29.1.1`), 1-min load sampled around the runs
(0.19–6.96 on a 14-core host).
These are **counts**, not timings: host load does not affect their validity (only their wall time),
which is why a census may run beside a measurement pass.
The corpus is discovered generically — **38** committed `tests/fixtures/*.beam` plus **1321**
installed-OTP `.beam`s (every `<lib>/<app>-<vsn>/ebin`, no hardcoded app names, cap
`OTP_MODULE_CAP = 4096`) = **1359** modules / **103 079** functions, `functions_skipped = 0`.
The **fixture half is version-controlled** and the OTP half is not: any figure that must not drift
with the host is taken over the fixtures alone (see Table 2).
**Every figure below was re-derived at `85e5293043`** by running the gate commands under `## Gate
commands` (one `--lib <test name> -- --nocapture` run each, `--test-threads=1` for the multi-test
filter); each table quotes the current reading, and where a mechanism moved a figure that mechanism
is named. Nothing here is hand-carried from an older tree.

### The three instruction counting bases
The same corpus is quoted on three denominators, and they are not interchangeable:
- **Raw IR** — each loaded module's own instruction list, once per instruction
  (`function.code.len()`). No blocks, no compilation: the *text* of the program. **No instrument
  prints this total** (the refusal census and the `Move` breakdown walk on this base but print only
  their sub-populations), so it is named here to keep the other two from being mistaken for it.
- **Σ emitted-block ranges** — `block_ir_instructions` = Σ `end − start` over every `emit_block`
  call, IR blocks **and** continuation sub-blocks: **3 586 410** over **597 565** blocks.
- **Visited instances** — Σ `instr_instances`, what the lowering dispatch actually *looked at*:
  **3 114 364**. This is the deopt census' denominator, and the only base a deopt fraction may be
  taken on.
Why visited < ranges (the gap is **472 046 = 13.2 %**), in two mechanisms:
1. **The loop stops at a terminator.** `emit_block`'s range loop sets `terminated = true` and breaks
   at **nine** sites — one per terminator arm, each a `terminated = true` + `break;` pair, the `break;`s
   at `../emit/block.rs:374, 478, 493, 502, 570, 641, 678, 771, 862` (this is the tree's **only**
   per-site enumeration; prose elsewhere names the nine pairs and points here) — so every
   instruction of a block's `[start, end)` range after its first block-terminating instruction is
   never counted — while `record_block` records the full `end − start`.
2. **A continuation sub-block re-covers a tail.** `../emit/compile.rs::emit_sub_blocks` emits
   `[p, end)` of the containing IR block (capped at the next continuation address, so the sub-blocks
   are disjoint) for every frame-preserving call's continuation address — those instructions lie
   inside an IR block's range *and* in a sub-block's range, so the range total counts them twice.
Neither mechanism is a defect — each is a deliberate property of the emitter — but they mean a deopt
fraction is a fraction of the **visited** population, so `119 189 / 3 114 364 = 3.83 %` and
`119 189 / 3 586 410 = 3.32 %` describe the same deopts on two different (both defensible, never
interchangeable) denominators.

### Table 1 — the static deopt census (the primary evidence)
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_static_deopt_census_over_the_corpus_is_printed -- --nocapture`
| variant | deopts | instances | deopt ratio |
|---|---|---|---|
| `FuncInfo` | **103 079** | 103 079 | 1.0000 |
| `Bif` | 10 014 | 17 356 | 0.5770 |
| `GcBif` | 2 559 | 24 466 | 0.1046 |
| `Bif0` | 3 205 | 3 205 | 1.0000 |
| `NifStart` | 300 | 300 | 1.0000 |
| `Test(IsBoolean)` | 17 | 358 | 0.0475 |
| `Test(IsFunction2)` | 9 | 331 | 0.0272 |
| `OnLoad` | 6 | 6 | 1.0000 |
Totals: **119 189** deopts / **3 114 364** visited instances (**3.83 %**) over **8** deopting variants
out of **119** variants visited at all; every non-`FuncInfo` deopt together is **16 110 / 3 011 285 =
0.54 %**, and `FuncInfo` alone is **86.5 %** of all deopts.
`FuncInfo` deopts exactly once per compiled function (**103 079 = `functions_compiled`**, ratio
1.0000) because it is the failed-head-clause `function_clause` raise and is deliberately never
inlined — the *never-executed* shape, so its 86 % share of the count is 0 % of the run time.
The Y-window split on the same run: `blocks=185 457 publish=133 233 elided=52 224` (71.8 % of
Y-touching blocks publish) `invalidations=544 965`.
**The eight deopting rows are a property of the emitter, not of the inline set**, and it is worth
stating why: this table counts what *deopts*, and a `Bs*` shape never appears in it — the
bitstring family's fallback is the family's own `claimed` helper arm (`jit_bitstr`), not an
`emit_deopt`, so a shape entering or leaving the native bitstring set cannot move a deopt count. The
lowering's census evidence is Table 4b (the `inlined`/`refused` split), Table 3 (the emitted
bytes) and Table 4 (the per-kind sizes), never this table.

### Table 2 — the deopting-variant set over the **committed fixtures** (corpus-independent)
`ERTS_ORACLE=/nonexistent/erl erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_static_deopt_census_over_the_corpus_is_printed -- --nocapture`
(an unusable `ERTS_ORACLE` makes `otp_lib_root()` answer `None`, so the census runs the fixtures
alone and skips the OTP half loudly — the same shape a host without `erl` takes).
| variant | deopts | instances |
|---|---|---|
| `FuncInfo` | 422 | 422 |
| `Bif` | 11 | 11 |
| `GcBif` | 4 | 34 |
| `Bif0` | 3 | 3 |
Totals over 38 fixtures / 422 functions: **440** deopts / **4 500** visited instances over
**4** deopting variants out of **99** visited.
This is the one quantity in this node that **cannot drift with the installed OTP tree**, and it is
the set worth pinning: it says exactly which shapes fall back on the baseline tier for real compiler
output — the always-deopting `FuncInfo` (one per function), the un-armed `Bif0` dispatch, and the
`Bif`/`GcBif` shapes whose per-name predicates decline (Table 6 says which names; over the fixtures
`Bif` declines every instance it sees while `GcBif` inlines 30 of its 34).
`NifStart`/`OnLoad` deopt 1.0 as well but never occur in the fixtures, so they exist only in the OTP
half of Table 1.
The fixture corpus also gives the deterministic half of the register-cache totals:
`modules=38 functions=422 blocks=1192 ir_instrs=4570 emitted_bytes=342 362`; Table 4's per-fixture
`code_len` column is the artifact-side reading of the same set (**341 465 B** over the 38 fixtures,
`bytes` here additionally counting the continuation sub-blocks).
No committed fixture carries a lowerable `bs_create_bin` (Table 4b's fixture reading: **0** inlined,
**48** refused), so the fixture bytes are entirely the **match-context** shapes — and the seven
newly inlined ones are already present in them (`bs_match_string` 2, `bs_get_utf16` 3,
`bs_get_utf32` 2, `bs_get_utf8` 2, `bs_skip_utf16` 2, `bs_skip_utf32` 2, `bs_skip_utf8` 2
instances, **15** together). The fixture half is therefore independent of the installed OTP tree but
**moves with the inline set**, so its figures are quoted per record and never pinned.

### Table 3 — register-cache traffic
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_register_cache_traffic_census_over_the_corpus_is_printed -- --nocapture`
Grand total over 1359 modules / 103 079 functions / 597 565 blocks / 3 586 410 Σ ranges:
spill **stores** `dirty` **1 391 950**, `written` 4 409 437, `full-cache` 5 287 564 — the dirty-set
rule removes **3 017 487** already-clean stores (**−68.43 %** of the written-set model), exactly
7 B per removed store.
Fill **register loads**: observable (block-local liveness ∪ the instruction's own inline write set)
**1 550 926** vs full-cache 4 014 463.
Emitted bytes **300 271 737**; spill calls 2 519 833 (Helper 1 806 482 / Exit 713 351), fill calls
2 024 746 (Helper 1 427 181 / Prologue 597 565).
The three store models come from **one** recorded run, so they are directly comparable.
The OTP half alone reads `blocks=596 373 cache_slots=766 889 written_slots=626 764
emitted_bytes=299 929 375 ir_instrs=3 581 840`, spill stores `1 391 164 / 4 407 275 / 5 284 345`,
fill loads `1 549 820 / 4 011 784`, spill calls 2 516 491 (Helper 1 804 161 / Exit 712 330) and
fill calls 2 021 742 (Helper 1 425 369 / Prologue 596 373); the remainder is Table 2's fixture half.
A lowering changes a block's **contents**, never the block decomposition, so `blocks` and
`ir_instrs` are the two figures the inline set leaves alone while `emitted_bytes` and the store/fill
traffic move with it.

### Table 4 — per-kind emitted code size
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_per_kind_emitted_code_size_table_is_printed -- --nocapture`
The reference is a block with ONE always-deopting `func_info`: **195 bytes**, whose `2x − 1x`
marginal — the fixed per-occurrence deopt sequence every un-lowered instruction costs — is
**60 bytes**.
Every row is asserted **lowered**, i.e. its block records `deopts == 0`, so a row cannot describe a
shape that quietly went back to the helper.
| shape | inlined block | vs deopt ref | per-occurrence | lowered |
|---|---|---|---|---|
| `Move x<-int` | 158 | −37 | +10 | yes |
| `Move x<-literal` | 268 | +73 | +127 | yes |
| `Test Y-src (is_nil)` | 308 | +113 | +177 | yes |
| `SelectVal Y-src` | 243 | +48 | n/a (block ends) | yes |
| `SelectTupleArity Y-src` | 349 | +154 | n/a (block ends) | yes |
| `Call` (frame-preserving) | 170 | −25 | n/a (block ends) | yes |
| `Family arm (put_tuple2)` | 195 | +0 | +60 | yes |
| `BsGetPosition` | 340 | +145 | +193 | yes |
| `BsSetPosition` | 421 | +226 | +274 | yes |
| `BsTestTail2` | 349 | +154 | +208 | yes |
| `BsSkipBits2` | 347 | +152 | +206 | yes |
| `BsStartMatch3` | 431 | +236 | +291 | yes |
| `BsStartMatch4` | 431 | +236 | +291 | yes |
| `BsGetInteger2` | 726 | +531 | +586 | yes |
| `BsMatch (static cmds)` | 799 | +604 | +659 | yes |
| `BsCreateBin (6-bit append)` | 604 | +409 | +458 | yes |
| `CallLast` (unlinked exit) | 213 | +18 | n/a (block ends) | yes |
Σ of the seventeen deltas, one occurrence each: **+3037 B** (this is the current reading of the
instrument named above, not a hand-edited figure). Both figures are **whole-block** sizes, so
both carry the shared prologue/exit/epilogue; the `2N − N` **marginal** is the confound-free
per-occurrence figure and only exists where a second copy fits in the block.
An artifact-wide deopt-only figure would multiply each delta by that shape's occurrence count, which
this table deliberately does not do; the whole-artifact `code_len` over the 38 fixtures is
**341 465 B** (largest `bsmatch.beam` 51 272 B / 90 blocks / 385 emitted instances, `bssmall.beam`
37 876 B / 102 / 359).
The nine `Bs*` rows are the reason the bitstring arms are budgeted as their own code-size class; the
`bs_create_bin` row is the **bit-granular** route's (`private_append` over an x-register accumulator,
append unit `1`, one six-bit `integer` field — `body_bs_create_bin`, the shape the `emqx_base62`
decoder re-enters per character), so it prices the general door (`jit_open_append_bin_any`) **plus
the in-block field store** — a six-bit field is under `MAX_INLINE_FIELD_BITS` (57), so it costs no
`jit_write_bit_field` call — and not the aligned route's byte stores. Only the bitstring rows move
with the inline set; every other row is fixed by its group's emitter.
`emit/compile.rs::function_capacity_estimate` adds `BS_PATH_BYTES` (448) per bitstring-path
instruction, `BS_MATCH_COMMAND_BYTES` (320) per accepted `bs_match` command,
`CREATE_BIN_PATH_BYTES` (16 384) per `bs_create_bin`, `BS_MATCH_STRING_PATH_BYTES` (2 304) per
`bs_match_string`, `UTF32_PATH_BYTES`/`UTF16_PATH_BYTES` (1 024 each) per UTF-32/UTF-16 read or skip
and `UTF8_PATH_BYTES` (2 048) per UTF-8 one, so neither an unbounded `bs_match` list, a
maximum-width construction arm nor a longest literal pattern or character sequence can
under-allocate the `CodeBuffer`.

### Table 4b — bitstring refusal attribution
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_refusal_attribution_census_is_printed -- --nocapture`
The instrument is `emit/test/refusal_census.rs`; it walks the same corpus and classifies every `Bs*`
instruction instance as inlined, refused by one closed reason, or having no arm at all.
Current corpus reading (the run behind the rows above): **27 438** `Bs*` instructions —
**15 929** inlined, **6 735** refused, **4 774** with no arm.
The refuse rows: **3 247** `bs_match` allocating command, **1 764** `bs_create_bin` `other`,
**1 077** `bs_skip_bits2` runtime size/unit/stride, **243** `bs_get_utf16` signed/little/native
field, **171** `bs_get_integer2` runtime size/unit/stride, **73** `bs_match` width rule,
**61** `bs_get_integer2` signed/little/native field, **47** `bs_create_bin` literal-area subject,
**40** `bs_match` signed/little/native field, **5** `bs_match_string` pattern not lowered (offset,
length cap or bit width), **4** `bs_get_utf32` signed/little/native field, **1** `bs_match` >1 `Y`
destination, **1** `bs_skip_utf16` signed/little/native field, **1** `bs_skip_utf32`
signed/little/native field.
The no-arm rows are exactly the four deliberately un-armed opcodes: `bs_get_tail` 3 694,
`bs_get_binary2` 972, `bs_init_writable` 93, `bs_get_float2` 15 — every UTF opcode and
`bs_match_string` carries an arm, so it can only appear in the inlined/refused columns.
`bs_create_bin` is a **two-way** arm like the seven others in the accounting (`bs_match`,
`bs_match_string`, `bs_get_utf16`, `bs_get_utf32`, `bs_skip_bits2`, `bs_skip_utf16`,
`bs_skip_utf32`): of its **1 922** corpus instances **111** are inlined and **1 811** refused, split
**1 764** `other` (the shape predicate's own
declines — a plain/later-position or absent `private_append`, an append unit outside `1`/`2`/`4`/`8`,
a `binary`/`float`/`utf*` segment, an `all` or runtime (`Dynamic`) size, a `little`/`signed`/`native`
flag, a zero-width or wider-than-a-machine-word field, a `StrT` window the module cannot resolve, a
plan with no fields, a field list whose **bit** offsets do not sum to the plan's `tail_bits`, a
`Route::BitGranular` plan carrying a `string` (`Field::ConstBytes`) field, the
`MAX_FIELDS`/`MAX_TAIL_BYTES` caps) and **47** literal-area subject; the
instrument's own `inlined | refused | opcode` table carries that accounting for every arm
(`bs_get_integer2`'s 232 instances are all refused, `bs_skip_bits2`'s 3 of 1 080 inline,
`bs_create_bin`'s 111 of 1 922 inline).
Over the **committed fixtures alone** (`ERTS_ORACLE=/nonexistent/erl` on the same command) the
reading is `paths=38` / `bs_instructions=441 inlined=257 refused=101 no_arm=83`, and the seven new
match-context shapes carry **15** of the 257 inlined instances there; the fixture refusals are 39
`bs_create_bin` `other` + 9 `bs_create_bin` literal-area subject + 17+17+17 `bs_match` (allocating
command / signed-little-native field / width rule) + 1 `bs_get_utf16` + 1 `bs_skip_bits2`, and its
no-arm rows are `bs_get_tail` 73, `bs_get_float2` 8, `bs_get_binary2` 2.
`bs_instructions`, `no_arm` and `paths`/`modules_loaded` are a property of the corpus walk, so a
route or arm decision changes only the inlined/refused attribution.
The reason vocabulary is a closed set of `const` strings (`BS_REASON_ALLOC`, `_LITERAL`, `_RUNTIME`,
`_UNIT`, `_WIDTH`, `_NO_FAIL`, `_LABEL`, `_Y_DST`, `_FIELD`, `_OTHER`) plus the `bs_match_string`
arm's own inline literal ("pattern not lowered (offset, length cap or bit width)"), so a new refusal
must be named rather than folded into a catch-all, and the test pins the accounting
`inlined + refused + no_arm == bs_instructions` so no instance can be dropped silently.

### Table 5 — runtime deopt and dynamic execution (the hot in-crate shapes)
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_runtime_deopt_census_over_the_hot_shapes_is_printed -- --nocapture`
Runtime deopts over `crate::jit::bench::assert_tier_agreement()`: `step_one=320 step_ext=2560
total=2880` over **2** distinct variants — `CallExt` 2 560 and `SelectVal` 320.
`CallExt` is an **`jit_step_ext`**, i.e. the arm natively resolves the import and hands the driver the
`ENTER` word — a driver round-trip by design, not an un-lowered opcode, so the hot-shape runtime
residual is one `SelectVal` run-time guard rejection (not a static refusal).
Dynamic per-variant **execution** census, same workloads on `Tier::Interpreter`:
**31 720** instructions executed, **35** distinct variants, **0** typed errors; top rows `GcBif`
6 600, `Label` 4 135, `Move` 3 330, `Bif` 2 565, `CallExt` 1 280.
That tier pin is deliberate: a native tier emits a lowered instruction as native code and never
enters the funnel, so its counts would describe only the deopts the table above already covers.

### Table 6 — refusal attribution and the `Move` breakdown
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_refusal_attribution_census_is_printed -- --nocapture`
Corpus `paths=1359 modules_loaded=1359`.
`test` refusals — **26** total: `Test(IsBoolean)` 17 (`no true/false atom interned`),
`Test(IsFunction2)` 8 (`arity operand: X(in-bound,tr)`) + 1 (`arity operand: Y(tr)`).
`select_val` and `select_tuple_arity` refusals — **none**: every corpus instance of both is inlined
(a `Y`-source or register-source shape or a run-time guard; nothing is refused statically).
`bif` pool **8 633** — `erlang:get/1` 6 005, `erlang:element/2` 570, `erlang:map_get/2` 510,
`erlang:=:=/2` 239, `erlang:node/1` 210, `erlang:max/2` 165, `erlang:and/2` 123,
`erlang:is_map_key/2` 121, `erlang:not/1` 113, `erlang:tl/1` 111, then the `is_*/1` predicates and
comparisons at ≤ 92 each.
`gc_bif` pool **2 381** — `erlang:bit_size/1` 762, `erlang:bsl/2` 706, `erlang:bxor/2` 149,
`erlang:map_size/1` 138, `erlang:-/1` 125, `erlang:bsr/2` 120, `erlang:round/1` 91,
`erlang:abs/1` 81, `erlang:binary_part/3` 77, then the narrow conversions and bit operators at
≤ 51 each, and `erlang:div/2`/`erlang:rem/2` 1 apiece (the integer fast path admits both; each
residual is one shape it declines).
`bif0` pool — `erlang:self/0` 1 979, `erlang:node/0` 675 (no arm exists at all).
These pools count each IR instruction **once** (the census walks `function.code`), while Table 1
counts each **visited** instruction per emitted block; a `Bif` inside a continuation sub-block range
is therefore counted in both the parent's and the sub-block's visit there, which is why the two
`Bif` totals differ (10 014 vs 8 633) and why neither may be substituted for the other.
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_move_deopt_breakdown_over_the_corpus_is_printed -- --nocapture`
`Move`: **553 735** instances over the corpus (**315** in the fixtures), **all** inlined
(`inlined = total`, no refusal class) — the operand axis of the inline set is spent.

## The per-tier fallback / typed-error inventory
What happens when a tier cannot execute something, by tier. The inline/refusal **sets** are owned by
`../inlineset/CONTEXT.md` and `../cranelift/CONTEXT.md`; this is the *mechanism* each tier uses and
the error each surfaces.
| Tier | executes natively / inline | per-instruction fallback | typed `BeamError` |
|---|---|---|---|
| `Tier::Interpreter` | every variant it models (the reference semantics) | — (it *is* the fallback) | `Instr::Generic` → `unsupported_opcode: <ops.tab name>` (`../../generic.rs`); `bad_register(n)` → `BadArg`; `unknown_label` → `BadArg`; an unmodelled shape → `unsupported_opcode: widened opcode not yet executed: <name>`; the non-blocking entries' `blocked()` / `suspended()` |
| `Tier::BaselineJit` | the inline set (`../inlineset/CONTEXT.md`) | every un-lowered variant, and every run-time guard rejection of a lowered one, → **exactly one** `Machine::step_instruction_with_entry` (`jit_step_one`); the block continues in-block on `Step::Next`. `call_ext*` → the driver round-trip `jit_step_ext` (the host seam is consulted **once**) | none of its own: it *inherits* the interpreter's error surfaces through that step, so a `bad_register`/`unknown_label`/`Generic` deopt reproduces the interpreter word for word |
| `Tier::Cranelift` (feature `jit-cranelift`, default OFF) | its own (smaller) inline set | the same shared `jit_step_one`, one interpreter step per instruction; a cross-module callee it enters runs on the driver's per-instruction step (`module_blocks` default `None`) | feature off: `unsupported("jit-cranelift tier not built")` from all four `../../jit/mod.rs` entries and `../../jit/bench/mod.rs`; feature on: a resume goes through the shared `baseline::run_native_resume_outcome` and an address outside `CompiledFunctions::is_enterable` takes the same per-instruction step. **No `Instr` raises a typed error on this tier** |
Two fallbacks are **not** per instruction and are worth stating separately, because neither is an
error:
- **A traced run refuses native entry wholly.** `run_native_label_outcome`
  (`../mod.rs`, shared by both native tiers) latches `ctx.trace_enabled()` once per run and, when it
  is on, hands the **whole slice** to `Machine::drive` — a natively executed block emits no trace
  event, so letting native code run would make a traced tier-run observably different from a traced
  interpreter run. What executes instead is the interpreter's own driver, with `Machine::trace_on`
  set; nothing is reported as unsupported.
- **A block whose layout bound the machine cannot cover declines native entry per address.** When
  `CompiledModule::native_entry_cached` returns `(entry, Some(highest))` and the machine's `x` file is
  not longer than `highest`, `enter_compiled_cached` answers `None` and the driver takes **one
  interpreter step for that address**. There is no whole-function error: a function whose bound is
  never satisfied simply runs step-by-step through the interpreter, correct and un-diagnosed. The
  bound is layout-dependent (eager artifact = whole-module max, lazy = the entered function's own),
  so the same function can enter natively in one artifact layout and not in the other.
Also not a fallback: `LOCAL_DECLINE` (the local-call seam declined a same-module callee), `ENTER`
(a cross-module transfer, a driver round-trip **by design**), `WAIT_BIT` (a `receive` park) and
`SUSPEND` (a BIF that did not complete) are block-ABI words the driver consumes; only `SUSPEND`
re-executes an instruction, and it is the *host's* answer, not a tier refusal.

## Gate commands
- Static census: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_static_deopt_census_over_the_corpus_is_printed -- --nocapture`
- Emitted-byte table: the same command with `--lib the_per_kind_emitted_code_size_table_is_printed`.
- Register-cache census: the same with `--lib the_register_cache_traffic_census_over_the_corpus_is_printed`.
- Runtime deopt + dynamic execution: the same with `--lib the_runtime_deopt_census_over_the_hot_shapes_is_printed`.
- Refusal attribution: the same with `--lib the_refusal_attribution_census_is_printed`.
- `Move` breakdown: the same with `--lib the_move_deopt_breakdown_over_the_corpus_is_printed`.
- The **fixtures-only** reading of any of them (Table 2, and Table 4b's fixture half) is the same
  command with an unusable oracle: `ERTS_ORACLE=/nonexistent/erl …` makes `otp_lib_root()` answer
  `None`, so the walk covers the 38 committed `.beam`s alone and skips the OTP half loudly.
- All of them are run **one at a time** (`--test-threads=1` if several match the filter): each is a
  whole-corpus compilation pass, and a shared run only makes them slower, never wrong.

## Constraints
- `#[cfg(test)]`-only: no production path reaches these modules, and no measurement may change a
  result.
- `counters.rs` is `#[cfg(test)]`-only, so only a unit test under `src/jit/baseline/` can reach it;
  `census.rs` and `emit/test/refusal_census.rs` are the only non-`tests.rs` callers.
- A warmed `CodeCache` re-emits nothing, so a counter pin must compile cache-free through
  `emit::compile_function` (as `../call_tests.rs::the_frame_preserving_local_call_records_no_static_deopt`
  does). The census drivers call `clear_baseline_cache()` first and compile each function through the
  emitter directly.
- A census may run beside a measurement pass: these are counts. A *timing* run may not.

## Test Strategy
The instruments are their own suites: each prints its table and asserts only liveness/consistency,
and the structural counter pins live beside the lever they price (`../call_tests.rs`,
`../tail_exit_tests.rs`, `../bif_tests.rs`, `../tests.rs::the_y_window_traffic_of_the_committed_fixtures_is_measured`,
`../emit/test/refusal_census.rs`).
The enumeration is `../testing/CONTEXT.md`; the levers these instruments chose are `../measure/CONTEXT.md`;
the crate-level measured record is `../../../MEASURED.md`.
**What is pinned vs what is quoted:** the only corpus-independent quantity is the **fixture-only**
deopting-variant set (Table 2) — `FuncInfo`, `Bif`, `GcBif`, `Bif0` — which is a property of the
emitter and of the committed fixtures, so it can be asserted exactly without brittleness. Everything
else follows the installed OTP tree and is recorded as method + figure, never as an assertion.

## Known Issues
- **No census pins a corpus count.** Every corpus-level assertion is liveness/consistency only
  (`../census.rs`'s totals/blocks assertions, `../emit/test/refusal_census.rs`'s
  `modules_loaded > 0`, `../operand_tests.rs`'s `total > 0` + no unclassified `??`). A rerun on a
  different tree prints different numbers without any test failing, so the figures above must be
  re-derived from the commands, never quoted as expectations.
- **The only exact-number assertions in this node are on synthetic shapes** (the emitted-code-size
  reference block and the per-kind rows, which compile hand-built single-block modules).
- **Three different denominators are in circulation** (raw IR, Σ block ranges, visited instances) and
  no instrument prints the first. Quote a deopt fraction only against the **visited** total, and say
  so; `3.83 %` (visited) and `3.32 %` (Σ ranges) are the same 119 189 deopts.
- **The terminator sites are nine**: `../emit/block.rs` sets `terminated = true` and `break;`s on the
  next line, one such pair per terminator arm. The **per-site line enumeration lives only** in "The
  three instruction counting bases" above; every prose restatement elsewhere in the tree
  (`MEASURED.md` §register cache traffic, `PERF.md`, `src/jit/CONTEXT.md`) names the nine
  `terminated = true` + `break;` pairs and defers the numbers here instead of keeping a second list
  that drifts.
- **Crate-level copies of these figures are restatements, not the record.** The same corpus counters are
  quoted in `../../../MEASURED.md` (§register cache traffic: corpus line, the three store models and the
  byte deltas), `../../../PERF.md`, `../../../TEST_NOTES.md` (the census entry-point row),
  `../../../CONTEXT.md` (the inline-set row and the per-function `CodeBuffer` sizing row),
  `../../CONTEXT.md` (Known Issues: the prose-only-totals row) and
  `../{measure,emit/regs,emit/prologue,inlineset}/CONTEXT.md` (each lever's deterministic
  counters). This node owns the method and the corpus definition, so a figure quoted there must be
  re-derived from the command here, never copied onward.
- **A two-state A/B pair cannot be refreshed by one run.** Several recorded deltas are before/after pairs
  measured over **two tree states** at one corpus (`320 065 790 → 306 024 443` bytes, `7 167 996 →
  2 707 930` push/pop ops, `756 632 → 352 537` prologue loads). A single census run yields only the
  *current* absolute figure — re-measuring a pair means checking out both states, so a fresh absolute
  must never be substituted into a pair's row. The *ratios* are also corpus-dependent: the dirty-vs-written
  store ratio reads **−68.43 %** on today's 1359-module corpus against the **−65.9 %** recorded on the
  1353-module one, so neither the absolutes nor the percentage carries over.
- **A `Bif` total differs between Table 1 and Table 6** (10 014 vs 8 633) because the static census
  counts a **visited** instruction per emitted block while the refusal census counts each IR
  instruction **once**; neither is wrong and they are not comparable.
- **`../census.rs` is `#[cfg(test)]`-only and 1846 lines.** It is one measurement instrument with
  four views over one corpus-discovery helper, so splitting it is a separate, parity-neutral refactor.

## See Also
- `../CONTEXT.md` → the tier contract (inline set, deopt rule, routing table).
- `../inlineset/CONTEXT.md` → the authoritative inline set and each lowering's guards.
- `../measure/CONTEXT.md` → every figure these instruments produced and the levers they chose.
- `../testing/CONTEXT.md` → the test enumeration these suites are part of.
- `../runtime/CONTEXT.md` → the block-ABI words (`HALT`/`ERROR`/`ENTER`/`LOCAL_DECLINE`/`SUSPEND`/
  `WAIT_BIT`) the inventory above names.
- `../../CONTEXT.md` → the tier seam (`Tier`, `run_*_with_tier`, `available_tiers()`).
- `../../../MEASURED.md` → the crate-level measured record.
