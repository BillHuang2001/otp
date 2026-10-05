# Erlang Compiler — Sources (`lib/compiler/src`)

## Intent
Implementation of the `compiler` OTP application: translates Erlang source (`.erl`) or an intermediate form (`.abstr`, `.core`, `.S`) into BEAM object code.
Flat directory of ~68 modules with **no subdirectories** — one module per compiler pass/family, all installed under `compiler`.
Public API lives entirely in `compile.erl`; every other module is an internal pass or helper (`-moduledoc false` except `cerl`, `cerl_clauses`, `cerl_trees`, `compile`).

## API Surface — `compile.erl` (the module other apps/tools call)
- Entry points: `file/1,2`, `noenv_file/2` (skip `ERL_COMPILER_OPTIONS`), `string/1,2`, `forms/1,2`, `output_generated/1`.
- Legacy `erl_compile`/`erlc` interface: `compile/3`, `compile_asm/3`, `compile_core/3`, `compile_abstr/3` (take the `#options{}` record from `stdlib/include/erl_compile.hrl`).
- Introspection/utility: `options/0` (prints `--help`), `env_compiler_options/0`, `run_sub_passes/2`.
- Exported types: `option/0`, `forms/0` (abstract forms or `cerl:c_module()`), `comp_ret/0`.
- `cerl`/`cerl_clauses`/`cerl_trees` are the documented Core Erlang AST API (the only compiler modules with real `-moduledoc`, grouped as "Internal Modules" in `../doc/docs.exs`; used by `lfe`, `dialyzer`-adjacent tools, etc.); `cerl_inline` and `core_lib` are `-moduledoc false` but also used externally.

## The compilation pipeline (intermediate representations)
Source → Erlang **abstract code** (AST) → **Core Erlang** → **BEAM SSA** → BEAM **assembly** → **BEAM binary**, driven by `compile.erl`.
- Abstract level: `compile.erl` pass funs `parse_module` (epp), `transform_module` (parse transforms), `lint_module` (`erl_lint`), `beam_docs`, `remove_doc_attributes`, `compile_directives`, `expand_records`, `legalize_vars`, `core` (→ `v3_core`).
  - `sys_pre_attributes` reorders/normalizes source attributes; `sys_messages` formats errors/warnings; `sys_coverage` instruments code for line coverage (`line_coverage`); `beam_doc` builds the EEP-48 `Docs` chunk.
- Core Erlang: `v3_core` (AST→Core); `cerl`, `cerl_trees`, `cerl_clauses`, `core_lib` (AST API); `core_scan` + `core_parse.yrl`(→ generated `core_parse.erl`) parse Core text; `core_pp` pretty-prints it; `core_lint` validates; `rec_env` is the record environment (`cerl_inline`).
  - Optimizers: `sys_core_prepare`, `sys_core_fold` (+ `sys_core_fold_lists` for lists inlining), `cerl_inline` + `sys_core_inline` (inliner, `core_inline_module`), `sys_core_alias`, `sys_core_bsm`.
- BEAM SSA: `beam_core_to_ssa` (Core→SSA); `beam_ssa` + `beam_ssa.hrl` (format/utilities), `beam_ssa_lint` (internal consistency checks), `beam_ssa_pp` (pretty-printer), `beam_ssa_check` (`check_ssa` assertions in test suites).
  - SSA passes run from `kernel_passes/0`, in order: `beam_ssa_bool` → `beam_ssa_share` → `beam_ssa_recv` → `beam_ssa_bsm` → `beam_ssa_opt` → `beam_ssa_throw`.
  - `beam_ssa_dead`, `beam_ssa_bc_size`, `beam_ssa_alias`, `beam_ssa_destructive_update` run as sub-passes *inside* `beam_ssa_opt` (as does `beam_ssa_share:block/2`).
  - Type system: `beam_types` + `beam_types.hrl` (type lattice), `beam_ssa_type` (SSA type analysis run inside `beam_ssa_opt` via `opt_start/opt_continue/opt_finish`), `beam_call_types` (call return-type inference), `beam_bounds` (tight integer bounds, "Hacker's Delight").
  - `beam_ssa_pre_codegen` (prepare SSA for codegen: register allocation, frame size, native records), then `beam_ssa_codegen` (SSA→assembly).
