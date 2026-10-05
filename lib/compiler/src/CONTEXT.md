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
- `cerl`/`cerl_clauses`/`cerl_trees`/`cerl_inline` are the documented Core Erlang AST API (used by `lfe`, `dialyzer`-adjacent tools, etc.); `core_lib` is the (undocumented) Core AST helper library used externally too.

## The compilation pipeline (intermediate representations)
Source → Erlang **abstract code** (AST) → **Core Erlang** → **BEAM SSA** → BEAM **assembly** → **BEAM binary**, driven by `compile.erl`.
- Abstract level: `compile.erl` pass funs `parse_module` (epp), `transform_module` (parse transforms), `lint_module` (`erl_lint`), `beam_docs`, `remove_doc_attributes`, `compile_directives`, `expand_records`, `legalize_vars`, `core` (→ `v3_core`).
  - `sys_pre_attributes` reorders/normalizes source attributes; `sys_messages` formats errors/warnings; `sys_coverage` instruments code for line coverage (`line_coverage`); `beam_doc` builds the EEP-48 `Docs` chunk.
- Core Erlang: `v3_core` (AST→Core); `cerl`, `cerl_trees`, `cerl_clauses`, `core_lib` (AST API); `core_scan` + `core_parse.yrl`(→ generated `core_parse.erl`) parse Core text; `core_pp` pretty-prints it; `core_lint` validates; `rec_env` is the record environment (`cerl_inline`).
  - Optimizers: `sys_core_prepare`, `sys_core_fold` (+ `sys_core_fold_lists` for lists inlining), `cerl_inline` + `sys_core_inline` (inliner, `core_inline_module`), `sys_core_alias`, `sys_core_bsm`.
- BEAM SSA: `beam_core_to_ssa` (Core→SSA); `beam_ssa` + `beam_ssa.hrl` (format/utilities), `beam_ssa_lint` (internal consistency checks), `beam_ssa_pp` (pretty-printer), `beam_ssa_check` (`check_ssa` assertions in test suites).
  - Optimizers (sub-passes inside the `beam_ssa_opt` family): `beam_ssa_bool`, `beam_ssa_share`, `beam_ssa_recv`, `beam_ssa_bsm`, `beam_ssa_throw`, `beam_ssa_dead`, `beam_ssa_bc_size`, `beam_ssa_destructive_update`, `beam_ssa_alias` (uses `beam_ssa_ss`, the backward-dataflow sub-pass framework), and `beam_ssa_opt` (main SSA optimizer loop).
  - Type system: `beam_types` + `beam_types.hrl` (type lattice), `beam_ssa_type` (SSA type analysis run inside `beam_ssa_opt` via `opt_start/opt_continue/opt_finish`), `beam_call_types` (call return-type inference), `beam_bounds` (tight integer bounds, "Hacker's Delight").
  - `beam_ssa_pre_codegen` (prepare SSA for codegen), then `beam_ssa_codegen` (SSA→assembly).
- BEAM assembly/codegen: `beam_a` (reorder/annotate), `beam_block`, `beam_jump`, `beam_clean`, `beam_trim` (stack trimming), `beam_flatten`, `beam_z`, `beam_utils`, `beam_validator` (strong/weak stack/register validation), `beam_asm` (final assembler, emits the BEAM binary), `beam_dict` (literal/atom table), `beam_listing`, `beam_disasm` (+ `beam_disasm.hrl`).