- BEAM assembly/codegen: `beam_a` (normalize), `beam_block`, `beam_jump`, `beam_clean`, `beam_trim` (stack trimming), `beam_flatten`, `beam_z` (inverse of `beam_a` renames), `beam_utils`, `beam_validator` (strong/weak stack/register validation), `beam_asm` (final assembler, emits the BEAM binary), `beam_dict` (literal/atom table), `beam_listing`, `beam_disasm` (+ `beam_disasm.hrl`).

## The pass pipeline is data
- `passes/2` → `passes_1/1` picks the initial pass list by input type (`from_abstr`, `from_core`, `from_asm`, else `parse_module` + `standard_passes/0`); `fix_first_pass/1` adapts it for `forms` input.
- Pass-list builders define each stage: `standard_passes/0` (abstract + `abstr_passes/1`), `abstr_passes/1`, `core_passes/1`, `kernel_passes/0` (Core→SSA→codegen), `asm_passes/0` (assembly opts), `binary_passes/0`.
- `select_passes/2` expands commands into a flat `[{Name,Fun}]` list; `select_cond/5` + `select_list_passes/1` resolve conditions. Command forms: `{pass,Mod}` (calls `Mod:module/2`), `?pass(Name)`/`{Name,Fun}`, `{Name,Test,Fun}`, `{listing,Ext}`, `{src_listing,Ext}`, `done`/`{done,Ext}`, `{iff,Flag,Cmd}`, `{unless,Flag,Cmd}`, `{delay,[Cmds]}`.
- Pass state is the private (unnamed) `-record #compile{}` in `compile.erl` (filename, module, abstract_code, options, mod_options, errors, warnings, extra_chunks).
- New optimizations are inserted by editing the relevant builder list (`core_passes`/`kernel_passes`/`asm_passes`); toggleable passes are wrapped in `{unless,no_<x>_opt,…}` (see the `no_*` / listing flag options in `expand_opt/2` and `options/0`).
- `run_sub_passes/2` runs phase-level sub-passes with optional timing; the `time` option collects per-pass and per-sub-pass times (`?PASS_TIMES` / `?SUB_PASS_TIMES`).

## Core Erlang stage (`core_passes/1`, `compile.erl:1925-1952`)
Runs under `unless,no_copt` inside a `{delay,…}`, in order: `core_old_inliner` → `sys_core_fold` → `core_inline_module` → `core_fold_after_inlining` → `sys_core_alias` → `core_transforms`; then `kernel_passes/0` (`compile.erl:1954`) runs `sys_core_bsm` before `core_to_ssa`.
- `sys_core_prepare` (receive lowering) runs only for `non_verified_core` input (`compile.erl:1927-1932`), i.e. Core generated from Erlang, not verbatim `.core`.
- TWO Core inliners coexist:
  - legacy `sys_core_inline` via `core_old_inliner` (gate `test_old_inliner`, `compile.erl:2746` — any `{inline,_}` option); node-count weight threshold (`-record(inline,{exports,thresh,inline})`); effectively a no-op without explicit inline options.
  - default modern `cerl_inline:core_transform/2` via `core_inline_module` (gate `test_core_inliner`, `compile.erl:2753` — any `inline` option unless `no_inline`); Waddell/Dybvig algorithm, effort/size/unroll counters `default_effort()`=150, `default_size()`=24, `default_unroll()`=1 (`cerl_inline.erl:118-120`).
- `sys_core_fold` optimizes each function to a fixed point (`find_fixpoint/3`, max 20 iterations, `sys_core_fold.erl:133-150`): constant folding, call/case/guard/boolean/comprehension simplification, list-function inlining (`sys_core_fold_lists`).
- `core_lib` is small (180 lines), exports only `make_values/1` and `is_var_used/2` — not a general Core AST library.

## BEAM SSA optimizer structure (`beam_ssa_opt.erl`)
`module/2` (`beam_ssa_opt.erl:111`) runs six phases: `prologue_passes` → `module_passes` → `repeated_passes` (to fixpoint, `?MAX_REPETITIONS`=16) → `early_epilogue_passes` → `epilogue_module_passes` → `late_epilogue_passes` (lines ~304-382).
- Each phase runs every sub-pass on all functions before the next phase; a sub-pass is an in-file `ssa_opt_*` function or a delegate to a module: `ssa_opt_bc_size`→`beam_ssa_bc_size:opt/1`, `ssa_opt_alias`→`beam_ssa_alias:opt/2`, `ssa_opt_destructive_update`→`beam_ssa_destructive_update:opt/2`, `ssa_opt_type_start/continue/finish`→`beam_ssa_type:opt_*`.
- Function order is chosen by call order and alternated each fixpoint iteration; only changed functions (SSA or type info) are re-run (`changed/5`, `changed_types/5`).

## Type system (SSA)
- `beam_types` + `beam_types.hrl`: lattice of normal-form records (`#t_atom/#t_bitstring/#t_bs_context/#t_bs_matchable/#t_cons/#t_float/#t_fun/#t_integer/#t_list/#t_map/#t_number/#t_record/#t_tuple`) plus one `#t_union{}` (six non-overlapping slots); ops `meet/1,2`, `join/1,2`, `subtract/2`, `glb/2`, `lub/2`, `normalize/1`, `limit_depth/1`; union simplification is only `shrink_union/1` + `normalize/1` (no digraph).
- `beam_ssa_type`: `opt_start/2` (module-wide signature/success-typing pass), `opt_continue/4`, `opt_finish/3` (joins per-call-site param types and publishes the `parameter_info` annotation). Cross-function types flow through `#func_info{arg_types,succ_types}` (`beam_ssa_opt.erl:51-78`).
- `beam_call_types`: `types/3` (return type + arg types for builtins/known funcs), `will_succeed/3`, `arith_type/2`.
- `beam_bounds`: integer range lattice — `bounds/2,3`, `relop/3`, `infer_relop_types/3`, `is_masking_redundant/2`; ranges cap at `?NUM_BITS`=128.
- External type format: `beam_types:encode_ext/1` / `decode_ext/1` / `convert_ext/2` emit a compact `Type` chunk (version `?BEAM_TYPES_VERSION` in `beam_types.hrl`) consumed by the erts JIT; encoded in `beam_dict`/`beam_asm`, decoded by `beam_disasm`.

## Loop, fusion, specialization & plug-in point
- **No general loop optimizations exist**: no loop-invariant code motion, unrolling, loop strength reduction, or induction-variable optimization. The two "strength reduction" comments are local peepholes (`live_opt_is/3` ~`beam_ssa_opt.erl:1767`; `reduce_try` ~`:1920`). `beam_ssa:is_loop_header/1` (`beam_ssa.erl:397`) is receive-only (`peek_message`/`wait_timeout`).
- Loop-shaped infrastructure that DOES exist: `beam_ssa:rpo/1,2`, `linearize/1`, `dominators/2`, `dominators_from_predecessors/2`, `common_dominators/3`, `predecessors/1`, `successors/1` — a new loop pass must build its own loop recognition on these.
- Existing fusion — all block-local peepholes in `beam_ssa_opt.erl`; none fuses mixed ops or crosses blocks:
  - `merge_updates_bs` (~`:525`): merge consecutive `update_record` on the same record; folds `put_tuple`+`update_record`.
  - `update_tuple_opt` (~`:1081`): merge a chain of `setelement/3` into one `update_tuple` pseudo-instruction (undone by `ssa_opt_deoptimize_update_tuple` ~`:3953`, re-expanded in `beam_ssa_pre_codegen:expand_update_tuple`).
  - CSE `combine_put_maps` (~`:1392`): merge a `put_map` whose source is a preceding assoc `put_map`.
  - `record_opt` (~`:990`): fuse `is_tuple`+`tuple_size`+`get_tuple_element(0)` → `is_tagged_tuple`.
  - `ssa_opt_element` (~`:917`): reorder `element/2` calls (highest index first; no fusion).