## The pass pipeline is data
- `passes/2` → `passes_1/1` picks the initial pass list by input type (`from_abstr`, `from_core`, `from_asm`, else `parse_module` + `standard_passes/0`); `fix_first_pass/1` adapts it for `forms` input.
- Pass-list builders define each stage: `standard_passes/0` (abstract + `abstr_passes/1`), `abstr_passes/1`, `core_passes/1`, `kernel_passes/0` (Core→SSA→codegen), `asm_passes/0` (assembly opts), `binary_passes/0`.
- `select_passes/2` expands commands into a flat `[{Name,Fun}]` list; `select_cond/5` + `select_list_passes/1` resolve conditions. Command forms: `{pass,Mod}` (calls `Mod:module/2`), `?pass(Name)`/`{Name,Fun}`, `{Name,Test,Fun}`, `{listing,Ext}`, `{src_listing,Ext}`, `done`/`{done,Ext}`, `{iff,Flag,Cmd}`, `{unless,Flag,Cmd}`, `{delay,[Cmds]}`.
- Pass state is the private (unnamed) `-record #compile{}` in `compile.erl` (filename, module, abstract_code, options, mod_options, errors, warnings, extra_chunks).
- New optimizations are inserted by editing the relevant builder list (`core_passes`/`kernel_passes`/`asm_passes`); toggleable passes are wrapped in `{unless,no_<x>_opt,…}` (see the `no_*` / listing flag options in `expand_opt/2` and `options/0`).
- `run_sub_passes/2` runs phase-level sub-passes with optional timing; the `time` option collects per-pass and per-sub-pass times (`?PASS_TIMES` / `?SUB_PASS_TIMES`).

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
- `erl_bifs` is a static table of Erlang BIF properties consumed by Core and SSA optimizers (`sys_core_fold`, `beam_ssa_type`, `beam_call_types`, `beam_ssa_opt`, …).
- Test coverage is in the sibling `../test/` directory (many `*_SUITE.erl` named after passes); `compiler.spec` + `compiler.cover` there drive coverage runs. Writes to `../test/` belong to the parent node.

## Routing Table
- `compile.erl` → public compile API, pass-pipeline-as-data, `#compile{}` state, pass funs (parse/lint/transform only live here)
- `compiler.app.src`, `compiler.appup.src` → application resource templates (version substituted at build)
- Erlang abstract/AST & attributes: `sys_pre_attributes`, `sys_messages`, `sys_coverage`, `beam_doc`
- Core Erlang AST API & tools: `cerl`, `cerl_trees`, `cerl_clauses`, `core_lib`, `core_pp`, `core_scan`, `core_parse.yrl`, `core_lint`, `rec_env`
- AST→Core and Core optimizers: `v3_core`, `sys_core_prepare`, `sys_core_fold`, `sys_core_fold_lists`, `cerl_inline`, `sys_core_inline`, `sys_core_alias`, `sys_core_bsm`
- Core→SSA and SSA format: `beam_core_to_ssa`, `beam_ssa`, `beam_ssa.hrl`, `beam_ssa_lint`, `beam_ssa_pp`, `beam_ssa_check`
- SSA optimizations: `beam_ssa_opt`, `beam_ssa_bool`, `beam_ssa_share`, `beam_ssa_recv`, `beam_ssa_bsm`, `beam_ssa_throw`, `beam_ssa_dead`, `beam_ssa_bc_size`, `beam_ssa_destructive_update`, `beam_ssa_alias`, `beam_ssa_ss`
- Type analysis: `beam_types`, `beam_types.hrl`, `beam_ssa_type`, `beam_call_types`, `beam_bounds`
- SSA→codegen: `beam_ssa_pre_codegen`, `beam_ssa_codegen`
- BEAM assembly & validation: `beam_asm`, `beam_asm.hrl`, `beam_a`, `beam_block`, `beam_jump`, `beam_clean`, `beam_trim`, `beam_flatten`, `beam_z`, `beam_utils`, `beam_dict`, `beam_validator`, `beam_listing`
- Generated opcode tables: `genop.tab` (input) → `beam_opcodes.erl`/`.hrl` via `erts/emulator/utils/beam_makeops`
- Disassembly / BIF table: `beam_disasm`, `beam_disasm.hrl`, `erl_bifs`
- `../test/` → compiler Common Test suites (sibling — read-only, escalate writes to parent)
- `../doc/` → ExDoc-built compiler documentation (sibling — read-only, escalate writes to parent)