- Destructive/in-place reuse chaining is module-level: `beam_ssa_destructive_update.erl` (+ `beam_ssa_alias.erl` for uniqueness) marks `update_record` with the `inplace` hint and rewrites growing `bs_create_bin` chains to `bs_writable_binary` + `private_append`.
- Specialization points: `ssa_opt_is_between` (~`:625`) rewrites `is_integer/3` range tests to `is_integer/1` + two `=<`; `ssa_opt_ne` (~`:2762`) rewrites `=/=`→`=:=` / `/=`→`==` and swaps branch targets; `ssa_opt_sw` (~`:2713`) turns a single-value/boolean `#b_switch{}` into `#b_br{}`/`is_boolean`. Type-driven opcode selection lives in `beam_ssa_type`/`beam_call_types` and codegen (`beam_ssa_codegen`: `is_tagged_tuple` constraints, `has_map_field`→`has_map_fields`, `get_map_element`→`get_map_elements`). `beam_ssa_bc_size` computes exact writable-binary sizes.
- **Plug-in point for a new SSA pass**: add a `?PASS(name)`-wrapped `name/1 :: #opt_st{} -> #opt_st{}` fun to `prologue_passes/1`, `repeated_passes/0` (the fixpoint loop), `early_epilogue_passes/1`, or `late_epilogue_passes/1` in `beam_ssa_opt.erl` (pass lists are data; `passes_1/2` auto-generates the `no_<name>` toggle). Module-level passes take `{StMap, FuncDb}` and are wired in `module_passes/1` / `epilogue_module_passes/1`. Sub-pass contract is `compile:run_sub_passes/2` (runs `Fun(St) -> St`, ~`compile.erl:1187`). A wholly new IR would sit between `core_to_ssa` (`compile.erl:1962`) and `beam_ssa_pre_codegen` (`:1992`).

## `genop.tab` — external generic instruction table (format)
- Input to `erts/emulator/utils/beam_makeops -compiler` → `$(EGEN)/beam_opcodes.erl` (numbered opcode table, `format_number/0`) + `.hrl`; the **same** file feeds the emulator build, so opcode numbers must stay backward compatible. `Makefile:178` drives it; `OPCODES-GENERATED` is the git-ignored marker.
- One declaration per line: `N: name/arity` (external generic instruction) or `N: -name/arity` (obsoleted — `-` prefix: compiler must not emit it, loader refuses it). `N` is the opcode, 1-based, unique, never reused; new instructions must be **appended** with higher numbers (arity = operand count).
- Opcodes currently **1..193** (193 declared): 134 live, 59 obsolete (`-`-prefixed). Only `name/arity` is declared here — operand **types are NOT in this file**; they live in the emulator's `ops.tab` (types `x/y/l/i/a/n/q/f/p/u/…`).
- `BEAM_FORMAT_NUMBER=0` (line 22) — instruction-set format version stored in the BEAM code header; only bump if an existing op changes number/arity/semantics.
- `#` lines are comments; `## @spec …`/`## @doc …` lines are documentation **not consumed by `beam_makeops`**. No `%if`/`%hot`/C-code directives here (those are emulator `.tab` features).
- Existing opcodes' number/arity/semantics must not change (see `erts/emulator/internal_doc/beam_makeops.md`).

## SSA→BEAM lowering (`beam_ssa_codegen.erl`, entry `module/2`)
- Per function, `cg_fun/3` (`:213`) runs: `linearize` → `need_heap` → `prefer_xregs` → `liveness` → `defined` → `opt_allocate` → `fix_wait_timeout`/`add_debug_info` → `cg_linear` → `bs_translate`.
- Register allocation is NOT done here: it reads the var→`{x,N}|{y,N}|{z,N}|{fr,N}` map from the `registers` annotation (`:136`) and `frame_size` (`:2633`), both produced by `beam_ssa_pre_codegen`. This module computes liveness/`live`/`live_yregs` annotations and `allocate`/`test_heap` sizing.
- `#b_set{}` ops dispatch through `cg_instr/4` (values, `:2425`), `cg_test/5` (tests, `:2474`), `cg_call/4` (calls, `:2196`); `{bif,_}` → `{bif,...}`/`{gc_bif,...}` or a `{test,...}` via `bif_to_test/4` (`:2020`). `gc_bif` vs `bif` is a hardcoded table `is_gc_bif/2` (`:3026`).
- Operands from `beam_arg/2` (`:2962`): `#b_var{}`→register, `#b_literal{}`→`{atom,V}|{float,V}|{integer,V}|nil|{literal,V}`; labels via `use_block_label/2` (`:2987`) / `bif_fail/1` (`:1564`) / `ensure_label/2` (`:1578`); `typed_args/3` (`:2945`) wraps in `#tr{}`.
- `bs_*` is its own mini-instruction set: `bs_translate/1` (`:2829`) collects match ops into one `{bs_match,...,{commands,[...]}}` (`ensure_at_least`/`ensure_exactly`/`binary`/`integer`/`skip`/`get_tail`/`'=:='`) via `bs_translate_instr/1` (`:2909`).

## Generated files — never hand-edit
- `beam_opcodes.erl` / `beam_opcodes.hrl` — generated into `$(EGEN)` by `erts/emulator/utils/beam_makeops` (Perl) from `genop.tab`; marker file `OPCODES-GENERATED` (git-ignored).
- `core_parse.erl` — yecc output from `core_parse.yrl` (git-ignored).
- `compiler.app` / `compiler.appup` — generated from `compiler.app.src` / `compiler.appup.src` by `sed` substituting `%VSN%`.
- `ebin/` — build output (only `.gitignore` tracked).

## Build (`lib/compiler/src/Makefile`)
- `VSN = $(COMPILER_VSN)` from `../vsn.mk`; `ERL_COMPILE_FLAGS` include `-Werror +inline +warn_missing_spec -I../../stdlib/include -I$(EGEN)`.
- `beam_asm.erl` is compiled with `-DCOMPILER_VSN` (version recorded in the generated binary); `core_parse.erl` is compiled with `+inline` stripped (inlining it is slow with no benefit).
- `genop.tab` is the opcode source table (executable, Perl input).

## Constraints
- The compiler must build very early: it is the first group in `lib/Makefile` (`ERTS_APPLICATIONS = stdlib sasl kernel compiler`) and may only depend on `kernel` + `stdlib` at compile time.
- `compiler.app.src` declares `{applications,[kernel,stdlib]}`; `crypto` is a **runtime** dependency only (debug_info encryption) — used via `beam_lib:get_crypto_key/1` and `crypto:crypto_one_time/6` in `compile.erl` `encrypt_debug_info`/`debug_info_key` handling.
- Keep the code `-Werror` clean and fully spec'd (`+warn_missing_spec` is enforced).
- The public `compile` API and `cerl` API are widely used by other apps (e.g. `dialyzer`, `syntax_tools`, `eunit`, `lfe`) — treat them as stable.

## Notes for Agents
- Everything is a module, not a directory: to change a stage, edit the owning module and (if it is a toggleable pass) the corresponding list in `compile.erl`.
- Low-level BEAM assembly passes run in this fixed order (`compile.erl` `asm_passes/0`): `beam_a` → `beam_block` → `beam_jump` → `beam_clean` → `beam_trim` → `beam_flatten` (under `unless no_postopt`), then `beam_z`; they operate on the flat instruction stream and (apart from `beam_a`/`beam_z`) use the `{block,[{set,Dst,Src,Op}]}` intermediate representation created by `beam_block`. `beam_a`/`beam_z` are a rename/undo-rename pair.
- `beam_utils` is minimal: only `replace_labels/4` and `split_even/1`. BIF classification helpers (`is_pure/3`, `is_safe/3`, `is_exit_bif/3`) live in `erl_bifs`, NOT in `beam_utils`; low-level register-inspecting passes must handle `#tr{}` type-tagged regs (`beam_asm.hrl`).
- `erl_bifs` is a static table of Erlang BIF properties consumed by Core and SSA optimizers (`sys_core_fold`, `beam_ssa_type`, `beam_call_types`, `beam_ssa_opt`, …).
- Test coverage is in the sibling `../test/` directory (many `*_SUITE.erl` named after passes); `compiler.spec` + `compiler.cover` there drive coverage runs. Writes to `../test/` belong to the parent node.

## Routing Table
- `compile.erl` → public compile API, pass-pipeline-as-data, `#compile{}` state, pass funs (parse/lint/transform only live here)
- `compiler.app.src`, `compiler.appup.src` → application resource templates (version substituted at build)
- Erlang abstract/AST & attributes: `sys_pre_attributes`, `sys_messages`, `sys_coverage`, `beam_doc`
- Core Erlang AST API & tools: `cerl`, `cerl_trees`, `cerl_clauses`, `core_lib`, `core_pp`, `core_scan`, `core_parse.yrl`, `core_lint`, `rec_env`
- AST→Core and Core optimizers: `v3_core`, `sys_core_prepare`, `sys_core_fold`, `sys_core_fold_lists`, `cerl_inline`, `sys_core_inline`, `sys_core_alias`, `sys_core_bsm`
- Core→SSA and SSA format: `beam_core_to_ssa`, `beam_ssa`, `beam_ssa.hrl`, `beam_ssa_lint`, `beam_ssa_pp`, `beam_ssa_check`
- SSA optimization loop & sub-passes: `beam_ssa_opt` (six-phase driver), `beam_ssa_dead`, `beam_ssa_bc_size`, `beam_ssa_destructive_update`, `beam_ssa_alias`, `beam_ssa_ss`
- SSA pass modules run from `compile.erl` `kernel_passes/0`: `beam_ssa_bool`, `beam_ssa_share`, `beam_ssa_recv`, `beam_ssa_bsm`, `beam_ssa_throw`
- Type analysis: `beam_types`, `beam_types.hrl`, `beam_ssa_type`, `beam_call_types`, `beam_bounds`
- SSA→codegen: `beam_ssa_pre_codegen`, `beam_ssa_codegen`
- BEAM assembly & validation: `beam_asm`, `beam_asm.hrl`, `beam_a`, `beam_block`, `beam_jump`, `beam_clean`, `beam_trim`, `beam_flatten`, `beam_z`, `beam_utils`, `beam_dict`, `beam_validator`, `beam_listing`
- `beam_validator.erl` → abstract-interpretation validator of BEAM assembly; exports `validate/2` + `format_error/1`; run as the `beam_validator_strong`/`beam_validator_weak` passes (not called from `beam_asm`). Big `vi/2` dispatch, CFG dataflow in `#vst{}`/`#st{}`, `#value{}` lattice over `beam_types` plus validator-only `#t_abstract{}`; last-line safety net for optimizer correctness.
- Generated opcode tables: `genop.tab` (input) → `beam_opcodes.erl`/`.hrl` via `erts/emulator/utils/beam_makeops`
- Disassembly / BIF table: `beam_disasm`, `beam_disasm.hrl`, `erl_bifs`
- `../test/` → compiler Common Test suites (sibling — read-only, escalate writes to parent)
- `../doc/` → ExDoc-built compiler documentation (sibling — read-only, escalate writes to parent)
